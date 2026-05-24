use std::{
    io::{self, BufRead},
    thread::sleep,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use function_name::named;
use jiff::{SignedDuration, Timestamp, Zoned};
use log::{debug, error, info, trace, warn};
use rand::Rng;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::TomlError;

use crate::config::TeslaConfig;

const MIN_POLL_INTERVAL: SignedDuration = SignedDuration::from_mins(5);

const AUTH_BASE: &str = "https://fleet-auth.prd.vn.cloud.tesla.com";
const API_BASE: &str = "https://192.168.86.230";
const AUDIENCE: &str = "https://fleet-api.prd.na.vn.cloud.tesla.com";
const SCOPES: &str =
    "openid offline_access vehicle_device_data vehicle_location vehicle_charging_cmds";
const REDIRECT_URI: &str = "https://auth.tesla.com/void/callback";

#[derive(Error, Debug)]
pub enum TeslaError {
    #[error("Charger disconnected")]
    ChargerDisconnected,
    #[error("Charger not providing power")]
    ChargerWithoutPower,
    #[error("Unknown response {1} to {0}")]
    UnknownCommandResponse(&'static str, String),
    #[error("Request error: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Authentication error: {0}")]
    AuthError(&'static str),
    #[error("TOML error: {0}")]
    TomlError(#[from] TomlError),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Jiff error: {0}")]
    JiffError(#[from] jiff::Error),
    #[error("Car sleeping: {0}")]
    CarSleeping(String),
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

#[expect(unused)]
#[derive(Deserialize, Debug)]
pub struct VehicleData {
    pub charge_state: VehicleChargeState,
    pub drive_state: VehicleDriveState,
    pub vehicle_state: VehicleState,
}

#[expect(unused)]
#[derive(Deserialize, Debug)]
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

#[expect(unused)]
#[derive(Deserialize, Debug)]
pub struct VehicleDriveState {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Deserialize, Debug)]
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
    http: reqwest::blocking::Client,
    data: Option<(Timestamp, VehicleData)>,
    last_wake: Option<Timestamp>,
}

#[expect(dead_code)]
impl TeslaVehicle {
    pub fn new(config: TeslaConfig) -> Self {
        TeslaVehicle {
            config,
            http: reqwest::blocking::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("Failed to build HTTP client"),
            data: None,
            last_wake: None,
        }
    }

    pub fn is_home(&mut self) -> TeslaResult<bool> {
        Ok(self.update_state()?.vehicle_state.homelink_nearby)
    }

    pub fn plugged_in(&mut self) -> TeslaResult<bool> {
        Ok(self.update_state()?.charge_state.charging_state != "Disconnected")
    }

    pub fn battery_soc(&mut self) -> TeslaResult<u8> {
        Ok(self.update_state()?.charge_state.battery_level)
    }

    pub fn is_charging(&mut self) -> TeslaResult<bool> {
        let charging_state = self.update_state()?.charge_state.charging_state.as_str();
        Ok(match charging_state {
            "Charging" => true,
            "Stopped" | "Disconnected" => false,
            _ => false,
        })
    }

    pub fn is_full(&mut self) -> TeslaResult<bool> {
        let charging_state = self.update_state()?.charge_state.charging_state.as_str();
        Ok(match charging_state {
            "Complete" => true,
            "Charging" | "Stopped" | "Disconnected" => false,
            _ => false,
        })
    }

    pub fn charging_amps(&mut self) -> TeslaResult<u16> {
        Ok(self.update_state()?.charge_state.charge_amps)
    }

    pub fn charge_limit(&mut self) -> TeslaResult<u8> {
        Ok(self.update_state()?.charge_state.charge_limit_soc)
    }

    #[named]
    pub fn charge_start(&mut self) -> TeslaResult<()> {
        if self.is_charging()? {
            debug!("Got request to start charging, already charging");
            return Ok(());
        }

        info!(
            "Sending charge start to car (state: {})",
            self.update_state()?.charge_state.charging_state
        );
        let resp = self.send_command(function_name!(), serde_json::json!({}))?;
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
            Ok(())
        }
    }

    #[named]
    pub fn charge_stop(&mut self) -> TeslaResult<()> {
        if !self.is_charging()? {
            info!("Got request to stop charging, not charging");
            return Ok(());
        }

        info!("Sending charge stop to car");
        let resp = self.send_command(function_name!(), serde_json::json!({}))?;
        if !resp.result {
            match resp.reason.as_str() {
                "not_charging" => Ok(()),
                _ => Err(TeslaError::UnknownCommandResponse(
                    function_name!(),
                    resp.reason,
                ))?,
            }
        } else {
            Ok(())
        }
    }

    #[named]
    pub fn set_charging_amps(&mut self, amps: u8) -> TeslaResult<()> {
        let current_request = self.update_state()?.charge_state.charge_current_request;
        trace!("Got request for {amps} amps, currently {current_request}");
        if current_request == amps as u16 {
            debug!("Got request for {amps} amps, already set at {current_request}");
            return Ok(());
        }

        info!("Changing charge amps from {current_request} to {amps}");
        let resp =
            self.send_command(function_name!(), serde_json::json!({"charging_amps": amps}))?;

        if !resp.result {
            Err(TeslaError::UnknownCommandResponse(
                function_name!(),
                resp.reason,
            ))?
        } else {
            Ok(())
        }
    }

    #[named]
    pub fn set_charge_limit(&self, percent: u8) -> TeslaResult<()> {
        let resp = self.send_command(function_name!(), serde_json::json!({"percent": percent}))?;

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

    #[named]
    pub fn wake_up(&mut self) -> TeslaResult<()> {
        let now = Timestamp::now();
        if let Some(last_wake) = self.last_wake {
            let delta = (now - last_wake).get_hours();
            if delta < 6 {
                warn!("Only {delta} hours since last wake, refusing",)
            }
        }
        warn!("Sending wakeup");
        _ = self.send_command(function_name!(), serde_json::json!({}))?;
        self.last_wake = Some(now);
        Ok(())
    }

    fn update_state<'a>(&'a mut self) -> TeslaResult<&'a VehicleData> {
        let update = |vehicle: &mut TeslaVehicle| -> TeslaResult<()> {
            let vehicle_data = vehicle.get_vehicle_data()?;
            trace!("Got vehicle data: {vehicle_data:?}");
            vehicle.data = Some((Timestamp::now(), vehicle_data));
            Ok(())
        };

        let cur_time = Timestamp::now();
        let stale_time = cur_time.checked_sub(MIN_POLL_INTERVAL)?;
        match &mut self.data {
            Some((time, data)) if stale_time >= *time => {
                let update_age = (cur_time - *time).get_seconds();
                debug!("Stale vehicle data ({update_age} seconds old): polling",);
                if let Err(e) = update(self) {
                    self.handle_sleeping_car(e, update_age)?;
                    update(self)?
                }
            }
            None => {
                debug!("No vehicle data: polling");
                if let Err(e) = update(self) {
                    self.handle_sleeping_car(e, i64::MAX)?;
                    update(self)?
                }
            }
            Some(_) => {}
        }

        let Some(ref data) = self.data else {
            unreachable!()
        };
        Ok(&data.1)
    }

    fn handle_sleeping_car(&mut self, err: TeslaError, update_age: i64) -> TeslaResult<()> {
        if let TeslaError::CarSleeping(s) = err {
            if is_prime_time() && update_age > 7200 {
                self.wake_up()?;
                sleep(Duration::from_secs(5));
                Ok(())
            } else {
                Err(TeslaError::CarSleeping(s))
            }
        } else {
            Err(err)?
        }
    }

    pub fn get_vehicle_data(&self) -> TeslaResult<VehicleData> {
        let token = self.load_user_token()?;
        let resp = self
            .http
            .get(vehicle_data_url(&self.config.vin))
            .bearer_auth(&token.access_token)
            .send()?;

        if !resp.status().is_success() {
            error!("Error: {}", resp.status());
        }

        let resp = if resp.status() == StatusCode::UNAUTHORIZED {
            let token = self.refresh_token()?;
            self.http
                .get(vehicle_data_url(&self.config.vin))
                .bearer_auth(&token.access_token)
                .send()?
        } else if resp.status() == StatusCode::REQUEST_TIMEOUT {
            Err(TeslaError::CarSleeping(resp.status().to_string()))?
        } else {
            resp
        };

        let vehicle_data = resp
            .error_for_status()?
            .json::<VehicleDataResponseEnvelope>()?
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

    fn send_command(&self, command: &str, body: serde_json::Value) -> TeslaResult<CommandResponse> {
        let make_req = |token: &str| {
            self.http
                .post(get_command_url(command, &self.config.vin))
                .bearer_auth(token)
                .json(&body)
        };

        let token = self.load_user_token()?;
        let resp = make_req(&token.access_token).send()?;

        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let token = self.refresh_token()?;
            make_req(&token.access_token).send()?
        } else {
            resp
        };

        let body = resp.error_for_status()?.text()?;
        trace!("Response: {body}");

        #[derive(Deserialize, Debug)]
        struct CommandResponseEnvelope {
            pub response: CommandResponse,
        }

        let envelope: CommandResponseEnvelope = serde_json::from_str(&body)?;
        Ok(envelope.response)
    }

    fn exchange_code(&self, code: &str, verifier: &str) -> TeslaResult<UserToken> {
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: String,
        }

        let resp = self
            .http
            .post(format!("{AUTH_BASE}/oauth2/v3/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &self.config.client_id),
                ("client_secret", &self.config.client_secret),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", REDIRECT_URI),
                ("audience", AUDIENCE),
            ])
            .send()?
            .error_for_status()?
            .json::<Resp>()?;

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

    fn refresh_token(&self) -> TeslaResult<UserToken> {
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
            .post(format!("{AUTH_BASE}/oauth2/v3/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &self.config.client_id),
                ("refresh_token", refresh_token),
            ])
            .send()?
            .error_for_status()?
            .json::<Resp>()?;

        let token = UserToken {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
        };
        save_tokens_to_config(&token.access_token, &token.refresh_token)?;
        Ok(token)
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
        "{API_BASE}/api/1/vehicles/{vin}/vehicle_data?endpoints={}",
        urlencoding::encode("location_data;charge_state;vehicle_state")
    )
}

fn get_command_url(command: &str, vin: &str) -> String {
    format!("{API_BASE}/api/1/vehicles/{vin}/command/{command}")
}
