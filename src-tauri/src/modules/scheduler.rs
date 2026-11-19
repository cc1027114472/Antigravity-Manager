use crate::models::Account;
use crate::modules::{account, config, logger, quota};
use chrono::Utc;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::time::{self, Duration};

// Warmup history: key = "email:model_name:100", value = warmup timestamp
static WARMUP_HISTORY: Lazy<Mutex<HashMap<String, i64>>> =
    Lazy::new(|| Mutex::new(load_warmup_history()));

fn get_warmup_history_path() -> Result<PathBuf, String> {
    let data_dir = account::get_data_dir()?;
    Ok(data_dir.join("warmup_history.json"))
}

fn load_warmup_history() -> HashMap<String, i64> {
    match get_warmup_history_path() {
        Ok(path) if path.exists() => match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => HashMap::new(),
        },
        _ => HashMap::new(),
    }
}

fn save_warmup_history(history: &HashMap<String, i64>) {
    if let Ok(path) = get_warmup_history_path() {
        if let Ok(content) = serde_json::to_string_pretty(history) {
            let _ = std::fs::write(&path, content);
        }
    }
}

pub fn record_warmup_history(key: &str, timestamp: i64) {
    let mut history = WARMUP_HISTORY.lock().unwrap();
    history.insert(key.to_string(), timestamp);
    save_warmup_history(&history);
}

pub fn check_cooldown(key: &str, cooldown_seconds: i64) -> bool {
    let history = WARMUP_HISTORY.lock().unwrap();
    if let Some(&last_ts) = history.get(key) {
        let now = chrono::Utc::now().timestamp();
        now - last_ts < cooldown_seconds
    } else {
        false
    }
}

pub fn start_scheduler(
    app_handle: Option<tauri::AppHandle>,
    proxy_state: crate::commands::proxy::ProxyServiceState,
) {
    tauri::async_runtime::spawn(async move {
        logger::log_info("Smart Warmup Scheduler started. Monitoring quota at 100%...");

        // Delay first scan 90s after boot so restarts don't trigger immediate OAuth/quota bursts;
        // scan every 10 minutes thereafter.
        let mut interval = time::interval_at(
            time::Instant::now() + Duration::from_secs(90),
            Duration::from_secs(600),
        );

        loop {
            interval.tick().await;

            // Load configuration
            let Ok(app_config) = config::load_app_config() else {
                continue;
            };

            // Only run when auto warmup switch is enabled
            if !app_config.scheduled_warmup.enabled {
                continue;
            }

            // Get all accounts (no longer filtering by level)
            let Ok(accounts) = account::list_accounts() else {
                continue;
            };

            if accounts.is_empty() {
                continue;
            }

            logger::log_info(&format!(
                "[Scheduler] Scanning {} accounts for 100% quota models...",
                accounts.len()
            ));

            let mut warmup_tasks = Vec::new();
            let mut skipped_cooldown = 0;
            let mut updated_account_ids: Vec<String> = Vec::new();

            // Scan each active model for each non-disabled, non-forbidden account
            let mut online_checked_count: usize = 0;
            for account in &accounts {
                if account.disabled || account.proxy_disabled || account.is_hard_forbidden() {
                    continue;
                }

                // 前置冷却期预检：若该账号所有受监控模型（或所属计费组）均处于 4 小时 WARMUP_HISTORY 冷却期内，
                // 直接跳过，0 网络请求（不再每 10 分钟空刷 OAuth + 在线配额！）
                if !app_config.scheduled_warmup.monitored_models.is_empty() {
                    let check_ts = Utc::now().timestamp();
                    let all_in_cooldown = {
                        let history = WARMUP_HISTORY.lock().unwrap();
                        app_config
                            .scheduled_warmup
                            .monitored_models
                            .iter()
                            .all(|m| {
                                let model_key = format!("{}:{}:100", account.email, m);
                                let model_cool = history
                                    .get(&model_key)
                                    .map(|&ts| check_ts - ts < 14400)
                                    .unwrap_or(false);
                                if model_cool {
                                    return true;
                                }
                                if let Some(group) =
                                    crate::proxy::common::model_mapping::normalize_to_billing_group(m)
                                {
                                    let group_key =
                                        format!("{}:billing:{}:100", account.email, group);
                                    history
                                        .get(&group_key)
                                        .map(|&ts| check_ts - ts < 14400)
                                        .unwrap_or(false)
                                } else {
                                    false
                                }
                            })
                    };
                    if all_in_cooldown {
                        skipped_cooldown += app_config.scheduled_warmup.monitored_models.len();
                        continue;
                    }
                }

                // 错峰随机抖动 (2~5s)，防止定时扫描零间隔并发扫全库
                if online_checked_count > 0 {
                    let jitter_ms = {
                        use rand::Rng;
                        rand::thread_rng().gen_range(2000..=5000)
                    };
                    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                }
                online_checked_count += 1;

                // Get valid token
                let Ok((token, pid)) = quota::get_valid_token_for_warmup(account).await else {
                    continue;
                };

                // Get fresh quota
                let Ok((mut fresh_quota, _)) = quota::fetch_quota_with_cache(
                    &token,
                    &account.email,
                    Some(&pid),
                    Some(&account.id),
                )
                .await
                else {
                    continue;
                };

                // [FIX] 预热阶段检测到 403 时，使用统一禁用逻辑，确保账号文件和索引同时更新
                if fresh_quota.is_forbidden {
                    logger::log_warn(&format!(
                        "[Scheduler] Account {} returned 403 Forbidden during quota fetch, marking as forbidden",
                        account.email
                    ));
                    let _ = account::mark_account_forbidden(
                        &account.id,
                        "Scheduler: 403 Forbidden - quota fetch denied",
                    );
                    continue;
                }

                if fresh_quota.subscription_tier.is_none() {
                    fresh_quota.subscription_tier = account
                        .quota
                        .as_ref()
                        .and_then(|q| q.subscription_tier.clone());
                }
                if account::update_account_quota(&account.id, fresh_quota.clone()).is_ok() {
                    updated_account_ids.push(account.id.clone());
                }

                let now_ts = Utc::now().timestamp();
                let mut scheduled_groups: std::collections::HashSet<String> =
                    std::collections::HashSet::new();

                for model in fresh_quota.models {
                    // Core logic: detect 100% quota
                    if model.percentage == 100 {
                        let model_to_ping = model.name.clone();
                        let lower = model_to_ping.to_lowercase();
                        if lower.contains("2.5-") || lower.contains("2-5-") {
                            continue;
                        }

                        // Only warmup models configured by user (allowlist)
                        if !app_config
                            .scheduled_warmup
                            .monitored_models
                            .contains(&model_to_ping)
                        {
                            continue;
                        }

                        // 按官方计费组去重：单账号每个计费组 (gemini / claude) 最多仅预热 1 个代表模型
                        let group =
                            crate::proxy::common::model_mapping::normalize_to_billing_group(
                                &model_to_ping,
                            )
                            .unwrap_or_else(|| model_to_ping.clone());
                        if !scheduled_groups.insert(group.clone()) {
                            continue;
                        }

                        let history_key = format!("{}:{}:100", account.email, model_to_ping);
                        let group_history_key =
                            format!("{}:billing:{}:100", account.email, group);

                        // Check cooldown: do not repeat warmup within 4 hours
                        {
                            let history = WARMUP_HISTORY.lock().unwrap();
                            let cooldown_seconds = 14400;
                            let model_cool = history
                                .get(&history_key)
                                .map(|&ts| now_ts - ts < cooldown_seconds)
                                .unwrap_or(false);
                            let group_cool = history
                                .get(&group_history_key)
                                .map(|&ts| now_ts - ts < cooldown_seconds)
                                .unwrap_or(false);
                            if model_cool || group_cool {
                                skipped_cooldown += 1;
                                continue;
                            }
                        }

                        warmup_tasks.push((
                            account.id.clone(),
                            account.email.clone(),
                            model_to_ping.clone(),
                            token.clone(),
                            pid.clone(),
                            model.percentage,
                            history_key,
                            group_history_key,
                        ));

                        logger::log_info(&format!(
                            "[Scheduler] ✓ Scheduled warmup: {} [{}] @ {} (quota at 100%)",
                            model_to_ping, group, account.email
                        ));
                    } else if model.percentage < 100 {
                        // Quota not full, clear history, need to map name first
                        let model_to_ping = model.name.clone();
                        let history_key = format!("{}:{}:100", account.email, model_to_ping);

                        let mut history = WARMUP_HISTORY.lock().unwrap();
                        if history.remove(&history_key).is_some() {
                            save_warmup_history(&history);
                            logger::log_info(&format!(
                                "[Scheduler] Cleared history for {} @ {} (quota: {}%)",
                                model_to_ping, account.email, model.percentage
                            ));
                        }
                    }
                }
            }

            // Execute warmup tasks (strictly serial with 2~4s jitter; never trigger full-DB refresh_all_quotas_internal)
            if !warmup_tasks.is_empty() {
                let total = warmup_tasks.len();
                if skipped_cooldown > 0 {
                    logger::log_info(&format!(
                        "[Scheduler] Skipped {} models in cooldown, will warmup {}",
                        skipped_cooldown, total
                    ));
                }
                logger::log_info(&format!(
                    "[Scheduler] 🔥 Triggering {} warmup tasks (serial staggered)...",
                    total
                ));

                let handle_for_warmup = app_handle.clone();
                let state_for_warmup = proxy_state.clone();

                tokio::spawn(async move {
                    let mut success = 0;
                    let mut warmed_account_ids: std::collections::HashSet<String> =
                        std::collections::HashSet::new();

                    for (
                        task_idx,
                        (id, email, model, token, pid, pct, history_key, group_history_key),
                    ) in warmup_tasks.into_iter().enumerate()
                    {
                        if task_idx > 0 {
                            let jitter_ms: u64 = {
                                use rand::Rng;
                                rand::thread_rng().gen_range(2000..=4000)
                            };
                            tokio::time::sleep(tokio::time::Duration::from_millis(jitter_ms)).await;
                        }

                        logger::log_info(&format!(
                            "[Warmup {}/{}] {} @ {} ({}%)",
                            task_idx + 1,
                            total,
                            model,
                            email,
                            pct
                        ));

                        if quota::warmup_model_directly(
                            &token,
                            &model,
                            &pid,
                            &email,
                            pct,
                            Some(&id),
                        )
                        .await
                        {
                            success += 1;
                            let now_ts = chrono::Utc::now().timestamp();
                            record_warmup_history(&history_key, now_ts);
                            record_warmup_history(&group_history_key, now_ts);
                            warmed_account_ids.insert(id);
                        }
                    }

                    logger::log_info(&format!(
                        "[Scheduler] ✅ Warmup completed: {}/{} successful",
                        success, total
                    ));

                    // 仅同步受影响账号到内存并通知前端重读本地状态，严禁调用全库 refresh_all_quotas_internal 制造风暴
                    if !warmed_account_ids.is_empty() {
                        let instance_lock = state_for_warmup.instance.read().await;
                        if let Some(instance) = instance_lock.as_ref() {
                            for acc_id in &warmed_account_ids {
                                let _ = instance.token_manager.reload_account(acc_id).await;
                            }
                        }
                        if let Some(handle) = handle_for_warmup {
                            use tauri::Emitter;
                            let _ = handle.emit("accounts://refreshed", ());
                        }
                    }
                });
            } else if skipped_cooldown > 0 {
                logger::log_info(&format!(
                    "[Scheduler] Scan completed, all 100% models are in cooldown, skipped {}",
                    skipped_cooldown
                ));
            } else {
                logger::log_info(
                    "[Scheduler] Scan completed, no models with 100% quota need warmup",
                );
            }

            // 仅当本轮实际更新过账号配额时，通知前端重读本地磁盘状态（0 Google 网络请求，彻底移除每 10 分钟全库刷配额风暴）
            if !updated_account_ids.is_empty() {
                let instance_lock = proxy_state.instance.read().await;
                if let Some(instance) = instance_lock.as_ref() {
                    for acc_id in &updated_account_ids {
                        let _ = instance.token_manager.reload_account(acc_id).await;
                    }
                }
                if let Some(handle) = app_handle.as_ref() {
                    use tauri::Emitter;
                    let _ = handle.emit("accounts://refreshed", ());
                }
            }

            // Regularly clean up history (keep last 24 hours)
            {
                let now_ts = Utc::now().timestamp();
                let mut history = WARMUP_HISTORY.lock().unwrap();
                let cutoff = now_ts - 86400; // 24 hours ago
                history.retain(|_, &mut ts| ts > cutoff);
            }
        }
    });
}

/// Trigger immediate smart warmup check for a single account
pub async fn trigger_warmup_for_account(account: &Account) {
    // Get valid token
    let Ok((token, pid)) = quota::get_valid_token_for_warmup(account).await else {
        return;
    };

    // Get quota info (prefer cache as refresh command likely just updated disk/cache)
    let Ok((fresh_quota, _)) =
        quota::fetch_quota_with_cache(&token, &account.email, Some(&pid), Some(&account.id)).await
    else {
        return;
    };

    // [FIX] 预热阶段检测到 403 时，使用统一禁用逻辑，确保账号文件和索引同时更新
    if fresh_quota.is_forbidden {
        logger::log_warn(&format!(
            "[Scheduler] Account {} returned 403 Forbidden during quota fetch, marking as forbidden",
            account.email
        ));
        let _ = account::mark_account_forbidden(
            &account.id,
            "Scheduler: 403 Forbidden - quota fetch denied",
        );
        return;
    }

    // Load config once at the beginning
    let Ok(app_config) = config::load_app_config() else {
        logger::log_warn("[Scheduler] Failed to load app config, skipping warmup check");
        return;
    };

    if !app_config.scheduled_warmup.enabled {
        return;
    }

    let now_ts = Utc::now().timestamp();
    let mut tasks_to_run = Vec::new();
    let mut scheduled_groups: std::collections::HashSet<String> = std::collections::HashSet::new();

    for model in fresh_quota.models {
        let model_name = model.name.clone();
        let history_key = format!("{}:{}:100", account.email, model_name);

        if model.percentage == 100 {
            let lower = model_name.to_lowercase();
            if lower.contains("2.5-") || lower.contains("2-5-") {
                continue;
            }
            // First check if model is in user's monitored list
            if !app_config
                .scheduled_warmup
                .monitored_models
                .contains(&model_name)
            {
                continue;
            }

            let group =
                crate::proxy::common::model_mapping::normalize_to_billing_group(&model_name)
                    .unwrap_or_else(|| model_name.clone());
            if !scheduled_groups.insert(group.clone()) {
                continue;
            }
            let group_history_key = format!("{}:billing:{}:100", account.email, group);

            // Then check cooldown history
            {
                let history = WARMUP_HISTORY.lock().unwrap();
                let cooldown_seconds = 14400;
                let model_cool = history
                    .get(&history_key)
                    .map(|&ts| now_ts - ts < cooldown_seconds)
                    .unwrap_or(false);
                let group_cool = history
                    .get(&group_history_key)
                    .map(|&ts| now_ts - ts < cooldown_seconds)
                    .unwrap_or(false);
                if model_cool || group_cool {
                    continue;
                }
            }
            // Note: Don't write history here - only write after successful warmup

            tasks_to_run.push((model_name, model.percentage, history_key, group_history_key));
        } else if model.percentage < 100 {
            // Quota not full, clear history, allow warmup next time it's 100%
            let mut history = WARMUP_HISTORY.lock().unwrap();
            if history.remove(&history_key).is_some() {
                save_warmup_history(&history);
            }
        }
    }

    // Execute warmup and record history only on success
    if !tasks_to_run.is_empty() {
        logger::log_info(&format!(
            "[Scheduler] Found {} billing-group models ready for warmup on {}",
            tasks_to_run.len(),
            account.email
        ));

        for (idx, (model, pct, history_key, group_history_key)) in
            tasks_to_run.into_iter().enumerate()
        {
            if idx > 0 {
                let jitter_ms: u64 = {
                    use rand::Rng;
                    rand::thread_rng().gen_range(2000..=4000)
                };
                tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
            }
            logger::log_info(&format!(
                "[Scheduler] 🔥 Triggering individual warmup: {} @ {} (Sync)",
                model, account.email
            ));

            let success = quota::warmup_model_directly(
                &token,
                &model,
                &pid,
                &account.email,
                pct,
                Some(&account.id),
            )
            .await;

            // Only record history if warmup was successful
            if success {
                record_warmup_history(&history_key, now_ts);
                record_warmup_history(&group_history_key, now_ts);
            }
        }
    }
}
