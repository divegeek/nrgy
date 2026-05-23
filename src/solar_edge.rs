use reqwest::blocking::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::config::SolarEdgeconfig;

const API_BASE: &str = "https://monitoringapi.solaredge.com";

pub struct SolarEdge {
    config: SolarEdgeconfig,
    http: reqwest::blocking::Client,
}

#[derive(Error, Debug)]
pub enum SolarEdgeError {
    #[error("Request error: {0}")]
    ReqwestError(#[from] reqwest::Error),
}

pub type SolarEdgeResult<T> = std::result::Result<T, SolarEdgeError>;

#[expect(unused)]
#[derive(Debug)]
pub struct PowerFlow {
    /// Positive = exporting to grid, negative = importing from grid.
    pub grid_watts: i32,
    /// Battery state of charge as a percentage (0–100), or None if no battery.
    pub battery_soc: Option<u8>,
    /// Battery power in kW. Positive = charging, negative = discharging. None if no battery.
    pub battery_kw: Option<f64>,
}

impl SolarEdge {
    pub fn new(config: SolarEdgeconfig) -> Self {
        SolarEdge {
            config,
            http: Client::new(),
        }
    }

    pub fn power_flow(&self) -> SolarEdgeResult<PowerFlow> {
        let flow = self
            .http
            .get(format!(
                "{API_BASE}/site/{}/currentPowerFlow?api_key={}",
                self.config.site_id, self.config.api_key
            ))
            .send()?
            .error_for_status()?
            .json::<Response>()?
            .flow;

        let exporting = flow
            .connections
            .iter()
            .any(|c| c.from.eq_ignore_ascii_case("load") && c.to.eq_ignore_ascii_case("grid"));

        let grid_watts: i32 = if exporting {
            (flow.grid.current_power * 1000.0) as i32
        } else {
            (-flow.grid.current_power * 1000.0) as i32
        };

        Ok(PowerFlow {
            grid_watts,
            battery_soc: flow.storage.as_ref().map(|s| s.charge_level),
            battery_kw: flow.storage.map(|s| {
                let charging = flow
                    .connections
                    .iter()
                    .any(|c| c.to.eq_ignore_ascii_case("storage"));
                if charging {
                    s.current_power
                } else {
                    -s.current_power
                }
            }),
        })
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(rename = "siteCurrentPowerFlow")]
    flow: FlowData,
}

#[derive(Deserialize)]
struct FlowData {
    connections: Vec<Connection>,
    #[serde(rename = "GRID")]
    grid: Element,
    #[serde(rename = "STORAGE")]
    storage: Option<StorageElement>,
}

#[derive(Deserialize)]
struct Connection {
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct Element {
    #[serde(rename = "currentPower")]
    current_power: f64,
}

#[derive(Deserialize)]
struct StorageElement {
    #[serde(rename = "currentPower")]
    current_power: f64,
    #[serde(rename = "chargeLevel")]
    charge_level: u8,
}
