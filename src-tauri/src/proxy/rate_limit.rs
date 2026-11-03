use dashmap::DashMap;
use regex::Regex;
use std::time::{Duration, SystemTime};

/// 限流原因类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitReason {
    /// 配额耗尽 (QUOTA_EXHAUSTED)
    QuotaExhausted,
    /// 速率限制 (RATE_LIMIT_EXCEEDED)
    RateLimitExceeded,
    /// 模型容量耗尽 (MODEL_CAPACITY_EXHAUSTED)
    ModelCapacityExhausted,
    /// 服务器错误 (5xx)
    ServerError,
    /// 未知原因
    Unknown,
}

/// 限流信息
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RateLimitInfo {
    /// 限流重置时间
    pub reset_time: SystemTime,
    /// 重试间隔(秒)
    #[allow(dead_code)]
    pub retry_after_sec: u64,
    /// 检测时间
    #[allow(dead_code)]
    pub detected_at: SystemTime,
    /// 限流原因
    #[allow(dead_code)] // Used for logging and diagnostics
    pub reason: RateLimitReason,
    /// 关联的模型 (用于模型级别限流)
    /// None 表示账号级别限流,Some(model) 表示特定模型限流
    #[allow(dead_code)] // Used for model-level rate limiting
    pub model: Option<String>,
}

/// 连续失败计数过期时间：1小时（超过此时间未失败则重置计数）
const FAILURE_COUNT_EXPIRY_SECONDS: u64 = 3600;

/// 冷启动平滑缓冲：服务启动后前 60 秒内禁止大体量请求直接突刺上游，
/// 防止 docker restart 后积压请求瞬间涌入触发 Google 风控（TOS_VIOLATION）
const COLD_START_WARMUP_SECS: u64 = 60;

/// Dedicated proxy just recovered GREEN:
/// - Phase 1 (0..60s): hold ALL traffic tiers (Normal / Medium / Oversized) so the proxy tunnel settles completely.
/// - Phase 2 (60..180s): allow Normal traffic to warm up TLS identity, hold Medium and Oversized requests.
pub const EGRESS_SETTLE_ALL_TRAFFIC_SECS: u64 = 60;
pub const EGRESS_RECOVERY_COOLDOWN_SECS: u64 = 180;

/// 默认单账号并发槽位数（未在页面或账号单独配置时默认 2 并发槽）
pub const DEFAULT_CONCURRENCY_SLOTS: usize = 2;

/// 粘性会话允许等待单账号槽位呼吸冷却的最大毫秒数（18s：覆盖大包 8~15s 冷却，防止频繁解绑跳冷号，且低于客户端 30~60s TTFB 超时）
pub const STICKY_MAX_SLOT_WAIT_MS: u64 = 18_000;

/// 全号池均处于冷却延迟中时，兜底允许预约等待最短冷却槽位的最大毫秒数
pub const FALLBACK_MAX_SLOT_WAIT_MS: u64 = 5000;

/// 流量分级物理字节与 Token 边界（按 1 Token ≈ 4 Bytes 严格对齐）
pub const LIGHT_MEDIUM_TOKENS: u32 = 30_000; // 30K tokens（轻度微加热度门槛）
pub const MEDIUM_BODY_BYTES: usize = 200 * 1024; // 200 KB
pub const OVERSIZE_BODY_BYTES: usize = 400 * 1024; // 400 KB
pub const MEDIUM_TOKENS: u32 = 50_000; // 50K tokens
pub const HEAVY_TOKENS: u32 = 80_000; // 80K tokens
pub const OVERSIZE_TOKENS: u32 = 100_000; // 100K tokens

/// 热度半衰期：从 600 秒调整为 200 秒 (~3.3 分钟) 指数衰减，加快账号自然静默降温
const HEAT_HALF_LIFE_SECS: f64 = 200.0;

/// 单账号 60 秒滑动窗口请求上限（严格限制最多 5 次/分钟，达到后触发冷却并换号）
pub const ACCOUNT_MAX_REQUESTS_PER_MINUTE: usize = 5;

/// 超大请求（>100K tokens 或 >400KB）10 分钟滑动时间窗硬上限：任意 600 秒内严格 < 3 个（最多允许 2 个）
pub const OVERSIZE_WINDOW_SECS: u64 = 600;
pub const MAX_OVERSIZE_IN_WINDOW: usize = 2;

/// OAuth Token 刚刷新后的大包保护期（60 秒）：
/// 新签发的 ya29 Access Token 前 60 秒内严禁直接承接 Medium/Oversized（>50K tokens 或 >200KB）请求，
/// 仅允许 loadCodeAssist (40B) 握手与 Normal（<=50K tokens 且 <=200KB）轻量请求热身
pub const TOKEN_REFRESH_WARMUP_OVERSIZE_SECS: u64 = 60;

/// 各分级单次热度增量：
/// - Normal (<30K tokens): +0.0
/// - Light-Medium (30K~50K tokens): +0.08 (微加热度)
/// - Medium (50K~80K tokens): +0.35
/// - Heavy (80K~100K tokens): +0.8
/// - Oversized (>100K tokens): +1.5
pub const LIGHT_MEDIUM_HEAT_INCREMENT: f64 = 0.08;
const MEDIUM_HEAT_INCREMENT: f64 = 0.35;
const HEAVY_HEAT_INCREMENT: f64 = 0.8;
const OVERSIZE_HEAT_INCREMENT: f64 = 1.5;

/// 超大请求（>100K tokens 或 >400KB）准入红线：推演热度 (current_heat + 1.5) >= 3.0 即拒绝进入该账号。
/// 保证单账号在短时间窗口内承接 >100K 的请求严格少于 3 个（最多允许 2 个：1.5 -> ~2.9 < 3.0，第 3 个 >= 3.0 进门前拦截换号）。
pub const OVERSIZE_HEAT_CAP: f64 = 3.0;

/// 冷号池优选线：Heat < 1.5 视为清凉账号，在自由选号（P2C）中享受最高优先级
pub const COLD_POOL_HEAT_THRESHOLD: f64 = 1.5;

pub fn remaining_recovery_hold_secs_for_tier(elapsed_secs: u64, tier: TrafficTier) -> Option<u64> {
    if elapsed_secs < EGRESS_SETTLE_ALL_TRAFFIC_SECS {
        Some((EGRESS_SETTLE_ALL_TRAFFIC_SECS - elapsed_secs).max(5))
    } else if elapsed_secs < EGRESS_RECOVERY_COOLDOWN_SECS && tier != TrafficTier::Normal {
        Some((EGRESS_RECOVERY_COOLDOWN_SECS - elapsed_secs).max(10))
    } else {
        None
    }
}

/// 请求流量四级分级（L1 日常轻量 / L2 中大请求 / L3 超大高危；L4 为账号轮次微小憩）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficTier {
    /// <= 200KB 且 < 50K tokens：低延迟（0.8s~1.8s），热度 +0.0，占用 1 槽
    Normal,
    /// (200KB..=400KB) 或 [50K..=100K] tokens：少量延迟（2.0s~3.8s），热度 +0.5，占用 1 槽
    Medium,
    /// > 400KB 或 > 100K tokens：充分冷却（8.0s~15.0s），热度 +1.5（短时严格 < 3 个），独占全部并发槽（与小请求互斥）
    Oversized,
}

/// 前置提取的请求体量特征（在选号前算清，供 TokenManager 容量感知路由使用）
#[derive(Debug, Clone, Copy)]
pub struct RequestFootprint {
    pub prompt_tokens: u32,
    pub raw_body_len: usize,
    pub tier: TrafficTier,
}

impl Default for RequestFootprint {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl RequestFootprint {
    pub fn new(prompt_tokens: u32, raw_body_len: usize) -> Self {
        let tier = if raw_body_len > OVERSIZE_BODY_BYTES || prompt_tokens > OVERSIZE_TOKENS {
            TrafficTier::Oversized
        } else if raw_body_len > MEDIUM_BODY_BYTES || prompt_tokens >= MEDIUM_TOKENS {
            TrafficTier::Medium
        } else {
            TrafficTier::Normal
        };
        Self {
            prompt_tokens,
            raw_body_len,
            tier,
        }
    }

    #[inline]
    pub fn is_oversized(&self) -> bool {
        self.tier == TrafficTier::Oversized
    }

    #[inline]
    pub fn is_medium_or_oversized(&self) -> bool {
        self.tier != TrafficTier::Normal
    }

    pub fn heat_increment(&self) -> f64 {
        let tokens = self.prompt_tokens;
        let bytes = self.raw_body_len;

        if tokens >= OVERSIZE_TOKENS || bytes > OVERSIZE_BODY_BYTES {
            OVERSIZE_HEAT_INCREMENT
        } else if tokens >= HEAVY_TOKENS || bytes > 320 * 1024 {
            HEAVY_HEAT_INCREMENT
        } else if tokens >= MEDIUM_TOKENS || bytes > MEDIUM_BODY_BYTES {
            MEDIUM_HEAT_INCREMENT
        } else if tokens >= LIGHT_MEDIUM_TOKENS || bytes > 120 * 1024 {
            LIGHT_MEDIUM_HEAT_INCREMENT
        } else {
            0.0
        }
    }
}

/// 单个并发槽位的完整生命周期阶段（占用期 = 实际请求耗时 + 计算出的冷却延迟）
#[derive(Debug, Clone)]
pub enum SlotPhase {
    /// 槽位空闲打开，可零等待进入
    Free,
    /// 已被粘性微等（<=2s）或全池兜底请求原子预约锁定，等待 `ready_at` 到达后转为 InFlight
    Reserved {
        lease_id: u64,
        ready_at: std::time::Instant,
        restore_cooldown: Option<(std::time::Instant, bool)>,
        exclusive: bool,
    },
    /// 请求正在执行中（真实网络流式/非流式耗时阶段，槽位闭合）
    InFlight {
        lease_id: u64,
        started_at: std::time::Instant,
        exclusive: bool,
    },
    /// 响应已结束，但正处于计算出的冷却延迟倒计时内（槽位保持闭合，直至 `until` 才重新打开）
    CoolingDown {
        until: std::time::Instant,
        exclusive: bool,
    },
}

/// 账号前置准入拒绝原因（供路由层静默解绑切号或统计全池最短恢复秒数）
#[derive(Debug, Clone)]
pub enum LeaseRejectReason {
    /// 服务冷启动前 60s 缓冲（拦截超大请求突刺）
    ColdStartWarmup { retry_after_secs: u64 },
    /// 专属代理刚从 RED 恢复 GREEN 未满 180s 稳定期
    EgressRecovery { retry_after_secs: u64 },
    /// OAuth Access Token 刚完成刷新未满 60s，严禁直接作为首包或热身期承接 >200K 超大请求
    TokenRefreshWarmup { retry_after_secs: u64 },
    /// 账号处于 20~30 轮强制微小憩期间（休眠期全封闭，大小请求一律禁入）
    InNap { remaining_secs: u64 },
    /// 超大请求（>200K）触发热度红线（projected_heat >= 3.0，限制第 3 个大包进入）
    Overheated {
        current_heat: f64,
        projected_heat: f64,
        retry_after_secs: u64,
    },
    /// 并发槽全部处于执行中（InFlight/Reserved）或触发大小请求互斥锁
    SlotsBusy,
    /// 槽位处于冷却延迟或 60s RPM 限制中，且所需等待毫秒数超过当前模式上限（如粘性 > 2000ms）
    CooldownTooLong { wait_ms: u64 },
}

impl LeaseRejectReason {
    /// 若全池账号均被拒绝，折算该账号最早可恢复接单的秒数（用于全局兜底 Retry-After）
    pub fn estimated_recovery_secs(&self) -> Option<u64> {
        match self {
            Self::ColdStartWarmup { retry_after_secs } => Some(*retry_after_secs),
            Self::EgressRecovery { retry_after_secs } => Some(*retry_after_secs),
            Self::TokenRefreshWarmup { retry_after_secs } => Some(*retry_after_secs),
            Self::InNap { remaining_secs } => Some((*remaining_secs).max(1)),
            Self::Overheated {
                retry_after_secs, ..
            } => Some((*retry_after_secs).max(5)),
            Self::CooldownTooLong { wait_ms } => Some(((*wait_ms + 999) / 1000).max(1)),
            Self::SlotsBusy => Some(5),
        }
    }
}

/// 无副作用探测结果（供 P2C 候选池分层与比选使用）
#[derive(Debug, Clone)]
pub enum AccountProbe {
    Admissible {
        wait_ms: u64,
        current_heat: f64,
        free_slots: usize,
        total_slots: usize,
    },
    Rejected(LeaseRejectReason),
}

#[derive(Debug, Clone, Copy)]
enum InternalSlotEval {
    Ready { free_slots: usize, total_slots: usize },
    NeedWait { wait_ms: u64, total_slots: usize },
    Busy,
}

/// 单账号拟人化节奏、多并发槽位与热度风控状态
#[derive(Debug, Clone)]
pub struct AccountPacerState {
    /// 最近一次发起请求时间
    pub last_request_time: std::time::Instant,
    /// 最近一次流/响应完全结束的时间（EOF/[DONE]）
    pub last_response_completed_at: Option<std::time::Instant>,
    /// 下一次允许发起请求的最早时间点 (兼容单槽查询)
    pub next_allowed_request_at: std::time::Instant,
    /// 过去 60 秒内的请求时间戳历史（用于滚动 60s RPM，单号上限 ACCOUNT_MAX_REQUESTS_PER_MINUTE 次）
    pub rolling_requests: std::collections::VecDeque<std::time::Instant>,
    /// 过去 600 秒（10 分钟）内的超大请求（>200K）时间戳历史（严格限制 < 3 个，即最多 2 个）
    pub oversized_history: std::collections::VecDeque<std::time::Instant>,
    /// 历史累计请求总计数
    pub request_counter: u64,
    /// 当前小憩周期内已累计的对话轮数
    pub turns_since_nap: u64,
    /// 本周期触发强制小憩的目标轮数（每次休眠后重新随机抽取 15..=25）
    pub next_nap_threshold: u64,
    /// 当前波段基础间隔 (ms)
    pub current_interval_ms: u64,
    /// 微休眠截止时间
    pub nap_until: Option<std::time::Instant>,
    /// 保护阈值浮动
    pub threshold_jitter: i32,
    /// 大任务累积热度积分（600s 半衰期指数衰减）
    pub massive_heat: f64,
    /// 上次更新热度的时间
    pub last_heat_updated_at: std::time::Instant,
    /// 最近一次完成 OAuth Access Token 刷新的时间（刷新后前 60s 禁止承接 >200K 超大包）
    pub last_oauth_refresh_at: Option<std::time::Instant>,
    /// 多并发槽位表（槽位生命周期 = 实际请求耗时 + 计算延迟）
    pub slots: Vec<SlotPhase>,
    /// 今日累计已消耗 Output Tokens (用于日软上限 300,000 熔断)
    pub daily_output_tokens: u64,
    /// 当前小时累计已消耗 Output Tokens (用于小时软上限 60,000 熔断)
    pub hourly_output_tokens: u64,
    /// 上次小时窗口戳
    pub last_hourly_window: i64,
    /// 上次天窗口戳
    pub last_daily_window: i64,
    /// 触发疲劳熔断休眠截止时间
    pub fatigue_until: Option<std::time::Instant>,
}

impl AccountPacerState {
    pub fn new_default(account_id: &str, now: std::time::Instant, slot_count: usize) -> Self {
        use rand::Rng;
        let n = slot_count.max(1);
        let next_nap_threshold = rand::thread_rng().gen_range(15..=25);
        Self {
            last_request_time: now,
            last_response_completed_at: None,
            next_allowed_request_at: now,
            rolling_requests: std::collections::VecDeque::new(),
            oversized_history: std::collections::VecDeque::new(),
            request_counter: 0,
            turns_since_nap: 0,
            next_nap_threshold,
            current_interval_ms: 1200,
            nap_until: None,
            threshold_jitter: HumanizedPacer::get_threshold_jitter(account_id),
            massive_heat: 0.0,
            last_heat_updated_at: now,
            last_oauth_refresh_at: None,
            slots: vec![SlotPhase::Free; n],
            daily_output_tokens: 0,
            hourly_output_tokens: 0,
            last_hourly_window: chrono::Utc::now().timestamp() / 3600,
            last_daily_window: chrono::Utc::now().timestamp() / 86400,
            fatigue_until: None,
        }
    }
}

/// 单账号拟人化流控、并发槽位锁与防均质化调度器
pub struct HumanizedPacer {
    account_states: DashMap<String, AccountPacerState>,
    /// 服务进程启动时刻，用于冷启动平滑缓冲（重启后前 60s 限制大请求突刺）
    started_at: std::time::Instant,
    /// 全局递增租约 ID 生成器
    next_lease_id: std::sync::atomic::AtomicU64,
}

impl HumanizedPacer {
    pub fn new() -> Self {
        Self {
            account_states: DashMap::new(),
            started_at: std::time::Instant::now(),
            next_lease_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// 测试或特殊场景下跳过冷启动 60s 窗口的构造函数
    #[allow(dead_code)]
    pub fn new_warmed() -> Self {
        Self {
            account_states: DashMap::new(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(COLD_START_WARMUP_SECS + 10))
                .unwrap_or_else(std::time::Instant::now),
            next_lease_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    #[cfg(test)]
    pub fn test_clear_slot_cooldowns(&self, account_id: &str) {
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            let n = entry.slots.len().max(1);
            entry.slots = vec![SlotPhase::Free; n];
            entry.next_allowed_request_at = std::time::Instant::now();
        }
    }

    #[cfg(test)]
    pub fn test_set_all_slots_cooldown(
        &self,
        account_id: &str,
        slot_count: usize,
        cooldown_ms: u64,
        exclusive: bool,
    ) {
        let now = std::time::Instant::now();
        let until = now + Duration::from_millis(cooldown_ms);
        let mut entry = self
            .account_states
            .entry(account_id.to_string())
            .or_insert_with(|| AccountPacerState::new_default(account_id, now, slot_count));
        entry.slots = vec![SlotPhase::CoolingDown { until, exclusive }; slot_count.max(1)];
    }

    /// 根据四级流量分级计算响应结束后的槽位冷却延迟（毫秒）：
    /// - L1 Normal (<=200KB 且 <50K tokens)：低延迟 800ms ~ 1800ms（保障交互对话顺畅）
    /// - L2 Medium (200KB~400KB 或 50K~100K tokens)：拟人思考散热延迟 3500ms ~ 7500ms
    /// - L3 Oversized (>400KB 或 >100K tokens)：充分物理冷却 8000ms ~ 15000ms（被 18s 粘性等待窗口 100% 覆盖，防止误解绑跳冷号）
    pub fn compute_footprint_cooldown_ms(footprint: RequestFootprint) -> u64 {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        match footprint.tier {
            TrafficTier::Normal => {
                if footprint.prompt_tokens <= 30_000 && footprint.raw_body_len <= 50 * 1024 {
                    rng.gen_range(800..=1500)
                } else {
                    rng.gen_range(1100..=1800)
                }
            }
            // 5万~10万 Tokens 的中型请求：留出 3.5~7.5 秒拟人思考与散热时间
            TrafficTier::Medium => rng.gen_range(3500..=7500),
            // >10万 Tokens 极重计算：保持 8~15 秒充分冷却（完全在 18s 粘性等待上限内，杜绝旋转木马切号）
            TrafficTier::Oversized => rng.gen_range(8000..=15_000),
        }
    }

    /// 兼容旧签名：按 prompt_tokens 折算冷却时间
    pub fn compute_cooldown_ms(prompt_tokens: Option<u32>) -> u64 {
        let fp = RequestFootprint::new(prompt_tokens.unwrap_or(0), 0);
        Self::compute_footprint_cooldown_ms(fp)
    }

    /// 获取账号当前需要的拟人化波段间隔（毫秒，1000ms ~ 5000ms），带有正弦潮汐波与正态随机抖动
    pub fn compute_wave_interval_ms(counter: u64, _account_id: &str) -> u64 {
        use rand::Rng;
        let wave_phase = (counter as f64) * std::f64::consts::PI / 6.0;
        let sine_component = (wave_phase.sin() + 1.0) / 2.0;
        let base_interval = 1200.0 + sine_component * 3000.0;
        let jitter: f64 = rand::thread_rng().gen_range(-400.0..=600.0);
        let final_ms = (base_interval + jitter).round() as i64;
        final_ms.clamp(1000, 5000) as u64
    }

    /// 计算每个账号的专属保护阈值浮动 (-2% ~ +3%)，打散不同账号的停止水位，防止所有账号同时耗尽
    pub fn get_threshold_jitter(account_id: &str) -> i32 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        account_id.hash(&mut hasher);
        let h = hasher.finish();
        ((h % 6) as i32) - 2
    }

    /// 检查账号是否处于拟人化微休眠中 (Micro-Nap)
    pub fn is_in_nap(&self, account_id: &str) -> bool {
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            if let Some(nap) = entry.nap_until {
                let now = std::time::Instant::now();
                if now < nap {
                    return true;
                } else {
                    // 小憩 3~6 分钟结束：热度与疲劳轮数彻底清零，满血复活
                    entry.nap_until = None;
                    entry.massive_heat = 0.0;
                    entry.turns_since_nap = 0;
                    entry.last_heat_updated_at = now;
                    tracing::info!(
                        "[HumanizedPacer] Account {} finished nap (3-6m), heat and turns fully reset to 0.",
                        account_id
                    );
                }
            }
        }
        false
    }

    /// 获取账号剩余的拟人化微休眠秒数（若不在休眠中则返回 None）
    /// 记录账号消耗的 Output Tokens，维护滑动窗口统计，触发软上限疲劳熔断
    pub fn record_output_tokens(&self, account_id: &str, output_tokens: u64) {
        if output_tokens == 0 {
            return;
        }
        let now_epoch_hour = chrono::Utc::now().timestamp() / 3600;
        let now_epoch_day = chrono::Utc::now().timestamp() / 86400;
        let now = std::time::Instant::now();

        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            // 滑动小时窗口重置
            if entry.last_hourly_window != now_epoch_hour {
                entry.hourly_output_tokens = 0;
                entry.last_hourly_window = now_epoch_hour;
            }
            // 滑动天窗口重置
            if entry.last_daily_window != now_epoch_day {
                entry.daily_output_tokens = 0;
                entry.last_daily_window = now_epoch_day;
            }

            entry.hourly_output_tokens += output_tokens;
            entry.daily_output_tokens += output_tokens;

            // 软上限熔断检查：
            // 单账号小时 Output Tokens > 60,000 或 当日 Output Tokens > 300,000 时，进入疲劳深度休眠 1~2 小时
            const HOURLY_TOKEN_LIMIT: u64 = 60_000;
            const DAILY_TOKEN_LIMIT: u64 = 300_000;

            if entry.hourly_output_tokens >= HOURLY_TOKEN_LIMIT || entry.daily_output_tokens >= DAILY_TOKEN_LIMIT {
                if entry.fatigue_until.is_none() {
                    let cooldown_secs = 3600; // 冷却 1 小时，让其他空闲账号接力
                    entry.fatigue_until = Some(now + Duration::from_secs(cooldown_secs));
                    tracing::warn!(
                        "😴 [FatigueCooldown] Account {} hit token soft-cap (hourly: {}/{}, daily: {}/{}), entering fatigue cooldown for {}s to prevent ban",
                        account_id,
                        entry.hourly_output_tokens,
                        HOURLY_TOKEN_LIMIT,
                        entry.daily_output_tokens,
                        DAILY_TOKEN_LIMIT,
                        cooldown_secs
                    );
                }
            }
        }
    }

    /// 检查账号是否处于疲劳休眠冷却期中
    pub fn is_in_fatigue(&self, account_id: &str) -> bool {
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            if let Some(until) = entry.fatigue_until {
                let now = std::time::Instant::now();
                if now < until {
                    return true;
                } else {
                    entry.fatigue_until = None;
                    tracing::info!(
                        "☀️ [FatigueCooldown] Account {} finished fatigue cooldown, waking up refreshed",
                        account_id
                    );
                }
            }
        }
        false
    }

    /// 获取账号今日累计消耗的 Output Tokens (用于选号时最少使用优先调度)
    pub fn get_daily_output_tokens(&self, account_id: &str) -> u64 {
        let now_epoch_day = chrono::Utc::now().timestamp() / 86400;
        if let Some(entry) = self.account_states.get(account_id) {
            if entry.last_daily_window == now_epoch_day {
                return entry.daily_output_tokens;
            }
        }
        0
    }

    pub fn get_nap_remaining_seconds(&self, account_id: &str) -> Option<u64> {
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            if let Some(nap_until) = entry.nap_until {
                let now = std::time::Instant::now();
                if now < nap_until {
                    return Some(((nap_until - now).as_millis() as u64 + 999) / 1000);
                } else {
                    entry.nap_until = None;
                    entry.massive_heat = 0.0;
                    entry.turns_since_nap = 0;
                    entry.last_heat_updated_at = now;
                }
            }
        }
        None
    }

    fn decayed_heat_at(massive_heat: f64, last_updated: std::time::Instant, now: std::time::Instant) -> f64 {
        let elapsed_secs = now.saturating_duration_since(last_updated).as_secs_f64();
        let decayed = massive_heat * (0.5f64).powf(elapsed_secs / HEAT_HALF_LIFE_SECS);
        if decayed < 0.05 {
            0.0
        } else {
            decayed
        }
    }

    /// 计算当账号过热（current_heat + 1.5 >= 3.0，即 current_heat >= 1.5）时，衰减回 < 1.5 所需的秒数
    fn heat_recovery_secs(current_heat: f64) -> u64 {
        let target_heat = OVERSIZE_HEAT_CAP - OVERSIZE_HEAT_INCREMENT; // 1.5
        if current_heat < target_heat {
            return 0;
        }
        let secs = HEAT_HALF_LIFE_SECS * (current_heat / target_heat).log2();
        (secs.ceil() as u64).clamp(5, 600)
    }

    /// 清理超过 600s 的超大请求历史，并检查是否已满 2 个（若满 2 个则返回距离最早 1 个滑出窗口的秒数）
    fn clean_and_compute_oversize_window_wait_secs(
        entry: &mut AccountPacerState,
        now: std::time::Instant,
    ) -> Option<u64> {
        let window = Duration::from_secs(OVERSIZE_WINDOW_SECS);
        let cutoff = now.checked_sub(window).unwrap_or(now);
        while let Some(&front) = entry.oversized_history.front() {
            if front < cutoff {
                entry.oversized_history.pop_front();
            } else {
                break;
            }
        }
        if entry.oversized_history.len() >= MAX_OVERSIZE_IN_WINDOW {
            if let Some(&oldest) = entry.oversized_history.front() {
                let elapsed = now.saturating_duration_since(oldest).as_secs();
                return Some(OVERSIZE_WINDOW_SECS.saturating_sub(elapsed).clamp(5, OVERSIZE_WINDOW_SECS));
            }
            return Some(60);
        }
        None
    }

    /// 获取账号当前的大任务热度积分（按 180 秒半衰期指数衰减）
    pub fn get_account_heat(&self, account_id: &str) -> f64 {
        if let Some(entry) = self.account_states.get(account_id) {
            Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, std::time::Instant::now())
        } else {
            0.0
        }
    }

    /// 账号热度是否达到清凉池分界线 (Heat >= 1.5，整点敏感期 XX:05~XX:15 降为 1.0 提前避险)
    pub fn is_heat_saturated(&self, account_id: &str) -> bool {
        let current_heat = self.get_account_heat(account_id);
        use chrono::Timelike;
        let current_minute = chrono::Utc::now().minute();
        let threshold = if (5..=15).contains(&current_minute) {
            1.0
        } else {
            COLD_POOL_HEAT_THRESHOLD
        };
        current_heat >= threshold
    }

    /// 推演给定请求进入后账号的合成热度
    pub fn projected_heat_for_footprint(&self, account_id: &str, footprint: RequestFootprint) -> f64 {
        self.get_account_heat(account_id) + footprint.heat_increment()
    }

    /// 刷新已到期的冷却槽位，并动态对齐目标并发槽数 `target_slots`
    fn refresh_and_sync_slots(
        entry: &mut AccountPacerState,
        now: std::time::Instant,
        target_slots: usize,
    ) {
        let cap = target_slots.max(1);

        // 1. 将已过期的 CoolingDown 槽释放为 Free
        for slot in entry.slots.iter_mut() {
            if let SlotPhase::CoolingDown { until, .. } = slot {
                if now >= *until {
                    *slot = SlotPhase::Free;
                }
            }
        }

        // 检查当前是否处于大请求独占状态（Reserved / InFlight / CoolingDown 且 exclusive=true）
        let active_exclusive_cooldown: Option<std::time::Instant> =
            entry.slots.iter().find_map(|s| match s {
                SlotPhase::CoolingDown {
                    until,
                    exclusive: true,
                } if *until > now => Some(*until),
                _ => None,
            });
        let has_active_exclusive_inflight = entry.slots.iter().any(|s| {
            matches!(
                s,
                SlotPhase::Reserved {
                    exclusive: true,
                    ..
                } | SlotPhase::InFlight {
                    exclusive: true,
                    ..
                }
            )
        });

        // 2. 扩容槽位（若用户在页面调大了并发数）
        while entry.slots.len() < cap {
            if let Some(until) = active_exclusive_cooldown {
                entry.slots.push(SlotPhase::CoolingDown {
                    until,
                    exclusive: true,
                });
            } else if has_active_exclusive_inflight {
                // 大请求执行期间不开放额外新槽位，维持互斥
                break;
            } else {
                entry.slots.push(SlotPhase::Free);
            }
        }

        // 3. 缩容多余的 Free 槽位（若用户在页面调小了并发数，优先回收空闲槽，不破坏活跃租约）
        while entry.slots.len() > cap {
            if let Some(free_idx) = entry
                .slots
                .iter()
                .rposition(|s| matches!(s, SlotPhase::Free))
            {
                entry.slots.remove(free_idx);
            } else {
                break;
            }
        }
    }

    /// 计算滚动 60s RPM 所需等待毫秒数（单账号 60 秒上限 ACCOUNT_MAX_REQUESTS_PER_MINUTE 次，超限触发冷却换号）
    fn clean_and_compute_rpm_wait_ms(entry: &mut AccountPacerState, now: std::time::Instant) -> u64 {
        let cutoff = now.checked_sub(Duration::from_secs(60)).unwrap_or(now);
        while let Some(&front) = entry.rolling_requests.front() {
            if front < cutoff {
                entry.rolling_requests.pop_front();
            } else {
                break;
            }
        }

        if entry.rolling_requests.len() >= ACCOUNT_MAX_REQUESTS_PER_MINUTE {
            if let Some(&oldest) = entry.rolling_requests.front() {
                if now >= oldest {
                    let elapsed = now.duration_since(oldest).as_millis() as u64;
                    if elapsed < 60_000 {
                        return 60_000 - elapsed;
                    }
                } else {
                    // 含未来预约时间点
                    return (oldest - now).as_millis() as u64 + 60_000;
                }
            }
        }
        0
    }

    /// 评估当前槽位表对给定请求的准入状态（含大小请求互斥与全部槽位延迟计算）
    fn evaluate_slots(
        entry: &AccountPacerState,
        now: std::time::Instant,
        target_slots: usize,
        is_oversized: bool,
        rpm_wait_ms: u64,
    ) -> InternalSlotEval {
        let cap = target_slots.max(1);

        // 1. 只要存在正在执行或预约中的大请求独占锁，任何新请求一律互斥禁入
        let has_exclusive_running = entry.slots.iter().any(|s| {
            matches!(
                s,
                SlotPhase::Reserved {
                    exclusive: true,
                    ..
                } | SlotPhase::InFlight {
                    exclusive: true,
                    ..
                }
            )
        });
        if has_exclusive_running {
            return InternalSlotEval::Busy;
        }

        let active_count = entry
            .slots
            .iter()
            .filter(|s| matches!(s, SlotPhase::Reserved { .. } | SlotPhase::InFlight { .. }))
            .count();

        if is_oversized {
            // 大请求（>200K）独占全部槽位：只要有任意 1 个小请求在执行或预约，互斥禁入！
            if active_count > 0 {
                return InternalSlotEval::Busy;
            }
            // 所有槽均非 InFlight/Reserved，检查是否全部 Free 或需要等所有槽冷却彻底结束
            let mut max_cooldown_until: Option<std::time::Instant> = None;
            for slot in &entry.slots {
                if let SlotPhase::CoolingDown { until, .. } = slot {
                    if *until > now {
                        max_cooldown_until = Some(match max_cooldown_until {
                            Some(cur_max) => cur_max.max(*until),
                            None => *until,
                        });
                    }
                }
            }
            // 同时兼容旧接口直接写入的 next_allowed_request_at
            if entry.next_allowed_request_at > now {
                max_cooldown_until = Some(match max_cooldown_until {
                    Some(cur_max) => cur_max.max(entry.next_allowed_request_at),
                    None => entry.next_allowed_request_at,
                });
            }

            let slot_wait = max_cooldown_until
                .map(|u| u.saturating_duration_since(now).as_millis() as u64)
                .unwrap_or(0);
            let total_wait = slot_wait.max(rpm_wait_ms);
            if total_wait == 0 {
                InternalSlotEval::Ready {
                    free_slots: cap,
                    total_slots: cap,
                }
            } else {
                InternalSlotEval::NeedWait {
                    wait_ms: total_wait,
                    total_slots: cap,
                }
            }
        } else {
            // 普通/中大请求（占用 1 个槽）：
            // 若活跃槽已达并发上限，直接满载禁入
            if active_count >= cap {
                return InternalSlotEval::Busy;
            }

            // 若存在刚结束的大请求独占冷却，所有槽都在等该独占冷却走完
            if let Some(excl_until) = entry.slots.iter().find_map(|s| match s {
                SlotPhase::CoolingDown {
                    until,
                    exclusive: true,
                } if *until > now => Some(*until),
                _ => None,
            }) {
                let wait = excl_until
                    .saturating_duration_since(now)
                    .as_millis() as u64;
                return InternalSlotEval::NeedWait {
                    wait_ms: wait.max(rpm_wait_ms),
                    total_slots: cap,
                };
            }

            let free_slots = entry
                .slots
                .iter()
                .take(cap)
                .filter(|s| matches!(s, SlotPhase::Free))
                .count();

            // 兼容直接调用 mark_request_completed 的单槽旧测试路径
            let legacy_wait = if entry.next_allowed_request_at > now && entry.slots.iter().all(|s| matches!(s, SlotPhase::Free)) {
                entry.next_allowed_request_at.saturating_duration_since(now).as_millis() as u64
            } else {
                0
            };

            if free_slots > 0 && legacy_wait == 0 {
                if rpm_wait_ms == 0 {
                    InternalSlotEval::Ready {
                        free_slots,
                        total_slots: cap,
                    }
                } else {
                    InternalSlotEval::NeedWait {
                        wait_ms: rpm_wait_ms,
                        total_slots: cap,
                    }
                }
            } else if free_slots > 0 && legacy_wait > 0 {
                InternalSlotEval::NeedWait {
                    wait_ms: legacy_wait.max(rpm_wait_ms),
                    total_slots: cap,
                }
            } else {
                // 所有未执行的槽均处于 CoolingDown 延迟期：找出最早打开的那个槽
                let min_until = entry
                    .slots
                    .iter()
                    .take(cap)
                    .filter_map(|s| match s {
                        SlotPhase::CoolingDown { until, .. } if *until > now => Some(*until),
                        _ => None,
                    })
                    .min();

                match min_until {
                    Some(u) => {
                        let wait = u.saturating_duration_since(now).as_millis() as u64;
                        InternalSlotEval::NeedWait {
                            wait_ms: wait.max(rpm_wait_ms),
                            total_slots: cap,
                        }
                    }
                    None => InternalSlotEval::Busy,
                }
            }
        }
    }

    /// 记录账号刚完成一次 OAuth Access Token 刷新（开启 60s 新 Token 保护期，期间禁止直接承接 >200K 超大请求）
    pub fn note_oauth_refreshed(&self, account_id: &str) {
        let now = std::time::Instant::now();
        let mut entry = self
            .account_states
            .entry(account_id.to_string())
            .or_insert_with(|| AccountPacerState::new_default(account_id, now, 1));
        entry.last_oauth_refresh_at = Some(now);
    }

    /// 检查进程级冷启动、专属代理恢复期与新 OAuth Token 保护期对请求的限制：
    /// - 进程冷启动前 60s：拦截 Medium 与 Oversized 请求突刺；
    /// - 专属代理从 RED 恢复 GREEN：
    ///   1. 前 60s（静默期）：拦截所有流量（Normal / Medium / Oversized），确保代理网络彻底稳定后再发包；
    ///   2. 60..180s（预热期）：放行 Normal 轻量流量热身，拦截 Medium 和 Oversized 请求；
    /// - OAuth Token 刚刷新后前 60s：放行 Normal 流量，拦截 Medium（>50K/200KB）与 Oversized（>100K/400KB）超大请求。
    pub fn check_global_or_egress_hold(
        &self,
        account_id: &str,
        footprint: RequestFootprint,
    ) -> Option<LeaseRejectReason> {
        if footprint.is_medium_or_oversized() {
            let uptime_secs = self.started_at.elapsed().as_secs();
            if uptime_secs < COLD_START_WARMUP_SECS {
                let remain = (COLD_START_WARMUP_SECS - uptime_secs).max(5);
                return Some(LeaseRejectReason::ColdStartWarmup {
                    retry_after_secs: remain,
                });
            }

            if let Some(entry) = self.account_states.get(account_id) {
                if let Some(refreshed_at) = entry.last_oauth_refresh_at {
                    let elapsed = refreshed_at.elapsed().as_secs();
                    let is_cold = crate::proxy::account_selector::get_capacity_tier(account_id)
                        == crate::proxy::account_selector::CapacityTier::Tier1Cold;
                    if is_cold && elapsed < TOKEN_REFRESH_WARMUP_OVERSIZE_SECS {
                        let remain = (TOKEN_REFRESH_WARMUP_OVERSIZE_SECS - elapsed).max(5);
                        return Some(LeaseRejectReason::TokenRefreshWarmup {
                            retry_after_secs: remain,
                        });
                    }
                }
            }
        }

        if let Some(pool) = crate::proxy::proxy_pool::get_global_proxy_pool() {
            if let Some(elapsed) = pool.dedicated_recovery_elapsed_secs(account_id) {
                if let Some(wait) = remaining_recovery_hold_secs_for_tier(elapsed, footprint.tier) {
                    return Some(LeaseRejectReason::EgressRecovery {
                        retry_after_secs: wait,
                    });
                }
            }
        }

        None
    }

    /// 前置无副作用探测（供 P2C 选号器在全部候选账号上做准入过滤与梯队划分）
    pub fn probe_account(
        &self,
        account_id: &str,
        footprint: RequestFootprint,
        max_slots: usize,
    ) -> AccountProbe {
        if let Some(hold) = self.check_global_or_egress_hold(account_id, footprint) {
            return AccountProbe::Rejected(hold);
        }

        let now = std::time::Instant::now();
        let cap = max_slots.max(1);

        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            // 1. 小憩门禁（休眠期内全类型请求禁入）
            if let Some(nap) = entry.nap_until {
                if now < nap {
                    let rem = ((nap - now).as_millis() as u64 + 999) / 1000;
                    return AccountProbe::Rejected(LeaseRejectReason::InNap {
                        remaining_secs: rem.max(1),
                    });
                } else {
                    entry.nap_until = None;
                    entry.massive_heat = 0.0;
                    entry.turns_since_nap = 0;
                    entry.last_heat_updated_at = now;
                }
            }

            // 2. 热度与 10 分钟大包窗口双重硬门禁（>200K 超大请求推演 >= 3.0 或 600s 内已满 2 个即拦截，严格限制 < 3 个）
            let current_heat = Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, now);
            if footprint.is_oversized() {
                let projected = current_heat + footprint.heat_increment();
                let window_wait = Self::clean_and_compute_oversize_window_wait_secs(&mut entry, now);
                if projected >= OVERSIZE_HEAT_CAP || window_wait.is_some() {
                    let retry_secs = Self::heat_recovery_secs(current_heat)
                        .max(window_wait.unwrap_or(0))
                        .max(5);
                    return AccountProbe::Rejected(LeaseRejectReason::Overheated {
                        current_heat,
                        projected_heat: projected.max(OVERSIZE_HEAT_CAP),
                        retry_after_secs: retry_secs,
                    });
                }
            }

            // 3. 并发槽位与延迟计算
            Self::refresh_and_sync_slots(&mut entry, now, cap);
            let rpm_wait_ms = Self::clean_and_compute_rpm_wait_ms(&mut entry, now);
            match Self::evaluate_slots(&entry, now, cap, footprint.is_oversized(), rpm_wait_ms) {
                InternalSlotEval::Ready {
                    free_slots,
                    total_slots,
                } => AccountProbe::Admissible {
                    wait_ms: 0,
                    current_heat,
                    free_slots,
                    total_slots,
                },
                InternalSlotEval::NeedWait {
                    wait_ms,
                    total_slots,
                } => {
                    if wait_ms <= STICKY_MAX_SLOT_WAIT_MS {
                        AccountProbe::Admissible {
                            wait_ms,
                            current_heat,
                            free_slots: 0,
                            total_slots,
                        }
                    } else {
                        AccountProbe::Rejected(LeaseRejectReason::CooldownTooLong { wait_ms })
                    }
                }
                InternalSlotEval::Busy => AccountProbe::Rejected(LeaseRejectReason::SlotsBusy),
            }
        } else {
            // 全新冷账号：全部槽位空闲打开、零热度、零延迟
            AccountProbe::Admissible {
                wait_ms: 0,
                current_heat: 0.0,
                free_slots: cap,
                total_slots: cap,
            }
        }
    }

    /// 原子化前置准入与槽位租约获取：
    /// - 若通过准入且 `wait_ms <= max_wait_ms`，当场原子锁定槽位（`InFlight` 或 `Reserved`）、预占热度并推进轮次计数；
    /// - 返回 `Ok((SlotLeaseGuard, entered_nap_for_future))`；
    /// - 若不满足准入，返回 `Err(LeaseRejectReason)`，不产生任何状态污染。
    pub fn try_acquire_lease(
        self: & std::sync::Arc<Self>,
        account_id: &str,
        footprint: RequestFootprint,
        max_slots: usize,
        max_wait_ms: u64,
    ) -> Result<(SlotLeaseGuard, bool), LeaseRejectReason> {
        use rand::Rng;

        if let Some(hold) = self.check_global_or_egress_hold(account_id, footprint) {
            return Err(hold);
        }

        let now = std::time::Instant::now();
        let cap = max_slots.max(1);
        let mut entry = self
            .account_states
            .entry(account_id.to_string())
            .or_insert_with(|| AccountPacerState::new_default(account_id, now, cap));

        // 1. 小憩硬门禁
        if let Some(nap) = entry.nap_until {
            if now < nap {
                let rem = ((nap - now).as_millis() as u64 + 999) / 1000;
                return Err(LeaseRejectReason::InNap {
                    remaining_secs: rem.max(1),
                });
            } else {
                entry.nap_until = None;
                entry.massive_heat = 0.0;
                entry.turns_since_nap = 0;
                entry.last_heat_updated_at = now;
            }
        }

        // 2. 大请求过热与 10 分钟窗口双重前置硬门禁（进门前限制第 3 个大包）
        let current_heat = Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, now);
        let added_heat = footprint.heat_increment();
        if footprint.is_oversized() {
            let projected = current_heat + added_heat;
            let window_wait = Self::clean_and_compute_oversize_window_wait_secs(&mut entry, now);
            if projected >= OVERSIZE_HEAT_CAP || window_wait.is_some() {
                let retry_secs = Self::heat_recovery_secs(current_heat)
                    .max(window_wait.unwrap_or(0))
                    .max(5);
                return Err(LeaseRejectReason::Overheated {
                    current_heat,
                    projected_heat: projected.max(OVERSIZE_HEAT_CAP),
                    retry_after_secs: retry_secs,
                });
            }
        }

        // 3. 槽位同步与准入评估
        Self::refresh_and_sync_slots(&mut entry, now, cap);
        let rpm_wait_ms = Self::clean_and_compute_rpm_wait_ms(&mut entry, now);
        let wait_ms = match Self::evaluate_slots(&entry, now, cap, footprint.is_oversized(), rpm_wait_ms) {
            InternalSlotEval::Ready { .. } => 0,
            InternalSlotEval::NeedWait { wait_ms, .. } => {
                if wait_ms > max_wait_ms {
                    return Err(LeaseRejectReason::CooldownTooLong { wait_ms });
                }
                wait_ms
            }
            InternalSlotEval::Busy => return Err(LeaseRejectReason::SlotsBusy),
        };

        // 4. 通过全部门禁！原子分配租约 ID 并锁定槽位
        let lease_id = self
            .next_lease_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ready_at = now + Duration::from_millis(wait_ms);
        let is_exclusive = footprint.is_oversized();

        if is_exclusive {
            // 超大请求独占锁死全部槽位
            while entry.slots.len() < cap {
                entry.slots.push(SlotPhase::Free);
            }
            for slot in entry.slots.iter_mut() {
                let prev_cd = match *slot {
                    SlotPhase::CoolingDown { until, exclusive } if until > now => {
                        Some((until, exclusive))
                    }
                    _ => None,
                };
                if wait_ms == 0 {
                    *slot = SlotPhase::InFlight {
                        lease_id,
                        started_at: now,
                        exclusive: true,
                    };
                } else {
                    *slot = SlotPhase::Reserved {
                        lease_id,
                        ready_at,
                        restore_cooldown: prev_cd,
                        exclusive: true,
                    };
                }
            }
        } else {
            // 普通/中大请求锁定 1 个槽位（优先挑 Free 槽，否则挑最早结束冷却的 CoolingDown 槽）
            let target_idx = if let Some(idx) = entry
                .slots
                .iter()
                .take(cap)
                .position(|s| matches!(s, SlotPhase::Free))
            {
                idx
            } else {
                entry
                    .slots
                    .iter()
                    .take(cap)
                    .enumerate()
                    .filter_map(|(idx, s)| match s {
                        SlotPhase::CoolingDown { until, .. } => Some((idx, *until)),
                        _ => None,
                    })
                    .min_by_key(|(_, u)| *u)
                    .map(|(idx, _)| idx)
                    .ok_or(LeaseRejectReason::SlotsBusy)?
            };

            let prev_cd = match entry.slots[target_idx] {
                SlotPhase::CoolingDown { until, exclusive } if until > now => {
                    Some((until, exclusive))
                }
                _ => None,
            };
            entry.slots[target_idx] = if wait_ms == 0 {
                SlotPhase::InFlight {
                    lease_id,
                    started_at: now,
                    exclusive: false,
                }
            } else {
                SlotPhase::Reserved {
                    lease_id,
                    ready_at,
                    restore_cooldown: prev_cd,
                    exclusive: false,
                }
            };
        }

        // 5. 预占热度、记录滚动 RPM / 10 分钟大包历史与推进 20~30 轮微小憩计数
        entry.massive_heat = current_heat + added_heat;
        entry.last_heat_updated_at = now;
        entry.last_request_time = ready_at;
        entry.request_counter += 1;
        entry.turns_since_nap += 1;
        entry.rolling_requests.push_back(ready_at);
        if footprint.is_oversized() {
            entry.oversized_history.push_back(ready_at);
        }

        if added_heat > 0.0 {
            tracing::info!(
                "🔥 [HumanizedPacer] Account {} heat pre-allocated: {:.2} (+{:.1} from {:?}, {} tokens, {}B)",
                account_id,
                entry.massive_heat,
                added_heat,
                footprint.tier,
                footprint.prompt_tokens,
                footprint.raw_body_len
            );
        }

        let mut entered_nap = false;
        let is_overheated_fatigued = entry.turns_since_nap >= 10 && entry.massive_heat >= 2.0;
        let is_turn_exhausted = entry.turns_since_nap >= entry.next_nap_threshold;
        if is_overheated_fatigued || is_turn_exhausted {
            let mut rng = rand::thread_rng();
            // 小憩时间 3 ~ 6 分钟 (180 ~ 360 秒)
            let nap_secs = rng.gen_range(180..=360);
            entry.nap_until = Some(ready_at + Duration::from_secs(nap_secs));
            entry.turns_since_nap = 0;
            entry.next_nap_threshold = rng.gen_range(15..=25);
            entered_nap = true;
            tracing::info!(
                "[HumanizedPacer] Account {} entering nap for {}s (reason: high_heat_fatigue={}, turns={}, heat={:.2})",
                account_id,
                nap_secs,
                is_overheated_fatigued,
                is_turn_exhausted,
                entry.massive_heat
            );
        }

        let reserved_tokens = crate::proxy::account_selector::DEFAULT_IN_FLIGHT_RESERVED_TOKENS;
        crate::proxy::account_selector::reserve_in_flight(account_id, reserved_tokens);

        let guard = SlotLeaseGuard {
            pacer: std::sync::Arc::clone(self),
            account_id: account_id.to_string(),
            lease_id,
            footprint,
            preallocated_heat: added_heat,
            recorded_at: ready_at,
            reserved_wait_ms: wait_ms,
            triggered_nap: entered_nap,
            upstream_accepted: false,
            finished: false,
            reserved_inflight_tokens: reserved_tokens,
        };

        Ok((guard, entered_nap))
    }

    fn promote_reserved_to_inflight(&self, account_id: &str, lease_id: u64) {
        let now = std::time::Instant::now();
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            for slot in entry.slots.iter_mut() {
                if let SlotPhase::Reserved {
                    lease_id: id,
                    exclusive,
                    ..
                } = *slot
                {
                    if id == lease_id {
                        *slot = SlotPhase::InFlight {
                            lease_id,
                            started_at: now,
                            exclusive,
                        };
                    }
                }
            }
        }
    }

    fn complete_lease(&self, account_id: &str, lease_id: u64, footprint: RequestFootprint) {
        let now = std::time::Instant::now();
        let cooldown_ms = Self::compute_footprint_cooldown_ms(footprint);
        let until = now + Duration::from_millis(cooldown_ms);

        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            let decayed = Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, now);
            entry.massive_heat = decayed;
            entry.last_heat_updated_at = now;
            entry.last_response_completed_at = Some(now);
            entry.current_interval_ms = cooldown_ms;

            let mut matched_any = false;
            for slot in entry.slots.iter_mut() {
                match *slot {
                    SlotPhase::InFlight {
                        lease_id: id,
                        exclusive,
                        ..
                    }
                    | SlotPhase::Reserved {
                        lease_id: id,
                        exclusive,
                        ..
                    } if id == lease_id => {
                        *slot = SlotPhase::CoolingDown { until, exclusive };
                        matched_any = true;
                    }
                    _ => {}
                }
            }

            if matched_any {
                tracing::debug!(
                    "[HumanizedPacer] Account {} lease #{} completed ({:?}), slot closed for +{}ms cooldown",
                    account_id,
                    lease_id,
                    footprint.tier,
                    cooldown_ms
                );
            }
        }
    }

    fn rollback_lease(
        &self,
        account_id: &str,
        lease_id: u64,
        preallocated_heat: f64,
        recorded_at: std::time::Instant,
        triggered_nap: bool,
    ) {
        let now = std::time::Instant::now();
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            // 1. 还原槽位状态（回退到预约前的剩余冷却或直接 Free）
            for slot in entry.slots.iter_mut() {
                match *slot {
                    SlotPhase::Reserved {
                        lease_id: id,
                        restore_cooldown,
                        ..
                    } if id == lease_id => {
                        if let Some((u, prev_excl)) = restore_cooldown {
                            if u > now {
                                *slot = SlotPhase::CoolingDown {
                                    until: u,
                                    exclusive: prev_excl,
                                };
                            } else {
                                *slot = SlotPhase::Free;
                            }
                        } else {
                            *slot = SlotPhase::Free;
                        }
                    }
                    SlotPhase::InFlight { lease_id: id, .. } if id == lease_id => {
                        *slot = SlotPhase::Free;
                    }
                    _ => {}
                }
            }

            // 2. 退回预占但未实际消耗的热度与超大请求时间窗记录
            if preallocated_heat > 0.0 {
                let decayed =
                    Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, now);
                entry.massive_heat = (decayed - preallocated_heat).max(0.0);
                entry.last_heat_updated_at = now;
            }
            if preallocated_heat >= OVERSIZE_HEAT_INCREMENT {
                if let Some(pos) = entry
                    .oversized_history
                    .iter()
                    .rposition(|t| *t == recorded_at)
                {
                    entry.oversized_history.remove(pos);
                }
            }

            // 3. 回退未生效的轮次与滚动 RPM 时间戳
            if let Some(pos) = entry
                .rolling_requests
                .iter()
                .rposition(|t| *t == recorded_at)
            {
                entry.rolling_requests.remove(pos);
            }
            entry.request_counter = entry.request_counter.saturating_sub(1);
            if triggered_nap {
                entry.nap_until = None;
                entry.turns_since_nap = entry.next_nap_threshold.saturating_sub(1);
            } else {
                entry.turns_since_nap = entry.turns_since_nap.saturating_sub(1);
            }
        }
    }

    /// 检查账号距离下一次允许发起普通请求还需要等待多少毫秒（兼容状态查询与测试）
    pub fn get_required_wait_ms(&self, account_id: &str) -> u64 {
        let now = std::time::Instant::now();
        if let Some(mut entry) = self.account_states.get_mut(account_id) {
            let nap_wait = if let Some(nap) = entry.nap_until {
                if nap > now {
                    (nap - now).as_millis() as u64
                } else {
                    0
                }
            } else {
                0
            };

            let cap = entry.slots.len().max(1);
            Self::refresh_and_sync_slots(&mut entry, now, cap);
            let rpm_wait = Self::clean_and_compute_rpm_wait_ms(&mut entry, now);

            let slot_wait = match Self::evaluate_slots(&entry, now, cap, false, rpm_wait) {
                InternalSlotEval::Ready { .. } => 0,
                InternalSlotEval::NeedWait { wait_ms, .. } => wait_ms,
                InternalSlotEval::Busy => STICKY_MAX_SLOT_WAIT_MS + 1000,
            };

            return nap_wait.max(slot_wait).max(rpm_wait);
        }
        0
    }

    /// 记录账号发起了一次轻量请求（供非槽位租约路径兼容使用）
    pub fn mark_request_started(&self, account_id: &str) {
        use rand::Rng;
        let now = std::time::Instant::now();
        let mut entry = self
            .account_states
            .entry(account_id.to_string())
            .or_insert_with(|| {
                AccountPacerState::new_default(account_id, now, DEFAULT_CONCURRENCY_SLOTS)
            });

        entry.last_request_time = now;
        entry.request_counter += 1;
        entry.turns_since_nap += 1;
        entry.rolling_requests.push_back(now);

        let is_overheated_fatigued = entry.turns_since_nap >= 10 && entry.massive_heat >= 2.0;
        let is_turn_exhausted = entry.turns_since_nap >= entry.next_nap_threshold;
        if is_overheated_fatigued || is_turn_exhausted {
            let mut rng = rand::thread_rng();
            // 小憩时间 3 ~ 6 分钟 (180 ~ 360 秒)
            let nap_secs = rng.gen_range(180..=360);
            entry.nap_until = Some(now + Duration::from_secs(nap_secs));
            entry.turns_since_nap = 0;
            entry.next_nap_threshold = rng.gen_range(15..=25);
            tracing::info!(
                "[HumanizedPacer] Account {} entering nap for {}s (reason: high_heat_fatigue={}, turns={})",
                account_id,
                nap_secs,
                is_overheated_fatigued,
                is_turn_exhausted
            );
        }
    }

    /// 兼容旧路径：当响应完成时记录冷却截止时间
    pub fn mark_request_completed(&self, account_id: &str, prompt_tokens: Option<u32>) {
        let now = std::time::Instant::now();
        let cooldown_ms = Self::compute_cooldown_ms(prompt_tokens);
        let next_allowed = now + Duration::from_millis(cooldown_ms);

        let mut entry = self
            .account_states
            .entry(account_id.to_string())
            .or_insert_with(|| {
                AccountPacerState::new_default(account_id, now, DEFAULT_CONCURRENCY_SLOTS)
            });

        let decayed = Self::decayed_heat_at(entry.massive_heat, entry.last_heat_updated_at, now);
        entry.massive_heat = decayed;
        entry.last_heat_updated_at = now;
        entry.last_response_completed_at = Some(now);
        entry.next_allowed_request_at = next_allowed;
        entry.current_interval_ms = cooldown_ms;
    }
}

/// 并发槽位与热度生命周期租约守卫（RAII）：
/// - 选号通过瞬间即锁定槽位与预占热度；
/// - 若上游返回 200 OK（`mark_upstream_accepted`），则在流结束或客户端断连（Drop）时自动将槽位转入 `CoolingDown(实际结束时刻 + 计算延迟)`；
/// - 若未达上游即失败重试（`upstream_accepted == false`），Drop 或 `abort_and_rollback` 时自动退回预占热度并释放槽位。
pub struct SlotLeaseGuard {
    pacer: std::sync::Arc<HumanizedPacer>,
    account_id: String,
    lease_id: u64,
    footprint: RequestFootprint,
    preallocated_heat: f64,
    recorded_at: std::time::Instant,
    reserved_wait_ms: u64,
    triggered_nap: bool,
    upstream_accepted: bool,
    finished: bool,
    reserved_inflight_tokens: u64,
}

impl SlotLeaseGuard {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn reserved_wait_ms(&self) -> u64 {
        self.reserved_wait_ms
    }

    /// 若租约通过预约微等（如粘性会话 <= 2s 槽位冷却）获得，异步等待槽位冷却到期并正式转为 InFlight
    pub async fn wait_until_ready(&mut self) {
        if self.reserved_wait_ms > 0 {
            let sleep_ms = self.reserved_wait_ms;
            self.reserved_wait_ms = 0;
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            self.pacer
                .promote_reserved_to_inflight(&self.account_id, self.lease_id);
        }
    }

    /// 标记上游已成功受理请求（HTTP 200），此后无论正常结束或客户端中途断流均走完整冷却延迟
    pub fn mark_upstream_accepted(&mut self) {
        self.upstream_accepted = true;
    }

    /// 响应正常完成：将槽位从 InFlight 转入 CoolingDown（持续闭合直至计算延迟走完）
    pub fn complete(&mut self) {
        if !self.finished {
            self.upstream_accepted = true;
            if self.reserved_inflight_tokens > 0 {
                crate::proxy::account_selector::release_in_flight(&self.account_id, self.reserved_inflight_tokens);
                self.reserved_inflight_tokens = 0;
            }
            crate::proxy::account_selector::record_warm_interaction(&self.account_id);
            self.pacer
                .complete_lease(&self.account_id, self.lease_id, self.footprint);
            self.finished = true;
        }
    }

    /// 请求未达上游或瞬时失败需要切号重试：退回预占热度与轮次，立即释放槽位
    pub fn abort_and_rollback(&mut self) {
        if !self.finished {
            if self.reserved_inflight_tokens > 0 {
                crate::proxy::account_selector::release_in_flight(&self.account_id, self.reserved_inflight_tokens);
                self.reserved_inflight_tokens = 0;
            }
            self.pacer.rollback_lease(
                &self.account_id,
                self.lease_id,
                self.preallocated_heat,
                self.recorded_at,
                self.triggered_nap,
            );
            self.finished = true;
        }
    }
}

impl Drop for SlotLeaseGuard {
    fn drop(&mut self) {
        if !self.finished {
            if self.upstream_accepted {
                self.complete();
            } else {
                self.abort_and_rollback();
            }
        }
    }
}

/// 兼容旧接口的请求生命周期看门狗
pub struct RequestCompletionGuard {
    pacer: std::sync::Arc<HumanizedPacer>,
    account_id: String,
    prompt_tokens: Option<u32>,
    completed: bool,
}

impl RequestCompletionGuard {
    pub fn new(
        pacer: std::sync::Arc<HumanizedPacer>,
        account_id: String,
        prompt_tokens: Option<u32>,
    ) -> Self {
        Self {
            pacer,
            account_id,
            prompt_tokens,
            completed: false,
        }
    }

    pub fn complete(&mut self) {
        if !self.completed {
            self.pacer
                .mark_request_completed(&self.account_id, self.prompt_tokens);
            self.completed = true;
        }
    }
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        self.complete();
    }
}

/// 限流跟踪器
pub struct RateLimitTracker {
    limits: DashMap<String, RateLimitInfo>,
    /// 连续失败计数（用于智能指数退避），带时间戳用于自动过期
    failure_counts: DashMap<String, (u32, SystemTime)>,
}

impl RateLimitTracker {
    pub fn new() -> Self {
        Self {
            limits: DashMap::new(),
            failure_counts: DashMap::new(),
        }
    }

    /// 生成限流 Key
    /// - 账号级: "account_id"
    /// - 模型级: "account_id:model_id"
    fn get_limit_key(&self, account_id: &str, model: Option<&str>) -> String {
        match model {
            Some(m) if !m.is_empty() => format!("{}:{}", account_id, m),
            _ => account_id.to_string(),
        }
    }

    /// 获取账号剩余的等待时间(秒)
    /// 支持检查账号级和模型级锁
    pub fn get_remaining_wait(&self, account_id: &str, model: Option<&str>) -> u64 {
        let now = SystemTime::now();

        // 1. 检查全局账号锁
        if let Some(info) = self.limits.get(account_id) {
            if info.reset_time > now {
                return info
                    .reset_time
                    .duration_since(now)
                    .unwrap_or(Duration::from_secs(0))
                    .as_secs();
            }
        }

        // 2. 如果指定了模型，检查模型级锁
        if let Some(m) = model {
            let key = self.get_limit_key(account_id, Some(m));
            if let Some(info) = self.limits.get(&key) {
                if info.reset_time > now {
                    return info
                        .reset_time
                        .duration_since(now)
                        .unwrap_or(Duration::from_secs(0))
                        .as_secs();
                }
            }
        }

        0
    }

    /// 标记账号请求成功，重置连续失败计数
    ///
    /// 当账号成功完成请求后调用此方法，将其失败计数归零，
    /// 这样下次失败时会从最短的锁定时间（60秒）开始。
    pub fn mark_success(&self, account_id: &str) {
        if self.failure_counts.remove(account_id).is_some() {
            tracing::debug!("账号 {} 请求成功，已重置失败计数", account_id);
        }
        // 清除账号级限流
        self.limits.remove(account_id);
        // 注意：我们暂时无法清除该账号下的所有模型级锁，因为我们不知道哪些模型被锁了
        // 除非遍历 limits。考虑到模型级锁通常是 QuotaExhausted，让其自然过期也是可以接受的。
        // 或者我们可以引入索引，但为了简单，暂时只清除 Account 级锁。
    }

    /// 精确锁定账号到指定时间点
    ///
    /// 使用账号配额中的 reset_time 来精确锁定账号,
    /// 这比指数退避更加精准。
    ///
    /// # 参数
    /// - `model`: 可选的模型名称,用于模型级别限流。None 表示账号级别限流
    pub fn set_lockout_until(
        &self,
        account_id: &str,
        reset_time: SystemTime,
        reason: RateLimitReason,
        model: Option<String>,
    ) {
        let now = SystemTime::now();
        let mut retry_sec = reset_time
            .duration_since(now)
            .map(|d| d.as_secs())
            .unwrap_or(60); // 如果时间已过,使用默认 60 秒

        if retry_sec > 300 {
            tracing::info!(
                "Capping lockout time for {} from {}s to 300s (5 minutes)",
                account_id,
                retry_sec
            );
            retry_sec = 300;
        }

        let reset_time = now + Duration::from_secs(retry_sec);

        let info = RateLimitInfo {
            reset_time,
            retry_after_sec: retry_sec,
            detected_at: now,
            reason,
            model: model.clone(), // 🆕 支持模型级别限流
        };

        let key = self.get_limit_key(account_id, model.as_deref());
        self.limits.insert(key, info);

        if let Some(m) = &model {
            tracing::info!(
                "账号 {} 的模型 {} 已精确锁定到配额刷新时间,剩余 {} 秒",
                account_id,
                m,
                retry_sec
            );
        } else {
            tracing::info!(
                "账号 {} 已精确锁定到配额刷新时间,剩余 {} 秒",
                account_id,
                retry_sec
            );
        }
    }

    /// 使用 ISO 8601 时间字符串精确锁定账号
    ///
    /// 解析类似 "2026-01-08T17:00:00Z" 格式的时间字符串
    ///
    /// # 参数
    /// - `model`: 可选的模型名称,用于模型级别限流
    pub fn set_lockout_until_iso(
        &self,
        account_id: &str,
        reset_time_str: &str,
        reason: RateLimitReason,
        model: Option<String>,
    ) -> bool {
        // 尝试解析 ISO 8601 格式
        match chrono::DateTime::parse_from_rfc3339(reset_time_str) {
            Ok(dt) => {
                let reset_time =
                    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64);
                self.set_lockout_until(account_id, reset_time, reason, model);
                true
            }
            Err(e) => {
                tracing::warn!(
                    "无法解析配额刷新时间 '{}': {},将使用默认退避策略",
                    reset_time_str,
                    e
                );
                false
            }
        }
    }

    /// 从错误响应解析限流信息
    ///
    /// # Arguments
    /// * `account_id` - 账号 ID
    /// * `status` - HTTP 状态码
    /// * `retry_after_header` - Retry-After header 值
    /// * `body` - 错误响应 body
    pub fn parse_from_error(
        &self,
        account_id: &str,
        status: u16,
        retry_after_header: Option<&str>,
        body: &str,
        model: Option<String>,
        backoff_steps: &[u64], // [NEW] 传入退避配置
    ) -> Option<RateLimitInfo> {
        // 支持 429 (限流) 以及 500/503/529 (后端故障软避让)
        if status != 429 && status != 500 && status != 503 && status != 529 && status != 404 {
            return None;
        }

        // 1. 解析限流原因类型
        let reason = if status == 429 {
            tracing::warn!("Google 429 Error Body: {}", body);
            self.parse_rate_limit_reason(body)
        } else if status == 404 {
            tracing::warn!(
                "Google 404: model unavailable on this account, short lockout before rotation"
            );
            RateLimitReason::ServerError
        } else {
            RateLimitReason::ServerError
        };

        let mut retry_after_sec = None;

        // 2. 从 Retry-After header 提取
        if let Some(retry_after) = retry_after_header {
            if let Ok(seconds) = retry_after.parse::<u64>() {
                retry_after_sec = Some(seconds);
            }
        }

        // 3. 从错误消息提取 (优先尝试 JSON 解析，再试正则)
        if retry_after_sec.is_none() {
            retry_after_sec = self.parse_retry_time_from_body(body);
        }

        // 4. 处理默认值与软避让逻辑（根据限流类型设置不同默认值）
        let retry_sec = match retry_after_sec {
            Some(s) => {
                // 设置安全缓冲区：最小 2 秒，防止极高频无效重试
                if s < 2 {
                    2
                } else {
                    s
                }
            }
            None => {
                // 获取连续失败次数，用于指数退避（带自动过期逻辑）
                // [FIX] ServerError (5xx) 不累加 failure_count，避免污染 429 的退避阶梯
                let failure_count = if reason != RateLimitReason::ServerError {
                    // 只有非 ServerError 才累加失败计数（用于指数退避）
                    let now = SystemTime::now();
                    // 这里我们使用 account_id 作为 key，不区分模型，
                    // 因为这里是为了计算连续"账号级"问题的退避。
                    // 如果需要针对模型的连续失败计数，可能需要改变 failure_counts 的 key。
                    // 暂时保持 account_id，这样如果一个模型一直挂，也会增加计数，符合逻辑。
                    let mut entry = self
                        .failure_counts
                        .entry(account_id.to_string())
                        .or_insert((0, now));

                    let elapsed = now
                        .duration_since(entry.1)
                        .unwrap_or(Duration::from_secs(0))
                        .as_secs();
                    if elapsed > FAILURE_COUNT_EXPIRY_SECONDS {
                        tracing::debug!(
                            "账号 {} 失败计数已过期（{}秒），重置为 0",
                            account_id,
                            elapsed
                        );
                        *entry = (0, now);
                    }
                    entry.0 += 1;
                    entry.1 = now;
                    entry.0
                } else {
                    // ServerError (5xx) 使用固定值 1，不累加，避免污染 429 的退避阶梯
                    1
                };

                match reason {
                    RateLimitReason::QuotaExhausted => {
                        // [智能限流] 根据 failure_count 和配置的 backoff_steps 计算
                        let index = (failure_count as usize).saturating_sub(1);
                        let lockout = if index < backoff_steps.len() {
                            backoff_steps[index]
                        } else {
                            *backoff_steps.last().unwrap_or(&7200)
                        };

                        tracing::warn!(
                            "检测到配额耗尽 (QUOTA_EXHAUSTED)，第{}次连续失败，根据配置锁定 {} 秒",
                            failure_count,
                            lockout
                        );
                        lockout
                    }
                    RateLimitReason::RateLimitExceeded => {
                        // 速率限制 (TPM/RPM)
                        let body_lower = body.to_lowercase();
                        let lockout = if body_lower.contains("resource has been exhausted")
                            || body_lower.contains("resource_exhausted")
                        {
                            30
                        } else {
                            5
                        };
                        tracing::debug!(
                            "检测到速率限制 (RATE_LIMIT_EXCEEDED)，使用默认值 {}秒",
                            lockout
                        );
                        lockout
                    }
                    RateLimitReason::ModelCapacityExhausted => {
                        // 模型容量耗尽
                        let lockout = match failure_count {
                            1 => 5,
                            2 => 10,
                            _ => 15,
                        };
                        tracing::warn!(
                            "检测到模型容量不足 (MODEL_CAPACITY_EXHAUSTED)，第{}次失败，{}秒后重试",
                            failure_count,
                            lockout
                        );
                        lockout
                    }
                    RateLimitReason::ServerError => {
                        let lockout = if status == 404 { 5 } else { 8 };
                        tracing::warn!("检测到 {} 错误, 执行 {}s 软避让...", status, lockout);
                        lockout
                    }
                    RateLimitReason::Unknown => {
                        // 未知原因
                        tracing::debug!("无法解析 429 限流原因, 使用默认值 60秒");
                        60
                    }
                }
            }
        };

        let mut retry_sec = retry_sec;
        if retry_sec > 300 {
            tracing::info!(
                "Capping retry lockout time for {} from {}s to 300s (5 minutes)",
                account_id,
                retry_sec
            );
            retry_sec = 300;
        }

        let info = RateLimitInfo {
            reset_time: SystemTime::now() + Duration::from_secs(retry_sec),
            retry_after_sec: retry_sec,
            detected_at: SystemTime::now(),
            reason,
            model: model.clone(),
        };

        // [FIX] 使用复合 Key 存储 (如果是 Quota 且有 Model)
        // 只有 QuotaExhausted 适合做模型隔离，其他如 RateLimitExceeded 通常是全账号的 TPM
        let use_model_key = matches!(reason, RateLimitReason::QuotaExhausted) && model.is_some();
        let key = if use_model_key {
            self.get_limit_key(account_id, model.as_deref())
        } else {
            // 其他情况（如 RateLimitExceeded, ServerError）通常影响整个账号
            // 或者我们也可以根据配置决定是否隔离。
            // 简单起见，只有 QuotaExhausted 做细粒度隔离。
            account_id.to_string()
        };

        self.limits.insert(key, info.clone());

        tracing::warn!(
            "账号 {} [{}] 限流类型: {:?}, 重置延时: {}秒",
            account_id,
            status,
            reason,
            retry_sec
        );

        Some(info)
    }

    /// 解析限流原因类型
    fn parse_rate_limit_reason(&self, body: &str) -> RateLimitReason {
        // 尝试从 JSON 中提取 reason 字段
        let trimmed = body.trim();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(reason_str) = json
                    .get("error")
                    .and_then(|e| e.get("details"))
                    .and_then(|d| d.as_array())
                    .and_then(|a| a.get(0))
                    .and_then(|o| o.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    return match reason_str {
                        "QUOTA_EXHAUSTED" => RateLimitReason::QuotaExhausted,
                        "RATE_LIMIT_EXCEEDED" => RateLimitReason::RateLimitExceeded,
                        "MODEL_CAPACITY_EXHAUSTED" => RateLimitReason::ModelCapacityExhausted,
                        _ => RateLimitReason::Unknown,
                    };
                }
                // [NEW] 尝试从 message 字段进行文本匹配（防止 missed reason）
                if let Some(msg) = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                {
                    let msg_lower = msg.to_lowercase();
                    if msg_lower.contains("per minute") || msg_lower.contains("rate limit") {
                        return RateLimitReason::RateLimitExceeded;
                    }
                }
            }
        }

        // 如果无法从 JSON 解析，尝试从消息文本判断
        let body_lower = body.to_lowercase();
        // [FIX] 优先判断分钟级限制，避免将 TPM 误判为 Quota
        let generic_resource_exhausted = body_lower.contains("resource has been exhausted")
            || body_lower.contains("resource_exhausted");
        let explicit_quota_exhausted = body_lower.contains("quota_exhausted")
            || body_lower.contains("quotaresetdelay")
            || body_lower.contains("quota reset")
            || body_lower.contains("quota limit")
            || body_lower.contains("per day")
            || body_lower.contains("daily quota");

        if body_lower.contains("per minute")
            || body_lower.contains("rate limit")
            || body_lower.contains("too many requests")
            || (generic_resource_exhausted && !explicit_quota_exhausted)
        {
            RateLimitReason::RateLimitExceeded
        } else if body_lower.contains("exhausted") || body_lower.contains("quota") {
            RateLimitReason::QuotaExhausted
        } else {
            RateLimitReason::Unknown
        }
    }

    /// 通用时间解析函数：支持 "2h1m1s" 等所有格式组合
    fn parse_duration_string(&self, s: &str) -> Option<u64> {
        tracing::debug!("[时间解析] 尝试解析: '{}'", s);

        // 使用正则表达式提取小时、分钟、秒、毫秒
        // 支持格式："2h1m1s", "1h30m", "5m", "30s", "500ms", "510.790006ms" 等
        // 🔧 [FIX] 修改 ms 部分支持小数: (\d+)ms -> (\d+(?:\.\d+)?)ms
        let re = Regex::new(r"(?:(\d+)h)?(?:(\d+)m)?(?:(\d+(?:\.\d+)?)s)?(?:(\d+(?:\.\d+)?)ms)?")
            .ok()?;
        let caps = match re.captures(s) {
            Some(c) => c,
            None => {
                tracing::warn!("[时间解析] 正则未匹配: '{}'", s);
                return None;
            }
        };

        let hours = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        let minutes = caps
            .get(2)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        let seconds = caps
            .get(3)
            .and_then(|m| m.as_str().parse::<f64>().ok())
            .unwrap_or(0.0);
        // 🔧 [FIX] 毫秒也支持小数解析
        let milliseconds = caps
            .get(4)
            .and_then(|m| m.as_str().parse::<f64>().ok())
            .unwrap_or(0.0);

        tracing::debug!(
            "[时间解析] 提取结果: {}h {}m {:.3}s {:.3}ms",
            hours,
            minutes,
            seconds,
            milliseconds
        );

        // 🔧 [FIX] 计算总秒数，毫秒部分向上取整
        let total_seconds = hours * 3600
            + minutes * 60
            + seconds.ceil() as u64
            + (milliseconds / 1000.0).ceil() as u64;

        // 如果总秒数为 0，说明解析失败
        if total_seconds == 0 {
            tracing::warn!("[时间解析] 失败: '{}' (总秒数为0)", s);
            None
        } else {
            tracing::info!(
                "[时间解析] ✓ 成功: '{}' => {}秒 ({}h {}m {:.1}s {:.1}ms)",
                s,
                total_seconds,
                hours,
                minutes,
                seconds,
                milliseconds
            );
            Some(total_seconds)
        }
    }

    /// 从错误消息 body 中解析重置时间
    fn parse_retry_time_from_body(&self, body: &str) -> Option<u64> {
        // A. 优先尝试 JSON 精准解析
        let trimmed = body.trim();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                // 1. Google 常见的 quotaResetDelay 格式 (支持所有格式："2h1m1s", "1h30m", "42s", "500ms" 等)
                // 路径: error.details[0].metadata.quotaResetDelay
                if let Some(delay_str) = json
                    .get("error")
                    .and_then(|e| e.get("details"))
                    .and_then(|d| d.as_array())
                    .and_then(|a| a.get(0))
                    .and_then(|o| o.get("metadata")) // 添加 metadata 层级
                    .and_then(|m| m.get("quotaResetDelay"))
                    .and_then(|v| v.as_str())
                {
                    tracing::debug!("[JSON解析] 找到 quotaResetDelay: '{}'", delay_str);

                    // 使用通用时间解析函数
                    if let Some(seconds) = self.parse_duration_string(delay_str) {
                        return Some(seconds);
                    }
                }

                // 2. OpenAI 常见的 retry_after 字段 (数字)
                if let Some(retry) = json
                    .get("error")
                    .and_then(|e| e.get("retry_after"))
                    .and_then(|v| v.as_u64())
                {
                    return Some(retry);
                }
            }
        }

        // B. 正则匹配模式 (兜底)
        // 模式 1: "Try again in 2m 30s"
        if let Ok(re) = Regex::new(r"(?i)try again in (\d+)m\s*(\d+)s") {
            if let Some(caps) = re.captures(body) {
                if let (Ok(m), Ok(s)) = (caps[1].parse::<u64>(), caps[2].parse::<u64>()) {
                    return Some(m * 60 + s);
                }
            }
        }

        // 模式 2: "Try again in 30s" 或 "backoff for 42s"
        if let Ok(re) = Regex::new(r"(?i)(?:try again in|backoff for|wait)\s*(\d+)s") {
            if let Some(caps) = re.captures(body) {
                if let Ok(s) = caps[1].parse::<u64>() {
                    return Some(s);
                }
            }
        }

        // 模式 3: "quota will reset in X seconds"
        if let Ok(re) = Regex::new(r"(?i)quota will reset in (\d+) second") {
            if let Some(caps) = re.captures(body) {
                if let Ok(s) = caps[1].parse::<u64>() {
                    return Some(s);
                }
            }
        }

        // 模式 4: OpenAI 风格的 "Retry after (\d+) seconds"
        if let Ok(re) = Regex::new(r"(?i)retry after (\d+) second") {
            if let Some(caps) = re.captures(body) {
                if let Ok(s) = caps[1].parse::<u64>() {
                    return Some(s);
                }
            }
        }

        // 模式 5: 括号形式 "(wait (\d+)s)"
        if let Ok(re) = Regex::new(r"\(wait (\d+)s\)") {
            if let Some(caps) = re.captures(body) {
                if let Ok(s) = caps[1].parse::<u64>() {
                    return Some(s);
                }
            }
        }

        None
    }

    /// 获取账号的限流信息
    pub fn get(&self, account_id: &str) -> Option<RateLimitInfo> {
        self.limits.get(account_id).map(|r| r.clone())
    }

    /// 检查账号是否仍在限流中
    /// 检查账号是否仍在限流中 (支持模型级)
    pub fn is_rate_limited(&self, account_id: &str, model: Option<&str>) -> bool {
        // Checking using get_remaining_wait which handles both global and model keys
        self.get_remaining_wait(account_id, model) > 0
    }

    /// 获取距离限流重置还有多少秒
    pub fn get_reset_seconds(&self, account_id: &str) -> Option<u64> {
        if let Some(info) = self.get(account_id) {
            info.reset_time
                .duration_since(SystemTime::now())
                .ok()
                .map(|d| d.as_secs())
        } else {
            None
        }
    }

    /// 清除过期的限流记录
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) -> usize {
        let now = SystemTime::now();
        let mut count = 0;

        self.limits.retain(|_k, v| {
            if v.reset_time <= now {
                count += 1;
                false
            } else {
                true
            }
        });

        if count > 0 {
            tracing::debug!("清除了 {} 个过期的限流记录", count);
        }

        count
    }

    /// 清除指定账号的限流记录
    pub fn clear(&self, account_id: &str) -> bool {
        self.limits.remove(account_id).is_some()
    }

    /// 清除所有限流记录 (乐观重置策略)
    ///
    /// 用于乐观重置机制,当所有账号都被限流但等待时间很短时,
    /// 清除所有限流记录以解决时序竞争条件
    pub fn clear_all(&self) {
        let count = self.limits.len();
        self.limits.clear();
        tracing::warn!(
            "🔄 Optimistic reset: Cleared all {} rate limit record(s)",
            count
        );
    }
}

impl Default for RateLimitTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_retry_time_minutes_seconds() {
        let tracker = RateLimitTracker::new();
        let body = "Rate limit exceeded. Try again in 2m 30s";
        let time = tracker.parse_retry_time_from_body(body);
        assert_eq!(time, Some(150));
    }

    #[test]
    fn test_parse_google_json_delay() {
        let tracker = RateLimitTracker::new();
        let body = r#"{
            "error": {
                "details": [
                    { 
                        "metadata": {
                            "quotaResetDelay": "42s" 
                        }
                    }
                ]
            }
        }"#;
        let time = tracker.parse_retry_time_from_body(body);
        assert_eq!(time, Some(42));
    }

    #[test]
    fn test_parse_retry_after_ignore_case() {
        let tracker = RateLimitTracker::new();
        let body = "Quota limit hit. Retry After 99 Seconds";
        let time = tracker.parse_retry_time_from_body(body);
        assert_eq!(time, Some(99));
    }

    #[test]
    fn test_get_remaining_wait() {
        let tracker = RateLimitTracker::new();
        tracker.parse_from_error("acc1", 429, Some("30"), "", None, &[]);
        let wait = tracker.get_remaining_wait("acc1", None);
        assert!(wait > 25 && wait <= 30);
    }

    #[test]
    fn test_safety_buffer() {
        let tracker = RateLimitTracker::new();
        // 如果 API 返回 1s，我们强制设为 2s
        tracker.parse_from_error("acc1", 429, Some("1"), "", None, &[]);
        let wait = tracker.get_remaining_wait("acc1", None);
        // Due to time passing, it might be 1 or 2
        assert!(wait >= 1 && wait <= 2);
    }

    #[test]
    fn test_tpm_exhausted_is_rate_limit_exceeded() {
        let tracker = RateLimitTracker::new();
        // 模拟真实世界的 TPM 错误，同时包含 "Resource exhausted" 和 "per minute"
        let body = "Resource has been exhausted (e.g. check quota). Quota limit 'Tokens per minute' exceeded.";
        let reason = tracker.parse_rate_limit_reason(body);
        // 应该被识别为 RateLimitExceeded，而不是 QuotaExhausted
        assert_eq!(reason, RateLimitReason::RateLimitExceeded);
    }

    #[test]
    fn test_generic_resource_exhausted_is_short_rate_limit() {
        let tracker = RateLimitTracker::new();
        let body = r#"{
            "error": {
                "code": 429,
                "message": "Resource has been exhausted (e.g. check quota).",
                "status": "RESOURCE_EXHAUSTED"
            }
        }"#;
        let reason = tracker.parse_rate_limit_reason(body);
        assert_eq!(reason, RateLimitReason::RateLimitExceeded);
    }

    #[test]
    fn test_server_error_does_not_accumulate_failure_count() {
        let tracker = RateLimitTracker::new();
        let backoff_steps = vec![60, 300, 1800, 7200];

        // 模拟连续 5 次 5xx 错误
        for i in 1..=5 {
            let info = tracker.parse_from_error(
                "acc1",
                503,
                None,
                "Service Unavailable",
                None,
                &backoff_steps,
            );
            assert!(info.is_some(), "第 {} 次 5xx 应该返回 RateLimitInfo", i);
            let info = info.unwrap();
            // 5xx 应该始终锁定 8 秒，不受 failure_count 影响
            assert_eq!(info.retry_after_sec, 8, "5xx 第 {} 次应该锁定 8 秒", i);
        }

        // 现在触发一次 429 QuotaExhausted（没有 quotaResetDelay）
        let quota_body = r#"{"error":{"details":[{"reason":"QUOTA_EXHAUSTED"}]}}"#;
        let info = tracker.parse_from_error("acc1", 429, None, quota_body, None, &backoff_steps);
        assert!(info.is_some());
        let info = info.unwrap();

        // 关键断言：429 应该从第 1 次开始（锁 60 秒），而不是继承 5xx 的计数
        assert_eq!(
            info.retry_after_sec, 60,
            "429 应该从第 1 次退避开始(60秒),而不是被 5xx 污染"
        );
    }

    #[test]
    fn test_quota_exhausted_does_accumulate_failure_count() {
        let tracker = RateLimitTracker::new();
        let backoff_steps = vec![60, 300, 1800, 7200];
        let quota_body = r#"{"error":{"details":[{"reason":"QUOTA_EXHAUSTED"}]}}"#;

        // 第 1 次 429 → 60 秒
        let info = tracker.parse_from_error("acc2", 429, None, quota_body, None, &backoff_steps);
        assert_eq!(info.unwrap().retry_after_sec, 60);

        // 第 2 次 429 → 300 秒
        let info = tracker.parse_from_error("acc2", 429, None, quota_body, None, &backoff_steps);
        assert_eq!(info.unwrap().retry_after_sec, 300);

        // 第 3 次 429 → 1800 秒（受 300 秒 / 5 分钟上限保护截断为 300 秒）
        let info = tracker.parse_from_error("acc2", 429, None, quota_body, None, &backoff_steps);
        assert_eq!(info.unwrap().retry_after_sec, 300);

        // 第 4 次 429 → 7200 秒（受 300 秒上限保护截断为 300 秒）
        let info = tracker.parse_from_error("acc2", 429, None, quota_body, None, &backoff_steps);
        assert_eq!(info.unwrap().retry_after_sec, 300);
    }

    #[test]
    fn test_humanized_pacer_nap_countdown() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        assert_eq!(pacer.get_nap_remaining_seconds("non_existent"), None);

        let now = std::time::Instant::now();
        let mut state = AccountPacerState::new_default("test_acc", now, 2);
        state.request_counter = 25;
        state.nap_until = Some(now + Duration::from_secs(120));
        pacer.account_states.insert("test_acc".to_string(), state);

        assert!(pacer.is_in_nap("test_acc"));
        let remaining = pacer.get_nap_remaining_seconds("test_acc");
        assert!(remaining.is_some());
        let secs = remaining.unwrap();
        assert!(secs >= 118 && secs <= 120);

        // 休眠中的账号在 get_required_wait_ms 中必须强制等待满休眠时长
        let wait_ms = pacer.get_required_wait_ms("test_acc");
        assert!(wait_ms >= 118_000 && wait_ms <= 120_000);

        // 小憩期间大小请求一律在前置准入阶段被拒绝 (InNap)
        let normal_fp = RequestFootprint::new(10_000, 20 * 1024);
        let oversize_fp = RequestFootprint::new(250_000, 250 * 1024);
        assert!(matches!(
            pacer.try_acquire_lease("test_acc", normal_fp, 2, 2000),
            Err(LeaseRejectReason::InNap { .. })
        ));
        assert!(matches!(
            pacer.try_acquire_lease("test_acc", oversize_fp, 2, 2000),
            Err(LeaseRejectReason::InNap { .. })
        ));
    }

    #[test]
    fn test_humanized_pacer_cooldown_and_rolling_rpm() {
        let pacer = HumanizedPacer::new_warmed();
        let acc = "test_pacer_acc";

        // 1. L1 Normal (<=200KB 且 <50K tokens)：低延迟 800ms ~ 1800ms
        pacer.mark_request_completed(acc, Some(20_000));
        let wait = pacer.get_required_wait_ms(acc);
        assert!(wait >= 700 && wait <= 1850, "Normal wait was {}ms", wait);

        // 2. L2 Medium (50K~100K tokens 或 200KB~400KB)：3500ms ~ 7500ms
        pacer.mark_request_completed(acc, Some(75_000));
        let wait_medium = pacer.get_required_wait_ms(acc);
        assert!(
            wait_medium >= 3400 && wait_medium <= 7600,
            "Medium wait was {}ms",
            wait_medium
        );

        // 3. L3 Oversized (>100K tokens 或 >400KB)：8000ms ~ 15000ms
        pacer.mark_request_completed(acc, Some(150_000));
        let wait_huge = pacer.get_required_wait_ms(acc);
        assert!(
            wait_huge >= 7900 && wait_huge <= 15_100,
            "Oversized wait was {}ms",
            wait_huge
        );

        // 4. 模拟 ACCOUNT_MAX_REQUESTS_PER_MINUTE 次请求打满 60s 滚动 RPM
        let now = std::time::Instant::now();
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            entry.next_allowed_request_at = now; // 清空冷却以便测试 RPM
            entry.rolling_requests.clear();
            for _ in 0..ACCOUNT_MAX_REQUESTS_PER_MINUTE {
                entry.rolling_requests.push_back(now);
            }
        }
        let rpm_wait = pacer.get_required_wait_ms(acc);
        assert!(rpm_wait > 58_000 && rpm_wait <= 60_000);
    }

    #[test]
    fn test_footprint_tiers_and_oversized_strict_less_than_3_cap() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "heat_cap_acc";

        // 分级校验：<=200KB & <50K -> Normal, 50K~100K 或 200KB~400KB -> Medium, >400KB 或 >100K -> Oversized
        let fp_normal = RequestFootprint::new(25_000, 100 * 1024);
        let fp_medium = RequestFootprint::new(75_000, 250 * 1024);
        let fp_oversize = RequestFootprint::new(120_000, 450 * 1024);
        assert_eq!(fp_normal.tier, TrafficTier::Normal);
        assert_eq!(fp_medium.tier, TrafficTier::Medium);
        assert_eq!(fp_oversize.tier, TrafficTier::Oversized);

        // 第 1 个 >100K/400KB 请求：heat 0.0 -> 1.5，放行
        let (mut g1, _) = pacer
            .try_acquire_lease(acc, fp_oversize, 2, 0)
            .expect("1st oversized should pass");
        g1.complete();
        // 手动跳过槽位冷却以专测短时间热度门禁
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            entry.slots = vec![SlotPhase::Free; 2];
        }
        assert!(pacer.get_account_heat(acc) >= 1.45);

        // 第 2 个 >100K/400KB 请求：projected ~2.99 < 3.0，放行
        let (mut g2, _) = pacer
            .try_acquire_lease(acc, fp_oversize, 2, 0)
            .expect("2nd oversized should pass");
        g2.complete();
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            entry.slots = vec![SlotPhase::Free; 2];
        }
        assert!(pacer.get_account_heat(acc) >= 2.9);

        // 第 3 个 >100K/400KB 请求：projected ~4.49 >= 3.0，必须在进门前被 Overheated 拒绝！（严格 < 3 个）
        let third = pacer.try_acquire_lease(acc, fp_oversize, 2, 2000);
        assert!(
            matches!(third, Err(LeaseRejectReason::Overheated { .. })),
            "3rd oversized must be rejected pre-admission"
        );

        // 但 Medium 中等请求与 Normal 普通请求不受过热硬封锁，仍可正常进入空闲槽
        let (mut gm, _) = pacer
            .try_acquire_lease(acc, fp_medium, 2, 0)
            .expect("Medium request should still be allowed on warm account");
        gm.abort_and_rollback();
    }

    #[test]
    fn test_two_slot_lifecycle_sticky_2s_wait_and_mutual_exclusion() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "slot_acc";
        let fp_small = RequestFootprint::new(15_000, 40 * 1024);
        let fp_large = RequestFootprint::new(220_000, 220 * 1024);

        // 1. 2 并发槽下，连续进入 2 个小请求均成功闭合槽位
        let (mut s1, _) = pacer
            .try_acquire_lease(acc, fp_small, 2, 0)
            .expect("slot 1 open");
        // 有 1 个小请求在执行时，大请求 (>200K) 必须互斥拒绝！
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_large, 2, 2000),
            Err(LeaseRejectReason::SlotsBusy)
        ));

        let (mut s2, _) = pacer
            .try_acquire_lease(acc, fp_small, 2, 0)
            .expect("slot 2 open");
        // 2 个槽全处于 InFlight 时，第 3 个小请求即使在粘性模式下也立即拒绝
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_small, 2, 2000),
            Err(LeaseRejectReason::SlotsBusy)
        ));

        // 2. 两个小请求结束响应，进入 800~1800ms (<=2s) 冷却期：两个槽在冷却延迟内依然闭合！
        s1.complete();
        s2.complete();

        // 自由选号模式 (max_wait_ms = 0) 不允许进入全部处于冷却延迟的账号
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_small, 2, 0),
            Err(LeaseRejectReason::CooldownTooLong { .. })
        ));

        // 粘性会话模式 (max_wait_ms = 2000)：因延迟 <= 2s，成功原子预约最早冷却槽位！
        let (mut s_reserved, _) = pacer
            .try_acquire_lease(acc, fp_small, 2, STICKY_MAX_SLOT_WAIT_MS)
            .expect("sticky session should reserve slot when cooldown <= 2s");
        assert!(
            s_reserved.reserved_wait_ms() > 0 && s_reserved.reserved_wait_ms() <= 2000,
            "reserved wait should be within 2s"
        );
        // 若中途回滚，槽位退回原冷却状态，不污染计数
        s_reserved.abort_and_rollback();

        // 3. 若两个槽冷却剩余 > STICKY_MAX_SLOT_WAIT_MS，粘性会话果断拒绝以触发换号
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            let until = std::time::Instant::now() + Duration::from_millis(STICKY_MAX_SLOT_WAIT_MS + 2000);
            entry.slots = vec![
                SlotPhase::CoolingDown {
                    until,
                    exclusive: false,
                },
                SlotPhase::CoolingDown {
                    until,
                    exclusive: false,
                },
            ];
        }
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_small, 2, STICKY_MAX_SLOT_WAIT_MS),
            Err(LeaseRejectReason::CooldownTooLong { .. })
        ));
    }

    #[test]
    fn test_turn_based_micro_nap_15_to_25_turns() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "nap_turn_acc";
        let fp = RequestFootprint::new(5_000, 10 * 1024);

        let mut nap_triggered_at = None;
        for turn in 1..=31 {
            // 清空冷却与滚动 RPM 仅测试轮次小憩
            if let Some(mut entry) = pacer.account_states.get_mut(acc) {
                entry.slots = vec![SlotPhase::Free; 2];
                entry.rolling_requests.clear();
            }
            let (mut guard, entered_nap) = pacer
                .try_acquire_lease(acc, fp, 2, 0)
                .expect("should admit until nap triggers");
            guard.complete();
            if entered_nap {
                nap_triggered_at = Some(turn);
                break;
            }
        }

        let triggered_turn = nap_triggered_at.expect("must trigger micro-nap within 15..=25 turns");
        assert!(
            (15..=25).contains(&triggered_turn),
            "nap triggered at turn {}, expected 15..=25",
            triggered_turn
        );
        assert!(pacer.is_in_nap(acc));
    }

    #[test]
    fn test_early_nap_triggered_by_heat_and_turns() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "early_nap_acc";
        let fp_small = RequestFootprint::new(5_000, 10 * 1024);
        let fp_heavy = RequestFootprint::new(85_000, 300 * 1024); // Heavy: heat increment +0.8

        // Run 9 small turns
        for _ in 1..=9 {
            if let Some(mut entry) = pacer.account_states.get_mut(acc) {
                entry.slots = vec![SlotPhase::Free; 2];
                entry.rolling_requests.clear();
            }
            let (mut guard, entered_nap) = pacer
                .try_acquire_lease(acc, fp_small, 2, 0)
                .expect("should admit");
            guard.complete();
            assert!(!entered_nap, "should not enter nap before 10 turns");
        }

        // Set heat to 1.3 (below saturation threshold 1.5)
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            entry.massive_heat = 1.3;
            entry.slots = vec![SlotPhase::Free; 2];
            entry.rolling_requests.clear();
        }

        // 10th turn with fp_heavy (+0.8 heat) -> massive_heat = 1.3 + 0.8 = 2.1 >= 2.0
        // turns_since_nap = 10 >= 10
        // Should trigger early nap!
        let (mut guard, entered_nap) = pacer
            .try_acquire_lease(acc, fp_heavy, 2, 0)
            .expect("should admit 10th turn");
        guard.complete();
        assert!(entered_nap, "10th turn with heat >= 2.0 must trigger early nap");
        assert!(pacer.is_in_nap(acc), "account must be in nap state");
    }

    #[test]
    fn test_two_stage_egress_recovery_hold() {
        // Phase 1 (0..60s): ALL tiers (Normal, Medium, Oversized) are held
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(15, TrafficTier::Normal),
            Some(45)
        );
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(15, TrafficTier::Medium),
            Some(45)
        );
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(15, TrafficTier::Oversized),
            Some(45)
        );

        // Phase 2 (60..180s): Normal is allowed, Medium and Oversized are still held
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(90, TrafficTier::Normal),
            None
        );
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(90, TrafficTier::Medium),
            Some(90)
        );
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(90, TrafficTier::Oversized),
            Some(90)
        );

        // Phase 3 (>=180s): ALL tiers allowed
        assert_eq!(
            remaining_recovery_hold_secs_for_tier(180, TrafficTier::Oversized),
            None
        );
    }

    #[test]
    fn test_oversize_10min_window_blocks_3rd_even_after_3min() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "win10m_acc";
        let fp_oversize = RequestFootprint::new(120_000, 450 * 1024);

        // Admit 2 oversized requests
        let (mut g1, _) = pacer
            .try_acquire_lease(acc, fp_oversize, 2, 0)
            .expect("1st oversized admitted");
        g1.complete();
        pacer.test_clear_slot_cooldowns(acc);

        let (mut g2, _) = pacer
            .try_acquire_lease(acc, fp_oversize, 2, 0)
            .expect("2nd oversized admitted");
        g2.complete();
        pacer.test_clear_slot_cooldowns(acc);

        // Simulate 200s (3m20s) passing — previously 180s half-life would let heat decay < 1.5
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            let past = std::time::Instant::now() - Duration::from_secs(200);
            entry.last_heat_updated_at = past;
            entry.oversized_history.clear();
            entry.oversized_history.push_back(past);
            entry.oversized_history.push_back(past);
        }

        // 3rd oversized within 10 minutes must STILL be rejected!
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_oversize, 2, 2000),
            Err(LeaseRejectReason::Overheated { .. })
        ));
    }

    #[test]
    fn test_oauth_token_refresh_warmup_blocks_medium_and_oversized_allows_normal() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "fresh_oauth_acc";
        let fp_normal = RequestFootprint::new(15_000, 30 * 1024);
        let fp_medium = RequestFootprint::new(75_000, 250 * 1024);
        let fp_oversize = RequestFootprint::new(120_000, 450 * 1024);

        // Mark account as having just refreshed its OAuth token
        pacer.note_oauth_refreshed(acc);

        // Oversized (>100K/400KB) and Medium (>50K/200KB) must be blocked during 60s TokenRefreshWarmup
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_oversize, 2, 2000),
            Err(LeaseRejectReason::TokenRefreshWarmup { .. })
        ));
        assert!(matches!(
            pacer.try_acquire_lease(acc, fp_medium, 2, 2000),
            Err(LeaseRejectReason::TokenRefreshWarmup { .. })
        ));

        // Normal (<=50K and <=200KB) is allowed during the 60s warmup for handshakes and initial traffic
        let (mut g_norm, _) = pacer
            .try_acquire_lease(acc, fp_normal, 2, 0)
            .expect("Normal tier must be allowed after OAuth refresh");
        g_norm.complete();
        pacer.test_clear_slot_cooldowns(acc);

        // Simulate 61s elapsed since OAuth refresh -> Medium and Oversized now allowed
        if let Some(mut entry) = pacer.account_states.get_mut(acc) {
            entry.last_oauth_refresh_at =
                Some(std::time::Instant::now() - Duration::from_secs(61));
        }
        let (mut g_med, _) = pacer
            .try_acquire_lease(acc, fp_medium, 2, 0)
            .expect("Medium must be allowed after 60s OAuth warmup expires");
        g_med.complete();
        pacer.test_clear_slot_cooldowns(acc);

        let (mut g_over, _) = pacer
            .try_acquire_lease(acc, fp_oversize, 2, 0)
            .expect("Oversized must be allowed after 60s OAuth warmup expires");
        g_over.complete();
    }

    #[test]
    fn test_rolling_rpm_5_cap_and_rejection() {
        let pacer = std::sync::Arc::new(HumanizedPacer::new_warmed());
        let acc = "rpm5_acc";
        let fp_small = RequestFootprint::new(10_000, 20 * 1024);

        // 连续进入 5 个请求（中间清空槽位冷却以专注测试 60s RPM 门禁）
        for i in 1..=5 {
            let (mut g, _) = pacer
                .try_acquire_lease(acc, fp_small, 2, 0)
                .unwrap_or_else(|e| panic!("Request #{} should pass RPM check: {:?}", i, e));
            g.complete();
            pacer.test_clear_slot_cooldowns(acc);
        }

        // 第 6 个请求在 60s 窗口内到来：必须被 CooldownTooLong 拒绝（wait > 2000ms），触发上层切号！
        let sixth = pacer.try_acquire_lease(acc, fp_small, 2, STICKY_MAX_SLOT_WAIT_MS);
        assert!(
            matches!(sixth, Err(LeaseRejectReason::CooldownTooLong { wait_ms }) if wait_ms > 2000),
            "6th request within 60s must be rejected by RPM cap"
        );
    }
}
