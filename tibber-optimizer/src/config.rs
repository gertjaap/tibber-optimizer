use serde::Deserialize;
use std::path::Path;
use anyhow::Result;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub tibber: TibberConfig,
    pub mqtt: MqttConfig,
    pub battery: BatteryConfig,
    pub optimizer: OptimizerConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TibberConfig {
    pub api_token: String,
    #[serde(default = "default_tibber_url")]
    pub api_url: String,
    /// How often to refresh prices (in seconds), default 15 minutes
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
}

fn default_tibber_url() -> String {
    "https://api.tibber.com/v1-beta/gql".to_string()
}

fn default_refresh_interval() -> u64 {
    900 // 15 minutes
}

#[derive(Debug, Deserialize, Clone)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Topic to subscribe to for battery State of Charge (0-100)
    pub soc_topic: String,
    /// Topic to subscribe to for current grid setpoint (N/...for Victron)
    pub grid_setpoint_read_topic: String,
    /// Topic to publish the grid setpoint to (W/... for Victron)
    pub grid_setpoint_write_topic: String,
    /// Topic to publish current price info
    pub price_topic: String,
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_client_id() -> String {
    "tibber-optimizer".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct BatteryConfig {
    /// Battery capacity in kWh
    pub capacity_kwh: f64,
    /// Round-trip efficiency (0.0 - 1.0), e.g., 0.90 for 90%
    pub round_trip_efficiency: f64,
    /// Minimum SoC to maintain (0-100)
    #[serde(default = "default_min_soc")]
    pub min_soc_percent: f64,
    /// Maximum SoC target (0-100)
    #[serde(default = "default_max_soc")]
    pub max_soc_percent: f64,
    /// Maximum charge power in watts
    #[serde(default = "default_max_power")]
    pub max_charge_power_w: f64,
    /// Maximum discharge power in watts
    #[serde(default = "default_max_power")]
    pub max_discharge_power_w: f64,
}

fn default_min_soc() -> f64 {
    10.0
}

fn default_max_soc() -> f64 {
    100.0
}

fn default_max_power() -> f64 {
    15000.0
}

#[derive(Debug, Deserialize, Clone)]
pub struct OptimizerConfig {
    /// Minimum price spread (EUR) to consider grid discharge worthwhile
    /// Accounts for round-trip losses
    #[serde(default = "default_min_spread")]
    pub min_discharge_spread: f64,
    /// Price percentile for FULL power charging (cheapest X%)
    #[serde(default = "default_cheapest_percentile")]
    pub cheapest_percentile: f64,
    /// Price percentile threshold for reduced charging (cheap X%)
    #[serde(default = "default_charge_percentile")]
    pub charge_percentile: f64,
    /// Price percentile threshold for expensive (prevent grid pull)
    #[serde(default = "default_expensive_percentile")]
    pub expensive_percentile: f64,
    /// Price percentile threshold for grid discharge (premium - top X%)
    #[serde(default = "default_discharge_percentile")]
    pub discharge_percentile: f64,
    /// Base house consumption estimate in watts (used for planning)
    #[serde(default = "default_base_consumption")]
    pub base_consumption_w: f64,
    /// Setpoint offset in watts for self-consumption modes
    /// Positive = pull from grid, Negative = feed to grid
    #[serde(default = "default_setpoint_offset")]
    pub setpoint_offset_w: f64,
}

fn default_min_spread() -> f64 {
    0.05 // 5 cents minimum spread
}

fn default_cheapest_percentile() -> f64 {
    10.0 // Full power charge in cheapest 10%
}

fn default_charge_percentile() -> f64 {
    25.0 // Reduced charge when price is in lowest 25%
}

fn default_expensive_percentile() -> f64 {
    25.0 // Prevent grid pull when price is in top 25%
}

fn default_discharge_percentile() -> f64 {
    90.0 // Only discharge to grid when price is in top 10%
}

fn default_base_consumption() -> f64 {
    500.0 // 500W base consumption estimate
}

fn default_setpoint_offset() -> f64 {
    200.0 // 200W offset to account for ESS response lag
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    pub fn load_from_env_or_file() -> Result<Self> {
        // Home Assistant addons typically use /data/options.json
        let ha_options = Path::new("/data/options.json");
        if ha_options.exists() {
            let content = std::fs::read_to_string(ha_options)?;
            let config: Config = serde_json::from_str(&content)?;
            return Ok(config);
        }

        // Fall back to config.yaml in current directory or /config
        let paths = ["config.yaml", "/config/tibber-optimizer.yaml"];
        for path in paths {
            if Path::new(path).exists() {
                return Self::load(path);
            }
        }

        anyhow::bail!("No configuration file found")
    }
}
