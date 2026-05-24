use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::{config::SolarEdgeconfig, poll_thread::Pollable};

const API_BASE: &str = "https://monitoringapi.solaredge.com";

#[derive(Error, Debug)]
pub enum SolarEdgeError {
    #[error("Request error: {0}")]
    ReqwestError(#[from] reqwest::Error),
}

pub type SolarEdgeResult<T> = std::result::Result<T, SolarEdgeError>;

#[expect(unused)]
#[derive(Debug, Default)]
pub struct PowerFlow {
    /// Positive = exporting to grid, negative = importing from grid.
    pub grid_watts: f64,
    /// Solar production in watts.
    pub pv_watts: f64,
    /// Total site consumption in watts.
    pub load_watts: f64,
    /// Battery state of charge as a percentage (0–100), or None if no battery.
    pub battery_soc: Option<u8>,
    /// Battery power in kW. Positive = charging, negative = discharging. None if no battery.
    pub battery_kw: Option<f64>,
}

#[derive(Debug, Default)]
pub struct SolarEdge {
    config: SolarEdgeconfig,
    http: reqwest::blocking::Client,
    pub power_flow: PowerFlow,
}

impl Pollable for SolarEdge {
    fn name(&self) -> &'static str {
        "SolarEdge"
    }

    fn init(&mut self) -> crate::NrgyResult<()> {
        Ok(self.power_flow = self.power_flow()?)
    }

    fn poll(&mut self) -> crate::NrgyResult<()> {
        Ok(self.power_flow = self.power_flow()?)
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(290)
    }
}

impl SolarEdge {
    pub fn new(config: SolarEdgeconfig) -> Self {
        SolarEdge {
            config,
            http: Client::new(),
            ..Default::default()
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

        let grid_watts = if exporting {
            flow.grid.current_power * 1000.0
        } else {
            -flow.grid.current_power * 1000.0
        };

        Ok(PowerFlow {
            grid_watts,
            pv_watts: flow.pv.current_power * 1000.0,
            load_watts: flow.load.current_power * 1000.0,
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
    #[serde(rename = "PV")]
    pv: Element,
    #[serde(rename = "LOAD")]
    load: Element,
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
