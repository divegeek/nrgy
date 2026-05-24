use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Config {
    pub tesla: TeslaConfig,
    pub solar_edge: SolarEdgeconfig,
    pub open_evse: OpenEvseConfig,
}

#[expect(unused)]
#[derive(Deserialize, Debug, Default)]
pub struct TeslaConfig {
    pub client_id: String,
    pub client_secret: String,
    pub private_key: String,
    pub vin: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SolarEdgeconfig {
    pub site_id: u32,
    pub api_key: String,
}

#[derive(Deserialize)]
pub struct OpenEvseConfig {
    pub hostname: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub fn load() -> Result<Config, Box<dyn std::error::Error>> {
    let path = config_path();
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let config = toml::from_str(&text)?;
    Ok(config)
}

pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("nrgy")
        .join("config.toml")
}
