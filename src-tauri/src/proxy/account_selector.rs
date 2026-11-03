//! 账号选择与负载均衡调度器 (Account Selector)
//!
//! 【模块三：账号负载削峰填谷与单账号 Token 软上限 + 疲劳休眠 + 最少使用优先调度】
//!
//! 1. 单账号软上限熔断保护 (Soft Cap Circuit Breaker):
//!    - 单账号当日 Output Tokens 达到 30 万 (300,000)，或单小时达到 6 万 (60,000) 时，自动进入疲劳休眠。
//!    - 疲劳休眠时间为 1~2 小时随机抖动 (fatigue_until = now + 3600..=7200s)，避免同一批账号同时苏醒造成集中峰值。
//! 2. 候选账号可用性过滤:
//!    - 在可用账号过滤流程中，严格排除处于疲劳休眠期的账号 (now < fatigue_until)。
//!    - 超过休眠期后，账号自动解除休眠，恢复正常候选可用状态。
//! 3. 最少使用优先调度 (Least-Tokens-Used Scheduling):
//!    - 针对全池 60+ 账号，根据当日 Output / 累计 Token 消耗进行排序，优先调度消耗最少的健康账号。
//!    - 在极小消耗差值梯队内引入微抖动平滑调度 (Anti-Concentration)，实现削峰填谷，彻底防止头部账号单日刷到 60 万 Tokens 被上游风控封禁。

use std::collections::HashMap;
use std::sync::Arc;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// 单账号当日 Output Tokens 软上限 (30 万)
pub const DAILY_OUTPUT_TOKEN_SOFT_LIMIT: u64 = 300_000;

/// 单账号单小时 Output Tokens 软上限 (6 万)
pub const HOURLY_OUTPUT_TOKEN_SOFT_LIMIT: u64 = 60_000;

/// 疲劳休眠最小持续时间 (秒): 1 小时 (3600s)
pub const FATIGUE_NAP_MIN_SECS: i64 = 3600;

/// 疲劳休眠最大持续时间 (秒): 2 小时 (7200s)
pub const FATIGUE_NAP_MAX_SECS: i64 = 7200;

/// 削峰填谷平滑抖动阈值：当两个账号当日 Output Token 差距在 5000 tokens 以内时，视为同一消耗梯队
pub const LOAD_SMOOTHING_TOKEN_DIFF_THRESHOLD: u64 = 5000;

/// 软上限配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoftLimitConfig {
    pub daily_output_limit: u64,
    pub hourly_output_limit: u64,
    pub min_fatigue_nap_secs: i64,
    pub max_fatigue_nap_secs: i64,
}

impl Default for SoftLimitConfig {
    fn default() -> Self {
        Self {
            daily_output_limit: DAILY_OUTPUT_TOKEN_SOFT_LIMIT,
            hourly_output_limit: HOURLY_OUTPUT_TOKEN_SOFT_LIMIT,
            min_fatigue_nap_secs: FATIGUE_NAP_MIN_SECS,
            max_fatigue_nap_secs: FATIGUE_NAP_MAX_SECS,
        }
    }
}

/// 疲劳触发原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FatigueReason {
    /// 单小时 Output Tokens 达到或超过 6 万
    HourlyOutputLimitExceeded,
    /// 单日 Output Tokens 达到或超过 30 万
    DailyOutputLimitExceeded,
    /// 单小时与单日均超标
    BothLimitsExceeded,
    /// 人工或外部触发
    Manual,
}

impl std::fmt::Display for FatigueReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HourlyOutputLimitExceeded => write!(f, "单小时 Output Token 达到 6 万软上限"),
            Self::DailyOutputLimitExceeded => write!(f, "当日 Output Token 达到 30 万软上限"),
            Self::BothLimitsExceeded => write!(f, "单小时 (6万) 与当日 (30万) Output Token 均达到软上限"),
            Self::Manual => write!(f, "人工或策略手动设置"),
        }
    }
}

/// 账号疲劳休眠详细记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FatigueRecord {
    pub account_id: String,
    pub email: String,
    pub fatigue_until: i64,
    pub entered_at: i64,
    pub nap_duration_secs: i64,
    pub reason: FatigueReason,
    pub hourly_output_tokens: u64,
    pub daily_output_tokens: u64,
}

/// 账号实时 Token 统计快照
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountTokenUsage {
    pub account_id: String,
    pub email: String,
    /// 当前自然小时内（或最近 1 小时内）产出的 Output Tokens
    pub hourly_output_tokens: u64,
    /// 当日（当前自然日或最近 24 小时）产出的 Output Tokens
    pub daily_output_tokens: u64,
    /// 当日总 Tokens (Input + Output)
    pub daily_total_tokens: u64,
    /// 历史累计 Output Tokens
    pub total_output_tokens: u64,
    /// 历史累计总 Tokens
    pub total_tokens: u64,
    /// 请求计数
    pub request_count: u64,
    /// 最后一次更新时间戳
    pub last_updated_at: i64,
}

/// 全局单例疲劳休眠注册表：account_id -> FatigueRecord
static GLOBAL_FATIGUE_REGISTRY: Lazy<Arc<DashMap<String, FatigueRecord>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例实时 Token 使用追踪器：account_id -> AccountTokenUsage
static GLOBAL_USAGE_TRACKER: Lazy<Arc<DashMap<String, AccountTokenUsage>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 可参与调度的账号候选对象 Trait (解耦 Account 与 ProxyToken)
pub trait AccountCandidate {
    fn account_id(&self) -> &str;
    fn email(&self) -> &str;
    fn is_disabled(&self) -> bool;
    fn health_score(&self) -> f32 {
        1.0
    }
    fn remaining_quota(&self) -> Option<i32> {
        None
    }
    fn is_model_protected(&self, _model: &str) -> bool {
        false
    }
    fn fatigue_until(&self) -> Option<i64> {
        None
    }
}

// 为模型层 Account 实现 AccountCandidate
impl AccountCandidate for crate::models::Account {
    fn account_id(&self) -> &str {
        &self.id
    }
    fn email(&self) -> &str {
        &self.email
    }
    fn is_disabled(&self) -> bool {
        self.disabled || self.proxy_disabled || self.validation_blocked
    }
    fn health_score(&self) -> f32 {
        1.0
    }
    fn remaining_quota(&self) -> Option<i32> {
        self.quota.as_ref().and_then(|q| {
            q.models
                .values()
                .filter_map(|m| m.percentage)
                .min()
                .map(|v| v as i32)
        })
    }
    fn is_model_protected(&self, model: &str) -> bool {
        self.protected_models.contains(model)
    }
    fn fatigue_until(&self) -> Option<i64> {
        self.fatigue_until
    }
}

// 为代理层 ProxyToken 实现 AccountCandidate
impl AccountCandidate for crate::proxy::token_manager::ProxyToken {
    fn account_id(&self) -> &str {
        &self.account_id
    }
    fn email(&self) -> &str {
        &self.email
    }
    fn is_disabled(&self) -> bool {
        self.validation_blocked
    }
    fn health_score(&self) -> f32 {
        self.health_score
    }
    fn remaining_quota(&self) -> Option<i32> {
        self.remaining_quota
    }
    fn is_model_protected(&self, model: &str) -> bool {
        crate::proxy::token_manager::TokenManager::is_model_quota_protected(
            &self.protected_models,
            model,
        )
    }
}

/// 账号选择器服务 (AccountSelector)
#[derive(Clone, Debug)]
pub struct AccountSelector {
    config: SoftLimitConfig,
    fatigue_registry: Arc<DashMap<String, FatigueRecord>>,
    usage_tracker: Arc<DashMap<String, AccountTokenUsage>>,
}

impl Default for AccountSelector {
    fn default() -> Self {
        Self::new(SoftLimitConfig::default())
    }
}

impl AccountSelector {
    pub fn new(config: SoftLimitConfig) -> Self {
        Self {
            config,
            fatigue_registry: Arc::clone(&GLOBAL_FATIGUE_REGISTRY),
            usage_tracker: Arc::clone(&GLOBAL_USAGE_TRACKER),
        }
    }

    /// 获取软上限配置引用
    pub fn config(&self) -> &SoftLimitConfig {
        &self.config
    }

    // ==========================================
    // 1. 软上限熔断保护 (Soft Cap Circuit Breaker)
    // ==========================================

    /// 评估账号是否触达软上限阈值。
    /// 若触达，自动为该账号开启疲劳休眠（fatigue_until = now + 3600~7200s），并返回疲劳记录；
    /// 若未触达，返回 None。
    pub fn check_and_update_fatigue(
        &self,
        account_id: &str,
        email: &str,
        hourly_output: u64,
        daily_output: u64,
    ) -> Option<FatigueRecord> {
        let now = chrono::Utc::now().timestamp();

        // 检查是否已经在休眠期中
        if let Some(existing) = self.fatigue_registry.get(account_id) {
            if now < existing.fatigue_until {
                return Some(existing.clone());
            }
        }

        let hourly_exceeded = hourly_output >= self.config.hourly_output_limit;
        let daily_exceeded = daily_output >= self.config.daily_output_limit;

        if !hourly_exceeded && !daily_exceeded {
            return None;
        }

        let reason = if hourly_exceeded && daily_exceeded {
            FatigueReason::BothLimitsExceeded
        } else if hourly_exceeded {
            FatigueReason::HourlyOutputLimitExceeded
        } else {
            FatigueReason::DailyOutputLimitExceeded
        };

        // 随机休眠 3600 ~ 7200 秒
        let mut rng = rand::thread_rng();
        let nap_duration = rng.gen_range(self.config.min_fatigue_nap_secs..=self.config.max_fatigue_nap_secs);
        let fatigue_until = now + nap_duration;

        let record = FatigueRecord {
            account_id: account_id.to_string(),
            email: email.to_string(),
            fatigue_until,
            entered_at: now,
            nap_duration_secs: nap_duration,
            reason,
            hourly_output_tokens: hourly_output,
            daily_output_tokens: daily_output,
        };

        tracing::warn!(
            "💤 [AccountSelector] Account {} ({}) triggered soft cap circuit breaker! Reason: {}, hourly_output: {}/{} tokens, daily_output: {}/{} tokens. Entering fatigue nap for {}s (until {}).",
            account_id,
            email,
            reason,
            hourly_output,
            self.config.hourly_output_limit,
            daily_output,
            self.config.daily_output_limit,
            nap_duration,
            fatigue_until
        );

        self.fatigue_registry.insert(account_id.to_string(), record.clone());
        Some(record)
    }

    /// 记录并累加账号的 Token 使用量，并实时执行软上限熔断检查
    pub fn record_token_usage(
        &self,
        account_id: &str,
        email: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Option<FatigueRecord> {
        let now = chrono::Utc::now().timestamp();
        let total_tokens = (input_tokens as u64) + (output_tokens as u64);
        let output_u64 = output_tokens as u64;

        let mut entry = self
            .usage_tracker
            .entry(account_id.to_string())
            .or_insert_with(|| AccountTokenUsage {
                account_id: account_id.to_string(),
                email: email.to_string(),
                last_updated_at: now,
                ..Default::default()
            });

        entry.hourly_output_tokens += output_u64;
        entry.daily_output_tokens += output_u64;
        entry.daily_total_tokens += total_tokens;
        entry.total_output_tokens += output_u64;
        entry.total_tokens += total_tokens;
        entry.request_count += 1;
        entry.last_updated_at = now;

        let hourly_out = entry.hourly_output_tokens;
        let daily_out = entry.daily_output_tokens;
        drop(entry);

        self.check_and_update_fatigue(account_id, email, hourly_out, daily_out)
    }

    /// 检查指定账号是否正处于疲劳休眠期
    /// 如果已超过休眠截止时间，自动解除休眠并返回 false
    pub fn is_account_fatigued(&self, account_id: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        if let Some(entry) = self.fatigue_registry.get(account_id) {
            if now < entry.fatigue_until {
                return true;
            }
        }
        // 如果已超时，自动清理过期记录
        if self.fatigue_registry.contains_key(account_id) {
            if let Some((_, record)) = self.fatigue_registry.remove(account_id) {
                tracing::info!(
                    "⏰ [AccountSelector] Account {} ({}) fatigue sleep ended (woke up at {}). Re-admitted to scheduling pool.",
                    record.account_id,
                    record.email,
                    now
                );
            }
        }
        false
    }

    /// 获取账号疲劳休眠截止时间戳
    pub fn get_fatigue_until(&self, account_id: &str) -> Option<i64> {
        if self.is_account_fatigued(account_id) {
            self.fatigue_registry.get(account_id).map(|r| r.fatigue_until)
        } else {
            None
        }
    }

    /// 手动设置疲劳休眠
    pub fn set_fatigue_sleep(
        &self,
        account_id: &str,
        email: &str,
        duration_secs: i64,
        reason: FatigueReason,
    ) {
        let now = chrono::Utc::now().timestamp();
        let fatigue_until = now + duration_secs;
        self.fatigue_registry.insert(
            account_id.to_string(),
            FatigueRecord {
                account_id: account_id.to_string(),
                email: email.to_string(),
                fatigue_until,
                entered_at: now,
                nap_duration_secs: duration_secs,
                reason,
                hourly_output_tokens: 0,
                daily_output_tokens: 0,
            },
        );
    }

    /// 清除疲劳休眠状态
    pub fn clear_fatigue(&self, account_id: &str) -> bool {
        self.fatigue_registry.remove(account_id).is_some()
    }

    /// 获取所有处于休眠期的账号列表
    pub fn list_fatigued_accounts(&self) -> Vec<FatigueRecord> {
        let now = chrono::Utc::now().timestamp();
        let mut result = Vec::new();
        let mut expired = Vec::new();

        for item in self.fatigue_registry.iter() {
            if now < item.fatigue_until {
                result.push(item.clone());
            } else {
                expired.push(item.key().clone());
            }
        }

        for exp_key in expired {
            self.fatigue_registry.remove(&exp_key);
        }

        result
    }

    /// 获取指定账号的 Token 使用量快照
    pub fn get_account_usage(&self, account_id: &str) -> Option<AccountTokenUsage> {
        self.usage_tracker.get(account_id).map(|u| u.clone())
    }

    /// 重置账号单小时统计（用于整点重置）
    pub fn reset_hourly_usage(&self) {
        for mut item in self.usage_tracker.iter_mut() {
            item.hourly_output_tokens = 0;
        }
    }

    /// 重置账号当日统计（用于每日 0 点重置）
    pub fn reset_daily_usage(&self) {
        for mut item in self.usage_tracker.iter_mut() {
            item.daily_output_tokens = 0;
            item.daily_total_tokens = 0;
        }
    }

    // ==========================================
    // 2. 候选账号可用性过滤 (Availability Filter)
    // ==========================================

    /// 过滤可用的候选账号：
    /// 1. 排除已被软/硬禁用的账号 (is_disabled)；
    /// 2. 排除目标模型受保护禁用的账号 (is_model_protected)；
    /// 3. 【核心要求】严格排除正处于疲劳休眠期的账号 (is_fatigued)；
    pub fn filter_available<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Vec<&'a T> {
        let now = chrono::Utc::now().timestamp();
        let mut fatigued_count = 0;

        let available: Vec<&'a T> = candidates
            .iter()
            .filter(|c| !c.is_disabled())
            .filter(|c| {
                if let Some(model) = target_model {
                    !c.is_model_protected(model)
                } else {
                    true
                }
            })
            .filter(|c| {
                // 优先检查全局疲劳注册表
                let in_registry_fatigue = self.is_account_fatigued(c.account_id());
                // 兼顾候选对象自带的 fatigue_until (若有)
                let in_model_fatigue = c.fatigue_until().map(|u| now < u).unwrap_or(false);

                if in_registry_fatigue || in_model_fatigue {
                    fatigued_count += 1;
                    tracing::debug!(
                        "💤 [AccountSelector] Account {} ({}) is in fatigue sleep, excluding from candidate pool.",
                        c.account_id(),
                        c.email()
                    );
                    false
                } else {
                    true
                }
            })
            .collect();

        if available.is_empty() && fatigued_count > 0 {
            tracing::warn!(
                "⚠️ [AccountSelector] All eligible candidate accounts are currently in fatigue sleep ({} accounts asleep). Holding dispatch.",
                fatigued_count
            );
        }

        available
    }

    // ==========================================
    // 3. 最少使用优先调度策略 (Least-Tokens-Used)
    // ==========================================

    /// 最少使用优先（Least-Tokens-Used）调度与排序：
    /// - 根据当日 Output Tokens / 累计 Token 消耗进行排序；
    /// - 优先调度消耗最少的健康账号，使全池 60+ 账号均匀分摊压力；
    /// - 防止头部账号单日刷到 60 万 Tokens 被风控盯上；
    /// - 在最低消耗梯队（差值 <= 5000 tokens）中进行微平滑选择，消除并发惊群效应。
    pub fn select_least_used<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Option<&'a T> {
        // 1. 过滤掉禁用与休眠账号
        let available = self.filter_available(candidates, target_model);
        if available.is_empty() {
            return None;
        }
        if available.len() == 1 {
            return Some(available[0]);
        }

        // 2. 构造账号使用量快照并排序
        let mut candidate_metrics: Vec<(&'a T, u64, u64, f32)> = available
            .into_iter()
            .map(|c| {
                let (daily_out, total_tok) = if let Some(usage) = self.usage_tracker.get(c.account_id()) {
                    (usage.daily_output_tokens, usage.total_tokens)
                } else {
                    (0, 0)
                };
                let health = c.health_score();
                (c, daily_out, total_tok, health)
            })
            .collect();

        // 排序规则：
        // 1. 当日 Output Tokens 升序 (最核心：防止单日超量被风控)
        // 2. 历史累计 Tokens 升序 (打破平局)
        // 3. 健康分数 降序 (同等消耗下优先用更健康的号)
        candidate_metrics.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
        });

        // 3. 削峰填谷负载平滑：
        // 找到与当前最低消耗账号差距在 LOAD_SMOOTHING_TOKEN_DIFF_THRESHOLD (5000 tokens) 以内的“第一梯队”
        let min_daily_out = candidate_metrics[0].1;
        let pool_tier: Vec<&'a T> = candidate_metrics
            .iter()
            .take(5) // 取最多前 5 个
            .filter(|(_, daily_out, _, _)| *daily_out <= min_daily_out + LOAD_SMOOTHING_TOKEN_DIFF_THRESHOLD)
            .map(|(c, _, _, _)| *c)
            .collect();

        if pool_tier.len() <= 1 {
            let selected = candidate_metrics[0].0;
            tracing::debug!(
                "🎯 [AccountSelector] Selected least-used account: {} ({}), daily_output: {} tokens, total: {} tokens",
                selected.account_id(),
                selected.email(),
                candidate_metrics[0].1,
                candidate_metrics[0].2
            );
            return Some(selected);
        }

        // 第一梯队内存在 2 个或以上消耗相近的健康账号：使用轻量随机抖动分摊并发
        let mut rng = rand::thread_rng();
        let pick_idx = rng.gen_range(0..pool_tier.len());
        let selected = pool_tier[pick_idx];

        tracing::debug!(
            "🎯 [AccountSelector] Selected account from lowest-usage tier ({}/{} candidates, diff < 5k tokens): {} ({})",
            pick_idx + 1,
            pool_tier.len(),
            selected.account_id(),
            selected.email()
        );

        Some(selected)
    }

    /// 对候选账号列表直接按照 Least-Tokens-Used 策略进行完整排序输出
    pub fn rank_candidates<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Vec<&'a T> {
        let available = self.filter_available(candidates, target_model);
        if available.len() <= 1 {
            return available;
        }

        let mut candidate_metrics: Vec<(&'a T, u64, u64, f32)> = available
            .into_iter()
            .map(|c| {
                let (daily_out, total_tok) = if let Some(usage) = self.usage_tracker.get(c.account_id()) {
                    (usage.daily_output_tokens, usage.total_tokens)
                } else {
                    (0, 0)
                };
                let health = c.health_score();
                (c, daily_out, total_tok, health)
            })
            .collect();

        candidate_metrics.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
        });

        candidate_metrics.into_iter().map(|(c, _, _, _)| c).collect()
    }
}

// ==========================================
// 全局便捷函数包装 (Global Convenience APIs)
// ==========================================

/// 获取全局默认账号选择器实例
pub fn get_default_selector() -> AccountSelector {
    AccountSelector::default()
}

/// 全局便捷检查与触发疲劳休眠
pub fn check_and_update_fatigue(
    account_id: &str,
    email: &str,
    hourly_output: u64,
    daily_output: u64,
) -> Option<FatigueRecord> {
    AccountSelector::default().check_and_update_fatigue(account_id, email, hourly_output, daily_output)
}

/// 全局便捷记录使用量
pub fn record_token_usage(
    account_id: &str,
    email: &str,
    input_tokens: u32,
    output_tokens: u32,
) -> Option<FatigueRecord> {
    AccountSelector::default().record_token_usage(account_id, email, input_tokens, output_tokens)
}

/// 全局检查指定账号是否正处于疲劳休眠期
pub fn is_account_fatigued(account_id: &str) -> bool {
    AccountSelector::default().is_account_fatigued(account_id)
}

/// 全局清除指定账号的疲劳休眠状态
pub fn clear_account_fatigue(account_id: &str) -> bool {
    AccountSelector::default().clear_fatigue(account_id)
}

/// 全局可用候选账号过滤（排除禁用、配额受限及处于疲劳休眠期的账号）
pub fn filter_available_candidates<'a, T: AccountCandidate>(
    candidates: &'a [T],
    target_model: Option<&str>,
) -> Vec<&'a T> {
    AccountSelector::default().filter_available(candidates, target_model)
}

/// 全局最少使用优先调度
pub fn select_least_used_account<'a, T: AccountCandidate>(
    candidates: &'a [T],
    target_model: Option<&str>,
) -> Option<&'a T> {
    AccountSelector::default().select_least_used(candidates, target_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct MockAccount {
        id: String,
        email: String,
        disabled: bool,
        health: f32,
        protected: bool,
        fatigue_until: Option<i64>,
    }

    impl AccountCandidate for MockAccount {
        fn account_id(&self) -> &str {
            &self.id
        }
        fn email(&self) -> &str {
            &self.email
        }
        fn is_disabled(&self) -> bool {
            self.disabled
        }
        fn health_score(&self) -> f32 {
            self.health
        }
        fn is_model_protected(&self, _model: &str) -> bool {
            self.protected
        }
        fn fatigue_until(&self) -> Option<i64> {
            self.fatigue_until
        }
    }

    fn make_test_selector() -> AccountSelector {
        AccountSelector::new(SoftLimitConfig {
            daily_output_limit: 300_000,
            hourly_output_limit: 60_000,
            min_fatigue_nap_secs: 3600,
            max_fatigue_nap_secs: 7200,
        })
    }

    #[test]
    fn test_hourly_soft_limit_trigger() {
        let selector = make_test_selector();
        let acc_id = "test_acc_hourly";
        let email = "hourly@example.com";
        selector.clear_fatigue(acc_id);

        // 单小时 59,999 未触发
        let res1 = selector.check_and_update_fatigue(acc_id, email, 59_999, 100_000);
        assert!(res1.is_none());
        assert!(!selector.is_account_fatigued(acc_id));

        // 单小时 60,000 触发疲劳休眠
        let now = chrono::Utc::now().timestamp();
        let res2 = selector.check_and_update_fatigue(acc_id, email, 60_000, 100_000);
        assert!(res2.is_some());
        let rec = res2.unwrap();
        assert_eq!(rec.reason, FatigueReason::HourlyOutputLimitExceeded);
        assert!(rec.fatigue_until >= now + 3600);
        assert!(rec.fatigue_until <= now + 7200 + 5);
        assert!(selector.is_account_fatigued(acc_id));

        selector.clear_fatigue(acc_id);
    }

    #[test]
    fn test_daily_soft_limit_trigger() {
        let selector = make_test_selector();
        let acc_id = "test_acc_daily";
        let email = "daily@example.com";
        selector.clear_fatigue(acc_id);

        // 当日 299,999 未触发
        let res1 = selector.check_and_update_fatigue(acc_id, email, 10_000, 299_999);
        assert!(res1.is_none());
        assert!(!selector.is_account_fatigued(acc_id));

        // 当日 300,000 触发疲劳休眠
        let now = chrono::Utc::now().timestamp();
        let res2 = selector.check_and_update_fatigue(acc_id, email, 10_000, 300_000);
        assert!(res2.is_some());
        let rec = res2.unwrap();
        assert_eq!(rec.reason, FatigueReason::DailyOutputLimitExceeded);
        assert!(rec.fatigue_until >= now + 3600);
        assert!(rec.fatigue_until <= now + 7200 + 5);
        assert!(selector.is_account_fatigued(acc_id));

        selector.clear_fatigue(acc_id);
    }

    #[test]
    fn test_filter_excludes_fatigued_accounts() {
        let selector = make_test_selector();
        let now = chrono::Utc::now().timestamp();

        let acc1 = MockAccount {
            id: "acc_active".to_string(),
            email: "active@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };
        let acc2 = MockAccount {
            id: "acc_fatigued".to_string(),
            email: "fatigued@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: Some(now + 3600), // 休眠中
        };
        let acc3 = MockAccount {
            id: "acc_woken".to_string(),
            email: "woken@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: Some(now - 10), // 已过休眠期，自动苏醒
        };
        let acc4 = MockAccount {
            id: "acc_disabled".to_string(),
            email: "disabled@example.com".to_string(),
            disabled: true,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };

        let list = vec![acc1.clone(), acc2.clone(), acc3.clone(), acc4.clone()];
        let available = selector.filter_available(&list, None);

        // acc2 (fatigued) 和 acc4 (disabled) 应该被排除，acc1 和 acc3 (已醒) 可用
        assert_eq!(available.len(), 2);
        assert!(available.iter().any(|a| a.account_id() == "acc_active"));
        assert!(available.iter().any(|a| a.account_id() == "acc_woken"));
        assert!(!available.iter().any(|a| a.account_id() == "acc_fatigued"));
        assert!(!available.iter().any(|a| a.account_id() == "acc_disabled"));
    }

    #[test]
    fn test_least_tokens_used_scheduling() {
        let selector = make_test_selector();

        let acc_heavy = MockAccount {
            id: "acc_heavy".to_string(),
            email: "heavy@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };
        let acc_medium = MockAccount {
            id: "acc_medium".to_string(),
            email: "medium@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };
        let acc_light = MockAccount {
            id: "acc_light".to_string(),
            email: "light@example.com".to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };

        // 注入不同的 Token 消耗
        selector.record_token_usage("acc_heavy", "heavy@example.com", 10_000, 250_000);
        selector.record_token_usage("acc_medium", "medium@example.com", 10_000, 100_000);
        selector.record_token_usage("acc_light", "light@example.com", 10_000, 20_000);

        let list = vec![acc_heavy.clone(), acc_medium.clone(), acc_light.clone()];

        // 排序检查：消耗最少的应该排最前
        let ranked = selector.rank_candidates(&list, None);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].account_id(), "acc_light");
        assert_eq!(ranked[1].account_id(), "acc_medium");
        assert_eq!(ranked[2].account_id(), "acc_heavy");

        // 单独选择检查：应当选中消耗最少的 acc_light
        let chosen = selector.select_least_used(&list, None);
        assert!(chosen.is_some());
        assert_eq!(chosen.unwrap().account_id(), "acc_light");
    }

    #[test]
    fn test_load_balance_60_accounts() {
        let selector = make_test_selector();
        let mut accounts = Vec::new();

        // 模拟 60 个账号，初始用量从 0 到 120_000 渐进分布
        for i in 0..60 {
            let id = format!("acc_{:02}", i);
            let email = format!("user_{:02}@example.com", i);
            let used = (i as u64) * 2000;
            selector.record_token_usage(&id, &email, 0, used as u32);

            accounts.push(MockAccount {
                id,
                email,
                disabled: false,
                health: 1.0,
                protected: false,
                fatigue_until: None,
            });
        }

        // 调度 100 次，检验前几名低消耗账号被优先调度
        for _ in 0..100 {
            let chosen = selector.select_least_used(&accounts, None);
            assert!(chosen.is_some());
            let chosen_acc = chosen.unwrap();
            let usage = selector.get_account_usage(chosen_acc.account_id()).unwrap();
            // 选中的账号应当处于第一梯队（用量必须远小于高消耗账号）
            assert!(usage.daily_output_tokens <= 20_000);

            // 模拟该账号被调度后产生了 2000 tokens output
            selector.record_token_usage(chosen_acc.account_id(), chosen_acc.email(), 500, 2000);
        }

        // 100 次调度后，原本最低消耗的一批账号被均匀消耗分摊，实现了负载削峰填谷
        let final_ranked = selector.rank_candidates(&accounts, None);
        assert_eq!(final_ranked.len(), 60);
    }
}
