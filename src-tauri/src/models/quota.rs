use serde::{Deserialize, Serialize};

/// 单个配额桶 (对应 retrieveUserQuotaSummary 里的一个 bucket)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaBucket {
    /// 桶 ID,如 "gemini-weekly" / "gemini-5h" / "3p-weekly" / "3p-5h"
    pub bucket_id: String,
    /// 窗口类型: "weekly" / "5h"
    pub window: String,
    /// 剩余比例 0.0-1.0
    pub remaining_fraction: f64,
    /// 重置时间 (RFC3339)
    pub reset_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 一个模型组 (如 Gemini Models / Claude and GPT models)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaGroup {
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub buckets: Vec<QuotaBucket>,
}

/// 模型配额信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelQuota {
    pub name: String,
    pub percentage: i32, // 剩余百分比 0-100
    pub reset_time: String,

    // -- 动态参数解析与持久化 --
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_mime_types: Option<std::collections::HashMap<String, bool>>,
}

/// 配额数据结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaData {
    pub models: Vec<ModelQuota>,
    pub last_updated: i64,
    #[serde(default)]
    pub is_forbidden: bool,
    /// 禁止访问的原因 (403 详细信息)
    #[serde(default)]
    pub forbidden_reason: Option<String>,
    /// 订阅等级 (FREE/PRO/ULTRA)
    #[serde(default)]
    pub subscription_tier: Option<String>,
    /// 模型淘汰重定向规则表 (old_model_id -> new_model_id)
    #[serde(default)]
    pub model_forwarding_rules: std::collections::HashMap<String, String>,
    /// 按模型组的配额摘要 (weekly + 5h 双窗口),来自 retrieveUserQuotaSummary
    #[serde(default)]
    pub quota_groups: Option<Vec<QuotaGroup>>,
}

impl QuotaData {
    pub fn new() -> Self {
        Self {
            models: Vec::new(),
            last_updated: chrono::Utc::now().timestamp(),
            is_forbidden: false,
            forbidden_reason: None,
            subscription_tier: None,
            model_forwarding_rules: std::collections::HashMap::new(),
            quota_groups: None,
        }
    }

    pub fn add_model(&mut self, model: ModelQuota) {
        self.models.push(model);
    }
}

impl Default for QuotaData {
    fn default() -> Self {
        Self::new()
    }
}

fn default_hundred() -> i32 {
    100
}

/// Local estimated remaining quota for one standard model id (ledger).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatedModelQuota {
    /// Standard model id (e.g. `claude`, `gemini`)
    pub model: String,
    /// Effective remaining percentage 0-100 (min of active windows, for backward compatibility)
    pub percentage: i32,
    /// Estimated remaining percentage in 5h window (0-100)
    #[serde(default = "default_hundred")]
    pub percentage_5h: i32,
    /// Estimated remaining percentage in 7d (weekly) window (0-100)
    #[serde(default = "default_hundred")]
    pub percentage_weekly: i32,
    /// 5h reset timestamp (unix seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_time_5h: Option<i64>,
    /// Weekly reset timestamp (unix seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_time_weekly: Option<i64>,
    /// Last calibration delta 5h: (local_est - remote_actual)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_5h: Option<i32>,
    /// Last calibration delta weekly: (local_est - remote_actual)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_weekly: Option<i32>,
    /// Active limiting window: "5h" | "weekly" | "none"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limiting_window: Option<String>,
    /// Last online percentage applied during calibration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_online_pct: Option<i32>,
    /// Unix timestamp of last online calibration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_calibrated_at: Option<i64>,
}

impl EstimatedModelQuota {
    pub fn from_online(model: String, online_pct: i32, now: i64) -> Self {
        let pct = online_pct.clamp(0, 100);
        Self {
            model,
            percentage: pct,
            percentage_5h: pct,
            percentage_weekly: pct,
            reset_time_5h: None,
            reset_time_weekly: None,
            delta_5h: None,
            delta_weekly: None,
            limiting_window: Some("5h".to_string()),
            last_online_pct: Some(pct),
            last_calibrated_at: Some(now),
        }
    }

    pub fn from_online_dual(
        model: String,
        online_5h: i32,
        online_weekly: i32,
        reset_5h: Option<i64>,
        reset_weekly: Option<i64>,
        now: i64,
        prev: Option<&EstimatedModelQuota>,
    ) -> Self {
        let p5h = online_5h.clamp(0, 100);
        let pw = online_weekly.clamp(0, 100);
        let delta_5h = prev.map(|p| p.percentage_5h - p5h);
        let delta_weekly = prev.map(|p| p.percentage_weekly - pw);
        let eff = p5h.min(pw);
        let limiting_window = if pw <= 0 || pw < p5h {
            Some("weekly".to_string())
        } else {
            Some("5h".to_string())
        };
        Self {
            model,
            percentage: eff,
            percentage_5h: p5h,
            percentage_weekly: pw,
            reset_time_5h: reset_5h,
            reset_time_weekly: reset_weekly,
            delta_5h,
            delta_weekly,
            limiting_window,
            last_online_pct: Some(eff),
            last_calibrated_at: Some(now),
        }
    }
}
