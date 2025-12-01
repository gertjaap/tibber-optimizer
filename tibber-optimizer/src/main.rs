mod config;
mod mqtt;
mod optimizer;
mod tibber;

use anyhow::Result;
use std::time::Duration;
use tracing::{error, info, warn};

use config::Config;
use mqtt::{MqttClient, OptimizerStatus, PriceStatsJson};
use optimizer::BatteryOptimizer;
use tibber::TibberClient;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tibber_optimizer=info".parse()?)
                .add_directive("rumqttc=warn".parse()?),
        )
        .init();

    info!("Tibber Battery Optimizer starting up");

    // Load configuration
    let config = Config::load_from_env_or_file()?;
    info!("Configuration loaded successfully");

    // Initialize components
    let tibber_client = TibberClient::new(config.tibber.clone());
    let mqtt_client = MqttClient::new(config.mqtt.clone()).await?;
    let optimizer = BatteryOptimizer::new(config.battery.clone(), config.optimizer.clone());

    // Initial price fetch
    info!("Fetching initial price data from Tibber...");
    if let Err(e) = tibber_client.fetch_prices().await {
        error!("Failed to fetch initial prices: {}", e);
        // Continue anyway, will retry later
    }

    // Main loop - run every minute
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    let mut last_setpoint: Option<f64> = None;

    loop {
        interval.tick().await;

        // Refresh prices if needed
        if let Err(e) = tibber_client.refresh_if_needed().await {
            warn!("Failed to refresh prices: {}", e);
        }

        // Get current state
        let price_cache = tibber_client.get_cache().await;
        let current_price = match tibber_client.get_current_price().await {
            Some(p) => p,
            None => {
                warn!("No current price available, skipping optimization cycle");
                continue;
            }
        };

        let battery_state = mqtt_client.get_battery_state().await;

        // Check if we have valid battery state
        if battery_state.last_soc_update.is_none() {
            warn!("No battery SoC data received yet, using default self-consumption mode");
            if let Err(e) = mqtt_client.publish_grid_setpoint(200.0).await {
                error!("Failed to publish grid setpoint: {}", e);
            }
            continue;
        }

        // Run optimization
        let result = optimizer.optimize(battery_state.soc, &current_price, &price_cache);

        info!(
            "Optimization result: mode={}, setpoint={:.0}W, soc={:.1}%, price={:.4} EUR - {}",
            result.mode, result.grid_setpoint_w, battery_state.soc, current_price.total, result.reason
        );

        // Only publish setpoint if it changed (avoid MQTT spam)
        let should_publish = match last_setpoint {
            None => true,
            Some(last) => (last - result.grid_setpoint_w).abs() > 10.0,
        };

        if should_publish {
            if let Err(e) = mqtt_client.publish_grid_setpoint(result.grid_setpoint_w).await {
                error!("Failed to publish grid setpoint: {}", e);
            } else {
                last_setpoint = Some(result.grid_setpoint_w);
            }
        }

        // Always publish current price
        if let Err(e) = mqtt_client.publish_price_info(&current_price).await {
            error!("Failed to publish price info: {}", e);
        }

        // Publish extended status
        let forecast = optimizer.get_forecast_info(&price_cache);
        let status = OptimizerStatus {
            current_price: current_price.total,
            current_mode: result.mode.to_string(),
            grid_setpoint_w: result.grid_setpoint_w,
            actual_setpoint_w: battery_state.current_setpoint_w,
            battery_soc: battery_state.soc,
            price_stats: price_cache.price_stats().map(|s| PriceStatsJson {
                min: s.min,
                max: s.max,
                avg: s.avg,
                p25: s.p25,
                p75: s.p75,
                p90: s.p90,
            }),
            next_cheap_slot: forecast.next_cheap_slot,
            next_expensive_slot: forecast.next_expensive_slot,
            cheap_slots_remaining: forecast.cheap_slots_remaining,
            cheapest_slots_remaining: forecast.cheapest_slots_remaining,
        };

        if let Err(e) = mqtt_client.publish_status(&status).await {
            error!("Failed to publish status: {}", e);
        }
    }
}
