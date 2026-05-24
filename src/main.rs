use std::{thread::sleep, time::Duration};

use anyhow::Result;
use jiff::Zoned;
use log::{debug, error, info, trace, warn};
use thiserror::Error;

use crate::{
    open_evse::{EvseError, EvseState, OpenEvse},
    poll_thread::PollThread,
    solar_edge::{SolarEdge, SolarEdgeError},
    tesla::{TeslaError, TeslaVehicle},
};

mod config;
mod open_evse;
mod poll_thread;
mod rmp;
mod solar_edge;
mod tesla;
mod units;

const NORMAL_POLL_INTERVAL: u64 = 30;
const MAX_CHARGE_AMPS: u8 = 48;
const URGENT_CHARGE_THRESHOLD: u8 = 40;
const SHOULD_CHARGE_THRESHOLD: u8 = 60;
const TYPICAL_VOLTS: f64 = 245.0;

#[derive(Error, Debug)]
enum NrgyError {
    #[error("Tesla error: {0}")]
    TeslaError(#[from] TeslaError),
    #[error("SolarEdge error: {0}")]
    SolarEdgeError(#[from] SolarEdgeError),
    #[error("Time error: {0}")]
    TimeError(#[from] jiff::Error),
    #[error("OpenEVSE error: {0}")]
    OpenEvseError(#[from] EvseError),
}

type NrgyResult<T> = std::result::Result<T, NrgyError>;

fn main() -> Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .filter_module("reqwest", log::LevelFilter::Warn)
        .init();

    let config = config::load().expect("Failed to load config");
    let vehicle = TeslaVehicle::new(config.tesla);
    let solar = SolarEdge::new(config.solar_edge);
    let open_evse = OpenEvse::new(config.open_evse);

    if std::env::args().any(|a| a == "--auth") {
        vehicle.authenticate()?;
        return Ok(());
    }

    // Start polling threads.
    let open_evse = PollThread::start(open_evse)?;
    let vehicle = PollThread::start(vehicle)?;
    let solar = PollThread::start(solar)?;

    open_evse.lock().sleep()?;

    let poll_interval = NORMAL_POLL_INTERVAL;
    loop {
        match poll(&vehicle, &open_evse, &solar) {
            Ok(()) => (),
            Err(NrgyError::TeslaError(TeslaError::ReqwestError(e))) => {
                error!("Tesla request error {e}");
            }
            Err(e) => {
                error!("Error {e}")
            }
        }
        sleep(Duration::from_secs(poll_interval));
    }
}

fn poll(
    vehicle: &PollThread<TeslaVehicle>,
    open_evse: &PollThread<OpenEvse>,
    solar: &PollThread<SolarEdge>,
) -> NrgyResult<()> {
    if !open_evse.lock().plugged_in()? {
        trace!("Not plugged in");
        return Ok(());
    }

    if vehicle.lock().is_full() {
        trace!("Vehicle is full");
        return Ok(());
    }

    let soc = vehicle.lock().battery_soc();
    let charging_current = open_evse.lock().charging_current;
    let charging = charging_current > 0.0;
    trace!("Vehicle SoC {soc}, Charging {charging_current}");

    let charge_amps = if soc < URGENT_CHARGE_THRESHOLD {
        if !charging {
            warn!("SoC {soc} below {URGENT_CHARGE_THRESHOLD}.  Urgently need to charge.")
        };

        MAX_CHARGE_AMPS
    } else if soc < SHOULD_CHARGE_THRESHOLD {
        let now = Zoned::now();
        if now.hour() < 18 || now.hour() >= 22 {
            if !charging {
                info!("SoC {soc} below {SHOULD_CHARGE_THRESHOLD}.  Should charge.");
            }
            MAX_CHARGE_AMPS
        } else {
            0
        }
    } else {
        let now = Zoned::now();
        if now.hour() > 20 || now.hour() < 8 {
            trace!("It's dark, assuming no excess solar, waiting until morning");
            return Ok(());
        }

        let excess_amps = excess_amps(&open_evse.lock(), &solar.lock())?;
        if excess_amps > 0 {
            info!("Excess solar, enabling charging with {excess_amps}")
        } else {
            info!("No excess solar.")
        }
        excess_amps
    };

    if charge_amps > 0 {
        let open_evse = open_evse.lock();
        trace!("Setting charging amps to {charge_amps}");
        open_evse.set_current_capacity(charge_amps)?;
        open_evse.enable()?;
        vehicle.lock().charge_start()?;
    } else {
        vehicle.lock().charge_stop()?;
    }
    Ok(())
}

fn excess_amps(evse: &OpenEvse, solar: &solar_edge::SolarEdge) -> NrgyResult<u8> {
    trace!("{evse:?}");
    let voltage = evse.charging_voltage.max(TYPICAL_VOLTS);
    let (min_amps, max_amps) = evse.current_capacity_range;

    let car_power = if evse.is_charging() {
        evse.charging_current * voltage
    } else {
        0.0
    };
    let power_flow = &solar.power_flow;
    debug!(
        "Excess amps calc: Car: {car_power} Grid: {} Voltage: {voltage}",
        power_flow.grid_watts
    );

    let excess_power = (power_flow.grid_watts as f64 + car_power) * 0.9;
    let excess_amps = ((excess_power / voltage) as u8)
        .clamp(min_amps, max_amps)
        .min(MAX_CHARGE_AMPS);

    debug!(
        "Excess power {excess_power} (90% of {} + {car_power}) amps {excess_amps}",
        power_flow.grid_watts
    );
    Ok(excess_amps)
}
