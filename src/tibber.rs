use anyhow::Result;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::TibberConfig;

const GRAPHQL_QUERY: &str = r#"
{
  viewer {
    homes {
      currentSubscription {
        priceInfo(resolution: QUARTER_HOURLY) {
          current {
            total
            energy
            tax
            startsAt
          }
          today {
            total
            energy
            tax
            startsAt
          }
          tomorrow {
            total
            energy
            tax
            startsAt
          }
        }
      }
    }
  }
}
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub total: f64,
    pub energy: f64,
    pub tax: f64,
    #[serde(rename = "startsAt")]
    pub starts_at: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Default)]
pub struct PriceCache {
    pub current: Option<PricePoint>,
    pub today: Vec<PricePoint>,
    pub tomorrow: Vec<PricePoint>,
    pub last_fetch: Option<DateTime<FixedOffset>>,
}

impl PriceCache {
    /// Get all available prices (today + tomorrow) sorted by time
    pub fn all_prices(&self) -> Vec<&PricePoint> {
        let mut prices: Vec<&PricePoint> = self.today.iter().chain(self.tomorrow.iter()).collect();
        prices.sort_by_key(|p| p.starts_at);
        prices
    }

    /// Get future prices (from now onwards)
    pub fn future_prices(&self) -> Vec<&PricePoint> {
        let now = chrono::Utc::now();
        self.all_prices()
            .into_iter()
            .filter(|p| p.starts_at.with_timezone(&chrono::Utc) >= now)
            .collect()
    }

    /// Calculate price statistics
    pub fn price_stats(&self) -> Option<PriceStats> {
        let prices = self.future_prices();
        if prices.is_empty() {
            return None;
        }

        let totals: Vec<f64> = prices.iter().map(|p| p.total).collect();
        let mut sorted = totals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let min = *sorted.first()?;
        let max = *sorted.last()?;
        let avg = totals.iter().sum::<f64>() / totals.len() as f64;

        // Calculate percentile thresholds
        let p25_idx = (sorted.len() as f64 * 0.25) as usize;
        let p75_idx = (sorted.len() as f64 * 0.75) as usize;
        let p90_idx = (sorted.len() as f64 * 0.90) as usize;

        Some(PriceStats {
            min,
            max,
            avg,
            p25: sorted.get(p25_idx).copied().unwrap_or(min),
            p75: sorted.get(p75_idx).copied().unwrap_or(max),
            p90: sorted.get(p90_idx).copied().unwrap_or(max),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PriceStats {
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub p25: f64,
    pub p75: f64,
    pub p90: f64,
}

// API Response structures
#[derive(Debug, Deserialize)]
struct ApiResponse {
    data: ApiData,
}

#[derive(Debug, Deserialize)]
struct ApiData {
    viewer: Viewer,
}

#[derive(Debug, Deserialize)]
struct Viewer {
    homes: Vec<Home>,
}

#[derive(Debug, Deserialize)]
struct Home {
    #[serde(rename = "currentSubscription")]
    current_subscription: Option<Subscription>,
}

#[derive(Debug, Deserialize)]
struct Subscription {
    #[serde(rename = "priceInfo")]
    price_info: PriceInfo,
}

#[derive(Debug, Deserialize)]
struct PriceInfo {
    current: Option<PricePoint>,
    today: Vec<PricePoint>,
    tomorrow: Vec<PricePoint>,
}

pub struct TibberClient {
    config: TibberConfig,
    http_client: reqwest::Client,
    cache: Arc<RwLock<PriceCache>>,
}

impl TibberClient {
    pub fn new(config: TibberConfig) -> Self {
        let http_client = reqwest::Client::new();
        Self {
            config,
            http_client,
            cache: Arc::new(RwLock::new(PriceCache::default())),
        }
    }

    pub async fn fetch_prices(&self) -> Result<()> {
        info!("Fetching prices from Tibber API");

        let response = self
            .http_client
            .post(&self.config.api_url)
            .header("Authorization", format!("Bearer {}", self.config.api_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "query": GRAPHQL_QUERY
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Tibber API error: {} - {}", status, body);
        }

        let api_response: ApiResponse = response.json().await?;

        // Get first home's subscription
        let home = api_response
            .data
            .viewer
            .homes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No homes found in Tibber account"))?;

        let subscription = home
            .current_subscription
            .ok_or_else(|| anyhow::anyhow!("No active subscription found"))?;

        let price_info = subscription.price_info;

        // Update cache
        let mut cache = self.cache.write().await;
        cache.current = price_info.current;
        cache.today = price_info.today;
        cache.tomorrow = price_info.tomorrow;
        cache.last_fetch = Some(chrono::Utc::now().fixed_offset());

        info!(
            "Fetched {} today prices, {} tomorrow prices",
            cache.today.len(),
            cache.tomorrow.len()
        );

        if cache.tomorrow.is_empty() {
            debug!("Tomorrow's prices not yet available (usually published after 14:00)");
        }

        Ok(())
    }

    pub async fn get_cache(&self) -> PriceCache {
        self.cache.read().await.clone()
    }

    pub async fn get_current_price(&self) -> Option<PricePoint> {
        let cache = self.cache.read().await;

        // Try to get the actual current price slot based on time
        let now = chrono::Utc::now();

        // Find the price slot that contains the current time
        for price in cache.today.iter().chain(cache.tomorrow.iter()) {
            let slot_start = price.starts_at.with_timezone(&chrono::Utc);
            let slot_end = slot_start + chrono::Duration::minutes(15);

            if now >= slot_start && now < slot_end {
                return Some(price.clone());
            }
        }

        // Fall back to the "current" field from API
        cache.current.clone()
    }

    /// Check if cache needs refresh
    pub async fn needs_refresh(&self) -> bool {
        let cache = self.cache.read().await;

        match cache.last_fetch {
            None => true,
            Some(last_fetch) => {
                let elapsed = chrono::Utc::now()
                    .signed_duration_since(last_fetch.with_timezone(&chrono::Utc));
                elapsed.num_seconds() as u64 >= self.config.refresh_interval_secs
            }
        }
    }

    /// Refresh prices if needed
    pub async fn refresh_if_needed(&self) -> Result<bool> {
        if self.needs_refresh().await {
            self.fetch_prices().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
