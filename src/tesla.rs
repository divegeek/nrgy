use std::{
    io::{self, BufRead},
    sync::{Arc, Condvar, Mutex},
    thread::sleep,
    time::Duration,
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use function_name::named;
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
    config::{PhotonConfig, TeslaConfig},
    poll_thread::Pollable,
    tesla::{
        ble::BleBridge,
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
            vcsec::{
                FromVcsecMessage, InformationRequest, InformationRequestType,
                OperationStatusE as VcsecOperationStatusE, RkeActionE, UnsignedMessage,
                VehicleSleepStatusE, from_vcsec_message::SubMessage as FromVcsecSubMessage,
                unsigned_message::SubMessage as VcsecSubMessage,
            },
        },
    },
};

mod ble;
mod command_signing;
mod proto;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);
pub const VERY_SLOW_POLL_INTERVAL: Duration = Duration::from_hours(1);

const AUTH_BASE: &str = "https://fleet-auth.prd.vn.cloud.tesla.com";
const COMMAND_API_BASE: &str = "https://fleet-api.prd.na.vn.cloud.tesla.com";
const SCOPES: &str =
    "openid offline_access vehicle_device_data vehicle_location vehicle_charging_cmds";
const REDIRECT_URI: &str = "https://auth.tesla.com/void/callback";

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum TeslaError {
    #[error("Charger disconnected")]
    ChargerDisconnected,
    #[error("Charger not providing power")]
    ChargerWithoutPower,
    #[error("Car sleeping")]
    CarSleeping,
    #[error("Unknown response {1} to {0}")]
    UnknownCommandResponse(&'static str, String),
    #[error("Authentication error: {0}")]
    AuthError(&'static str),
    #[error("Signing error {0}")]
    SigningError(#[from] SigningError),
    #[error("Request error: {0}")]
    UreqError(#[from] ureq::Error),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
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

// ── Public data types ─────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct VehicleDataResponseEnvelope {
    response: VehicleData,
}

#[expect(unused)]
#[derive(Deserialize, Debug, Default)]
pub struct VehicleData {
    pub charge_state: VehicleChargeState,
    pub drive_state: VehicleDriveState,
    pub vehicle_state: VehicleState,
}

#[expect(unused)]
#[derive(Deserialize, Debug, Default)]
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
#[derive(Deserialize, Debug, Default)]
pub struct VehicleDriveState {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Deserialize, Debug, Default)]
pub struct VehicleState {
    pub homelink_nearby: bool,
}

#[derive(Deserialize, Debug)]
struct CommandResponse {
    pub result: bool,
    pub reason: String,
}

struct UserToken {
    access_token: String,
    refresh_token: String,
}

// ── Internal state structs ────────────────────────────────────────────────────

/// State touched by Fleet API polling and cloud commands.
struct CloudState {
    http: ureq::Agent,
    signer: CommandSigner,
    config: TeslaConfig,
    data: VehicleData,
    last_update: Option<Timestamp>,
    last_wake: Option<Timestamp>,
}

/// State touched exclusively by the BLE reader thread and BLE commands.
struct BleState {
    bridge: Option<BleBridge>,
    infotainment_signer: CommandSigner,
    vcsec_signer: CommandSigner,
    sleep_status: i32,
    /// A session_info_request has been sent but the response not yet received.
    infotainment_session_pending: bool,
    vcsec_session_pending: bool,
}

impl BleState {
    pub fn bridge_good(&self) -> bool {
        self.bridge.as_ref().is_some_and(|b| !b.failed())
    }
}

/// BLE state + condvar, shared with the reader thread via `Arc`.
pub struct BleShared {
    state: Mutex<BleState>,
    changed: Condvar,
}

impl BleShared {
    fn new(infotainment_signer: CommandSigner, vcsec_signer: CommandSigner) -> Self {
        Self {
            state: Mutex::new(BleState {
                bridge: None,
                infotainment_signer,
                vcsec_signer,
                sleep_status: 0,
                infotainment_session_pending: false,
                vcsec_session_pending: false,
            }),
            changed: Condvar::new(),
        }
    }
}

// ── TeslaVehicle ──────────────────────────────────────────────────────────────

/// All methods are `&self`; internal mutexes provide synchronisation.
pub struct TeslaVehicle {
    photon: Option<PhotonConfig>,
    cloud: Mutex<CloudState>,
    ble: Arc<BleShared>,
}

impl Pollable for TeslaVehicle {
    fn name(&self) -> &'static str {
        "TeslaVehicle"
    }

    fn init(&self) -> crate::NrgyResult<()> {
        self.ensure_ble_bridge();
        match self.update_state() {
            Ok(()) => Ok(()),
            Err(e) => match e {
                TeslaError::CarSleeping => {
                    info!("Car is asleep.  Assuming full.");
                    self.cloud.lock().unwrap().data = VehicleData {
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

    fn poll(&self) -> crate::NrgyResult<()> {
        self.update_state()?;
        Ok(())
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_POLL_INTERVAL
    }
}

#[expect(dead_code)]
impl TeslaVehicle {
    pub fn new(config: TeslaConfig, photon: Option<PhotonConfig>) -> TeslaResult<Arc<Self>> {
        let signer = CommandSigner::new(&config.private_key, &config.vin)?;
        let infotainment_signer = CommandSigner::new(&config.private_key, &config.vin)?;
        let vcsec_signer = CommandSigner::new(&config.private_key, &config.vin)?;

        Ok(Arc::new(Self {
            photon,
            cloud: Mutex::new(CloudState {
                http: ureq::Agent::config_builder()
                    .tls_config(
                        ureq::tls::TlsConfig::builder()
                            .disable_verification(true)
                            .build(),
                    )
                    .build()
                    .new_agent(),
                signer,
                config,
                data: VehicleData::default(),
                last_update: None,
                last_wake: None,
            }),
            ble: Arc::new(BleShared::new(infotainment_signer, vcsec_signer)),
        }))
    }

    // ── BLE wakeup ────────────────────────────────────────────────────────────

    /// Wait until the car reports AWAKE via VCSEC VehicleStatus, or until
    /// `timeout` expires.  Safe to call while holding the cloud lock.
    pub fn wait_for_wakeup(&self, timeout: Duration) -> bool {
        let guard = self.ble.state.lock().unwrap();
        let (_guard, timed_out) = self
            .ble
            .changed
            .wait_timeout_while(guard, timeout, |s| {
                s.sleep_status != VehicleSleepStatusE::VehicleSleepStatusAwake as i32
            })
            .unwrap();
        !timed_out.timed_out()
    }

    // ── Vehicle state accessors ───────────────────────────────────────────────

    pub fn is_home(&self) -> bool {
        let cloud = self.cloud.lock().unwrap();
        cloud.data.vehicle_state.homelink_nearby
    }

    pub fn plugged_in(&self) -> bool {
        self.cloud.lock().unwrap().data.charge_state.charging_state != "Disconnected"
    }

    pub fn battery_soc(&self) -> u8 {
        self.cloud.lock().unwrap().data.charge_state.battery_level
    }

    pub fn is_charging(&self) -> bool {
        let cloud = self.cloud.lock().unwrap();
        matches!(cloud.data.charge_state.charging_state.as_str(), "Charging")
    }

    pub fn is_full(&self) -> bool {
        let cloud = self.cloud.lock().unwrap();
        matches!(cloud.data.charge_state.charging_state.as_str(), "Complete")
    }

    pub fn charging_amps(&self) -> u16 {
        self.cloud.lock().unwrap().data.charge_state.charge_amps
    }

    pub fn charge_limit(&self) -> u8 {
        let cloud = self.cloud.lock().unwrap();
        cloud.data.charge_state.charge_limit_soc
    }

    // ── Charging commands ─────────────────────────────────────────────────────

    #[named]
    pub fn charge_start(&self) -> TeslaResult<()> {
        info!("Sending charge start to car");
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
            Ok(())
        }
    }

    #[named]
    pub fn charge_stop(&self) -> TeslaResult<()> {
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
            Ok(())
        }
    }

    #[named]
    pub fn set_charging_amps(&self, amps: u8) -> TeslaResult<()> {
        if self.photon.is_none() {
            let cloud = self.cloud.lock().unwrap();
            let cur_req = cloud.data.charge_state.charge_current_request;
            trace!("Got request for {amps} amps, currently {cur_req}");
            if cur_req == amps as u16 {
                debug!("Got request for {amps} amps, already set at {cur_req}");
                return Ok(());
            }
        }
        info!("Changing car charge amps to {amps}");
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
            Ok(())
        }
    }

    #[named]
    pub fn set_charge_limit(&self, percent: u8) -> TeslaResult<()> {
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

    // ── BLE bridge management ─────────────────────────────────────────────────

    /// Connect the persistent BLE bridge if not already connected.
    /// Proactively sends session_info_requests on fresh connections.
    /// Returns false if the Photon is unreachable.
    pub fn ensure_ble_bridge(&self) -> bool {
        let Some(photon) = self.photon.clone() else {
            return false;
        };
        if self.ble.state.lock().unwrap().bridge_good() {
            return true;
        } else {
            debug!("BLE bridge failed, reconnecting");
        }
        let ble = self.ble.clone();
        let on_message = move |msg: Option<RoutableMessage>| {
            {
                let mut state = ble.state.lock().unwrap();
                match msg {
                    Some(m) => process_ble_msg(&mut state, m),
                    None => {
                        debug!("BLE bridge closed (car disconnected from BLE)");
                        state.bridge.as_mut().map(|b| b.set_failed());
                        state.infotainment_signer.invalidate_session();
                        state.vcsec_signer.invalidate_session();
                        state.infotainment_session_pending = false;
                        state.vcsec_session_pending = false;
                    }
                }
            }
            ble.changed.notify_all();
        };

        match ble::BleBridge::connect(
            &photon.host,
            photon.port,
            Duration::from_secs(3),
            on_message,
        ) {
            Ok(mut bridge) => {
                let mut state = self.ble.state.lock().unwrap();
                state.infotainment_signer.invalidate_session();
                state.vcsec_signer.invalidate_session();
                let req_i = state
                    .infotainment_signer
                    .session_info_request(Domain::Infotainment);
                let req_v = state
                    .vcsec_signer
                    .session_info_request(Domain::VehicleSecurity);
                let _ = bridge.send(&req_i);
                let _ = bridge.send(&req_v);
                state.infotainment_session_pending = true;
                state.vcsec_session_pending = true;
                state.bridge = Some(bridge);
                true
            }
            Err(_) => false,
        }
    }

    // ── Session wait ──────────────────────────────────────────────────────────

    /// Ensure a BLE session is established for the given domain.
    /// If not already established, (re-)sends a session_info_request and waits
    /// on the condvar until the reader thread delivers the session_info response.
    fn wait_for_ble_session(&self, vcsec: bool, timeout: Duration) -> TeslaResult<()> {
        let mut state = self.ble.state.lock().unwrap();
        let already = if vcsec {
            state.vcsec_signer.has_session()
        } else {
            state.infotainment_signer.has_session()
        };
        if already {
            return Ok(());
        }
        if state.bridge.is_none() {
            return Err(TeslaError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "BLE bridge not connected",
            )));
        }
        // Send a session_info_request only if one is not already in flight.
        let pending = if vcsec {
            state.vcsec_session_pending
        } else {
            state.infotainment_session_pending
        };
        if !pending {
            let domain = if vcsec {
                Domain::VehicleSecurity
            } else {
                Domain::Infotainment
            };
            let req = if vcsec {
                state.vcsec_signer.session_info_request(domain)
            } else {
                state.infotainment_signer.session_info_request(domain)
            };
            if let Some(ref mut b) = state.bridge {
                let _ = b.send(&req);
                if vcsec {
                    state.vcsec_session_pending = true;
                } else {
                    state.infotainment_session_pending = true;
                }
            }
        }

        let (_guard, timed_out) = self
            .ble
            .changed
            .wait_timeout_while(state, timeout, |s| {
                if vcsec {
                    !s.vcsec_signer.has_session()
                } else {
                    !s.infotainment_signer.has_session()
                }
            })
            .unwrap();
        if timed_out.timed_out() {
            return Err(TeslaError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "BLE session establishment timed out",
            )));
        }
        Ok(())
    }

    /// Send a VCSEC GetStatus request; the car responds with a VehicleStatus
    /// broadcast that the reader thread logs.  Useful for observing state transitions.
    pub fn request_ble_status(&self) -> TeslaResult<()> {
        if !self.ensure_ble_bridge() {
            return Err(TeslaError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Photon not reachable",
            )));
        }
        self.wait_for_ble_session(true, Duration::from_secs(30))?;
        let mut state = self.ble.state.lock().unwrap();
        let payload = UnsignedMessage {
            sub_message: Some(VcsecSubMessage::InformationRequest(InformationRequest {
                information_request_type: InformationRequestType::GetStatus as i32,
                key: None,
            })),
        };
        let mut message = RoutableMessage {
            to_destination: Some(Destination {
                sub_destination: Some(SubDestination::Domain(Domain::VehicleSecurity as i32)),
            }),
            from_destination: Some(Destination {
                sub_destination: Some(SubDestination::RoutingAddress(
                    rand::random::<[u8; 16]>().to_vec(),
                )),
            }),
            payload: Some(Payload::ProtobufMessageAsBytes(payload.encode_to_vec())),
            uuid: rand::random::<[u8; 16]>().to_vec(),
            ..Default::default()
        };
        state
            .vcsec_signer
            .encrypt(&mut message, Duration::from_secs(30))?;
        if let Some(ref mut b) = state.bridge {
            b.send(&message)?;
        }
        Ok(())
    }

    // ── Wake up ───────────────────────────────────────────────────────────────

    fn wake_via_ble(&self) -> TeslaResult<()> {
        if !self.ensure_ble_bridge() {
            return Err(TeslaError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Photon not reachable",
            )));
        }
        self.wait_for_ble_session(true, Duration::from_secs(30))?;
        let mut state = self.ble.state.lock().unwrap();
        let payload = UnsignedMessage {
            sub_message: Some(VcsecSubMessage::RkeAction(
                RkeActionE::RkeActionWakeVehicle as i32,
            )),
        };
        let mut message = RoutableMessage {
            to_destination: Some(Destination {
                sub_destination: Some(SubDestination::Domain(Domain::VehicleSecurity as i32)),
            }),
            from_destination: Some(Destination {
                sub_destination: Some(SubDestination::RoutingAddress(
                    rand::random::<[u8; 16]>().to_vec(),
                )),
            }),
            payload: Some(Payload::ProtobufMessageAsBytes(payload.encode_to_vec())),
            uuid: rand::random::<[u8; 16]>().to_vec(),
            ..Default::default()
        };
        state
            .vcsec_signer
            .encrypt(&mut message, Duration::from_secs(30))?;
        if let Some(ref mut b) = state.bridge {
            b.send(&message)?;
            info!("BLE wake command sent");
        }
        Ok(())
    }

    pub fn wake_up(&self) -> TeslaResult<()> {
        if self.photon.is_some() {
            debug!("Attempting BLE wakeup");
            self.wake_via_ble()?;
            if self.wait_for_wakeup(Duration::from_secs(10)) {
                info!("Car awake");
            } else {
                warn!("Car did not wake within timeout");
            }
            return Ok(());
        }

        warn!("Sending cloud wakeup");
        let now = Timestamp::now();
        let (vin, last_wake, token) = {
            let cloud = self.cloud.lock().unwrap();
            (
                cloud.config.vin.clone(),
                cloud.last_wake,
                self.load_user_token_from(&cloud)?,
            )
        };
        if let Some(lw) = last_wake {
            if (now - lw).get_hours() < 6 {
                warn!(
                    "Only {} hours since last wake, refusing",
                    (now - lw).get_hours()
                );
                return Ok(());
            }
        }
        let url = format!("{COMMAND_API_BASE}/api/1/vehicles/{vin}/wake_up");
        let http = self.cloud.lock().unwrap().http.clone();
        let resp = match http
            .post(&url)
            .header("Authorization", &format!("Bearer {}", token.access_token))
            .send(())
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => {
                let token = self.refresh_token()?;
                http.post(&url)
                    .header("Authorization", &format!("Bearer {}", token.access_token))
                    .send(())?
            }
            Err(e) => return Err(e.into()),
        };
        let body = resp.into_body().read_to_string()?;
        trace!("Response: {body}");
        self.cloud.lock().unwrap().last_wake = Some(now);
        Ok(())
    }

    // ── Cloud API state management ────────────────────────────────────────────

    fn update_state(&self) -> TeslaResult<()> {
        let update_age = {
            let cloud = self.cloud.lock().unwrap();
            (Timestamp::now() - cloud.last_update.unwrap_or(Timestamp::MIN)).get_seconds()
        };

        let vehicle_data = match self.get_vehicle_data() {
            Ok(data) => data,
            Err(e) => {
                self.handle_sleeping_car(e, update_age)?;
                // Retry with exponential backoff while the car brings its connection up.
                let mut total_ms = 0u64;
                let mut result = Err(TeslaError::CarSleeping);
                for delay_ms in [
                    500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
                ] {
                    sleep(Duration::from_millis(delay_ms));
                    total_ms += delay_ms;
                    result = self.get_vehicle_data();
                    if result.is_ok() {
                        info!("Fleet API ready after {total_ms}ms");
                        break;
                    }
                }
                match result {
                    Ok(data) => data,
                    Err(e) => {
                        warn!("Fleet API not ready after {total_ms}ms");
                        return Err(e);
                    }
                }
            }
        };

        let mut cloud = self.cloud.lock().unwrap();
        trace!("Got vehicle data: {vehicle_data:?}");
        debug!(
            "Vehicle state changed: {}",
            Comparison::new(&cloud.data, &vehicle_data)
        );
        cloud.data = vehicle_data;
        cloud.last_update = Some(Timestamp::now());
        Ok(())
    }

    fn handle_sleeping_car(&self, err: TeslaError, update_age: i64) -> TeslaResult<()> {
        if let TeslaError::CarSleeping = err {
            let should_wake = self.photon.is_some() || (is_prime_time() && update_age > 7200);
            if should_wake {
                self.wake_up()?;
                // For the cloud path, sleep briefly to give the car time to come online.
                if self.photon.is_none() {
                    sleep(Duration::from_secs(5));
                }
                Ok(())
            } else {
                Err(TeslaError::CarSleeping)
            }
        } else {
            Err(err)?
        }
    }

    pub fn get_vehicle_data(&self) -> TeslaResult<VehicleData> {
        let (url, token, http) = {
            let cloud = self.cloud.lock().unwrap();
            (
                vehicle_data_url(&cloud.config.vin),
                self.load_user_token_from(&cloud)?,
                cloud.http.clone(),
            )
        };

        let mut resp = match http
            .get(&url)
            .header("Authorization", &format!("Bearer {}", token.access_token))
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(401)) => {
                let token = self.refresh_token()?;
                let http = self.cloud.lock().unwrap().http.clone();
                http.get(&url)
                    .header("Authorization", &format!("Bearer {}", token.access_token))
                    .call()?
            }
            Err(ureq::Error::StatusCode(408)) => return Err(TeslaError::CarSleeping),
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

    // ── Authentication ────────────────────────────────────────────────────────

    pub fn authenticate(&self) -> TeslaResult<()> {
        let verifier = random_string(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_string(16);
        let (client_id, redirect_enc, scope_enc) = {
            let cloud = self.cloud.lock().unwrap();
            (
                cloud.config.client_id.clone(),
                urlencoding::encode(REDIRECT_URI).to_string(),
                urlencoding::encode(SCOPES).to_string(),
            )
        };
        let auth_url = format!(
            "{AUTH_BASE}/oauth2/v3/authorize\
             ?client_id={client_id}&locale=en-US&prompt=login\
             &redirect_uri={redirect_enc}&response_type=code\
             &scope={scope_enc}&state={state}\
             &code_challenge={challenge}&code_challenge_method=S256",
        );
        println!("Open this URL in a browser:\n\n{auth_url}\n");
        println!("After authorizing, paste the full redirect URL and press Enter:");
        let mut input = String::new();
        io::stdin().lock().read_line(&mut input)?;
        let code = extract_query_param(input.trim(), "code")
            .ok_or(TeslaError::AuthError("No 'code' parameter found in URL"))?;
        let token = self.exchange_code(&code, &verifier)?;
        save_tokens_to_config(&token.access_token, &token.refresh_token)?;
        println!("Authentication successful.");
        Ok(())
    }

    // ── Session management (cloud HMAC) ───────────────────────────────────────

    pub fn establish_session(&self) -> TeslaResult<()> {
        info!("Establishing signed command session");
        let (req_bytes, token) = {
            let cloud = self.cloud.lock().unwrap();
            let req = cloud
                .signer
                .session_info_request(Domain::Infotainment)
                .encode_to_vec();
            let token = self.load_user_token_from(&cloud)?;
            (req, token)
        };
        let vin = self.cloud.lock().unwrap().config.vin.clone();
        let resp_msg = self.post_authenticated(
            &format!("{COMMAND_API_BASE}/api/1/vehicles/{vin}/signed_command"),
            serde_json::json!({ "routable_message": STANDARD.encode(&req_bytes) }),
            token,
        )?;
        let session_info_bytes = match resp_msg.payload {
            Some(Payload::SessionInfo(b)) => b,
            Some(p) => Err(TeslaError::UnknownCommandResponse(
                "session_info",
                format!("{p:?}"),
            ))?,
            _ => todo!(),
        };
        self.cloud
            .lock()
            .unwrap()
            .signer
            .update_session(&SessionInfo::decode(session_info_bytes.as_slice())?)?;
        Ok(())
    }

    // ── Command dispatch ──────────────────────────────────────────────────────

    fn send_signed_command(&self, action: Action) -> TeslaResult<CommandResponse> {
        if self.photon.is_some() {
            return self.send_ble_command(action);
        }
        // Cloud HMAC path.
        if !self.cloud.lock().unwrap().signer.has_session() {
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
            self.cloud.lock().unwrap().signer.invalidate_session();
            self.establish_session()?;
            return Self::parse_command_response(self.dispatch_signed(&action)?);
        }
        Self::parse_command_response(resp_msg)
    }

    fn send_ble_command(&self, action: Action) -> TeslaResult<CommandResponse> {
        if !self.ensure_ble_bridge() {
            return Err(TeslaError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Photon not reachable",
            )));
        }
        self.wait_for_ble_session(false, Duration::from_secs(30))?;
        let mut state = self.ble.state.lock().unwrap();
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
        state
            .infotainment_signer
            .encrypt(&mut message, Duration::from_secs(30))?;
        if let Some(ref mut b) = state.bridge {
            b.send(&message)?;
        }
        Ok(CommandResponse {
            result: true,
            reason: String::new(),
        })
    }

    fn dispatch_signed(&self, action: &Action) -> TeslaResult<RoutableMessage> {
        let (msg_bytes, token, vin) = {
            let mut cloud = self.cloud.lock().unwrap();
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
            cloud
                .signer
                .authorize_hmac(&mut message, Duration::from_secs(30))?;
            let token = self.load_user_token_from(&cloud)?;
            let vin = cloud.config.vin.clone();
            (message.encode_to_vec(), token, vin)
        };
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
        Self::parse_command_response_bytes(bytes)
    }

    fn parse_command_response_bytes(bytes: Vec<u8>) -> TeslaResult<CommandResponse> {
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

    // ── Token management ──────────────────────────────────────────────────────

    fn load_user_token_from(&self, cloud: &CloudState) -> TeslaResult<UserToken> {
        match (&cloud.config.access_token, &cloud.config.refresh_token) {
            (Some(at), Some(rt)) => Ok(UserToken {
                access_token: at.clone(),
                refresh_token: rt.clone(),
            }),
            _ => Err(TeslaError::AuthError("No user token. Run with --auth.")),
        }
    }

    fn load_user_token(&self) -> TeslaResult<UserToken> {
        self.load_user_token_from(&self.cloud.lock().unwrap())
    }

    fn refresh_token(&self) -> TeslaResult<UserToken> {
        let (client_id, refresh_token_val, http) = {
            let cloud = self.cloud.lock().unwrap();
            let rt = cloud
                .config
                .refresh_token
                .clone()
                .ok_or(TeslaError::AuthError("No refresh token. Run with --auth."))?;
            (cloud.config.client_id.clone(), rt, cloud.http.clone())
        };

        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: String,
        }
        let resp = http
            .post(&format!("{AUTH_BASE}/oauth2/v3/token"))
            .send_form([
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
                ("refresh_token", refresh_token_val.as_str()),
            ])?
            .into_body()
            .read_json::<Resp>()?;

        let token = UserToken {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
        };
        save_tokens_to_config(&token.access_token, &token.refresh_token)?;
        {
            let mut cloud = self.cloud.lock().unwrap();
            cloud.config.access_token = Some(token.access_token.clone());
            cloud.config.refresh_token = Some(token.refresh_token.clone());
        }
        Ok(token)
    }

    fn exchange_code(&self, code: &str, verifier: &str) -> TeslaResult<UserToken> {
        let (http, client_id, client_secret) = {
            let cloud = self.cloud.lock().unwrap();
            (
                cloud.http.clone(),
                cloud.config.client_id.clone(),
                cloud.config.client_secret.clone(),
            )
        };
        #[derive(Deserialize)]
        struct Resp {
            access_token: String,
            refresh_token: String,
        }
        let resp = http
            .post(&format!("{AUTH_BASE}/oauth2/v3/token"))
            .send_form([
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
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

    fn post_authenticated(
        &self,
        url: &str,
        json_body: serde_json::Value,
        token: UserToken,
    ) -> TeslaResult<RoutableMessage> {
        trace!("Sending authenticated post to {url}");
        let http = self.cloud.lock().unwrap().http.clone();
        let mut resp_body = match http
            .post(url)
            .header("Authorization", &format!("Bearer {}", token.access_token))
            .send_json(&json_body)
        {
            Ok(r) => r.into_body(),
            Err(ureq::Error::StatusCode(401)) => {
                warn!("Access token expired, refreshing");
                let token = self.refresh_token()?;
                let http = self.cloud.lock().unwrap().http.clone();
                http.post(url)
                    .header("Authorization", &format!("Bearer {}", token.access_token))
                    .send_json(&json_body)?
                    .into_body()
            }
            Err(ureq::Error::StatusCode(408)) => return Err(TeslaError::CarSleeping),
            Err(e) => Err(e)?,
        };
        let resp_json: serde_json::Value = resp_body.read_json()?;
        let encoded = resp_json["response"]
            .as_str()
            .ok_or(TeslaError::UnknownCommandResponse(
                "post",
                "expected string response".into(),
            ))?;
        Ok(RoutableMessage::decode(
            STANDARD.decode(encoded)?.as_slice(),
        )?)
    }
}

// ── BLE message processing (free function, operates on BleState) ──────────────

fn process_ble_msg(state: &mut BleState, msg: RoutableMessage) {
    let from_domain = msg
        .from_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            SubDestination::Domain(d) => Some(*d),
            _ => None,
        });

    if let Some(Payload::SessionInfo(ref bytes)) = msg.payload {
        if let Ok(si) = SessionInfo::decode(bytes.as_slice()) {
            match from_domain {
                Some(d) if d == Domain::VehicleSecurity as i32 => {
                    let _ = state.vcsec_signer.update_session(&si);
                    state.vcsec_session_pending = false;
                    trace!("BLE: VCSEC session ready");
                }
                Some(d) if d == Domain::Infotainment as i32 => {
                    let _ = state.infotainment_signer.update_session(&si);
                    state.infotainment_session_pending = false;
                    trace!("BLE: Infotainment session ready");
                }
                _ => {}
            }
        }
        return;
    }

    if let Some(Payload::ProtobufMessageAsBytes(ref bytes)) = msg.payload {
        if from_domain == Some(Domain::Infotainment as i32) {
            if let Some(status) = &msg.signed_message_status {
                let fault = status.signed_message_fault;
                if matches!(fault, 5 | 6 | 15) {
                    info!("BLE Infotainment session stale (fault {fault}), re-requesting");
                    state.infotainment_signer.invalidate_session();
                    let req = state
                        .infotainment_signer
                        .session_info_request(Domain::Infotainment);
                    if let Some(ref mut b) = state.bridge {
                        let _ = b.send(&req);
                    }
                    return;
                }
            }
            // Log command response.
            match state
                .infotainment_signer
                .decrypt(&msg, Domain::Infotainment as u8)
            {
                Ok(plaintext) => match TeslaVehicle::parse_command_response_bytes(plaintext) {
                    Ok(cr) if !cr.result => match cr.reason.as_str() {
                        "is_charging" => {}
                        _ => warn!("BLE command result: {}", cr.reason),
                    },
                    Ok(_) => trace!("BLE command: success"),
                    Err(e) => warn!("BLE decode error: {e}"),
                },
                Err(_) => match TeslaVehicle::parse_command_response(msg) {
                    Ok(cr) if !cr.result => match cr.reason.as_str() {
                        "is_charging" => {}
                        _ => warn!("BLE command result: {}", cr.reason),
                    },
                    Ok(_) => trace!("BLE command: success (plaintext)"),
                    Err(e) => warn!("BLE parse error: {e}"),
                },
            }
            return;
        }

        if from_domain == Some(Domain::VehicleSecurity as i32) {
            if let Ok(vcsec) = FromVcsecMessage::decode(bytes.as_slice()) {
                match vcsec.sub_message {
                    Some(FromVcsecSubMessage::VehicleStatus(vs)) => {
                        trace!(
                            "BLE VehicleStatus: sleep={} presence={}",
                            vs.vehicle_sleep_status, vs.user_presence
                        );
                        state.sleep_status = vs.vehicle_sleep_status;
                    }
                    Some(FromVcsecSubMessage::CommandStatus(s))
                        if s.operation_status
                            == VcsecOperationStatusE::OperationstatusError as i32 =>
                    {
                        warn!("BLE VCSEC command error: {s:?}");
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
