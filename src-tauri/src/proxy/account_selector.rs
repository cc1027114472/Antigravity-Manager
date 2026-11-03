//! 账号选择与负载均衡调度器 (Account Selector)
//!
//! 【模块三：拟人化账号负载削峰填谷与风控对抗体系】
//!
//! 1. 动态随机抖动软上限 (Jittered Soft Limits):
//!    - 坚决杜绝死板固定阈值（如整齐的 60,000 或 300,000），避免在平台侧审计日志中呈现明显的集群脚本特征。
//!    - 每个账号在 24 小时滑动周期内动态随机撒点：
//!      - 单小时 Output 上限：随机区间 50,000 ~ 68,000 tokens。
//!      - 24小时 Output 上限：随机区间 260,000 ~ 330,000 tokens。
//!    - 每天或每周期自动微扰刷新，保证账号行为呈现异构拟人化特征。
//!
//! 2. 流式输出在途预占机制 (In-flight Reservation):
//!    - 解决流式输出（Streaming）并发放行导致的软上限瞬间被穿透打爆（如 59k 时并发 5 个请求直接冲到 80k）。
//!    - 请求被派发时，先对该账号预占预估 Output Tokens（默认 2,000 tokens）。
//!    - 水位判断：(已用 + 在途预占) >= 动态软上限时，立即停止派发新请求，坚决阻断穿透。
//!    - 流式结束结算后，释放预占，按实际 Output Tokens 归档。
//!
//! 3. 24 小时真实滑动窗口 (Rolling 24h Window):
//!    - 摒弃单纯自然日（00:00 清零）易被卡点集火的缺陷（防止 23:50 跑满 00:10 又跑满）。
//!    - 采用 24 小时分桶滑动窗口，精确计算过去 1 小时与过去 24 小时的累积消耗。
//!
//! 4. 连续疲劳阶梯退避惩罚 (Consecutive Fatigue Backoff):
//!    - 第 1 次触发小时软上限：小憩 1.0 ~ 1.5 小时 (3600~5400s 随机)。
//!    - 第 2 次触发小时软上限：加深休息 2.0 ~ 3.5 小时 (7200~12600s 随机)。
//!    - 第 3 次及以上连续触发：深度休眠至次日 12 ~ 18 小时 (43200~64800s 随机)。
//!    - 苏醒后平稳运行超过 4 小时未再触顶，阶梯惩罚计数自动重置。
//!    - 疲劳期账号坚决排除，绝不兜底放行！

use std::collections::VecDeque;
use std::sync::Arc;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// 单请求流式默认预占 Output Tokens
pub const DEFAULT_IN_FLIGHT_RESERVED_TOKENS: u64 = 2000;

/// 单账号小时 Output 软上限随机区间：[50_000, 68_000]
pub const MIN_HOURLY_OUTPUT_TOKEN_LIMIT: u64 = 50_000;
pub const MAX_HOURLY_OUTPUT_TOKEN_LIMIT: u64 = 68_000;

/// 单账号 24 小时 Output 软上限随机区间：[260_000, 330_000]
pub const MIN_DAILY_OUTPUT_TOKEN_LIMIT: u64 = 260_000;
pub const MAX_DAILY_OUTPUT_TOKEN_LIMIT: u64 = 330_000;

/// 疲劳阶梯退避配置 (秒)
pub const TIER1_NAP_MIN_SECS: i64 = 3600;      // 1.0 小时
pub const TIER1_NAP_MAX_SECS: i64 = 5400;      // 1.5 小时
pub const TIER2_NAP_MIN_SECS: i64 = 7200;      // 2.0 小时
pub const TIER2_NAP_MAX_SECS: i64 = 12600;     // 3.5 小时
pub const TIER3_NAP_MIN_SECS: i64 = 43200;     // 12.0 小时
pub const TIER3_NAP_MAX_SECS: i64 = 64800;     // 18.0 小时

/// 阶梯惩罚重置静默期 (秒)：苏醒后 4 小时内未触顶则重置
pub const FATIGUE_STRIKE_COOLDOWN_SECS: i64 = 14400;

/// 削峰填谷平滑抖动阈值：当两个账号当日 Output Token 差距在 5000 tokens 以内时，视为同一消耗梯队
pub const LOAD_SMOOTHING_TOKEN_DIFF_THRESHOLD: u64 = 5000;

/// 单账号动态抖动软上限
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AccountJitterCap {
    pub hourly_limit: u64,
    pub daily_limit: u64,
    pub day_epoch: i64,
}

impl AccountJitterCap {
    pub fn generate_random(day_epoch: i64) -> Self {
        let mut rng = rand::thread_rng();
        Self {
            hourly_limit: rng.gen_range(MIN_HOURLY_OUTPUT_TOKEN_LIMIT..=MAX_HOURLY_OUTPUT_TOKEN_LIMIT),
            daily_limit: rng.gen_range(MIN_DAILY_OUTPUT_TOKEN_LIMIT..=MAX_DAILY_OUTPUT_TOKEN_LIMIT),
            day_epoch,
        }
    }
}

/// 疲劳触发原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FatigueReason {
    /// 过去 1 小时 (包含在途预占) 超过动态上限
    HourlyOutputLimitExceeded,
    /// 过去 24 小时 (包含在途预占) 超过动态上限
    DailyOutputLimitExceeded,
    /// 双上限同时超标
    BothLimitsExceeded,
    /// 手动设置
    Manual,
}

impl std::fmt::Display for FatigueReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HourlyOutputLimitExceeded => write!(f, "过去 1 小时 Output Tokens 触及动态软上限"),
            Self::DailyOutputLimitExceeded => write!(f, "过去 24 小时 Output Tokens 触及动态软上限"),
            Self::BothLimitsExceeded => write!(f, "小时与 24 小时滑动窗口 Output Tokens 均触及动态软上限"),
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
    pub consecutive_strikes: u32,
}

/// 单小时统计桶
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HourlyBucket {
    pub epoch_hour: i64,
    pub output_tokens: u64,
}

/// 账号实时 Token 统计快照
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountTokenUsage {
    pub account_id: String,
    pub email: String,
    pub hourly_output_tokens: u64,
    pub daily_output_tokens: u64,
    pub daily_total_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
    pub in_flight_reserved_tokens: u64,
    pub request_count: u64,
    pub last_updated_at: i64,
}

/// 全局单例：疲劳休眠注册表 account_id -> FatigueRecord
static GLOBAL_FATIGUE_REGISTRY: Lazy<Arc<DashMap<String, FatigueRecord>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例：账号动态抖动上限 account_id -> AccountJitterCap
static GLOBAL_JITTER_CAPS: Lazy<Arc<DashMap<String, AccountJitterCap>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例：24 小时分桶滑动窗口 account_id -> VecDeque<HourlyBucket>
static GLOBAL_ROLLING_BUCKETS: Lazy<Arc<DashMap<String, VecDeque<HourlyBucket>>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例：在途流式预占数量 account_id -> reserved_tokens
static GLOBAL_IN_FLIGHT_RESERVED: Lazy<Arc<DashMap<String, u64>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例：账号连续疲劳惩罚记录 account_id -> (strikes, last_strike_epoch)
static GLOBAL_FATIGUE_STRIKES: Lazy<Arc<DashMap<String, (u32, i64)>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 全局单例：实时使用量追踪
static GLOBAL_USAGE_TRACKER: Lazy<Arc<DashMap<String, AccountTokenUsage>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// 可参与调度的账号候选对象 Trait
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

impl AccountCandidate for crate::models::Account {
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
        self.health_score.unwrap_or(1.0)
    }
    fn remaining_quota(&self) -> Option<i32> {
        self.quota.as_ref().and_then(|q| {
            q.models
                .iter()
                .map(|m| m.percentage)
                .min()
        })
    }
    fn is_model_protected(&self, model: &str) -> bool {
        self.protected_models
            .as_ref()
            .map(|pm| pm.iter().any(|m| m.eq_ignore_ascii_case(model)))
            .unwrap_or(false)
    }
    fn fatigue_until(&self) -> Option<i64> {
        None
    }
}

impl AccountCandidate for crate::proxy::token_manager::ProxyToken {
    fn account_id(&self) -> &str {
        &self.account_id
    }
    fn email(&self) -> &str {
        &self.email
    }
    fn is_disabled(&self) -> bool {
        false
    }
    fn health_score(&self) -> f32 {
        self.health_score
    }
    fn remaining_quota(&self) -> Option<i32> {
        self.remaining_quota
    }
    fn is_model_protected(&self, model: &str) -> bool {
        self.protected_models
            .iter()
            .any(|m| m.eq_ignore_ascii_case(model))
    }
    fn fatigue_until(&self) -> Option<i64> {
        None
    }
}

/// 账号选择器调度器核心
#[derive(Debug, Clone, Default)]
pub struct AccountSelector;

impl AccountSelector {
    pub fn new() -> Self {
        Self
    }

    /// 获取或随机初始化当前账号的动态抖动软上限
    pub fn get_or_init_jitter_cap(&self, account_id: &str, now_ts: i64) -> AccountJitterCap {
        let current_day_epoch = now_ts / 86400;
        if let Some(entry) = GLOBAL_JITTER_CAPS.get(account_id) {
            if entry.day_epoch == current_day_epoch {
                return *entry;
            }
        }
        let new_cap = AccountJitterCap::generate_random(current_day_epoch);
        GLOBAL_JITTER_CAPS.insert(account_id.to_string(), new_cap);
        tracing::debug!(
            "🎲 [AccountSelector] Initialized jittered soft limit for account {}: hourly={}, daily={} (day_epoch={})",
            account_id,
            new_cap.hourly_limit,
            new_cap.daily_limit,
            current_day_epoch
        );
        new_cap
    }

    /// 获取过去 1 小时与过去 24 小时的真实滑动窗口消耗
    pub fn get_rolling_usage(&self, account_id: &str, now_ts: i64) -> (u64, u64) {
        let current_hour = now_ts / 3600;
        let min_retained_hour = current_hour - 23;

        let mut buckets = GLOBAL_ROLLING_BUCKETS
            .entry(account_id.to_string())
            .or_insert_with(VecDeque::new);

        // 清理超过 24 小时的过期桶
        while let Some(front) = buckets.front() {
            if front.epoch_hour < min_retained_hour {
                buckets.pop_front();
            } else {
                break;
            }
        }

        let mut hourly_output = 0u64;
        let mut daily_rolling_output = 0u64;

        for b in buckets.iter() {
            daily_rolling_output += b.output_tokens;
            if b.epoch_hour == current_hour {
                hourly_output += b.output_tokens;
            }
        }

        (hourly_output, daily_rolling_output)
    }

    /// 请求派遣时预占在途 Output Tokens（防并发穿透）
    pub fn reserve_in_flight(&self, account_id: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let mut entry = GLOBAL_IN_FLIGHT_RESERVED
            .entry(account_id.to_string())
            .or_insert(0);
        *entry += tokens;
    }

    /// 请求结束或中断时释放预占在途 Output Tokens
    pub fn release_in_flight(&self, account_id: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        if let Some(mut entry) = GLOBAL_IN_FLIGHT_RESERVED.get_mut(account_id) {
            *entry = entry.saturating_sub(tokens);
        }
    }

    /// 获取当前在途预占 Tokens
    pub fn get_in_flight_reserved(&self, account_id: &str) -> u64 {
        GLOBAL_IN_FLIGHT_RESERVED
            .get(account_id)
            .map(|r| *r)
            .unwrap_or(0)
    }

    /// 评估是否需要触发疲劳休眠（结合滑动窗口、在途预占与阶梯退避惩罚）
    pub fn check_and_update_fatigue(
        &self,
        account_id: &str,
        email: &str,
        now_ts: i64,
    ) -> Option<FatigueRecord> {
        // 如果正在休眠期中，直接返回
        if let Some(existing) = GLOBAL_FATIGUE_REGISTRY.get(account_id) {
            if now_ts < existing.fatigue_until {
                return Some(existing.clone());
            }
        }

        let jitter_cap = self.get_or_init_jitter_cap(account_id, now_ts);
        let (rolling_hourly, rolling_daily) = self.get_rolling_usage(account_id, now_ts);
        let in_flight = self.get_in_flight_reserved(account_id);

        let effective_hourly = rolling_hourly + in_flight;
        let effective_daily = rolling_daily + in_flight;

        let hourly_exceeded = effective_hourly >= jitter_cap.hourly_limit;
        let daily_exceeded = effective_daily >= jitter_cap.daily_limit;

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

        // 计算阶梯退避惩罚（Backoff）
        let mut strikes_entry = GLOBAL_FATIGUE_STRIKES
            .entry(account_id.to_string())
            .or_insert((0, 0));
        let (mut strikes, last_strike_time) = *strikes_entry;

        // 苏醒后超过 4 小时未再触顶，重置惩罚梯队
        if now_ts.saturating_sub(last_strike_time) > FATIGUE_STRIKE_COOLDOWN_SECS {
            strikes = 0;
        }
        strikes += 1;
        *strikes_entry = (strikes, now_ts);

        let mut rng = rand::thread_rng();
        let nap_duration = match strikes {
            1 => rng.gen_range(TIER1_NAP_MIN_SECS..=TIER1_NAP_MAX_SECS),
            2 => rng.gen_range(TIER2_NAP_MIN_SECS..=TIER2_NAP_MAX_SECS),
            _ => rng.gen_range(TIER3_NAP_MIN_SECS..=TIER3_NAP_MAX_SECS),
        };
        let fatigue_until = now_ts + nap_duration;

        let record = FatigueRecord {
            account_id: account_id.to_string(),
            email: email.to_string(),
            fatigue_until,
            entered_at: now_ts,
            nap_duration_secs: nap_duration,
            reason,
            hourly_output_tokens: effective_hourly,
            daily_output_tokens: effective_daily,
            consecutive_strikes: strikes,
        };

        GLOBAL_FATIGUE_REGISTRY.insert(account_id.to_string(), record.clone());

        tracing::warn!(
            "😴 [AccountSelector] Account {} ({}) triggered fatigue sleep! Strike #{}, sleeping for {}s (~{:.1}h) until {}. (hourly: {}/{}, rolling_24h: {}/{}, in_flight: {})",
            account_id,
            email,
            strikes,
            nap_duration,
            (nap_duration as f64) / 3600.0,
            fatigue_until,
            effective_hourly,
            jitter_cap.hourly_limit,
            effective_daily,
            jitter_cap.daily_limit,
            in_flight
        );

        Some(record)
    }

    /// 记录 Token 消耗并更新 24h 滑动窗口
    pub fn record_token_usage(
        &self,
        account_id: &str,
        email: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Option<FatigueRecord> {
        let now_ts = chrono::Utc::now().timestamp();
        let current_hour = now_ts / 3600;
        let total_tokens = (input_tokens as u64) + (output_tokens as u64);
        let output_u64 = output_tokens as u64;

        // 1. 记入 24h 分桶滑动窗口
        {
            let mut buckets = GLOBAL_ROLLING_BUCKETS
                .entry(account_id.to_string())
                .or_insert_with(VecDeque::new);

            if let Some(last) = buckets.back_mut() {
                if last.epoch_hour == current_hour {
                    last.output_tokens += output_u64;
                } else {
                    buckets.push_back(HourlyBucket {
                        epoch_hour: current_hour,
                        output_tokens: output_u64,
                    });
                }
            } else {
                buckets.push_back(HourlyBucket {
                    epoch_hour: current_hour,
                    output_tokens: output_u64,
                });
            }

            // 清理过期桶
            let min_retained = current_hour - 23;
            while let Some(front) = buckets.front() {
                if front.epoch_hour < min_retained {
                    buckets.pop_front();
                } else {
                    break;
                }
            }
        }

        // 2. 更新总体累计统计
        {
            let (rolling_hourly, rolling_daily) = self.get_rolling_usage(account_id, now_ts);
            let in_flight = self.get_in_flight_reserved(account_id);

            let mut entry = GLOBAL_USAGE_TRACKER
                .entry(account_id.to_string())
                .or_insert_with(|| AccountTokenUsage {
                    account_id: account_id.to_string(),
                    email: email.to_string(),
                    last_updated_at: now_ts,
                    ..Default::default()
                });

            entry.hourly_output_tokens = rolling_hourly;
            entry.daily_output_tokens = rolling_daily;
            entry.daily_total_tokens += total_tokens;
            entry.total_output_tokens += output_u64;
            entry.total_tokens += total_tokens;
            entry.in_flight_reserved_tokens = in_flight;
            entry.request_count += 1;
            entry.last_updated_at = now_ts;
        }

        // 3. 检查并触发疲劳熔断
        self.check_and_update_fatigue(account_id, email, now_ts)
    }

    /// 检查指定账号是否正处于疲劳休眠期（超时自动苏醒，休眠中坚决排除）
    pub fn is_account_fatigued(&self, account_id: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        if let Some(entry) = GLOBAL_FATIGUE_REGISTRY.get(account_id) {
            if now < entry.fatigue_until {
                return true;
            }
        }
        // 若休眠时间已过，自动移除休眠记录
        if GLOBAL_FATIGUE_REGISTRY.contains_key(account_id) {
            if let Some((_, record)) = GLOBAL_FATIGUE_REGISTRY.remove(account_id) {
                tracing::info!(
                    "⏰ [AccountSelector] Account {} ({}) fatigue sleep finished at {}. Re-admitted to available candidates.",
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
            GLOBAL_FATIGUE_REGISTRY.get(account_id).map(|r| r.fatigue_until)
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
        GLOBAL_FATIGUE_REGISTRY.insert(
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
                consecutive_strikes: 1,
            },
        );
    }

    /// 清除疲劳休眠状态
    pub fn clear_fatigue(&self, account_id: &str) -> bool {
        GLOBAL_FATIGUE_REGISTRY.remove(account_id).is_some()
    }

    /// 获取所有处于休眠期的账号列表
    pub fn list_fatigued_accounts(&self) -> Vec<FatigueRecord> {
        let now = chrono::Utc::now().timestamp();
        let mut result = Vec::new();
        let mut expired = Vec::new();

        for item in GLOBAL_FATIGUE_REGISTRY.iter() {
            if now < item.fatigue_until {
                result.push(item.clone());
            } else {
                expired.push(item.key().clone());
            }
        }

        for exp_key in expired {
            GLOBAL_FATIGUE_REGISTRY.remove(&exp_key);
        }

        result
    }

    /// 账号可用性过滤：严格排除处于疲劳期的账号，坚决不兜底放行！
    pub fn filter_available<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Vec<&'a T> {
        let now = chrono::Utc::now().timestamp();
        let mut fatigued_count = 0usize;

        let available: Vec<&'a T> = candidates
            .iter()
            .filter(|c| {
                if c.is_disabled() {
                    return false;
                }

                if let Some(model) = target_model {
                    if c.is_model_protected(model) {
                        return false;
                    }
                }

                // 检查候选对象本身的静态疲劳字段
                if let Some(until) = c.fatigue_until() {
                    if now < until {
                        fatigued_count += 1;
                        return false;
                    }
                }

                // 检查全局注册表中的疲劳休眠
                if self.is_account_fatigued(c.account_id()) {
                    fatigued_count += 1;
                    return false;
                }

                // 检查实时水位 + 在途预占是否已饱和（即时阻断流式并发穿透）
                let jitter_cap = self.get_or_init_jitter_cap(c.account_id(), now);
                let (rolling_hourly, rolling_daily) = self.get_rolling_usage(c.account_id(), now);
                let in_flight = self.get_in_flight_reserved(c.account_id());

                if (rolling_hourly + in_flight) >= jitter_cap.hourly_limit
                    || (rolling_daily + in_flight) >= jitter_cap.daily_limit
                {
                    // 立即触发休眠并排除
                    let _ = self.check_and_update_fatigue(c.account_id(), c.email(), now);
                    fatigued_count += 1;
                    return false;
                }

                true
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

    /// 最少使用优先（Least-Tokens-Used）候选排序
    pub fn rank_candidates<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Vec<&'a T> {
        let available = self.filter_available(candidates, target_model);
        if available.is_empty() {
            return Vec::new();
        }

        let now_ts = chrono::Utc::now().timestamp();
        let mut metrics: Vec<(&'a T, u64, u64, f32)> = available
            .into_iter()
            .map(|c| {
                let (rolling_hourly, rolling_daily) = self.get_rolling_usage(c.account_id(), now_ts);
                let in_flight = self.get_in_flight_reserved(c.account_id());
                let total_consumed = rolling_daily + in_flight;
                let total_tok = GLOBAL_USAGE_TRACKER
                    .get(c.account_id())
                    .map(|u| u.total_tokens)
                    .unwrap_or(0);
                let health = c.health_score();
                (c, total_consumed, total_tok, health)
            })
            .collect();

        metrics.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
        });

        metrics.into_iter().map(|(c, _, _, _)| c).collect()
    }

    /// 最少使用优先调度与负载削峰填谷
    pub fn select_least_used<'a, T: AccountCandidate>(
        &self,
        candidates: &'a [T],
        target_model: Option<&str>,
    ) -> Option<&'a T> {
        let available = self.filter_available(candidates, target_model);
        if available.is_empty() {
            return None;
        }
        if available.len() == 1 {
            return Some(available[0]);
        }

        let now_ts = chrono::Utc::now().timestamp();
        let mut candidate_metrics: Vec<(&'a T, u64, u64, f32)> = available
            .into_iter()
            .map(|c| {
                let (_, rolling_daily) = self.get_rolling_usage(c.account_id(), now_ts);
                let in_flight = self.get_in_flight_reserved(c.account_id());
                let daily_effective = rolling_daily + in_flight;
                let total_tok = GLOBAL_USAGE_TRACKER
                    .get(c.account_id())
                    .map(|u| u.total_tokens)
                    .unwrap_or(0);
                let health = c.health_score();
                (c, daily_effective, total_tok, health)
            })
            .collect();

        candidate_metrics.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
        });

        // 削峰填谷负载平滑：与最低消耗差距在 5000 tokens 以内的第一梯队随机分摊
        let min_daily = candidate_metrics[0].1;
        let pool_tier: Vec<&'a T> = candidate_metrics
            .iter()
            .take(5)
            .filter(|(_, daily_out, _, _)| *daily_out <= min_daily + LOAD_SMOOTHING_TOKEN_DIFF_THRESHOLD)
            .map(|(c, _, _, _)| *c)
            .collect();

        if pool_tier.len() <= 1 {
            return Some(candidate_metrics[0].0);
        }

        let mut rng = rand::thread_rng();
        let pick_idx = rng.gen_range(0..pool_tier.len());
        Some(pool_tier[pick_idx])
    }
}

// 兼容全局便捷函数
pub fn reserve_in_flight(account_id: &str, tokens: u64) {
    AccountSelector.reserve_in_flight(account_id, tokens);
}

pub fn release_in_flight(account_id: &str, tokens: u64) {
    AccountSelector.release_in_flight(account_id, tokens);
}

pub fn check_and_update_fatigue(
    account_id: &str,
    email: &str,
    _hourly_output: u64,
    _daily_output: u64,
) -> Option<FatigueRecord> {
    let now = chrono::Utc::now().timestamp();
    AccountSelector.check_and_update_fatigue(account_id, email, now)
}

pub fn record_token_usage(
    account_id: &str,
    email: &str,
    input_tokens: u32,
    output_tokens: u32,
) -> Option<FatigueRecord> {
    AccountSelector.record_token_usage(account_id, email, input_tokens, output_tokens)
}

pub fn is_account_fatigued(account_id: &str) -> bool {
    AccountSelector.is_account_fatigued(account_id)
}

pub fn clear_account_fatigue(account_id: &str) -> bool {
    AccountSelector.clear_fatigue(account_id)
}

pub fn filter_available_candidates<'a, T: AccountCandidate>(
    candidates: &'a [T],
    target_model: Option<&str>,
) -> Vec<&'a T> {
    AccountSelector.filter_available(candidates, target_model)
}

pub fn select_least_used_account<'a, T: AccountCandidate>(
    candidates: &'a [T],
    target_model: Option<&str>,
) -> Option<&'a T> {
    AccountSelector.select_least_used(candidates, target_model)
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

    #[test]
    fn test_jittered_soft_limits() {
        let selector = AccountSelector::new();
        let now_ts = 1700000000;
        let cap1 = selector.get_or_init_jitter_cap("acc_1", now_ts);
        assert!(cap1.hourly_limit >= MIN_HOURLY_OUTPUT_TOKEN_LIMIT && cap1.hourly_limit <= MAX_HOURLY_OUTPUT_TOKEN_LIMIT);
        assert!(cap1.daily_limit >= MIN_DAILY_OUTPUT_TOKEN_LIMIT && cap1.daily_limit <= MAX_DAILY_OUTPUT_TOKEN_LIMIT);
    }

    #[test]
    fn test_inflight_reservation_blocks_penetration() {
        let selector = AccountSelector::new();
        let acc_id = "acc_penetration_test";
        let email = "p@example.com";
        let now_ts = chrono::Utc::now().timestamp();

        let cap = selector.get_or_init_jitter_cap(acc_id, now_ts);
        // 模拟已消耗到达距离上限差 1500 tokens
        let used = cap.hourly_limit - 1500;
        selector.record_token_usage(acc_id, email, 0, used as u32);

        let acc = MockAccount {
            id: acc_id.to_string(),
            email: email.to_string(),
            disabled: false,
            health: 1.0,
            protected: false,
            fatigue_until: None,
        };

        // 此时未预占前，账号仍可用
        let avail = selector.filter_available(&[acc.clone()], None);
        assert_eq!(avail.len(), 1);

        // 现在第 1 个并发请求进来，发起 2000 tokens 预占
        selector.reserve_in_flight(acc_id, 2000);

        // 再次检查候选，已用 + 预占 >= hourly_limit，立即被阻断（filter 结果为 0）
        let avail_blocked = selector.filter_available(&[acc.clone()], None);
        assert_eq!(avail_blocked.len(), 0, "在途预占生效，成功阻断并发穿透打爆软上限！");

        // 请求结束释放预占
        selector.release_in_flight(acc_id, 2000);
    }

    #[test]
    fn test_consecutive_fatigue_backoff() {
        let selector = AccountSelector::new();
        let acc_id = "acc_backoff_test";
        let email = "backoff@example.com";
        let now_ts = chrono::Utc::now().timestamp();
        let cap = selector.get_or_init_jitter_cap(acc_id, now_ts);

        // 第 1 次触发
        selector.record_token_usage(acc_id, email, 0, cap.hourly_limit as u32);
        let rec1 = selector.check_and_update_fatigue(acc_id, email, now_ts).unwrap();
        assert_eq!(rec1.consecutive_strikes, 1);
        assert!(rec1.nap_duration_secs >= TIER1_NAP_MIN_SECS && rec1.nap_duration_secs <= TIER1_NAP_MAX_SECS);

        // 模拟第 1 次苏醒并迅速再次触顶 (第 2 次触发)
        selector.clear_fatigue(acc_id);
        selector.record_token_usage(acc_id, email, 0, cap.hourly_limit as u32);
        let rec2 = selector.check_and_update_fatigue(acc_id, email, now_ts + 3600).unwrap();
        assert_eq!(rec2.consecutive_strikes, 2);
        assert!(rec2.nap_duration_secs >= TIER2_NAP_MIN_SECS && rec2.nap_duration_secs <= TIER2_NAP_MAX_SECS);

        // 模拟第 3 次触发
        selector.clear_fatigue(acc_id);
        selector.record_token_usage(acc_id, email, 0, cap.hourly_limit as u32);
        let rec3 = selector.check_and_update_fatigue(acc_id, email, now_ts + 7200).unwrap();
        assert_eq!(rec3.consecutive_strikes, 3);
        assert!(rec3.nap_duration_secs >= TIER3_NAP_MIN_SECS && rec3.nap_duration_secs <= TIER3_NAP_MAX_SECS);
    }
}
