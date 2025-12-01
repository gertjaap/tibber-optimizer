use anyhow::Result;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::MqttConfig;

#[derive(Debug, Clone, Default)]
pub struct BatteryState {
    /// Current state of charge (0-100)
    pub soc: f64,
    /// Current grid setpoint as read from the system
    pub current_setpoint_w: Option<f64>,
    /// Last SoC update timestamp
    pub last_soc_update: Option<chrono::DateTime<chrono::Utc>>,
    /// Last setpoint update timestamp
    pub last_setpoint_update: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct MqttClient {
    client: AsyncClient,
    config: MqttConfig,
    battery_state: Arc<RwLock<BatteryState>>,
}

impl MqttClient {
    pub async fn new(config: MqttConfig) -> Result<Self> {
        let mut mqtt_options = MqttOptions::new(
            &config.client_id,
            &config.host,
            config.port,
        );

        mqtt_options.set_keep_alive(Duration::from_secs(30));

        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            mqtt_options.set_credentials(username, password);
        }

        let (client, mut eventloop) = AsyncClient::new(mqtt_options, 100);
        let battery_state = Arc::new(RwLock::new(BatteryState::default()));
        let battery_state_clone = battery_state.clone();
        let soc_topic = config.soc_topic.clone();
        let setpoint_read_topic = config.grid_setpoint_read_topic.clone();

        // Spawn event loop handler
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        if let Ok(payload_str) = std::str::from_utf8(&publish.payload) {
                            // Handle SoC updates (Victron format)
                            if publish.topic == soc_topic {
                                if let Some(value) = parse_victron_soc(payload_str) {
                                    let mut state = battery_state_clone.write().await;
                                    state.soc = value;
                                    state.last_soc_update = Some(chrono::Utc::now());
                                    debug!("Updated battery SoC: {:.1}%", value);
                                }
                            }
                            // Handle setpoint updates
                            else if publish.topic == setpoint_read_topic {
                                if let Some(value) = parse_mqtt_value(payload_str) {
                                    let mut state = battery_state_clone.write().await;
                                    state.current_setpoint_w = Some(value);
                                    state.last_setpoint_update = Some(chrono::Utc::now());
                                    debug!("Updated grid setpoint reading: {:.0}W", value);
                                }
                            }
                        }
                    }
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        info!("Connected to MQTT broker");
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {
                        debug!("Subscription acknowledged");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("MQTT connection error: {:?}", e);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });

        // Small delay to let connection establish
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Subscribe to SoC topic
        client
            .subscribe(&config.soc_topic, QoS::AtLeastOnce)
            .await?;
        info!("Subscribed to SoC topic: {}", config.soc_topic);

        // Subscribe to setpoint read topic
        client
            .subscribe(&config.grid_setpoint_read_topic, QoS::AtLeastOnce)
            .await?;
        info!("Subscribed to setpoint read topic: {}", config.grid_setpoint_read_topic);

        Ok(Self {
            client,
            config,
            battery_state,
        })
    }

    pub async fn get_battery_state(&self) -> BatteryState {
        self.battery_state.read().await.clone()
    }

    pub async fn publish_grid_setpoint(&self, setpoint_w: f64) -> Result<()> {
        let payload = serde_json::json!({
            "value": setpoint_w
        });

        self.client
            .publish(
                &self.config.grid_setpoint_write_topic,
                QoS::AtLeastOnce,
                false,
                payload.to_string(),
            )
            .await?;

        debug!("Published grid setpoint: {} W to {}", setpoint_w, self.config.grid_setpoint_write_topic);
        Ok(())
    }

    pub async fn publish_price_info(&self, price: &crate::tibber::PricePoint) -> Result<()> {
        let payload = serde_json::json!({
            "total": price.total,
            "energy": price.energy,
            "tax": price.tax,
            "starts_at": price.starts_at.to_rfc3339(),
            "currency": "EUR"
        });

        self.client
            .publish(
                &self.config.price_topic,
                QoS::AtLeastOnce,
                true, // Retain so new subscribers get last price
                payload.to_string(),
            )
            .await?;

        debug!("Published current price: {} EUR/kWh", price.total);
        Ok(())
    }

    /// Publish extended price and optimization info
    pub async fn publish_status(&self, status: &OptimizerStatus) -> Result<()> {
        let topic = format!("{}/status", self.config.price_topic.trim_end_matches("/current"));

        let payload = serde_json::to_string(status)?;

        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                true,
                payload,
            )
            .await?;

        Ok(())
    }
}

/// Parse a simple value from MQTT payload - handles raw numbers and JSON {"value": x}
fn parse_mqtt_value(payload: &str) -> Option<f64> {
    // Try parsing as plain number first
    if let Ok(value) = payload.trim().parse::<f64>() {
        return Some(value);
    }

    // Try parsing as JSON with "value" field
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(value) = json.get("value").and_then(|v| v.as_f64()) {
            return Some(value);
        }
    }

    warn!("Failed to parse MQTT value: '{}'", payload);
    None
}

/// Parse SoC from Victron battery JSON: {"value": [{"soc": 75.5, ...}]}
fn parse_victron_soc(payload: &str) -> Option<f64> {
    // Try the Victron format first: {"value": [{"soc": x}]}
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(value_array) = json.get("value").and_then(|v| v.as_array()) {
            if let Some(first) = value_array.first() {
                if let Some(soc) = first.get("soc").and_then(|v| v.as_f64()) {
                    return Some(soc);
                }
            }
        }
        // Fall back to simple {"value": x} format
        if let Some(value) = json.get("value").and_then(|v| v.as_f64()) {
            return Some(value);
        }
    }

    // Try plain number
    if let Ok(value) = payload.trim().parse::<f64>() {
        return Some(value);
    }

    warn!("Failed to parse SoC value: '{}'", payload);
    None
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OptimizerStatus {
    pub current_price: f64,
    pub current_mode: String,
    pub grid_setpoint_w: f64,
    pub actual_setpoint_w: Option<f64>,
    pub battery_soc: f64,
    pub price_stats: Option<PriceStatsJson>,
    pub next_cheap_slot: Option<String>,
    pub next_expensive_slot: Option<String>,
    pub cheap_slots_remaining: usize,
    pub cheapest_slots_remaining: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PriceStatsJson {
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub p25: f64,
    pub p75: f64,
    pub p90: f64,
}
