//! Headless-safe periodic online quota calibration (ledger sync).
//!
//! 低频错峰在线配额校准：
//! 1. 目标频率：每天 1~2 次（默认 12 小时间隔），仅用于校准本地账本偏差。
//! 2. 账号错峰：每个账号之间插入 6~18 秒的随机拟人化间隔，杜绝多账号瞬间并发刷新引发风控。
//! 3. 智能跳过：对 10 小时内已成功校准过的账号自动跳过，避免重复消耗上游配额。

use std::time::Duration;
use tracing::info;

/// 执行错峰在线校准
pub async fn run_staggered_calibration() -> Result<crate::modules::account::RefreshStats, String> {
    use rand::Rng;
    let accounts = crate::modules::account::list_accounts()?;
    let mut success = 0;
    let mut failed = 0;
    let mut details = Vec::new();
    let total = accounts.len();
    let now = chrono::Utc::now().timestamp();

    for mut account in accounts {
        if let Some(ref q) = account.quota {
            if q.is_forbidden {
                continue;
            }
        }
        let last_calibrated = account
            .estimated_quotas
            .values()
            .filter_map(|e| e.last_calibrated_at)
            .max()
            .unwrap_or(0);

        // 如果距离上次在线校准不到 10 小时，无需重复刷新
        if now - last_calibrated < 10 * 3600 && last_calibrated > 0 {
            tracing::debug!(
                "[QuotaCalibration] Account {} was calibrated {}s ago, skipping",
                account.email,
                now - last_calibrated
            );
            success += 1;
            continue;
        }

        // 账号之间随机等待 6~18 秒，平滑请求分布，彻底消除集中突发流量
        let jitter_secs = rand::thread_rng().gen_range(6..=18);
        tokio::time::sleep(Duration::from_secs(jitter_secs)).await;

        match crate::modules::account::fetch_quota_with_retry(&mut account).await {
            Ok(quota) => {
                if let Err(e) = crate::modules::account::update_account_quota(&account.id, quota) {
                    let msg = format!("Account {}: Save quota failed - {}", account.email, e);
                    tracing::error!("{}", msg);
                    details.push(msg);
                    failed += 1;
                } else {
                    tracing::info!("[QuotaCalibration] Successfully calibrated {}", account.email);
                    success += 1;
                }
            }
            Err(e) => {
                let msg = format!("Account {}: Fetch quota failed - {}", account.email, e);
                tracing::warn!("{}", msg);
                details.push(msg);
                failed += 1;
            }
        }
    }

    Ok(crate::modules::account::RefreshStats {
        total,
        success,
        failed,
        details,
    })
}

/// 启动低频错峰在线配额校准守护任务
pub fn start_quota_calibration_ticker() {
    let fut = async move {
        info!("[QuotaCalibration] Low-frequency staggered calibration ticker started");
        // 启动后先休眠 60 秒，避免应用刚启动时网络阻塞
        tokio::time::sleep(Duration::from_secs(60)).await;

        loop {
            let (enabled, interval_min) = match crate::modules::config::load_app_config() {
                // 最少 360 分钟（6小时），默认 720 分钟（12小时，一天2次）
                Ok(cfg) => (cfg.auto_refresh, (cfg.refresh_interval as u64).max(360)),
                Err(_) => (true, 720),
            };

            if !enabled {
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            info!(
                "[QuotaCalibration] Triggering scheduled staggered calibration (interval={}m)",
                interval_min
            );
            match run_staggered_calibration().await {
                Ok(stats) => {
                    info!(
                        "[QuotaCalibration] Calibration cycle done: total={} success={} failed={}",
                        stats.total, stats.success, stats.failed
                    );
                }
                Err(e) => {
                    tracing::warn!("[QuotaCalibration] Calibration cycle error: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(interval_min * 60)).await;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    } else {
        tauri::async_runtime::spawn(fut);
    }
}
