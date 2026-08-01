use crate::modules::cloudflared::CloudflaredConfig;
use crate::proxy::ProxyConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub language: String,
    pub theme: String,
    pub auto_refresh: bool,
    pub refresh_interval: i32, // minutes
    pub auto_sync: bool,
    pub sync_interval: i32, // minutes
    pub default_export_path: Option<String>,
    #[serde(default)]
    pub proxy: ProxyConfig,
    pub antigravity_executable: Option<String>, // [NEW] Manually specified Antigravity executable path
    pub antigravity_ide_executable: Option<String>, // [NEW] Manually specified Antigravity IDE executable path
    pub antigravity_cli_executable: Option<String>, // [NEW] Manually specified Antigravity CLI (agy) path
    pub antigravity_args: Option<Vec<String>>,      // [NEW] Antigravity startup arguments
    #[serde(default)]
    pub auto_launch: bool,     // Launch on startup
    #[serde(default)]
    pub scheduled_warmup: ScheduledWarmupConfig, // [NEW] Scheduled warmup configuration
    #[serde(default)]
    pub quota_protection: QuotaProtectionConfig, // [NEW] Quota protection configuration
    #[serde(default)]
    pub quota_ledger: QuotaLedgerConfig, // Local estimated quota ledger
    #[serde(default)]
    pub pinned_quota_models: PinnedQuotaModelsConfig, // [NEW] Pinned quota models list
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig, // [NEW] Circuit breaker configuration
    #[serde(default)]
    pub hidden_menu_items: Vec<String>, // Hidden menu item path list
    #[serde(default)]
    pub cloudflared: CloudflaredConfig, // [NEW] Cloudflared configuration
}

/// Scheduled warmup configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledWarmupConfig {
    /// Whether smart warmup is enabled
    pub enabled: bool,

    /// List of models to warmup
    #[serde(default = "default_warmup_models")]
    pub monitored_models: Vec<String>,
}

fn default_warmup_models() -> Vec<String> {
    vec![
        "gemini-3-flash".to_string(),
        "claude".to_string(),
        "gemini-3-pro-high".to_string(),
        "gemini-3.1-flash-image".to_string(),
    ]
}

impl ScheduledWarmupConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            monitored_models: default_warmup_models(),
        }
    }
}

impl Default for ScheduledWarmupConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Quota protection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaProtectionConfig {
    /// Whether quota protection is enabled
    pub enabled: bool,

    /// Reserved quota percentage (1-99)
    pub threshold_percentage: u32,

    /// List of monitored billing groups (e.g. gemini, claude)
    #[serde(default = "default_monitored_models")]
    pub monitored_models: Vec<String>,
}

fn default_monitored_models() -> Vec<String> {
    vec!["claude".to_string(), "gemini".to_string()]
}

impl QuotaProtectionConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            threshold_percentage: 10, // Default 10% reserve
            monitored_models: default_monitored_models(),
        }
    }
}

impl Default for QuotaProtectionConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Family keyword → multiplier (first match wins; order matters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyMultiplier {
    pub keyword: String,
    pub multiplier: f64,
}

/// Local quota ledger: burn on success, calibrate from online fetch.
/// Burn uses 5h Sprint Compute Units (CU) estimate; online remaining_fraction overwrites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaLedgerConfig {
    /// When false, selection/protection use online snapshot only.
    #[serde(default = "default_ledger_enabled")]
    pub enabled: bool,
    /// Minimum percentage burned per successful request (also floor when usage present).
    #[serde(default = "default_min_burn_pct")]
    pub min_burn_pct: u32,
    /// Legacy field kept for old config JSON; unused by CU burn.
    #[serde(default = "default_tokens_per_percent")]
    pub tokens_per_percent: u32,
    /// Weight for non-cached input tokens.
    #[serde(default = "default_w_in")]
    pub w_in: f64,
    /// Weight for cache-read tokens.
    #[serde(default = "default_w_cache")]
    pub w_cache: f64,
    /// Weight for output (+ thinking folded into output).
    #[serde(default = "default_w_out")]
    pub w_out: f64,
    /// Standard input-token equivalents per 1 CU (Pro anchor).
    #[serde(default = "default_tokens_per_cu")]
    pub tokens_per_cu: f64,
    /// 5h sprint capacity in CU (100% of local ledger bucket).
    #[serde(default = "default_sprint_capacity_cu")]
    pub sprint_capacity_cu: f64,
    /// Reserved 7d marathon capacity; not enforced locally.
    #[serde(default = "default_marathon_capacity_cu")]
    pub marathon_capacity_cu: f64,
    /// When true, billable_in = max(0, input - cache).
    #[serde(default = "default_cache_is_subset_of_input")]
    pub cache_is_subset_of_input: bool,
    /// Exact model id → multiplier (lookup after normalize).
    #[serde(default = "default_model_multipliers")]
    pub model_multipliers: HashMap<String, f64>,
    /// Keyword family multipliers; first contains() hit wins.
    #[serde(default = "default_family_multipliers")]
    pub family_multipliers: Vec<FamilyMultiplier>,
}

fn default_ledger_enabled() -> bool {
    true
}

fn default_min_burn_pct() -> u32 {
    1
}

fn default_tokens_per_percent() -> u32 {
    20_000
}

fn default_w_in() -> f64 {
    1.0
}

fn default_w_cache() -> f64 {
    0.15
}

fn default_w_out() -> f64 {
    3.5
}

fn default_tokens_per_cu() -> f64 {
    1000.0
}

fn default_sprint_capacity_cu() -> f64 {
    500.0
}

fn default_marathon_capacity_cu() -> f64 {
    2800.0
}

fn default_cache_is_subset_of_input() -> bool {
    true
}

fn default_model_multipliers() -> HashMap<String, f64> {
    let mut m = HashMap::new();
    m.insert("gemini-3-flash".into(), 0.25);
    m.insert("gemini-2.5-flash".into(), 0.25);
    m.insert("gemini-2.5-flash-lite".into(), 0.1);
    m.insert("gemini-3.5-flash".into(), 0.25);
    m.insert("gemini-3.6-flash".into(), 0.15);
    m.insert("gemini-3.1-pro-high".into(), 1.0);
    m.insert("gemini-3.1-pro-low".into(), 1.0);
    m.insert("gemini-3.1-pro-preview".into(), 1.0);
    m.insert("gemini-3.1-pro".into(), 1.0);
    m.insert("gemini-3-pro".into(), 1.0);
    m.insert("gemini-pro-agent".into(), 1.0);
    m.insert("gemini-3-pro-image".into(), 2.0);
    m.insert("claude-sonnet-4-6".into(), 3.0);
    m.insert("claude-sonnet-4-6-thinking".into(), 3.0);
    m.insert("claude-opus-4-6-thinking".into(), 8.0);
    m.insert("gpt-oss-120b".into(), 1.5);
    m.insert("default_fallback".into(), 3.0);
    m
}

fn default_family_multipliers() -> Vec<FamilyMultiplier> {
    vec![
        FamilyMultiplier {
            keyword: "flash-lite".into(),
            multiplier: 0.1,
        },
        FamilyMultiplier {
            keyword: "flash".into(),
            multiplier: 0.25,
        },
        FamilyMultiplier {
            keyword: "image".into(),
            multiplier: 2.0,
        },
        FamilyMultiplier {
            keyword: "pro".into(),
            multiplier: 1.0,
        },
        FamilyMultiplier {
            keyword: "sonnet".into(),
            multiplier: 3.0,
        },
        FamilyMultiplier {
            keyword: "opus".into(),
            multiplier: 8.0,
        },
    ]
}

impl QuotaLedgerConfig {
    pub fn new() -> Self {
        Self {
            enabled: true,
            min_burn_pct: 1,
            tokens_per_percent: 20_000,
            w_in: default_w_in(),
            w_cache: default_w_cache(),
            w_out: default_w_out(),
            tokens_per_cu: default_tokens_per_cu(),
            sprint_capacity_cu: default_sprint_capacity_cu(),
            marathon_capacity_cu: default_marathon_capacity_cu(),
            cache_is_subset_of_input: default_cache_is_subset_of_input(),
            model_multipliers: default_model_multipliers(),
            family_multipliers: default_family_multipliers(),
        }
    }

    /// Lowercase and strip common vendor prefixes before multiplier lookup.
    pub fn normalize_model_name_for_multiplier(model: &str) -> String {
        let mut s = model.trim().to_lowercase();
        for prefix in ["antigravity/", "models/", "google/", "anthropic/", "openai/"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.to_string();
            }
        }
        s
    }

    /// Exact model id → family keyword (first hit) → default_fallback (3.0).
    pub fn resolve_model_multiplier(&self, model: &str) -> f64 {
        let key = Self::normalize_model_name_for_multiplier(model);
        if let Some(m) = self.model_multipliers.get(&key) {
            return *m;
        }
        for fam in &self.family_multipliers {
            if !fam.keyword.is_empty() && key.contains(&fam.keyword.to_lowercase()) {
                return fam.multiplier;
            }
        }
        self.model_multipliers
            .get("default_fallback")
            .copied()
            .unwrap_or(3.0)
    }

    /// Weighted CU burn → integer % of sprint capacity.
    ///
    /// ```text
    /// billable_in = max(0, input - cache)  when cache_is_subset_of_input
    /// ΔCU = (billable_in×w_in + cache×w_cache + output×w_out) / tokens_per_cu × M
    /// burn% = max(min_burn, ceil(ΔCU / sprint_capacity × 100))  when any usage
    /// no usage → min_burn
    /// ```
    pub fn compute_burn_pct(
        &self,
        model: &str,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        cached_tokens: Option<u32>,
    ) -> i32 {
        let min_burn = self.min_burn_pct.max(1) as i32;
        let input = input_tokens.unwrap_or(0) as f64;
        let output = output_tokens.unwrap_or(0) as f64;
        let cache = cached_tokens.unwrap_or(0) as f64;
        let has_usage = input_tokens.is_some() || output_tokens.is_some() || cached_tokens.is_some();
        let any_tokens = input > 0.0 || output > 0.0 || cache > 0.0;

        if !has_usage || !any_tokens {
            return min_burn;
        }

        let billable_in = if self.cache_is_subset_of_input {
            (input - cache).max(0.0)
        } else {
            input
        };

        let tokens_per_cu = if self.tokens_per_cu > 0.0 {
            self.tokens_per_cu
        } else {
            1000.0
        };
        let sprint = if self.sprint_capacity_cu > 0.0 {
            self.sprint_capacity_cu
        } else {
            500.0
        };
        let m = self.resolve_model_multiplier(model);
        let weighted =
            billable_in * self.w_in + cache * self.w_cache + output * self.w_out;
        let delta_cu = (weighted / tokens_per_cu) * m;
        if delta_cu <= 0.0 {
            return min_burn;
        }
        let from_cu = (delta_cu / sprint * 100.0).ceil() as i32;
        min_burn.max(from_cu.max(1))
    }
}

impl Default for QuotaLedgerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Pinned quota models configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedQuotaModelsConfig {
    /// List of pinned models (displayed outside the account list)
    #[serde(default = "default_pinned_models")]
    pub models: Vec<String>,
}

fn default_pinned_models() -> Vec<String> {
    vec!["gemini".to_string(), "claude".to_string()]
}

impl PinnedQuotaModelsConfig {
    pub fn new() -> Self {
        Self {
            models: default_pinned_models(),
        }
    }
}

impl Default for PinnedQuotaModelsConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Circuit breaker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Whether circuit breaker is enabled
    pub enabled: bool,

    /// Unified backoff steps (seconds)
    /// Default: [60, 300, 1800, 7200]
    #[serde(default = "default_backoff_steps")]
    pub backoff_steps: Vec<u64>,
}

fn default_backoff_steps() -> Vec<u64> {
    vec![60, 300, 1800, 7200]
}

impl CircuitBreakerConfig {
    pub fn new() -> Self {
        Self {
            enabled: true,
            backoff_steps: default_backoff_steps(),
        }
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AppConfig {
    pub fn new() -> Self {
        Self {
            language: "zh".to_string(),
            theme: "system".to_string(),
            auto_refresh: true,
            refresh_interval: 15,
            auto_sync: false,
            sync_interval: 5,
            default_export_path: None,
            proxy: ProxyConfig::default(),
            antigravity_executable: None,
            antigravity_ide_executable: None,
            antigravity_cli_executable: None,
            antigravity_args: None,
            auto_launch: false,
            scheduled_warmup: ScheduledWarmupConfig::default(),
            quota_protection: QuotaProtectionConfig::default(),
            quota_ledger: QuotaLedgerConfig::default(),
            pinned_quota_models: PinnedQuotaModelsConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            hidden_menu_items: Vec::new(),
            cloudflared: CloudflaredConfig::default(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::new()
    }
}
