use chrono::{DateTime, FixedOffset};
use tracing::debug;

use crate::config::{BatteryConfig, OptimizerConfig};
use crate::tibber::{PriceCache, PricePoint};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BatteryMode {
    /// Charge from grid at maximum rate (cheapest slots)
    ChargeFull,
    /// Charge from grid at reduced rate (cheap but not cheapest)
    ChargeReduced,
    /// Discharge to grid at maximum rate (sell back at premium)
    DischargeToGrid,
    /// Self-consumption with slight grid bias (prevent feed-in at low prices)
    SelfConsumptionPreventFeedIn,
    /// Self-consumption with slight battery bias (prevent grid pull at high prices)
    SelfConsumptionPreventGridPull,
    /// Normal self-consumption (with offset for safety)
    SelfConsumption,
}

impl std::fmt::Display for BatteryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatteryMode::ChargeFull => write!(f, "charge_full"),
            BatteryMode::ChargeReduced => write!(f, "charge_reduced"),
            BatteryMode::DischargeToGrid => write!(f, "discharge_to_grid"),
            BatteryMode::SelfConsumptionPreventFeedIn => write!(f, "self_consumption_no_feedin"),
            BatteryMode::SelfConsumptionPreventGridPull => write!(f, "self_consumption_no_grid"),
            BatteryMode::SelfConsumption => write!(f, "self_consumption"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptimizationResult {
    pub mode: BatteryMode,
    pub grid_setpoint_w: f64,
    pub reason: String,
}

pub struct BatteryOptimizer {
    battery_config: BatteryConfig,
    optimizer_config: OptimizerConfig,
}

impl BatteryOptimizer {
    pub fn new(battery_config: BatteryConfig, optimizer_config: OptimizerConfig) -> Self {
        Self {
            battery_config,
            optimizer_config,
        }
    }

    /// Main optimization function - determines what the battery should do
    pub fn optimize(
        &self,
        current_soc: f64,
        current_price: &PricePoint,
        price_cache: &PriceCache,
    ) -> OptimizationResult {
        let future_prices = price_cache.future_prices();
        if future_prices.is_empty() {
            return OptimizationResult {
                mode: BatteryMode::SelfConsumption,
                grid_setpoint_w: self.optimizer_config.setpoint_offset_w,
                reason: "No price data available, defaulting to self-consumption".to_string(),
            };
        }

        let price = current_price.total;
        let tiers = self.calculate_price_tiers(price_cache);

        debug!(
            "Price: {:.4}, Tiers - Cheapest: {:.4}, Cheap: {:.4}, Expensive: {:.4}, Premium: {:.4}",
            price, tiers.cheapest_threshold, tiers.cheap_threshold,
            tiers.expensive_threshold, tiers.premium_threshold
        );

        // Check if we should discharge to grid (sell power) - HIGHEST PRIORITY when profitable
        if let Some(result) = self.check_grid_discharge(current_soc, price, &tiers, price_cache) {
            return result;
        }

        // Check charging modes with forward-looking planning
        if let Some(result) = self.check_charging(current_soc, price, &tiers, price_cache, &current_price.starts_at) {
            return result;
        }

        // Determine self-consumption mode based on price level
        self.determine_self_consumption_mode(price, &tiers)
    }

    fn check_grid_discharge(
        &self,
        soc: f64,
        price: f64,
        tiers: &PriceTiers,
        cache: &PriceCache,
    ) -> Option<OptimizationResult> {
        // Need sufficient SoC to discharge
        if soc <= self.battery_config.min_soc_percent + 15.0 {
            return None;
        }

        // Only discharge at premium prices
        if price < tiers.premium_threshold {
            return None;
        }

        // Calculate if discharging is profitable considering round-trip efficiency
        let efficiency = self.battery_config.round_trip_efficiency;
        let min_profitable_price = tiers.cheapest_threshold / efficiency + self.optimizer_config.min_discharge_spread;

        if price < min_profitable_price {
            debug!(
                "Price {:.4} below profitable threshold {:.4} (efficiency-adjusted)",
                price, min_profitable_price
            );
            return None;
        }

        // Check if there are enough cheap hours coming to recharge
        let energy_available = (soc - self.battery_config.min_soc_percent) / 100.0
            * self.battery_config.capacity_kwh;
        let hours_to_recharge = energy_available / (self.battery_config.max_charge_power_w / 1000.0 * efficiency);
        let slots_needed = (hours_to_recharge * 4.0).ceil() as usize;

        let cheap_slots = self.count_slots_below_threshold(cache, tiers.cheap_threshold);

        if cheap_slots < slots_needed / 2 {
            debug!(
                "Only {} cheap slots available, need at least {} to recharge",
                cheap_slots, slots_needed / 2
            );
            return None;
        }

        Some(OptimizationResult {
            mode: BatteryMode::DischargeToGrid,
            grid_setpoint_w: -self.battery_config.max_discharge_power_w,
            reason: format!(
                "Premium price {:.4} EUR (threshold {:.4}), discharging to grid. {} cheap slots available for recharge.",
                price, tiers.premium_threshold, cheap_slots
            ),
        })
    }

    fn check_charging(
        &self,
        soc: f64,
        price: f64,
        tiers: &PriceTiers,
        cache: &PriceCache,
        current_time: &DateTime<FixedOffset>,
    ) -> Option<OptimizationResult> {
        // Don't charge if already at max SoC
        if soc >= self.battery_config.max_soc_percent {
            return None;
        }

        // Calculate charge planning parameters
        let plan = self.calculate_charge_plan(soc, cache, current_time);

        debug!(
            "Charge plan: need {:.1}kWh, {} cheap slots available, {} cheapest slots, target SoC: {:.1}%",
            plan.energy_needed_kwh, plan.cheap_slots_available, plan.cheapest_slots_available, plan.target_soc
        );

        // FULL POWER charging during the absolute cheapest slots
        if price <= tiers.cheapest_threshold {
            return Some(OptimizationResult {
                mode: BatteryMode::ChargeFull,
                grid_setpoint_w: self.battery_config.max_charge_power_w,
                reason: format!(
                    "Cheapest price tier {:.4} EUR, charging at full power. SoC: {:.1}% -> target {:.1}%",
                    price, soc, plan.target_soc
                ),
            });
        }

        // Charging during cheap (but not cheapest) slots
        // Always charge if we're in a cheap slot and haven't reached target
        if price <= tiers.cheap_threshold && soc < plan.target_soc {
            // Calculate how aggressively we need to charge based on available slots
            let power_factor = self.calculate_charge_power_factor(&plan, price, tiers);
            let charge_power = self.battery_config.max_charge_power_w * power_factor;

            return Some(OptimizationResult {
                mode: if power_factor >= 0.9 { BatteryMode::ChargeFull } else { BatteryMode::ChargeReduced },
                grid_setpoint_w: charge_power,
                reason: format!(
                    "Cheap price tier {:.4} EUR, charging at {:.0}% power ({:.0}W). SoC: {:.1}% -> target {:.1}%, {} slots remaining",
                    price, power_factor * 100.0, charge_power, soc, plan.target_soc, plan.cheap_slots_available
                ),
            });
        }

        // Emergency charging if SoC is critically low
        if soc < self.battery_config.min_soc_percent + 5.0 && price < tiers.expensive_threshold {
            return Some(OptimizationResult {
                mode: BatteryMode::ChargeReduced,
                grid_setpoint_w: self.battery_config.max_charge_power_w * 0.5,
                reason: format!(
                    "Critical SoC {:.1}%, emergency charging at 50% power despite moderate price {:.4} EUR",
                    soc, price
                ),
            });
        }

        None
    }

    /// Calculate a forward-looking charge plan
    fn calculate_charge_plan(
        &self,
        current_soc: f64,
        cache: &PriceCache,
        current_time: &DateTime<FixedOffset>,
    ) -> ChargePlan {
        let tiers = self.calculate_price_tiers(cache);

        // Count cheap and cheapest slots
        let cheap_slots_available = self.count_slots_below_threshold(cache, tiers.cheap_threshold);
        let cheapest_slots_available = self.count_slots_below_threshold(cache, tiers.cheapest_threshold);

        // Calculate hours until next cheap period (for planning reserves)
        let hours_until_cheap = self.hours_until_next_cheap_period(cache, &tiers, current_time);

        // Estimate energy consumption during expensive period
        let consumption_kwh = hours_until_cheap * (self.optimizer_config.base_consumption_w / 1000.0);

        // Target SoC: enough to cover consumption until next cheap period + buffer
        // Minimum target is to always have reserves for one expensive cycle
        let min_reserve_kwh = consumption_kwh + (self.battery_config.capacity_kwh * 0.2); // 20% buffer
        let min_reserve_soc = (min_reserve_kwh / self.battery_config.capacity_kwh * 100.0)
            .min(self.battery_config.max_soc_percent);

        // During cheap periods, aim to charge fully
        // We want to maximize our charge during cheap periods
        let target_soc = self.battery_config.max_soc_percent;

        // Energy needed to reach target
        let energy_needed_kwh = (target_soc - current_soc) / 100.0 * self.battery_config.capacity_kwh;

        // Effective charge rate per slot (15 minutes = 0.25 hours)
        let efficiency = self.battery_config.round_trip_efficiency;
        let kwh_per_slot = (self.battery_config.max_charge_power_w / 1000.0) * 0.25 * efficiency;

        // Slots needed at full power
        let slots_needed_full_power = (energy_needed_kwh / kwh_per_slot).ceil() as usize;

        ChargePlan {
            target_soc,
            min_reserve_soc,
            energy_needed_kwh,
            cheap_slots_available,
            cheapest_slots_available,
            slots_needed_full_power,
            hours_until_cheap,
        }
    }

    /// Calculate how aggressively we should charge based on available slots and energy needed
    fn calculate_charge_power_factor(&self, plan: &ChargePlan, price: f64, tiers: &PriceTiers) -> f64 {
        // If we have more cheap slots than needed, we can charge at a lower rate
        // If we have fewer, we need to charge more aggressively

        if plan.cheap_slots_available == 0 {
            return 1.0; // Full power if this is our only chance
        }

        // Calculate the ratio of needed slots to available slots
        let slot_ratio = plan.slots_needed_full_power as f64 / plan.cheap_slots_available as f64;

        // If we need more slots than available, charge at full power
        if slot_ratio >= 1.0 {
            return 1.0;
        }

        // If we have plenty of slots, scale power based on how cheap this slot is
        // Cheapest slots: 100% power
        // Less cheap slots: proportionally less, but minimum 40%
        let price_range = tiers.cheap_threshold - tiers.cheapest_threshold;
        if price_range <= 0.0 {
            return 1.0;
        }

        let price_position = ((price - tiers.cheapest_threshold) / price_range).clamp(0.0, 1.0);

        // Scale from 100% at cheapest to 40% at cheap threshold
        // But increase if we're running low on slots
        let base_factor = 1.0 - (price_position * 0.6);

        // Adjust based on slot availability - if running low, charge harder
        let urgency_factor = slot_ratio.max(0.4);

        (base_factor * urgency_factor).clamp(0.4, 1.0)
    }

    /// Calculate hours until the next cheap price period
    fn hours_until_next_cheap_period(
        &self,
        cache: &PriceCache,
        tiers: &PriceTiers,
        _current_time: &DateTime<FixedOffset>,
    ) -> f64 {
        let future_prices = cache.future_prices();

        // Find the first expensive slot, then find how long until cheap prices return
        let mut in_expensive_period = false;
        let mut expensive_start: Option<DateTime<FixedOffset>> = None;

        for price in &future_prices {
            if price.total > tiers.cheap_threshold {
                if !in_expensive_period {
                    in_expensive_period = true;
                    expensive_start = Some(price.starts_at);
                }
            } else if in_expensive_period {
                // Found cheap price after expensive period
                if let Some(start) = expensive_start {
                    let duration = price.starts_at.signed_duration_since(start);
                    return duration.num_minutes() as f64 / 60.0;
                }
            }
        }

        // If we didn't find a transition, estimate based on typical daily cycle
        // Assume ~8 hours of expensive period
        8.0
    }

    fn determine_self_consumption_mode(&self, price: f64, tiers: &PriceTiers) -> OptimizationResult {
        let offset = self.optimizer_config.setpoint_offset_w;

        if price >= tiers.expensive_threshold {
            // High price - prevent pulling from grid, prefer battery
            // Negative setpoint means "try to feed X watts to grid" which forces battery use
            OptimizationResult {
                mode: BatteryMode::SelfConsumptionPreventGridPull,
                grid_setpoint_w: -offset,
                reason: format!(
                    "Expensive price {:.4} EUR (>= {:.4}), setpoint -{:.0}W to prevent grid pull",
                    price, tiers.expensive_threshold, offset
                ),
            }
        } else if price <= tiers.cheap_threshold {
            // Low price but not charging (already full?) - prevent feeding back to grid
            OptimizationResult {
                mode: BatteryMode::SelfConsumptionPreventFeedIn,
                grid_setpoint_w: offset,
                reason: format!(
                    "Low price {:.4} EUR but not charging, setpoint +{:.0}W to prevent feed-in",
                    price, offset
                ),
            }
        } else {
            // Moderate price - slight positive offset to prefer grid over battery discharge
            OptimizationResult {
                mode: BatteryMode::SelfConsumption,
                grid_setpoint_w: offset,
                reason: format!(
                    "Moderate price {:.4} EUR, setpoint +{:.0}W (preserve battery for expensive periods)",
                    price, offset
                ),
            }
        }
    }

    fn calculate_price_tiers(&self, cache: &PriceCache) -> PriceTiers {
        let prices = cache.future_prices();
        if prices.is_empty() {
            return PriceTiers::default();
        }

        let mut sorted: Vec<f64> = prices.iter().map(|p| p.total).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let len = sorted.len();

        // Cheapest tier: bottom 10% of prices (full power charging)
        let cheapest_idx = ((len as f64 * self.optimizer_config.cheapest_percentile / 100.0) as usize).max(1).min(len - 1);
        // Cheap tier: bottom 25% of prices (reduced charging)
        let cheap_idx = ((len as f64 * self.optimizer_config.charge_percentile / 100.0) as usize).min(len - 1);
        // Expensive tier: top 25% (prevent grid pull)
        let expensive_idx = ((len as f64 * (100.0 - self.optimizer_config.expensive_percentile) / 100.0) as usize).min(len - 1);
        // Premium tier: top 10% (discharge to grid)
        let premium_idx = ((len as f64 * self.optimizer_config.discharge_percentile / 100.0) as usize).min(len - 1);

        PriceTiers {
            cheapest_threshold: sorted[cheapest_idx],
            cheap_threshold: sorted[cheap_idx],
            expensive_threshold: sorted[expensive_idx],
            premium_threshold: sorted[premium_idx],
        }
    }

    fn count_slots_below_threshold(&self, cache: &PriceCache, threshold: f64) -> usize {
        cache
            .future_prices()
            .iter()
            .filter(|p| p.total <= threshold)
            .count()
    }

    /// Get information about upcoming price conditions
    pub fn get_forecast_info(&self, cache: &PriceCache) -> ForecastInfo {
        let tiers = self.calculate_price_tiers(cache);
        let future = cache.future_prices();

        let next_cheap = future
            .iter()
            .find(|p| p.total <= tiers.cheapest_threshold)
            .map(|p| p.starts_at.to_rfc3339());

        let next_expensive = future
            .iter()
            .find(|p| p.total >= tiers.premium_threshold)
            .map(|p| p.starts_at.to_rfc3339());

        ForecastInfo {
            next_cheap_slot: next_cheap,
            next_expensive_slot: next_expensive,
            cheap_slots_remaining: self.count_slots_below_threshold(cache, tiers.cheap_threshold),
            cheapest_slots_remaining: self.count_slots_below_threshold(cache, tiers.cheapest_threshold),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PriceTiers {
    /// Bottom 10% - full power charging
    cheapest_threshold: f64,
    /// Bottom 25% - reduced charging
    cheap_threshold: f64,
    /// Top 25% - prevent grid pull
    expensive_threshold: f64,
    /// Top 10% - discharge to grid
    premium_threshold: f64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ChargePlan {
    /// Target SoC to reach during cheap period
    target_soc: f64,
    /// Minimum SoC to maintain as reserve
    min_reserve_soc: f64,
    /// Energy needed to reach target (kWh)
    energy_needed_kwh: f64,
    /// Number of cheap price slots available
    cheap_slots_available: usize,
    /// Number of cheapest price slots available
    cheapest_slots_available: usize,
    /// Slots needed at full power to reach target
    slots_needed_full_power: usize,
    /// Hours until next cheap period
    hours_until_cheap: f64,
}

#[derive(Debug, Clone)]
pub struct ForecastInfo {
    pub next_cheap_slot: Option<String>,
    pub next_expensive_slot: Option<String>,
    pub cheap_slots_remaining: usize,
    pub cheapest_slots_remaining: usize,
}
