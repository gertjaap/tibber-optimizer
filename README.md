# Tibber Battery Optimizer

A Home Assistant addon that optimizes battery charge/discharge cycles based on Tibber's dynamic quarter-hourly energy prices.

## Features

- Fetches quarter-hourly energy prices from Tibber API
- Publishes current energy price to MQTT
- Monitors battery State of Charge via MQTT
- Smart tiered charging strategy based on price percentiles
- Accounts for charge/discharge efficiency losses
- Compensates for ESS response lag with setpoint offsets
- Publishes grid setpoint to control Victron VenusOS ESS

## Operation Modes

The optimizer uses a tiered price strategy:

| Mode | Condition | Setpoint | Description |
|------|-----------|----------|-------------|
| **ChargeFull** | Price in bottom 10% | +15000W | Maximum rate charging |
| **ChargeReduced** | Price in bottom 25% | Variable | Proportional charging (30-100%) |
| **SelfConsumption** | Moderate price | +200W | Preserve battery for expensive periods |
| **PreventFeedIn** | Low price, not charging | +200W | Positive offset |
| **PreventGridPull** | High price (top 25%) | -200W | Negative offset |
| **DischargeToGrid** | Premium price (top 10%) | -15000W | Sell back to grid |

**Note:** The setpoint is the grid target, not battery power. Setting 15kW allows house loads (up to ~2kW) on top of battery charging while staying under a typical 3x25A (17.25kW) connection. The battery inverters will self-limit to their actual capacity.

### Setpoint Offset Logic

The ESS doesn't respond instantly to load changes. The setpoint offset compensates:
- **Low prices (+offset)**: Prevents accidentally feeding solar back at cheap rates
- **High prices (-offset)**: Prevents accidentally pulling from grid at expensive rates

### Smart Charging

The algorithm calculates how many 15-minute slots are needed to reach target SoC:
- **Full power** only during the absolute cheapest slots (bottom 10%)
- **Reduced power** during other cheap slots (10-25%) only if there aren't enough cheapest slots
- This spreads charging while prioritizing the best prices

### Grid Discharge Safety

Only discharges to grid when ALL conditions are met:
1. Price is in top 10% (premium)
2. SoC is above minimum + 15%
3. Profit exceeds efficiency-adjusted threshold
4. Enough cheap slots exist to recharge

## Installation

### As Home Assistant Addon

1. Add this repository to your Home Assistant addon store
2. Install the "Tibber Battery Optimizer" addon
3. Configure via the HA UI (all settings available there)
4. Start the addon

### Standalone Docker

```bash
docker build -t tibber-optimizer .
docker run -v $(pwd)/config.yaml:/app/config.yaml tibber-optimizer
```

### From Source

```bash
cargo build --release
./target/release/tibber-optimizer
```

## Configuration

When running as an HA addon, all configuration is done through the Home Assistant UI.

For standalone use, copy `config.example.yaml` to `config.yaml`.

### Key Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `cheapest_percentile` | 10% | Full power charging threshold |
| `charge_percentile` | 25% | Reduced charging threshold |
| `expensive_percentile` | 25% | Prevent grid pull threshold |
| `discharge_percentile` | 90% | Grid discharge threshold |
| `setpoint_offset_w` | 200W | ESS lag compensation |
| `min_discharge_spread` | 0.05 EUR | Minimum profitable spread |

### Victron VenusOS MQTT Topics

```yaml
mqtt:
  soc_topic: "N/<portal_id>/system/0/Batteries/Soc"
  grid_setpoint_topic: "W/<portal_id>/settings/0/Settings/CGwacs/AcPowerSetPoint"
```

## MQTT Output

### Grid Setpoint
```json
{"value": 0}
```

### Current Price
```json
{
  "total": 0.2468,
  "energy": 0.0819,
  "tax": 0.1649,
  "starts_at": "2025-12-01T09:45:00+01:00",
  "currency": "EUR"
}
```

### Status
```json
{
  "current_price": 0.2468,
  "current_mode": "self_consumption_no_grid",
  "grid_setpoint_w": -100,
  "battery_soc": 75.5,
  "price_stats": {
    "min": 0.2177,
    "max": 0.2923,
    "avg": 0.2485
  },
  "next_cheap_slot": "2025-12-01T05:00:00+01:00",
  "next_expensive_slot": "2025-12-01T09:00:00+01:00",
  "cheap_slots_remaining": 24,
  "cheapest_slots_remaining": 10
}
```

## Algorithm Details

### Charge Planning Example

With a 32kWh battery at 50% SoC and 10kW max charge (9kWh effective at 90% efficiency):
- Energy needed: 16kWh to reach 100%
- Time to full: ~1.8 hours = ~7 slots at full power

If only 4 cheapest slots available but 12 cheap slots total:
- Charge at full power during 4 cheapest slots
- Charge at reduced power during remaining cheap slots

### Price Tier Calculation

Given 96 quarter-hourly slots (24 hours):
- **Cheapest** (10%): ~10 slots
- **Cheap** (25%): ~24 slots
- **Expensive** (top 25%): ~24 slots
- **Premium** (top 10%): ~10 slots

## License

MIT
