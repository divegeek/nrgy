use std::{thread::sleep, time::Duration};

use anyhow::Result;
use jiff::Zoned;
use log::{debug, error, info, trace, warn};
use thiserror::Error;

use crate::{
    solar_edge::{SolarEdge, SolarEdgeError},
    tesla::{TeslaError, TeslaVehicle},
};

mod config;
mod rmp;
mod solar_edge;
mod tesla;
mod units;

const NORMAL_POLL_INTERVAL: u64 = 5;
const SLOW_POLL_INTERVAL: u64 = 30;
const VERY_SLOW_POLL_INTERVAL: u64 = 90;
const MIN_CHARGE_AMPS: i8 = 5;
const MAX_CHARGE_AMPS: i8 = 48;
const TYPICAL_VOLTS: i16 = 245;
const MIN_CHARGE_WATTS: i32 = MIN_CHARGE_AMPS as i32 * TYPICAL_VOLTS as i32;
const URGENT_CHARGE_THRESHOLD: u8 = 40;
const SHOULD_CHARGE_THRESHOLD: u8 = 60;

#[derive(Error, Debug)]
enum NrgyError {
    #[error("Tesla error: {0}")]
    TeslaError(#[from] TeslaError),
    #[error("SolarEdge error: {0}")]
    SolarEdgeError(#[from] SolarEdgeError),
    #[error("Time error: {0}")]
    TimeError(#[from] jiff::Error),
}

type NrgyResult<T> = std::result::Result<T, NrgyError>;

fn main() -> Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .filter_module("reqwest", log::LevelFilter::Warn)
        .init();

    let config = config::load().expect("Failed to load config");
    let mut vehicle = tesla::TeslaVehicle::new(config.tesla);
    let solar = solar_edge::SolarEdge::new(config.solar_edge);

    if std::env::args().any(|a| a == "--auth") {
        vehicle.authenticate()?;
    }

    let mut poll_interval = NORMAL_POLL_INTERVAL;
    loop {
        match poll(&mut vehicle, &solar) {
            Ok(Some(new_interval)) => {
                if new_interval != poll_interval {
                    info!("Changing poll interval from {poll_interval} to {new_interval}");
                }
                poll_interval = new_interval
            }
            Ok(None) => (),
            Err(NrgyError::TeslaError(TeslaError::ReqwestError(e))) => {
                error!("Tesla request error {e}");
                poll_interval = NORMAL_POLL_INTERVAL;
            }
            Err(e) => {
                error!("Error {e}")
            }
        }

        info!("Sleeping for {poll_interval} minutes");
        sleep(Duration::from_mins(poll_interval));
    }
}

fn poll(vehicle: &mut TeslaVehicle, solar: &SolarEdge) -> NrgyResult<Option<u64>> {
    let mut new_poll_interval = None;

    if !vehicle.is_home()? || !vehicle.plugged_in()? {
        return Ok(Some(SLOW_POLL_INTERVAL));
    }

    let soc = vehicle.battery_soc()?;

    if vehicle.is_full()? {
        info!("Vehicle is full");
        return Ok(Some(SLOW_POLL_INTERVAL));
    }

    let charge_amps = if soc < URGENT_CHARGE_THRESHOLD {
        warn!("SoC {soc} below {URGENT_CHARGE_THRESHOLD}.  Need to charge.");
        MAX_CHARGE_AMPS
    } else if soc < SHOULD_CHARGE_THRESHOLD {
        info!("SoC {soc} below {SHOULD_CHARGE_THRESHOLD}. Need to charge.");
        let now = Zoned::now();
        if now.hour() < 18 || now.hour() >= 22 {
            MAX_CHARGE_AMPS
        } else {
            0
        }
    } else {
        let now = Zoned::now();
        if now.hour() > 20 || now.hour() < 8 {
            trace!("It's dark, assuming no excess solar, waiting until morning");
            return Ok(Some(VERY_SLOW_POLL_INTERVAL));
        }

        let excess_amps = excess_amps(vehicle, &solar)?;
        if excess_amps > 0 {
            info!("Excess solar, enabling charging with {excess_amps}")
        } else {
            info!("No excess solar.")
        }
        excess_amps
    };

    if charge_amps > 0 {
        new_poll_interval = Some(NORMAL_POLL_INTERVAL);
        vehicle.set_charging_amps(charge_amps as u8)?;
        vehicle.charge_start()?;
    } else {
        vehicle.charge_stop()?;
    }

    Ok(new_poll_interval)
}

fn excess_amps(vehicle: &mut TeslaVehicle, solar: &solar_edge::SolarEdge) -> NrgyResult<i8> {
    let car_power = if vehicle.is_charging()? {
        vehicle.charging_amps()? as i16 * TYPICAL_VOLTS
    } else {
        0
    };
    let power_flow = solar.power_flow()?;
    trace!("Excess amps calc: {car_power} {power_flow:?}");

    let excess_power = (power_flow.grid_watts + car_power as i32) * 19 / 20;
    let excess_amps: i8 = if excess_power > MIN_CHARGE_WATTS {
        (excess_power / TYPICAL_VOLTS as i32)
            .min(MAX_CHARGE_AMPS as i32)
            .try_into()
            .unwrap()
    } else {
        0
    };
    debug!(
        "Excess power {excess_power} ({} + {car_power}) amps {excess_amps}",
        power_flow.grid_watts
    );
    Ok(excess_amps)
}
