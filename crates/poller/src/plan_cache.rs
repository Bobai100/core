//! High-performance in-memory caching layer for analytics endpoints.
//! Replaces expensive raw SQL aggregation queries on `GET /api/analytics/plan-statistics`
//! with an optimized, thread-safe asynchronous cache (`PlanCache`).

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Aggregated statistics for a monitored contract plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStatistics {
    pub plan_id: String,
    pub total_monitored_contracts: u64,
    pub total_transactions_evaluated: u64,
    pub total_alerts_dispatched: u64,
    pub total_volume_stroops: u64,
    pub cache_hit: bool,
    pub cached_at: String,
}

struct CacheEntry {
    stats: PlanStatistics,
    expires_at: Instant,
}

/// Thread-safe in-memory cache with configurable TTL for plan analytics.
#[derive(Clone)]
pub struct PlanCache {
    ttl: Duration,
    entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl PlanCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Retrieve plan statistics from the cache if available and unexpired.
    pub async fn get(&self, plan_id: &str) -> Option<PlanStatistics> {
        let now = Instant::now();
        let reader = self.entries.read().await;
        if let Some(entry) = reader.get(plan_id) {
            if entry.expires_at > now {
                let mut stats = entry.stats.clone();
                stats.cache_hit = true;
                debug!(plan_id, "PlanCache hit for plan-statistics");
                return Some(stats);
            }
        }
        None
    }

    /// Store fresh plan statistics into the cache with TTL.
    pub async fn put(&self, plan_id: String, mut stats: PlanStatistics) {
        let expires_at = Instant::now() + self.ttl;
        stats.cache_hit = false;
        stats.cached_at = Utc::now().to_rfc3339();

        let mut writer = self.entries.write().await;
        writer.insert(plan_id.clone(), CacheEntry { stats, expires_at });
        debug!(plan_id = %plan_id, ttl_secs = self.ttl.as_secs(), "PlanCache updated entry");
    }

    /// Optimized getter for `GET /api/analytics/plan-statistics`:
    /// Checks cache first; only invokes fallback calculation on cache miss.
    pub async fn get_or_calculate<F, Fut>(
        &self,
        plan_id: &str,
        calculate_fallback: F,
    ) -> Result<PlanStatistics>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<PlanStatistics>>,
    {
        if let Some(cached) = self.get(plan_id).await {
            return Ok(cached);
        }

        info!(plan_id, "PlanCache miss — computing plan statistics");
        let computed = calculate_fallback().await?;
        self.put(plan_id.to_string(), computed.clone()).await;
        Ok(computed)
    }

    /// Invalidate cache for a specific plan or during configuration reloads.
    pub async fn invalidate(&self, plan_id: &str) {
        let mut writer = self.entries.write().await;
        writer.remove(plan_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_plan_cache_hit_and_expiration() {
        let cache = PlanCache::new(Duration::from_millis(50));
        let sample = PlanStatistics {
            plan_id: "plan-pro-01".into(),
            total_monitored_contracts: 10,
            total_transactions_evaluated: 50_000,
            total_alerts_dispatched: 120,
            total_volume_stroops: 5_000_000_000,
            cache_hit: false,
            cached_at: Utc::now().to_rfc3339(),
        };

        // Cache miss initially
        assert!(cache.get("plan-pro-01").await.is_none());

        // Put and get hit
        cache.put("plan-pro-01".into(), sample.clone()).await;
        let retrieved = cache.get("plan-pro-01").await.unwrap();
        assert!(retrieved.cache_hit);
        assert_eq!(retrieved.total_monitored_contracts, 10);

        // Sleep to test expiration
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(cache.get("plan-pro-01").await.is_none());
    }
}
