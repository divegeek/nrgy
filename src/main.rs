use std::{io::Write as _, thread::sleep, time::Duration};

use anyhow::Result;
use jiff::Zoned;
use log::{debug, error, info, trace, warn};
use solaredge_modbus::{MeterClient, SlotNumber};
use thiserror::Error;

use crate::{
    NrgyError::ModbusError,
    config::SolarEdgeModbusConfig,
    open_evse::{EvseError, OpenEvse},
    poll_thread::PollThread,
    tesla::{TeslaError, TeslaVehicle},
};

mod config;
mod open_evse;
mod poll_thread;
mod rmp;
mod tesla;
mod units;

const NORMAL_POLL_INTERVAL: u64 = 30;
const MIN_CHARGE_AMPS: u8 = 5;
const MIN_EVSE_CHARGE_AMPS: u8 = 10;
const MAX_CHARGE_AMPS: u8 = 48;
const URGENT_CHARGE_THRESHOLD: u8 = 40;
const SHOULD_CHARGE_THRESHOLD: u8 = 60;

#[derive(Error, Debug)]
enum NrgyError {
    #[error("Tesla error: {0}")]
    TeslaError(#[from] TeslaError),
    #[error("Time error: {0}")]
    TimeError(#[from] jiff::Error),
    #[error("OpenEVSE error: {0}")]
    OpenEvseError(#[from] EvseError),
    #[error("Modbus error {0}")]
    ModbusError(#[from] solaredge_modbus::Error),
}

type NrgyResult<T> = std::result::Result<T, NrgyError>;

fn main() -> Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .filter_module("ureq", log::LevelFilter::Warn)
        .filter_module("rustls", log::LevelFilter::Warn)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Zoned::now().strftime("%D %l:%M:%S.%3f %p %Z"),
                record.level(),
                record.args()
            )
        })
        .init();

    let config = config::load().expect("Failed to load config");
    let vehicle = TeslaVehicle::new(config.tesla);
    let mut open_evse = OpenEvse::new(config.open_evse);
    let mut solar_meter = create_modbus_client(&config.solaredge_modbus)?;

    if std::env::args().any(|a| a == "--auth") {
        vehicle.authenticate()?;
        return Ok(());
    }

    // Start polling threads.
    let vehicle = PollThread::start(vehicle)?;

    let poll_interval = NORMAL_POLL_INTERVAL;
    loop {
        match poll(&vehicle, &mut open_evse, &mut solar_meter) {
            Ok(()) => (),
            Err(NrgyError::TeslaError(TeslaError::UreqError(e))) => {
                error!("Tesla request error {e}");
            }
            Err(ModbusError(e)) => {
                error!("Modbus error {e}, restarting Modbus client");
                solar_meter = create_modbus_client(&config.solaredge_modbus)?;
            }
            Err(e) => {
                error!("Error {e}")
            }
        }
        sleep(Duration::from_secs(poll_interval));
    }
}

fn create_modbus_client(config: &SolarEdgeModbusConfig) -> NrgyResult<MeterClient> {
    Ok(MeterClient::new(
        &config.host,
        config.port,
        config.device_id,
        match config.slot {
            1 => SlotNumber::One,
            2 => SlotNumber::Two,
            3 => SlotNumber::Three,
            _ => panic!("Invalid SolarEdge modbus meter slot {}", config.slot),
        },
    )?)
}

fn poll(
    vehicle: &PollThread<TeslaVehicle>,
    open_evse: &mut OpenEvse,
    solar_meter: &mut MeterClient,
) -> NrgyResult<()> {
    if !open_evse.plugged_in()? {
        trace!("Not plugged in");
        return Ok(());
    }

    if vehicle.lock().is_full() {
        trace!("Vehicle is full");
        return Ok(());
    }

    let soc = vehicle.lock().battery_soc();
    let (charging_amps, _) = open_evse.charging_current_and_voltage()?;
    let (grid_export, voltage) = solar_meter.grid_power_and_voltage()?;
    let charging = charging_amps > 0.0;
    debug!("Vehicle SoC {soc} Charging {charging_amps} Grid {grid_export}");

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

        let excess_amps = excess_amps(charging_amps, grid_export, voltage)?;
        if excess_amps > 0 {
            info!("Excess solar, enabling charging with {excess_amps}")
        } else {
            info!("No excess solar.")
        }
        excess_amps
    };

    if charge_amps >= MIN_EVSE_CHARGE_AMPS {
        trace!("Setting charging amps to {charge_amps} using EVSE");
        let mut vehicle = vehicle.lock();

        open_evse.set_current_capacity(charge_amps)?;
        open_evse.enable()?;
        vehicle.set_charging_amps(MAX_CHARGE_AMPS)?;
        vehicle.charge_start()?;
    } else if charge_amps >= MIN_CHARGE_AMPS {
        trace!("Setting charging amps to {charge_amps} using Tesla");
        let mut vehicle = vehicle.lock();

        vehicle.set_charging_amps(charge_amps)?;
        vehicle.charge_start()?;
        open_evse.set_current_capacity(MAX_CHARGE_AMPS)?;
        open_evse.enable()?;
    } else {
        trace!("Excess is {charge_amps}, stop charging.");
        vehicle.lock().charge_stop()?;
    }
    Ok(())
}
const HIGH_THRESHOLD: f64 = 1000.0;
const LOW_THRESHOLD: f64 = 500.0;

fn excess_amps(charging_amps: f64, grid_export: f64, voltage: f64) -> NrgyResult<u8> {
    let car_power = charging_amps * voltage;

    if grid_export < LOW_THRESHOLD {
        // We can't trust `excess_power = grid_export + car_power` because if the `grid_export` is
        // low, the system may be drawing on the house battery to prop it up.  Only if
        // `grid_export > LOW_THRESHOLD` do we trust that the system wouldn't be putting that much
        // into the grid if it were using the batteries.
        //
        // So if we can't use that excess_power calculation, how do we determine excess power?  We
        // can't.  Instead, we just drop the charge rate, first to the OpenEVSE minimum, then to
        // the car minimum, then to zero, to see if that results in non-trivial power going into
        // the grid -- at which point we can trust our excess power calculation.
        return Ok(if charging_amps > MIN_EVSE_CHARGE_AMPS as f64 {
            debug!("{grid_export} < {LOW_THRESHOLD}, reducing to {MIN_EVSE_CHARGE_AMPS}");
            MIN_EVSE_CHARGE_AMPS
        } else if charging_amps > MIN_CHARGE_AMPS as f64 {
            debug!("{grid_export} <  {LOW_THRESHOLD}, reducing to {MIN_CHARGE_AMPS}");
            MIN_CHARGE_AMPS
        } else if charging_amps > 0.0 {
            debug!("{grid_export} <  {LOW_THRESHOLD}, reducing to 0");
            0
        } else {
            debug!("{grid_export} < {LOW_THRESHOLD}, staying at 0");
            0
        });
    }

    if grid_export < HIGH_THRESHOLD {
        // If grid_export is between LOW_THRESHOLD and HIGH_THRESHOLD, just leave it.
        return Ok(charging_amps as u8);
    }

    // grid_export is above HIGH_THRESHOLD, so we think we may have some additional power to
    // charge with.  However, to "creep" to the right target, we only adjust halfway to it, then
    // we clamp to within the feasible range.
    let excess_power = grid_export + car_power - LOW_THRESHOLD;
    let excess_amps = excess_power / voltage;
    let new_amps =
        ((charging_amps + (excess_amps - charging_amps) / 2.0) as u8).clamp(0, MAX_CHARGE_AMPS);

    // Adjusting the rate is free and easy as long as we're making the adjustment on the OpenEVSE.
    // But we want to be more careful if the new rate is below the OpenEVSE minimum threshold,
    // because that means we'll have to use the car API.
    //
    // We also want to be a little careful not to flip back and forth between car limiting and
    // OpenEVSE limiting.  To do that, if the new_amps is in the car-control range, we'll clamp it
    // to the car minimum.
    let target_amps = if new_amps >= MIN_EVSE_CHARGE_AMPS {
        new_amps
    } else if new_amps >= MIN_CHARGE_AMPS {
        MIN_CHARGE_AMPS
    } else {
        0
    };

    debug!(
        "Excess power {excess_power} -> excess amps {excess_amps} -> adjust from \
    {charging_amps} to new amps {new_amps} -> target {target_amps}",
    );

    Ok(target_amps)
}
