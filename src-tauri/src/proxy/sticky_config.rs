use serde::{Deserialize, Serialize};

/// 调度模式枚举
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SchedulingMode {
    /// 缓存优先 (Cache-first): 尽可能锁定同一账号，限流时优先等待，极大提升 Prompt Caching 命中率
    CacheFirst,
    /// 平衡模式 (Balance): 锁定同一账号，限流时立即切换到备选账号，兼顾成功率和性能
    Balance,
    /// 性能优先 (Performance-first): 纯轮询模式 (Round-robin)，账号负载最均衡，但不利用缓存
    PerformanceFirst,
}

impl Default for SchedulingMode {
    fn default() -> Self {
        Self::Balance
    }
}

/// 粘性会话配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StickySessionConfig {
    /// 当前调度模式
    pub mode: SchedulingMode,
    /// 缓存优先模式下的最大等待时间 (秒)
    pub max_wait_seconds: u64,
}

impl Default for StickySessionConfig {
    fn default() -> Self {
        Self {
            mode: SchedulingMode::Balance,
            max_wait_seconds: 60,
        }
    }
}

/// 串行号池开启时与 sticky / CacheFirst / PerformanceFirst 并存的提示（调度以游标为准）
pub fn warn_serial_pool_scheduling_override(mode: SchedulingMode) {
    match mode {
        SchedulingMode::CacheFirst => {
            tracing::warn!(
                "[SerialPool] serial_pool.enabled=true: ignoring CacheFirst rate-limit wait; advancing cursor instead"
            );
        }
        SchedulingMode::PerformanceFirst => {
            tracing::warn!(
                "[SerialPool] serial_pool.enabled=true: ignoring PerformanceFirst round-robin; using global preferred cursor"
            );
        }
        SchedulingMode::Balance => {
            tracing::info!(
                "[SerialPool] serial_pool.enabled=true: sticky bindings defer to preferred cursor"
            );
        }
    }
}
