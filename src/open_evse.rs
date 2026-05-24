use std::time::Duration;

use jiff::civil::DateTime;
use log::trace;
use reqwest::blocking::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::{NrgyResult, config::OpenEvseConfig, poll_thread::Pollable};

#[derive(Error, Debug)]
pub enum EvseError {
    #[error("EVSE command failed")]
    CommandFailed,
    #[error("Unexpected response: {0}")]
    ParseError(String),
    #[error("Request error: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Not charging")]
    NotCharging,
    #[error("No clock installed")]
    NoClock,
}

pub type EvseResult<T> = Result<T, EvseError>;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum EvseState {
    #[default]
    Unknown,
    NotConnected,
    Connected,
    Charging,
    VentRequired,
    DiodeCheckFailed,
    GfciFault,
    NoGround,
    StuckRelay,
    GfciSelfTestFailure,
    OverTemperature,
    Sleeping,
    Disabled,
}

#[expect(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LcdColor {
    #[default]
    Off = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Violet = 5,
    Teal = 6,
    White = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LcdType {
    Monochrome = 0,
    #[default]
    Rgb = 1,
}

#[expect(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ServiceLevel {
    Auto,
    Level1,
    Level2,
}

#[expect(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct EvseFlags {
    pub service_level: u8,
    pub diode_check: bool,
    pub vent_required: bool,
    pub ground_check: bool,
    pub stuck_relay_check: bool,
    pub auto_service_level: bool,
    pub auto_start: bool,
    pub serial_debug: bool,
    pub lcd_type: LcdType,
    pub gfi_self_test: bool,
}

#[expect(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct FaultCounters {
    pub gfi_self_test: u32,
    pub ground: u32,
    pub stuck_relay: u32,
}

#[expect(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct Temperature {
    pub ds3231: f64,
    pub mcp9808: f64,
    pub tmp007: f64,
}

#[expect(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct ElapsedSession {
    pub seconds: u32,
    pub wh: f64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OpenEvseTimer {
    start: Option<(u8, u8)>,
    end: Option<(u8, u8)>,
}

#[derive(Default, Debug)]
pub struct OpenEvse {
    base_url: String,
    credentials: Option<(String, String)>,
    http: Client,
    pub state: EvseState,
    pub flags: EvseFlags,
    pub temperature: Temperature,
    pub current_capacity: u8,
    pub current_capacity_range: (u8, u8),
    pub timer: OpenEvseTimer,
    pub charging_current: f64,
    pub charging_voltage: f64,
}

impl Pollable for OpenEvse {
    fn name(&self) -> &'static str {
        "OpenEVSE"
    }

    fn init(&mut self) -> NrgyResult<()> {
        self.set_timer(self.timer)?;
        self.poll()?;
        Ok(())
    }

    fn poll(&mut self) -> crate::NrgyResult<()> {
        self.state()?;
        self.charging_current_and_voltage()?;
        self.current_capacity()?;
        self.current_capacity_range()?;
        self.flags()?;
        Ok(())
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
}

#[expect(dead_code)]
impl OpenEvse {
    pub fn new(config: OpenEvseConfig) -> Self {
        OpenEvse {
            base_url: format!("http://{}", config.hostname),
            credentials: config.username.zip(config.password),
            http: Client::new(),
            ..Default::default()
        }
    }

    pub fn state(&mut self) -> EvseResult<EvseState> {
        trace!("Getting EVSE state");
        let data = self.request(&["GS"])?;
        trace!("Got {data:?}");
        let code = Self::parse_u8(&data, 0, "state")?;
        self.state = match code {
            1 => EvseState::NotConnected,
            2 => EvseState::Connected,
            3 => EvseState::Charging,
            4 => EvseState::VentRequired,
            5 => EvseState::DiodeCheckFailed,
            6 => EvseState::GfciFault,
            7 => EvseState::NoGround,
            8 => EvseState::StuckRelay,
            9 => EvseState::GfciSelfTestFailure,
            10 => EvseState::OverTemperature,
            254 => EvseState::Sleeping,
            255 => EvseState::Disabled,
            _ => EvseState::Unknown,
        };
        trace!("State: {:?}", self.state);
        Ok(self.state)
    }

    pub fn plugged_in(&mut self) -> EvseResult<bool> {
        match self.state {
            EvseState::Connected | EvseState::Charging => Ok(true),
            EvseState::NotConnected => Ok(false),
            _ => {
                let mut state = self.state;
                if [EvseState::Unknown, EvseState::Sleeping, EvseState::Disabled].contains(&state) {
                    // If it was one of the above states, try to wake it up to get the state.
                    self.enable()?;
                    state = self.state()?;
                    match state {
                        EvseState::Unknown | EvseState::Sleeping => self.sleep()?,
                        EvseState::Disabled => self.disable()?,
                        _ => {}
                    }
                }
                Ok([EvseState::Charging, EvseState::Connected].contains(&state))
            }
        }
    }

    pub fn is_charging(&self) -> bool {
        self.state == EvseState::Charging
    }

    pub fn enable(&self) -> EvseResult<()> {
        self.request(&["FE"])?;
        Ok(())
    }

    pub fn disable(&self) -> EvseResult<()> {
        self.request(&["FD"])?;
        Ok(())
    }

    pub fn sleep(&self) -> EvseResult<()> {
        self.request(&["FS"])?;
        Ok(())
    }

    pub fn lcd_backlight_color(&self, color: LcdColor) -> EvseResult<()> {
        self.request(&["FB", &(color as u8).to_string()])?;
        Ok(())
    }

    pub fn display_text(&self, x: u8, y: u8, text: &str) -> EvseResult<()> {
        self.request(&["FP", &x.to_string(), &y.to_string(), text])?;
        Ok(())
    }

    pub fn time_limit(&self) -> EvseResult<u32> {
        let data = self.request(&["G3"])?;
        Ok(Self::parse_u32(&data, 0, "time limit")? * 15)
    }

    pub fn set_time_limit(&self, minutes: u32) -> EvseResult<()> {
        let quarters = (minutes as f64 / 15.0).round() as u32;
        self.request(&["S3", &quarters.to_string()])?;
        Ok(())
    }

    pub fn current_capacity(&mut self) -> EvseResult<u8> {
        let data = self.request(&["GE"])?;
        self.current_capacity = Self::parse_u8(&data, 0, "capacity")?;
        Ok(self.current_capacity)
    }

    pub fn set_current_capacity(&self, amps: u8) -> EvseResult<()> {
        self.request(&["SC", &amps.to_string()])?;
        Ok(())
    }

    pub fn current_capacity_range(&mut self) -> EvseResult<(u8, u8)> {
        trace!("Requesting current capacity range");
        let data = self.request(&["GC"])?;
        trace!("Got {data:?}");
        self.current_capacity_range = (
            Self::parse_u8(&data, 0, "min")?,
            Self::parse_u8(&data, 1, "max")?,
        );
        trace!("Parsed: {:?}", self.current_capacity_range);
        Ok(self.current_capacity_range)
    }

    pub fn charging_current_and_voltage(&mut self) -> EvseResult<(f64, f64)> {
        trace!("Getting charging current and voltage");
        let data = self.request(&["GG"])?;
        trace!("Got {data:?}");
        self.charging_current =
            Self::parse_f64(&data, 0, "amps")?.clamp(0.0, f64::INFINITY) / 1000.0;
        self.charging_voltage =
            Self::parse_f64(&data, 1, "volts")?.clamp(0.0, f64::INFINITY) / 1000.0;
        trace!(
            "Parsed: current: {} voltage: {}",
            self.charging_current, self.charging_voltage
        );
        Ok((self.charging_current, self.charging_voltage))
    }

    pub fn temperature(&mut self) -> EvseResult<Temperature> {
        let data = self.request(&["GP"])?;
        self.temperature = Temperature {
            ds3231: Self::parse_f64(&data, 0, "ds3231")? / 10.0,
            mcp9808: Self::parse_f64(&data, 1, "mcp9808")? / 10.0,
            tmp007: Self::parse_f64(&data, 2, "tmp007")? / 10.0,
        };
        Ok(self.temperature.clone())
    }

    pub fn flags(&mut self) -> EvseResult<EvseFlags> {
        let data = self.request(&["GE"])?;
        let raw = u16::from_str_radix(
            data.get(1)
                .ok_or_else(|| EvseError::ParseError("missing flags in GE response".into()))?,
            16,
        )
        .map_err(|_| EvseError::ParseError("invalid flags".into()))?;
        self.flags = EvseFlags {
            service_level: (raw & 0x0001) as u8 + 1,
            diode_check: raw & 0x0002 == 0,
            vent_required: raw & 0x0004 == 0,
            ground_check: raw & 0x0008 == 0,
            stuck_relay_check: raw & 0x0010 == 0,
            auto_service_level: raw & 0x0020 == 0,
            auto_start: raw & 0x0040 == 0,
            serial_debug: raw & 0x0080 != 0,
            lcd_type: if raw & 0x0100 != 0 {
                LcdType::Monochrome
            } else {
                LcdType::Rgb
            },
            gfi_self_test: raw & 0x0200 == 0,
        };
        Ok(self.flags.clone())
    }

    pub fn reset(&self) -> EvseResult<()> {
        self.request(&["FR"])?;
        Ok(())
    }

    pub fn set_diode_check(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["FF", "D", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn set_gfi_self_test(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["FF", "F", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn set_ground_check(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["FF", "G", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn set_vent_required(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["FF", "V", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn set_stuck_relay_check(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["FF", "R", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn charge_limit_kwh(&self) -> EvseResult<u32> {
        let data = self.request(&["GH"])?;
        Self::parse_u32(&data, 0, "charge limit")
    }

    pub fn set_charge_limit_kwh(&self, kwh: u32) -> EvseResult<()> {
        self.request(&["SH", &kwh.to_string()])?;
        Ok(())
    }

    pub fn accumulated_wh(&self) -> EvseResult<u32> {
        let data = self.request(&["GU"])?;
        Self::parse_u32(&data, 1, "accumulated wh")
    }

    pub fn set_accumulated_wh(&self, wh: u32) -> EvseResult<()> {
        self.request(&["SK", &wh.to_string()])?;
        Ok(())
    }

    pub fn set_service_level(&self, level: ServiceLevel) -> EvseResult<()> {
        let code = match level {
            ServiceLevel::Auto => "A",
            ServiceLevel::Level1 => "1",
            ServiceLevel::Level2 => "2",
        };
        self.request(&["SL", code])?;
        Ok(())
    }

    pub fn time(&self) -> EvseResult<DateTime> {
        let data = self.request(&["GT"])?;
        if data.len() >= 6
            && data[0] == "165"
            && data[1] == "165"
            && data[2] == "165"
            && data[3] == "165"
            && data[4] == "165"
            && data[5] == "85"
        {
            return Err(EvseError::NoClock);
        }
        let year = Self::parse_u32(&data, 0, "year")? as i16 + 2000;
        let month = Self::parse_u8(&data, 1, "month")? as i8;
        let day = Self::parse_u8(&data, 2, "day")? as i8;
        let hour = Self::parse_u8(&data, 3, "hour")? as i8;
        let minute = Self::parse_u8(&data, 4, "minute")? as i8;
        let second = Self::parse_u8(&data, 5, "second")? as i8;
        DateTime::new(year, month, day, hour, minute, second, 0)
            .map_err(|e| EvseError::ParseError(format!("invalid datetime: {e}")))
    }

    pub fn set_time(&self, dt: &DateTime) -> EvseResult<()> {
        self.request(&[
            "S1",
            &(dt.year() % 100).to_string(),
            &dt.month().to_string(),
            &dt.day().to_string(),
            &dt.hour().to_string(),
            &dt.minute().to_string(),
            &dt.second().to_string(),
        ])?;
        Ok(())
    }

    pub fn ammeter_calibration(&self, enabled: bool) -> EvseResult<()> {
        self.request(&["S2", if enabled { "1" } else { "0" }])?;
        Ok(())
    }

    pub fn ammeter_settings(&self) -> EvseResult<(i32, i32)> {
        let data = self.request(&["GA"])?;
        Ok((
            Self::parse_i32(&data, 0, "scalefactor")?,
            Self::parse_i32(&data, 1, "offset")?,
        ))
    }

    pub fn set_ammeter_settings(&self, scalefactor: i32, offset: i32) -> EvseResult<()> {
        self.request(&["SA", &scalefactor.to_string(), &offset.to_string()])?;
        Ok(())
    }

    pub fn voltmeter_settings(&self) -> EvseResult<(i32, i32)> {
        let data = self.request(&["GM"])?;
        Ok((
            Self::parse_i32(&data, 0, "scalefactor")?,
            Self::parse_i32(&data, 1, "offset")?,
        ))
    }

    pub fn set_voltmeter_settings(&self, scalefactor: i32, offset: i32) -> EvseResult<()> {
        self.request(&["SM", &scalefactor.to_string(), &offset.to_string()])?;
        Ok(())
    }

    pub fn set_timer(&mut self, timer: OpenEvseTimer) -> EvseResult<()> {
        let (sh, sm, eh, em) = match (timer.start, timer.end) {
            (Some((sh, sm)), Some((eh, em))) => (sh, sm, eh, em),
            _ => (0, 0, 0, 0),
        };
        self.request(&[
            "ST",
            &sh.to_string(),
            &sm.to_string(),
            &eh.to_string(),
            &em.to_string(),
        ])?;
        self.timer = timer;
        Ok(())
    }

    pub fn fault_counters(&self) -> EvseResult<FaultCounters> {
        let data = self.request(&["GF"])?;
        Ok(FaultCounters {
            gfi_self_test: Self::parse_u32_hex(&data, 0, "gfi_self_test")?,
            ground: Self::parse_u32_hex(&data, 1, "ground")?,
            stuck_relay: Self::parse_u32_hex(&data, 2, "stuck_relay")?,
        })
    }

    pub fn elapsed(&self) -> EvseResult<ElapsedSession> {
        let status_data = self.request(&["GS"])?;
        if Self::parse_u8(&status_data, 0, "state")? != 3 {
            return Err(EvseError::NotCharging);
        }
        let seconds = Self::parse_u32(&status_data, 1, "elapsed seconds")?;
        let energy_data = self.request(&["GU"])?;
        let wh = Self::parse_f64(&energy_data, 0, "wh")? / 3600.0;
        Ok(ElapsedSession { seconds, wh })
    }

    pub fn version(&self) -> EvseResult<(String, String)> {
        let data = self.request(&["GV"])?;
        let firmware = data
            .get(0)
            .ok_or_else(|| EvseError::ParseError("missing firmware version".into()))?
            .clone();
        let protocol = data
            .get(1)
            .ok_or_else(|| EvseError::ParseError("missing protocol version".into()))?
            .clone();
        Ok((firmware, protocol))
    }

    fn request(&self, args: &[&str]) -> EvseResult<Vec<String>> {
        let url = format!("{}/r?json=1&rapi=%24{}", self.base_url, args.join("+"));

        let mut req = self.http.get(&url);
        if let Some((user, pass)) = &self.credentials {
            req = req.basic_auth(user, Some(pass));
        }

        #[derive(Deserialize)]
        struct Response {
            ret: String,
        }

        let resp: Response = req.send()?.error_for_status()?.json()?;

        // Response format: "$OK arg1 arg2^checksum" or "$NK^checksum"
        let ret = resp.ret.trim_start_matches('$');
        let ret = ret.split('^').next().unwrap_or(ret);
        let mut parts = ret.split_whitespace();

        match parts.next() {
            Some("OK") => Ok(parts.map(String::from).collect()),
            _ => Err(EvseError::CommandFailed),
        }
    }

    fn parse_u8(data: &[String], index: usize, field: &str) -> EvseResult<u8> {
        data.get(index)
            .ok_or_else(|| EvseError::ParseError(format!("missing {field}")))?
            .parse()
            .map_err(|_| EvseError::ParseError(format!("invalid {field}: {data:?}")))
    }

    fn parse_u32(data: &[String], index: usize, field: &str) -> EvseResult<u32> {
        data.get(index)
            .ok_or_else(|| EvseError::ParseError(format!("missing {field}")))?
            .parse()
            .map_err(|_| EvseError::ParseError(format!("invalid {field}: {data:?}")))
    }

    fn parse_u32_hex(data: &[String], index: usize, field: &str) -> EvseResult<u32> {
        u32::from_str_radix(
            data.get(index)
                .ok_or_else(|| EvseError::ParseError(format!("missing {field}: {data:?}")))?,
            16,
        )
        .map_err(|_| EvseError::ParseError(format!("invalid {field}")))
    }

    fn parse_i32(data: &[String], index: usize, field: &str) -> EvseResult<i32> {
        data.get(index)
            .ok_or_else(|| EvseError::ParseError(format!("missing {field}")))?
            .parse()
            .map_err(|_| EvseError::ParseError(format!("invalid {field}: {data:?}")))
    }

    fn parse_f64(data: &[String], index: usize, field: &str) -> EvseResult<f64> {
        data.get(index)
            .ok_or_else(|| EvseError::ParseError(format!("missing {field}")))?
            .parse()
            .map_err(|_| EvseError::ParseError(format!("invalid {field}: {data:?}")))
    }
}
