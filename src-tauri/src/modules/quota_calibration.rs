//! Headless-safe periodic online quota calibration (ledger sync).
//!
//! Independent of the disabled smart scheduler / warmup. Driven by
//! `AppConfig.auto_refresh` + `refresh_interval` (minutes).

use std::time::Duration;
use tracing::info;

/// Spawn a background loop that calls `refresh_all_quotas_logic` on an interval.
/// Safe to call once at process start (GUI or headless).
pub fn start_quota_calibration_ticker() {
    tokio::spawn(async move {
        info!("[QuotaCalibration] ticker started (follows auto_refresh / refresh_interval)");
        loop {
            let (enabled, interval_min) = match crate::modules::config::load_app_config() {
                Ok(cfg) => (cfg.auto_refresh, cfg.refresh_interval.max(1) as u64),
                Err(_) => (true, 15),
            };

            if !enabled {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }

            tokio::time::sleep(Duration::from_secs(interval_min * 60)).await;

            // Re-check after sleep in case user disabled mid-wait.
            let still_on = crate::modules::config::load_app_config()
                .map(|c| c.auto_refresh)
                .unwrap_or(true);
            if !still_on {
                continue;
            }

            info!(
                "[QuotaCalibration] running online refresh (interval={}m)",
                interval_min
            );
            match crate::modules::account::refresh_all_quotas_logic().await {
                Ok(stats) => {
                    info!(
                        "[QuotaCalibration] done total={} success={} failed={}",
                        stats.total, stats.success, stats.failed
                    );
                }
                Err(e) => {
                    tracing::warn!("[QuotaCalibration] refresh failed: {}", e);
                }
            }
        }
    });
}
