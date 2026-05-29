use std::{
    io::{self, BufRead},
    thread::sleep,
    time::Duration,
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use function_name::named;
use ignorable::PartialEq;
use jiff::{Timestamp, Zoned};
use log::{debug, error, info, trace, warn};
use pretty_assertions::Comparison;
use prost::Message;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::TomlError;

use crate::{
    config::TeslaConfig,
    poll_thread::Pollable,
    tesla::{
        command_signing::{CommandSigner, SigningError},
        proto::{
            car_server::{
                Action, ChargingSetLimitAction, ChargingStartStopAction, OperationStatusE,
                Response, SetChargingAmpsAction, VehicleAction, Void, action::ActionMsg,
                charging_start_stop_action::ChargingAction as StartStopAction, result_reason,
                vehicle_action::VehicleActionMsg,
            },
            signatures::SessionInfo,
            universal_message::{
                Destination, Domain, RoutableMessage, destination::SubDestination,
                routable_message::Payload,
            },
        },
    },
};

mod command_signing;
mod proto;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);
pub const VERY_SLOW_POLL_INTERVAL: Duration = Duration::from_hours(2);

const AUTH_BASE: &str = "https://fleet-auth.prd.vn.cloud.tesla.com";
const COMMAND_API_BASE: &str = "https://fleet-api.prd.na.vn.cloud.tesla.com";
const SCOPES: &str =
    "openid offline_access vehicle_device_data vehicle_location vehicle_charging_cmds";
const REDIRECT_URI: &str = "https://auth.tesla.com/void/callback";

#[derive(Error, Debug)]
pub enum TeslaError {
    // Vehicle errors
    #[error("Charger disconnected")]
    ChargerDisconnected,
    #[error("Charger not providing power")]
    ChargerWithoutPower,
    #[error("Car sleeping")]
    CarSleeping,

    // Protocol errors
    #[error("Unknown response {1} to {0}")]
    UnknownCommandResponse(&'static str, String),
    #[error("Authentication error: {0}")]
    AuthError(&'static str),
    #[error("Signing error {0}")]
    SigningError(#[from] SigningError),

    // IO errors
    #[error("Request error: {0}")]
    UreqError(#[from] ureq::Error),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    // Decoding errors
    #[error("TOML error: {0}")]
    TomlError(#[from] TomlError),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Jiff error: {0}")]
    JiffError(#[from] jiff::Error),
    #[error("Base64 decode error {0}")]
    Base64DecodeError(#[from] base64::DecodeError),
    #[error("Proto decode error {0}")]
    ProtoDecodeError(#[from] prost::DecodeError),
}

pub type TeslaResult<T> = Result<T, TeslaError>;

struct UserToken {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize, Debug)]
pub struct VehicleDataResponseEnvelope {
    response: VehicleData,
}

#[derive(Deserialize, Debug, Default, PartialEq)]
pub struct VehicleData {
    pub charge_state: VehicleChargeState,
    pub vehicle_state: VehicleState,
}

#[derive(Deserialize, Debug, Default, PartialEq)]
pub struct VehicleChargeState {
    pub battery_level: u8,
    pub charge_amps: u16,
    pub charge_current_request: u16,
    pub charge_current_request_max: u16,
    pub charge_limit_soc: u8,
    pub charge_limit_soc_max: u8,
    pub charge_port_latch: String,
    pub charge_rate: f32,
    pub charger_voltage: u16,
    pub charging_state: String,
    pub minutes_to_full_charge: u16,
}

#[derive(Deserialize, Debug, Default, PartialEq)]
pub struct VehicleState {
    pub homelink_nearby: bool,
}

#[derive(Deserialize, Debug)]
struct CommandResponse {
    pub result: bool,
    pub reason: String,
}

pub struct TeslaVehicle {
    config: TeslaConfig,
    http: ureq::Agent,
    signer: CommandSigner,
    pub data: VehicleData,
    pub last_update: Option<Timestamp>,
    pub last_wake: Option<Timestamp>,
}

impl Pollable for TeslaVehicle {
    fn name(&self) -> &'static str {
        "TeslaVehicle"
    }

    fn init(&mut self) -> crate::NrgyResult<()> {
        match self.update_state() {
            Ok(_) => Ok(()),
            Err(e) => match e {
                TeslaError::CarSleeping => {
                    info!("Car is asleep.  Assuming full.");
                    self.data = VehicleData {
                        charge_state: VehicleChargeState {
                            battery_level: 80,
                            charging_state: "Complete".to_string(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    Ok(())
                }
                e => Err(e)?,
            },
        }
    }

    fn poll(&mut self) -> crate::NrgyResult<()> {
        self.update_state()?;
        Ok(())
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_POLL_INTERVAL
    }
}

#[expect(dead_code)]
impl TeslaVehicle {
    pub fn new(config: TeslaConfig) -> TeslaResult<Self> {
        let signer = CommandSigner::new(&config.private_key, &config.vin)?;

        Ok(TeslaVehicle {
            config,
            http: ureq::Agent::config_builder()
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .disable_verification(true)
                        .build(),
                )
                .build()
                .new_agent(),
            signer,
            data: VehicleData::default(),
            last_update: None,
            last_wake: None,
        })
    }

    pub fn establish_session(&mut self) -> TeslaResult<()> {
        info!("Establishing signed command session");
        let req_bytes = self
            .signer
            .session_info_request(Domain::Infotainment)
            .encode_to_vec();

        let vin = &self.config.vin;
        let resp_msg = self.post_authenticated(
            &format!("{COMMAND_API_BASE}/api/1/vehicles/{vin}/signed_command"),
            serde_json::json!({ "routable_message": STANDARD.encode(&req_bytes) }),
            self.load_user_token()?,
        )?;

        let session_info_bytes = match resp_msg.payload {
            Some(Payload::SessionInfo(b)) => b,
            Some(p) => Err(TeslaError::UnknownCommandResponse(
                "session_info",
                format!("wrong payload {p:?}"),
            ))?,
            _ => todo!(),
        };
        self.signer
            .update_session(&SessionInfo::decode(session_info_bytes.as_slice())?)?;

        Ok(())
    }

    pub fn is_home(&self) -> bool {
        self.data.vehicle_state.homelink_nearby
    }

    pub fn plugged_in(&self) -> bool {
        self.data.charge_state.charging_state != "Disconnected"
    }

    pub fn battery_soc(&mut self) -> u8 {
        self.data.charge_state.battery_level
    }

    pub fn is_charging(&self) -> bool {
        let charging_state = self.data.charge_state.charging_state.as_str();
        match charging_state {
            "Charging" => true,
            "Stopped" | "Disconnected" => false,
            _ => false,
        }
    }

    pub fn is_full(&self) -> bool {
        let charging_state = self.data.charge_state.charging_state.as_str();
        match charging_state {
            "Complete" => true,
            "Charging" | "Stopped" | "Disconnected" => false,
            _ => false,
        }
    }

    pub fn charging_amps(&self) -> u16 {
        self.data.charge_state.charge_amps
    }

    pub fn charge_limit(&self) -> u8 {
        self.data.charge_state.charge_limit_soc
    }

    #[named]
    pub fn charge_start(&mut self) -> TeslaResult<()> {
        if self.is_charging() {
            trace!("Got request to start charging, already charging");
            return Ok(());
        }

        info!(
            "Sending charge start to car (state: {})",
            self.data.charge_state.charging_state
        );
        let resp = self.send_signed_command(Action {
            action_msg: Some(ActionMsg::VehicleAction(VehicleAction {
                vehicle_action_msg: Some(VehicleActionMsg::ChargingStartStopAction(
                    ChargingStartStopAction {
                        charging_action: Some(StartStopAction::Start(Void {})),
                    },
                )),
            })),
        })?;
        if !resp.result {
            match resp.reason.as_str() {
                "complete" | "is_charging" | "requested" => Ok(()),
                "disconnected" => Err(TeslaError::ChargerDisconnected)?,
                "no_power" => Err(TeslaError::ChargerWithoutPower)?,
                _ => Err(TeslaError::UnknownCommandResponse(
                    function_name!(),
                    resp.reason,
                ))?,
            }
        } else {
            self.data.charge_state.charging_state = "Charging".into();
            Ok(())
        }
    }

    #[named]
    pub fn charge_stop(&mut self) -> TeslaResult<()> {
        if !self.is_charging() {
            trace!("Got request to stop charging, not charging");
            return Ok(());
        }

        info!("Sending charge stop to car");
        let resp = self.send_signed_command(Action {
            action_msg: Some(ActionMsg::VehicleAction(VehicleAction {
                vehicle_action_msg: Some(VehicleActionMsg::ChargingStartStopAction(
                    ChargingStartStopAction {
                        charging_action: Some(StartStopAction::Stop(Void {})),
                    },
                )),
            })),
        })?;
        if !resp.result {
            match resp.reason.as_str() {
                "not_charging" => Ok(()),
                _ => Err(TeslaError::UnknownCommandResponse(
                    function_name!(),
                    resp.reason,
                ))?,
            }
        } else {
            self.data.charge_state.charging_state = "Stopped".into();
            Ok(())
        }
    }

    #[named]
    pub fn set_charging_amps(&mut self, amps: u8) -> TeslaResult<()> {
        let current_request = self.data.charge_state.charge_current_request;
        trace!("Got request for {amps} amps, currently {current_request}");
        if current_request == amps as u16 {
            debug!("Got request for {amps} amps, already set at {current_request}");
            return Ok(());
        }

        info!("Changing car charge amps from {current_request} to {amps}");
        let resp = self.send_signed_command(Action {
            action_msg: Some(ActionMsg::VehicleAction(VehicleAction {
                vehicle_action_msg: Some(VehicleActionMsg::SetChargingAmpsAction(
                    SetChargingAmpsAction {
                        charging_amps: amps as i32,
                    },
                )),
            })),
        })?;
        if !resp.result {
            Err(TeslaError::UnknownCommandResponse(
                function_name!(),
                resp.reason,
            ))?
        } else {
            self.data.charge_state.charge_amps = amps as u16;
            Ok(())
        }
    }

    #[named]
    pub fn set_charge_limit(&mut self, percent: u8) -> TeslaResult<()> {
        let resp = self.send_signed_command(Action {
            action_msg: Some(ActionMsg::VehicleAction(VehicleAction {
                vehicle_action_msg: Some(VehicleActionMsg::ChargingSetLimitAction(
                    ChargingSetLimitAction {
                        percent: percent as i32,
                    },
                )),
            })),
        })?;
        if !resp.result {
            match resp.reason.as_str() {
                "already_set" => Ok(()),
                _ => Err(TeslaError::UnknownCommandResponse(
                    function_name!(),
                    resp.reason,
                ))?,
            }
        } else {
            Ok(())
        }
    }

    pub fn wake_up(&mut self) -> TeslaResult<()> {
        let now = Timestamp::now();
        if let Some(last_wake) = self.last_wake {
            let delta = (now - last_wake).get_hours();
            if delta < 6 {
                warn!("Only {delta} hours since last wake, refusing",)
            }
        }

        warn!("Sending wakeup");
        let vin = &self.config.vin;
        let url = format!("{COMMAND_API_BASE}/api/1/vehicles/{vin}/wake_up");
        let agent = self.http.clone();
        let make_req = |token: &str| {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {token}"))
        };

        let token = self.load_user_token()?;
        let resp = match make_req(&token.access_token).send(()) {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => {
                let token = self.refresh_token()?;
                make_req(&token.access_token).send(())?
            }
            Err(e) => return Err(e.into()),
        };

        let body = resp.into_body().read_to_string()?;
        trace!("Response: {body}");
        self.last_wake = Some(now);

        Ok(())
    }

    fn update_state(&mut self) -> TeslaResult<&VehicleData> {
        let update_age =
            (Timestamp::now() - self.last_update.unwrap_or(Timestamp::MIN)).get_seconds();

        let vehicle_data = match self.get_vehicle_data() {
            Ok(data) => data,
            Err(e) => {
                self.handle_sleeping_car(e, update_age)?;
                self.get_vehicle_data()?
            }
        };

        trace!("Got vehicle data: {vehicle_data:?}");
        if self.data != vehicle_data {
            debug!(
                "Vehicle state changed: {}",
                Comparison::new(&self.data, &vehicle_data)
            );
        }
        self.data = vehicle_data;
        self.last_update = Some(Timestamp::now());

        Ok(&self.data)
    }

    fn handle_sleeping_car(&mut self, err: TeslaError, update_age: i64) -> TeslaResult<()> {
        if let TeslaError::CarSleeping = err {
            if is_prime_time() && update_age > 7200 {
                self.wake_up()?;
                sleep(Duration::from_secs(5));
                Ok(())
            } else {
                Err(TeslaError::CarSleeping)
            }
        } else {
            Err(err)?
        }
    }

    pub fn get_vehicle_data(&mut self) -> TeslaResult<VehicleData> {
        let url = vehicle_data_url(&self.config.vin);
        let agent = self.http.clone();
        let make_req = |token: &str| {
            agent
                .get(&url)
                .header("Authorization", &format!("Bearer {token}"))
        };

        let token = self.load_user_token()?;
        let mut resp = match make_req(&token.access_token).call() {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => {
                let token = self.refresh_token()?;
                make_req(&token.access_token).call()?
            }
            Err(ureq::Error::StatusCode(408)) => {
                return Err(TeslaError::CarSleeping);
            }
            Err(e) => {
                if let ureq::Error::StatusCode(code) = &e {
                    error!("Error: {code}");
                }
                return Err(e.into());
            }
        };

        let vehicle_data = resp
            .body_mut()
            .read_json::<VehicleDataResponseEnvelope>()?
            .response;

        validate_charging_state(&vehicle_data.charge_state.charging_state);

        Ok(vehicle_data)
    }

    pub fn authenticate(&self) -> TeslaResult<()> {
        let verifier = random_string(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_string(16);

        let auth_url = format!(
            "{AUTH_BASE}/oauth2/v3/authorize\
             ?client_id={client_id}\
             &locale=en-US\
             &prompt=login\
             &redirect_uri={redirect_uri_enc}\
             &response_type=code\
             &scope={scope_enc}\
             &state={state}\
             &code_challenge={challenge}\
             &code_challenge_method=S256",
            client_id = self.config.client_id,
            redirect_uri_enc = urlencoding::encode(REDIRECT_URI),
            scope_enc = urlencoding::encode(SCOPES),
        );

        println!("Open this URL in a browser:\n\n{auth_url}\n");
        println!("After authorizing, paste the full redirect URL and press Enter:");

        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;

        let code = extract_query_param(input.trim(), "code")
            .ok_or_else(|| TeslaError::AuthError("No 'code' parameter found in URL"))?;

        let token = self.exchange_code(&code, &verifier)?;
        save_tokens_to_config(&token.access_token, &token.refresh_token)?;
        println!("Authentication successful.");
        Ok(())
    }

    fn send_signed_command(&mut self, action: Action) -> TeslaResult<CommandResponse> {
        if !self.signer.has_session() {
            self.establish_session()?;
        }
        let resp_msg = self.dispatch_signed(&action)?;
        let fault = resp_msg
            .signed_message_status
            .as_ref()
            .map(|s| s.signed_message_fault)
            .unwrap_or(0);
        if matches!(fault, 5 | 6 | 15) {
            info!("Session stale (fault {fault}), re-establishing");
            self.signer.invalidate_session();
            self.establish_session()?;
            return Self::parse_command_response(self.dispatch_signed(&action)?);
        }
        Self::parse_command_response(resp_msg)
    }

    fn dispatch_signed(&mut self, action: &Action) -> TeslaResult<RoutableMessage> {
        let mut message = RoutableMessage {
            to_destination: Some(Destination {
                sub_destination: Some(SubDestination::Domain(Domain::Infotainment as i32)),
            }),
            from_destination: Some(Destination {
                sub_destination: Some(SubDestination::RoutingAddress(
                    rand::random::<[u8; 16]>().to_vec(),
                )),
            }),
            payload: Some(Payload::ProtobufMessageAsBytes(action.encode_to_vec())),
            uuid: rand::random::<[u8; 16]>().to_vec(),
            ..Default::default()
        };
        self.signer
            .authorize_hmac(&mut message, Duration::from_secs(30))?;
        let msg_bytes = message.encode_to_vec();
        let vin = self.config.vin.clone();
        let token = self.load_user_token()?;
        self.post_authenticated(
            &format!("{COMMAND_API_BASE}/api/1/vehicles/{vin}/signed_command"),
            serde_json::json!({ "routable_message": STANDARD.encode(&msg_bytes) }),
            token,
        )
    }

    fn parse_command_response(resp_msg: RoutableMessage) -> TeslaResult<CommandResponse> {
        let bytes = match resp_msg.payload {
            Some(Payload::ProtobufMessageAsBytes(b)) => b,
            other => {
                return Err(TeslaError::UnknownCommandResponse(
                    "signed_command",
                    format!("unexpected payload: {other:?}"),
                ));
            }
        };
        let response = Response::decode(bytes.as_slice())?;
        let (result, reason) = match response.action_status {
            Some(status) => {
                let ok = status.result == OperationStatusE::OperationstatusOk as i32;
                let reason = status
                    .result_reason
                    .and_then(|r| r.reason)
                    .map(|result_reason::Reason::PlainText(s)| s)
                    .unwrap_or_default();
                (ok, reason)
            }
            None => (false, String::new()),
        };
        Ok(CommandResponse { result, reason })
    }

    fn exchange_code(&self, code: &str, verifier: &str) -> TeslaResult<UserToken> {
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: String,
        }

        let resp = self
            .http
            .post(&format!("{AUTH_BASE}/oauth2/v3/token"))
            .send_form([
                ("grant_type", "authorization_code"),
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", REDIRECT_URI),
                ("audience", COMMAND_API_BASE),
            ])?
            .into_body()
            .read_json::<Resp>()?;

        Ok(UserToken {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
        })
    }

    fn load_user_token(&self) -> TeslaResult<UserToken> {
        match (&self.config.access_token, &self.config.refresh_token) {
            (Some(access_token), Some(refresh_token)) => Ok(UserToken {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
            }),
            _ => Err(TeslaError::AuthError(
                "No user token found. Run with --auth to authenticate.",
            ))?,
        }
    }

    fn refresh_token(&mut self) -> TeslaResult<UserToken> {
        let refresh_token = self
            .config
            .refresh_token
            .as_deref()
            .ok_or(TeslaError::AuthError(
                "No refresh token found. Run with --auth to authenticate.",
            ))?;

        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: String,
        }

        let resp = self
            .http
            .post(&format!("{AUTH_BASE}/oauth2/v3/token"))
            .send_form([
                ("grant_type", "refresh_token"),
                ("client_id", self.config.client_id.as_str()),
                ("refresh_token", refresh_token),
            ])?
            .into_body()
            .read_json::<Resp>()?;

        let token = UserToken {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
        };
        save_tokens_to_config(&token.access_token, &token.refresh_token)?;
        self.config.access_token = Some(token.access_token.clone());
        self.config.refresh_token = Some(token.refresh_token.clone());
        Ok(token)
    }

    fn post_authenticated(
        &mut self,
        url: &str,
        json_body: serde_json::Value,
        token: UserToken,
    ) -> Result<RoutableMessage, TeslaError> {
        trace!("Sending authenticated post to {url}, body: {json_body}");
        let mut resp_body = match self
            .http
            .post(url)
            .header("Authorization", &format!("Bearer {}", token.access_token))
            .send_json(&json_body)
        {
            Ok(resp_json) => resp_json.into_body(),
            Err(ureq::Error::StatusCode(401)) => {
                warn!("Access token expired, refreshing");
                let token = self.refresh_token()?;
                self.http
                    .post(url)
                    .header("Authorization", &format!("Bearer {}", token.access_token))
                    .send_json(&json_body)?
                    .into_body()
            }
            Err(ureq::Error::StatusCode(408)) => return Err(TeslaError::CarSleeping),
            Err(e) => Err(e)?,
        };

        let resp_json: serde_json::Value = resp_body.read_json()?;

        let encoded_bytes =
            resp_json["response"]
                .as_str()
                .ok_or(TeslaError::UnknownCommandResponse(
                    "authenticated post",
                    format!("JSON response from {url} Should be string value"),
                ))?;
        let resp_message = RoutableMessage::decode(STANDARD.decode(encoded_bytes)?.as_slice())?;

        Ok(resp_message)
    }

    // #[allow(dead_code)]
    // fn partner_token(&self) -> Result<String> {
    //     let mut partner_token = self.partner_token.borrow_mut();
    //     if partner_token.is_none() {
    //         let resp = self
    //             .http
    //             .post(format!("{AUTH_BASE}/oauth2/v3/token"))
    //             .form(&[
    //                 ("grant_type", "client_credentials"),
    //                 ("client_id", &self.config.client_id),
    //                 ("client_secret", &self.config.client_secret),
    //                 ("scope", SCOPES),
    //                 ("audience", AUDIENCE),
    //             ])
    //             .send()?
    //             .error_for_status()?
    //             .json::<PartnerTokenResponse>()?;
    //         *partner_token = Some(resp.access_token);
    //     }
    //     Ok(partner_token.as_ref().cloned().unwrap())
    // }
}

fn is_prime_time() -> bool {
    let now = Zoned::now();
    now.hour() > 11 && now.hour() < 18
}

fn validate_charging_state(charging_state: &str) {
    match charging_state {
        "Charging" | "Disconnected" | "Stopped" | "Complete" => (),
        _ => warn!("Unknown charging state: {charging_state}"),
    }
}

fn random_string(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let query = url.splitn(2, '?').nth(1)?;
    for param in query.split('&') {
        let mut kv = param.splitn(2, '=');
        if kv.next()? == key {
            return kv.next().map(String::from);
        }
    }
    None
}

fn save_tokens_to_config(access_token: &str, refresh_token: &str) -> TeslaResult<()> {
    let path = crate::config::config_path();
    let text = std::fs::read_to_string(&path)?;
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    doc["tesla"]["access_token"] = toml_edit::value(access_token);
    doc["tesla"]["refresh_token"] = toml_edit::value(refresh_token);
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

fn vehicle_data_url(vin: &str) -> String {
    format!(
        "{COMMAND_API_BASE}/api/1/vehicles/{vin}/vehicle_data?endpoints={}",
        urlencoding::encode("location_data;charge_state;vehicle_state")
    )
}
