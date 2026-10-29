use crate::proxy::config::{ProxyEntry, ProxyPoolConfig, ProxySelectionStrategy};
use dashmap::DashMap;
use futures::{stream, StreamExt};
use rquest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use rquest_util::Emulation;
use std::sync::OnceLock;

/// Runtime egress usage status from live upstream requests (not health probes).
/// Missing keys are treated as `unknown` (yellow) by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressUsageStatus {
    /// Verified: received an HTTP response via this proxy.
    Ok,
    /// Transport failure (connect / DNS / timeout / proxy handshake).
    Failed,
}

/// 全局代理池管理器单例
pub static GLOBAL_PROXY_POOL: OnceLock<Arc<ProxyPoolManager>> = OnceLock::new();

/// 获取全局代理池管理器
pub fn get_global_proxy_pool() -> Option<Arc<ProxyPoolManager>> {
    GLOBAL_PROXY_POOL.get().cloned()
}

/// 初始化全局代理池管理器
pub fn init_global_proxy_pool(config: Arc<RwLock<ProxyPoolConfig>>) -> Arc<ProxyPoolManager> {
    let manager = Arc::new(ProxyPoolManager::new(config));
    let _ = GLOBAL_PROXY_POOL.set(manager.clone());
    manager
}

/// 代理配置 (用于构建 reqwest Client)
/// 注意：重命名为 PoolProxyConfig 以避免与 config::ProxyConfig 冲突
#[derive(Debug, Clone)]
pub struct PoolProxyConfig {
    pub proxy: rquest::Proxy,
    pub entry_id: String,
}

/// Actual HTTP egress used for a request (pool node / app upstream / direct).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressRoute {
    /// Bound or pool-selected proxy node
    Pool { entry_id: String },
    /// App-config global upstream proxy
    Upstream,
    /// No proxy
    Direct,
}

impl EgressRoute {
    /// Chinese prefix for error messages shown in the UI.
    pub fn error_prefix(&self) -> String {
        match self {
            EgressRoute::Pool { entry_id } => format!("[代理池:{}]", entry_id),
            EgressRoute::Upstream => "[上游代理]".to_string(),
            EgressRoute::Direct => "[直连]".to_string(),
        }
    }

    /// Resolve upstream vs direct from app config (when no pool proxy was selected).
    pub fn from_app_upstream_fallback() -> Self {
        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                return EgressRoute::Upstream;
            }
        }
        EgressRoute::Direct
    }
}

/// Pins a resolved egress for one account so nested ops (token/quota/project)
/// reuse the same proxy without re-running pool selection (e.g. RoundRobin).
pub struct AccountEgressGuard {
    pool: Arc<ProxyPoolManager>,
    account_id: String,
}

impl Drop for AccountEgressGuard {
    fn drop(&mut self) {
        self.pool.forced_egress.remove(&self.account_id);
    }
}

/// 代理池管理器
pub struct ProxyPoolManager {
    config: Arc<RwLock<ProxyPoolConfig>>,

    /// 代理使用计数 (proxy_id -> count)
    usage_counter: Arc<DashMap<String, usize>>,

    /// 账号到代理的绑定 (account_id -> proxy_id)
    account_bindings: Arc<DashMap<String, String>>,

    /// 轮询索引 (用于 RoundRobin 策略)
    round_robin_index: Arc<AtomicUsize>,

    /// Temporary resolve-once pin: `None` forces upstream/direct (skip pool re-pick).
    forced_egress: Arc<DashMap<String, Option<PoolProxyConfig>>>,

    /// Live request outcome per pool proxy (in-memory only; resets on restart).
    egress_usage: Arc<DashMap<String, EgressUsageStatus>>,

    /// Consecutive successful health probes while RED. GREEN requires
    /// [`GREEN_CONFIRM_SUCCESSES`] in a row so a single lucky 204 cannot flap the node.
    green_confirm_streak: Arc<DashMap<String, u8>>,

    /// Instant the node last transitioned RED → GREEN in this process.
    /// Used to hold traffic until the egress tunnel is completely settled.
    green_at: Arc<DashMap<String, Instant>>,

    /// Monotonic generation counter bumped whenever a proxy turns RED or fails transport,
    /// allowing [`UpstreamClient`] to evict its stale cached HTTP/TLS client pool for that proxy
    /// without touching the proxy configuration or account bindings.
    evict_generation: Arc<DashMap<String, u64>>,
}

/// Health probes that must succeed in a row (each with latency <= [`MAX_HEALTHY_LATENCY_MS`])
/// before a RED node is marked GREEN.
pub const GREEN_CONFIRM_SUCCESSES: u8 = 3;

/// Maximum acceptable latency (ms) for a proxy health probe (`generate_204`).
/// Probes taking longer than 2500ms indicate severe tunnel congestion or packet loss
/// and are treated as unhealthy to prevent mid-stream resets on Google endpoints.
pub const MAX_HEALTHY_LATENCY_MS: u64 = 2500;

/// Next `(is_healthy, confirm_streak)` after one health probe.
fn next_health_state(was_healthy: bool, probe_ok: bool, streak: u8) -> (bool, u8) {
    if was_healthy {
        return if probe_ok { (true, 0) } else { (false, 0) };
    }
    if !probe_ok {
        return (false, 0);
    }
    let streak = streak.saturating_add(1);
    if streak >= GREEN_CONFIRM_SUCCESSES {
        (true, 0)
    } else {
        (false, streak)
    }
}

impl ProxyPoolManager {
    pub fn new(config: Arc<RwLock<ProxyPoolConfig>>) -> Self {
        // 从配置中加载已保存的绑定关系
        let account_bindings = Arc::new(DashMap::new());

        // 使用 blocking 方式读取配置（因为 new 不是 async）
        // 注意：这里使用 try_read 避免死锁
        if let Ok(cfg) = config.try_read() {
            for (account_id, proxy_id) in &cfg.account_bindings {
                account_bindings.insert(account_id.clone(), proxy_id.clone());
            }
            if !cfg.account_bindings.is_empty() {
                tracing::info!(
                    "[ProxyPool] Loaded {} account bindings from config",
                    cfg.account_bindings.len()
                );
            }
        }

        Self {
            config,
            usage_counter: Arc::new(DashMap::new()),
            account_bindings,
            round_robin_index: Arc::new(AtomicUsize::new(0)),
            forced_egress: Arc::new(DashMap::new()),
            egress_usage: Arc::new(DashMap::new()),
            green_confirm_streak: Arc::new(DashMap::new()),
            green_at: Arc::new(DashMap::new()),
            evict_generation: Arc::new(DashMap::new()),
        }
    }

    fn bump_evict_generation_map(map: &DashMap<String, u64>, proxy_id: &str) {
        let mut entry = map.entry(proxy_id.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
    }

    /// Current client-cache eviction generation for `proxy_id` (0 if never turned RED).
    pub fn proxy_evict_generation(&self, proxy_id: &str) -> u64 {
        self.evict_generation
            .get(proxy_id)
            .map(|v| *v.value())
            .unwrap_or(0)
    }

    fn note_proxy_turned_red(&self, proxy_id: &str) {
        self.green_confirm_streak.remove(proxy_id);
        self.green_at.remove(proxy_id);
        Self::bump_evict_generation_map(&self.evict_generation, proxy_id);
    }

    fn note_proxy_turned_green(&self, proxy_id: &str) {
        self.green_confirm_streak.remove(proxy_id);
        self.green_at
            .insert(proxy_id.to_string(), Instant::now());
        Self::bump_evict_generation_map(&self.evict_generation, proxy_id);
    }

    /// Seconds since this account's dedicated pool node last recovered GREEN.
    /// `None` if the account is unbound, the node never recovered this process, or the stamp is gone.
    pub fn dedicated_recovery_elapsed_secs(&self, account_id: &str) -> Option<u64> {
        let proxy_id = self.account_bindings.get(account_id)?.value().clone();
        let stamped = self.green_at.get(&proxy_id)?;
        Some(stamped.elapsed().as_secs())
    }

    /// True when this account is pinned to a dedicated pool node (must not change egress).
    pub fn has_dedicated_binding(&self, account_id: &str) -> bool {
        self.account_bindings.contains_key(account_id)
    }

    /// Record live egress outcome for a pool proxy. Direct/upstream (no entry_id) is ignored.
    pub fn record_egress_usage(&self, proxy_id: &str, status: EgressUsageStatus) {
        if proxy_id.is_empty() {
            return;
        }
        self.egress_usage
            .insert(proxy_id.to_string(), status);
        tracing::debug!(
            "[ProxyPool] egress usage {:?} for proxy {}",
            status,
            proxy_id
        );
        if status == EgressUsageStatus::Failed {
            Self::bump_evict_generation_map(&self.evict_generation, proxy_id);
            let pool_config = Arc::clone(&self.config);
            let streak = Arc::clone(&self.green_confirm_streak);
            let green_at = Arc::clone(&self.green_at);
            let pid = proxy_id.to_string();
            tokio::spawn(async move {
                let mut changed = false;
                {
                    let mut config = pool_config.write().await;
                    if let Some(proxy) = config.proxies.iter_mut().find(|p| p.id == pid) {
                        if proxy.is_healthy {
                            tracing::warn!(
                                "[ProxyPool] Marking proxy {} ({}) as UNHEALTHY (RED) due to transport failure",
                                proxy.name,
                                proxy.id
                            );
                            proxy.is_healthy = false;
                            proxy.last_check_time = Some(chrono::Utc::now().timestamp());
                            changed = true;
                        }
                    }
                }
                streak.remove(&pid);
                green_at.remove(&pid);
                if changed {
                    if let Ok(mut app_config) = crate::modules::config::load_app_config() {
                        let config = pool_config.read().await;
                        app_config.proxy.proxy_pool = config.clone();
                        let _ = crate::modules::config::save_app_config(&app_config);
                    }
                }
            });
        }
    }

    /// Mark a proxy as unhealthy (red) immediately and persist configuration
    pub async fn mark_proxy_unhealthy(&self, proxy_id: &str) {
        if proxy_id.is_empty() {
            return;
        }
        self.record_egress_usage(proxy_id, EgressUsageStatus::Failed);
        let mut changed = false;
        {
            let mut config = self.config.write().await;
            if let Some(proxy) = config.proxies.iter_mut().find(|p| p.id == proxy_id) {
                if proxy.is_healthy {
                    tracing::warn!(
                        "[ProxyPool] Explicitly marking proxy {} ({}) as UNHEALTHY (RED)",
                        proxy.name,
                        proxy.id
                    );
                    proxy.is_healthy = false;
                    proxy.last_check_time = Some(chrono::Utc::now().timestamp());
                    changed = true;
                }
            }
        }
        self.note_proxy_turned_red(proxy_id);
        if changed {
            self.persist_config().await;
        }
    }

    /// 持久化当前代理池配置到磁盘
    pub async fn persist_config(&self) {
        if let Ok(mut app_config) = crate::modules::config::load_app_config() {
            let config = self.config.read().await;
            app_config.proxy.proxy_pool = config.clone();
            if let Err(e) = crate::modules::config::save_app_config(&app_config) {
                tracing::error!("[ProxyPool] Failed to persist proxy pool config: {}", e);
            }
        }
    }

    /// 严格代理模式：检查指定账号是否拥有健康可用的代理。绝不允许直连！
    /// 1. 若账号专属绑定了代理，则绑定的代理必须存在、已启用且 is_healthy == true。
    /// 2. 若账号未绑定代理，但代理池开启，且池中有至少一个未被专属绑定、已启用且 is_healthy == true 的节点。
    /// 3. 若配置了全局上游代理 (UpstreamProxy) 且已启用，也视为可用。
    /// 4. 否则（直连 Direct）一律视为不可用 (false)！
    pub async fn is_account_proxy_available(&self, account_id: &str) -> bool {
        let config = self.config.read().await;

        // 1. 如果账号有专属绑定
        if let Some(proxy_id) = self.account_bindings.get(account_id) {
            if let Some(entry) = config.proxies.iter().find(|p| p.id == *proxy_id.value()) {
                return entry.enabled && entry.is_healthy;
            }
            return false;
        }

        // 2. 如果账号未绑定，检查代理池是否有可用的公用节点
        if config.enabled {
            let bound_ids: std::collections::HashSet<String> = self
                .account_bindings
                .iter()
                .map(|kv| kv.value().clone())
                .collect();
            let has_healthy_unbound = config.proxies.iter().any(|p| {
                p.enabled && p.is_healthy && !bound_ids.contains(&p.id)
            });
            if has_healthy_unbound {
                return true;
            }
        }

        // 3. 检查是否有启用的全局上游代理
        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                return true;
            }
        }

        false
    }

    /// Snapshot of known usage statuses (`ok` / `failed`). Absent ids = unknown.
    pub fn get_egress_usage_snapshot(&self) -> HashMap<String, EgressUsageStatus> {
        self.egress_usage
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
    }

    /// [NEW] 为指定账号获取“最终生效”的 HttpClient
    /// 逻辑：
    /// 1. 账号显式绑定代理优先 (Account-Proxy Binding)
    /// 2. 如果无绑定，且开启了“自动全局”，取池中第一个节点
    /// 3. 如果以上均无，则检查全局上游代理 (Upstream Proxy)
    /// [STRICT PROXY] 若最终未匹配到任何代理，返回“黑洞客户端”（禁止直连）
    pub async fn get_effective_client(
        &self,
        account_id: Option<&str>,
        timeout_secs: u64,
    ) -> Client {
        let mut builder = crate::utils::http::apply_chrome_identity(
            Client::builder().timeout(Duration::from_secs(timeout_secs)),
        );

        // 尝试获取代理配置
        let proxy_opt = if let Some(acc_id) = account_id {
            self.get_proxy_for_account(acc_id).await.ok().flatten()
        } else {
            // 没有 account_id 的通用请求，如果代理池启用，则默认从中选择节点作为出口
            let config = self.config.read().await;
            if config.enabled {
                let res = self.select_proxy_from_pool(&config).await.ok().flatten();
                if let Some(ref p) = res {
                    tracing::info!(
                        "[Proxy] Route: Generic Request -> Proxy {} (Pool)",
                        p.entry_id
                    );
                } else {
                    // [FIX #1583] 明确记录池中无可用代理的情况
                    tracing::warn!("[Proxy] Route: Generic Request -> No available proxy in pool, falling back to upstream");
                }
                res
            } else {
                tracing::debug!("[Proxy] Route: Generic Request -> Proxy pool disabled");
                None
            }
        };

        if let Some(proxy_cfg) = proxy_opt {
            builder = builder.proxy(proxy_cfg.proxy);
            // Already logged more detail in get_proxy_for_account or pool selection
        } else if account_id
            .map(|id| self.has_dedicated_binding(id))
            .unwrap_or(false)
        {
            tracing::error!(
                "[Proxy] Route: {} bound proxy unavailable (RED). Strict policy: BLACKHOLE, refuse upstream fallback to keep egress sticky.",
                account_id.unwrap_or("unknown")
            );
            return crate::utils::http::build_blackhole_client(
                "bound proxy unavailable: refuse upstream fallback",
            );
        } else {
            // Fallback 到应用配置的单上游代理
            if let Ok(app_cfg) = crate::modules::config::load_app_config() {
                let up = app_cfg.proxy.upstream_proxy;
                if up.enabled && !up.url.is_empty() {
                    match rquest::Proxy::all(&up.url) {
                        Ok(p) => {
                            tracing::info!(
                                "[Proxy] Route: {:?} -> Upstream: {} (AppConfig)",
                                account_id.unwrap_or("Generic"),
                                up.url
                            );
                            builder = builder.proxy(p);
                        }
                        Err(e) => {
                            tracing::error!(
                                "[Proxy] Upstream proxy URL invalid ({}): {}. Strict policy: refuse direct!",
                                up.url, e
                            );
                            return crate::utils::http::build_blackhole_client(&format!(
                                "upstream proxy URL invalid: {}",
                                up.url
                            ));
                        }
                    }
                } else {
                    tracing::error!(
                        "[Proxy] Route: {:?} -> NO proxy available (pool empty & upstream disabled). Strict policy: BLACKHOLE, refuse direct!",
                        account_id.unwrap_or("Generic")
                    );
                    return crate::utils::http::build_blackhole_client(
                        "no proxy available: pool empty & upstream disabled",
                    );
                }
            } else {
                tracing::error!(
                    "[Proxy] Failed to read app config for upstream fallback. Strict policy: BLACKHOLE, refuse direct!"
                );
                return crate::utils::http::build_blackhole_client(
                    "failed to load app config for upstream fallback",
                );
            }
        }

        match builder.build() {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(
                    "[Proxy] Client build failed ({}). Strict policy: BLACKHOLE, refuse bare client!",
                    e
                );
                crate::utils::http::build_blackhole_client(&format!(
                    "client build failed: {}",
                    e
                ))
            }
        }
    }

    /// Same egress selection as [`Self::get_effective_client`], Chrome132 identity (quota / OAuth).
    pub async fn get_effective_standard_client(
        &self,
        account_id: Option<&str>,
        timeout_secs: u64,
    ) -> Client {
        self.get_effective_standard_client_with_route(account_id, timeout_secs)
            .await
            .0
    }

    /// Same egress selection as `get_effective_standard_client`, plus the resolved route label.
    pub async fn get_effective_standard_client_with_route(
        &self,
        account_id: Option<&str>,
        timeout_secs: u64,
    ) -> (Client, EgressRoute) {
        let mut builder = crate::utils::http::apply_chrome_identity(
            Client::builder().timeout(Duration::from_secs(timeout_secs)),
        );

        // 尝试获取代理配置
        let proxy_opt = if let Some(acc_id) = account_id {
            self.get_proxy_for_account(acc_id).await.ok().flatten()
        } else {
            // 没有 account_id 的通用请求，如果代理池启用，则默认从中选择节点作为出口
            let config = self.config.read().await;
            if config.enabled {
                let res = self.select_proxy_from_pool(&config).await.ok().flatten();
                if let Some(ref p) = res {
                    tracing::info!(
                        "[Proxy] Route: Generic Request (Standard Client) -> Proxy {} (Pool)",
                        p.entry_id
                    );
                } else {
                    tracing::warn!("[Proxy] Route: Generic Request (Standard Client) -> No available proxy in pool, falling back to upstream or direct");
                }
                res
            } else {
                tracing::debug!(
                    "[Proxy] Route: Generic Request (Standard Client) -> Proxy pool disabled"
                );
                None
            }
        };

        let route = if let Some(proxy_cfg) = proxy_opt {
            let entry_id = proxy_cfg.entry_id.clone();
            builder = builder.proxy(proxy_cfg.proxy);
            EgressRoute::Pool { entry_id }
        } else if account_id
            .map(|id| self.has_dedicated_binding(id))
            .unwrap_or(false)
        {
            tracing::error!(
                "[Proxy] Route: {} (Standard Client) bound proxy unavailable (RED). Strict policy: BLACKHOLE, refuse upstream fallback.",
                account_id.unwrap_or("unknown")
            );
            return (
                crate::utils::http::build_blackhole_client(
                    "bound proxy unavailable: refuse upstream fallback (standard client)",
                ),
                EgressRoute::Direct,
            );
        } else {
            // Fallback 到应用配置的单上游代理
            let mut resolved = EgressRoute::Direct;
            if let Ok(app_cfg) = crate::modules::config::load_app_config() {
                let up = app_cfg.proxy.upstream_proxy;
                if up.enabled && !up.url.is_empty() {
                    match rquest::Proxy::all(&up.url) {
                        Ok(p) => {
                            tracing::info!(
                                "[Proxy] Route: {:?} (Standard Client) -> Upstream: {} (AppConfig)",
                                account_id.unwrap_or("Generic"),
                                up.url
                            );
                            builder = builder.proxy(p);
                            resolved = EgressRoute::Upstream;
                        }
                        Err(e) => {
                            tracing::error!(
                                "[Proxy] (Standard Client) upstream proxy URL invalid ({}): {}. Strict policy: refuse direct!",
                                up.url, e
                            );
                            return (
                                crate::utils::http::build_blackhole_client(&format!(
                                    "upstream proxy URL invalid: {}",
                                    up.url
                                )),
                                EgressRoute::Direct,
                            );
                        }
                    }
                } else {
                    tracing::error!(
                        "[Proxy] Route: {:?} (Standard Client) -> NO proxy available (pool empty & upstream disabled). Strict policy: BLACKHOLE, refuse direct!",
                        account_id.unwrap_or("Generic")
                    );
                    // [STRICT PROXY] 双保险：即使调用方漏检 route，黑洞客户端也无法直连
                    return (
                        crate::utils::http::build_blackhole_client(
                            "no proxy available: pool empty & upstream disabled (standard client)",
                        ),
                        EgressRoute::Direct,
                    );
                }
            } else {
                tracing::error!(
                    "[Proxy] (Standard Client) failed to read app config for upstream fallback. Strict policy: BLACKHOLE, refuse direct!"
                );
                return (
                    crate::utils::http::build_blackhole_client(
                        "failed to load app config for upstream fallback (standard client)",
                    ),
                    EgressRoute::Direct,
                );
            }
            resolved
        };

        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "[Proxy] (Standard Client) client build failed ({}). Strict policy: BLACKHOLE, refuse bare client!",
                    e
                );
                crate::utils::http::build_blackhole_client(&format!(
                    "standard client build failed: {}",
                    e
                ))
            }
        };
        (client, route)
    }

    fn fallback_egress_key() -> String {
        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                return "upstream".to_string();
            }
        }
        "direct".to_string()
    }

    fn egress_key_from_proxy(proxy: &Option<PoolProxyConfig>) -> String {
        match proxy {
            Some(p) => format!("proxy:{}", p.entry_id),
            None => Self::fallback_egress_key(),
        }
    }

    /// Pin resolved egress for nested account ops (batch refresh resolve-once).
    pub fn pin_account_egress(
        self: &Arc<Self>,
        account_id: String,
        proxy: Option<PoolProxyConfig>,
    ) -> AccountEgressGuard {
        self.forced_egress.insert(account_id.clone(), proxy);
        AccountEgressGuard {
            pool: Arc::clone(self),
            account_id,
        }
    }

    /// Resolve AI-identical egress once, pin it, and return the batch semaphore key.
    pub async fn resolve_and_pin_egress(
        self: &Arc<Self>,
        account_id: &str,
    ) -> (String, AccountEgressGuard) {
        let proxy = self
            .get_proxy_for_account(account_id)
            .await
            .ok()
            .flatten();
        let key = Self::egress_key_from_proxy(&proxy);
        let guard = self.pin_account_egress(account_id.to_string(), proxy);
        (key, guard)
    }

    /// 批量额度刷新用的出口键：与 AI 同源（bound → pool → upstream → direct）
    pub async fn egress_key_for_account(&self, account_id: &str) -> String {
        let proxy = self
            .get_proxy_for_account(account_id)
            .await
            .ok()
            .flatten();
        Self::egress_key_from_proxy(&proxy)
    }

    /// 为账号获取代理（与 AI 反代相同：bound → 未绑定池选择）
    pub async fn get_proxy_for_account(
        &self,
        account_id: &str,
    ) -> Result<Option<PoolProxyConfig>, String> {
        // Resolve-once pin for nested ops within a batch task
        if let Some(pinned) = self.forced_egress.get(account_id) {
            return Ok(pinned.value().clone());
        }

        let config = self.config.read().await;

        if !config.enabled || config.proxies.is_empty() {
            return Ok(None);
        }

        // 1. 优先使用账号绑定 (专属 IP)
        if self.account_bindings.contains_key(account_id) {
            if let Some(proxy) = self.get_bound_proxy(account_id, &config).await? {
                tracing::info!(
                    "[Proxy] Route: Account {} -> Proxy {} (Bound)",
                    account_id,
                    proxy.entry_id
                );
                return Ok(Some(proxy));
            } else {
                tracing::warn!(
                    "[Proxy] Account {} bound proxy is disabled or UNHEALTHY (RED). Strict policy: refuse fallback or leak direct IP.",
                    account_id
                );
                return Ok(None);
            }
        }

        // 2. 否则从池中策略选择 (公用池)
        let res = self.select_proxy_from_pool(&config).await?;
        if let Some(ref p) = res {
            tracing::info!(
                "[Proxy] Route: Account {} -> Proxy {} (Pool)",
                account_id,
                p.entry_id
            );
        }
        Ok(res)
    }

    /// 获取账号绑定的代理
    async fn get_bound_proxy(
        &self,
        account_id: &str,
        config: &ProxyPoolConfig,
    ) -> Result<Option<PoolProxyConfig>, String> {
        if let Some(proxy_id) = self.account_bindings.get(account_id) {
            if let Some(entry) = config.proxies.iter().find(|p| p.id == *proxy_id.value()) {
                if entry.enabled {
                    if !entry.is_healthy {
                        tracing::warn!(
                            "[ProxyPool] Bound proxy {} for account {} is UNHEALTHY (RED)",
                            entry.id,
                            account_id
                        );
                        return Ok(None);
                    }
                    return Ok(Some(self.build_proxy_config(entry)?));
                }
            }
        }
        Ok(None)
    }

    /// 从代理池中选择代理
    async fn select_proxy_from_pool(
        &self,
        config: &ProxyPoolConfig,
    ) -> Result<Option<PoolProxyConfig>, String> {
        // [FIX] 专属隔离逻辑：剔除所有已被绑定的代理，保护专属 IP 账号的安全
        let bound_ids: std::collections::HashSet<String> = self
            .account_bindings
            .iter()
            .map(|kv| kv.value().clone())
            .collect();

        let healthy_proxies: Vec<_> = config
            .proxies
            .iter()
            .filter(|p| {
                if !p.enabled {
                    return false;
                }
                if config.auto_failover && !p.is_healthy {
                    return false;
                }
                // 如果该代理已被某个账号“专属绑定”，则不再参与公用轮询
                if bound_ids.contains(&p.id) {
                    return false;
                }
                true
            })
            .collect();

        if healthy_proxies.is_empty() {
            // 如果所有代理都被绑定了，或者池本身为空，尝试返回池中开启了且不依赖绑定的代理
            // (这里可以根据业务进一步调整，目前保持严谨隔离)
            return Ok(None);
        }

        let selected = match config.strategy {
            ProxySelectionStrategy::RoundRobin => self.select_round_robin(&healthy_proxies),
            ProxySelectionStrategy::Random => self.select_random(&healthy_proxies),
            ProxySelectionStrategy::Priority => self.select_by_priority(&healthy_proxies),
            ProxySelectionStrategy::LeastConnections => {
                self.select_least_connections(&healthy_proxies)
            }
            ProxySelectionStrategy::WeightedRoundRobin => self.select_weighted(&healthy_proxies),
        };

        if let Some(entry) = selected {
            // 更新计数
            *self.usage_counter.entry(entry.id.clone()).or_insert(0) += 1;
            Ok(Some(self.build_proxy_config(entry)?))
        } else {
            Ok(None)
        }
    }

    fn select_round_robin<'a>(&self, proxies: &[&'a ProxyEntry]) -> Option<&'a ProxyEntry> {
        if proxies.is_empty() {
            return None;
        }
        let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed);
        Some(proxies[index % proxies.len()])
    }

    fn select_random<'a>(&self, proxies: &[&'a ProxyEntry]) -> Option<&'a ProxyEntry> {
        if proxies.is_empty() {
            return None;
        }
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        proxies.choose(&mut rng).copied()
    }

    fn select_by_priority<'a>(&self, proxies: &[&'a ProxyEntry]) -> Option<&'a ProxyEntry> {
        // priority 越小越优先
        proxies.iter().min_by_key(|p| p.priority).copied()
    }

    fn select_least_connections<'a>(&self, proxies: &[&'a ProxyEntry]) -> Option<&'a ProxyEntry> {
        proxies
            .iter()
            .min_by_key(|p| self.usage_counter.get(&p.id).map(|v| *v).unwrap_or(0))
            .copied()
    }

    fn select_weighted<'a>(&self, proxies: &[&'a ProxyEntry]) -> Option<&'a ProxyEntry> {
        // 简单实现: 类似优先级的加权, 这里暂用 Priority 代替
        self.select_by_priority(proxies)
    }

    /// 构建 reqwest::Proxy 配置
    fn build_proxy_config(&self, entry: &ProxyEntry) -> Result<PoolProxyConfig, String> {
        let raw_url = crate::proxy::config::normalize_proxy_url(&entry.url);

        // Prefer explicit auth fields; also peel userinfo out of the URL so batch-imported
        // `http://user:pass@host:port` entries authenticate reliably via basic_auth.
        let (url_for_proxy, url_user, url_pass) =
            crate::proxy::config::split_proxy_url_credentials(&raw_url);

        let mut proxy = rquest::Proxy::all(&url_for_proxy)
            .map_err(|e| format!("Invalid proxy URL: {}", e))?;

        if let Some(auth) = &entry.auth {
            if !auth.username.is_empty() {
                proxy = proxy.basic_auth(&auth.username, &auth.password);
            }
        } else if let Some(user) = url_user {
            proxy = proxy.basic_auth(&user, url_pass.as_deref().unwrap_or(""));
        }

        Ok(PoolProxyConfig {
            proxy,
            entry_id: entry.id.clone(),
        })
    }

    /// 绑定账号到代理（内存校验 + 可选写盘）
    async fn bind_account_to_proxy_inner(
        &self,
        account_id: String,
        proxy_id: String,
        persist: bool,
    ) -> Result<(), String> {
        // 检查代理是否存在
        {
            let config = self.config.read().await;
            if !config.proxies.iter().any(|p| p.id == proxy_id) {
                return Err(format!("Proxy {} not found", proxy_id));
            }

            // 检查代理最大账号数限制
            if let Some(entry) = config.proxies.iter().find(|p| p.id == proxy_id) {
                if let Some(max) = entry.max_accounts {
                    if max > 0 {
                        // upsert：同账号换绑到同一代理不占用额外名额
                        let already_on_proxy = self
                            .account_bindings
                            .get(&account_id)
                            .map(|v| *v.value() == proxy_id)
                            .unwrap_or(false);
                        if !already_on_proxy {
                            let current_count = self
                                .account_bindings
                                .iter()
                                .filter(|kv| *kv.value() == proxy_id)
                                .count();
                            if current_count >= max {
                                return Err(format!(
                                    "Proxy {} has reached max accounts limit",
                                    proxy_id
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 更新内存中的绑定
        self.account_bindings
            .insert(account_id.clone(), proxy_id.clone());

        if persist {
            self.persist_bindings().await;
        }

        tracing::info!(
            "[ProxyPool] Bound account {} to proxy {}",
            account_id,
            proxy_id
        );
        Ok(())
    }

    /// 绑定账号到代理
    pub async fn bind_account_to_proxy(
        &self,
        account_id: String,
        proxy_id: String,
    ) -> Result<(), String> {
        self.bind_account_to_proxy_inner(account_id, proxy_id, true)
            .await
    }

    /// 批量绑定（upsert）；单行失败不回滚，成功项统一写盘一次
    pub async fn bind_accounts_batch(
        &self,
        entries: Vec<(String, String)>,
    ) -> BatchBindResult {
        let mut applied = Vec::new();
        let mut errors = Vec::new();

        for (account_id, proxy_id) in entries {
            match self
                .bind_account_to_proxy_inner(account_id.clone(), proxy_id.clone(), false)
                .await
            {
                Ok(()) => applied.push(BatchBindApplied {
                    account_id,
                    proxy_id,
                }),
                Err(message) => errors.push(BatchBindError {
                    account_id,
                    proxy_id,
                    message,
                }),
            }
        }

        if !applied.is_empty() {
            self.persist_bindings().await;
        }

        BatchBindResult {
            ok: errors.is_empty(),
            applied_count: applied.len(),
            error_count: errors.len(),
            applied,
            errors,
        }
    }

    /// 号池健康聚合快照（不触发探测）
    pub async fn pool_health_snapshot(
        &self,
        account_ids: &[String],
    ) -> PoolHealthSnapshot {
        let config = self.config.read().await;
        let bindings = self.get_all_bindings_snapshot();

        let unhealthy_proxies: Vec<UnhealthyProxyInfo> = config
            .proxies
            .iter()
            .filter(|p| !p.is_healthy)
            .map(|p| UnhealthyProxyInfo {
                id: p.id.clone(),
                name: Some(p.name.clone()),
                latency_ms: p.latency,
            })
            .collect();

        let unhealthy_ids: std::collections::HashSet<String> =
            unhealthy_proxies.iter().map(|p| p.id.clone()).collect();

        let bindings_on_unhealthy: Vec<BindingOnUnhealthy> = bindings
            .iter()
            .filter(|(_, proxy_id)| unhealthy_ids.contains(*proxy_id))
            .map(|(account_id, proxy_id)| BindingOnUnhealthy {
                account_id: account_id.clone(),
                proxy_id: proxy_id.clone(),
            })
            .collect();

        let unbound_account_ids: Vec<String> = account_ids
            .iter()
            .filter(|id| !bindings.contains_key(*id))
            .cloned()
            .collect();

        PoolHealthSnapshot {
            unbound_account_ids,
            unhealthy_proxies,
            bindings_on_unhealthy,
            bound_count: bindings.len(),
            proxy_count: config.proxies.len(),
            account_count: account_ids.len(),
        }
    }

    /// 解绑账号代理
    pub async fn unbind_account_proxy(&self, account_id: String) {
        self.account_bindings.remove(&account_id);

        // 持久化到配置文件
        self.persist_bindings().await;

        tracing::info!("[ProxyPool] Unbound account {}", account_id);
    }

    /// 获取账号当前绑定的代理ID
    pub fn get_account_binding(&self, account_id: &str) -> Option<String> {
        self.account_bindings
            .get(account_id)
            .map(|v| v.value().clone())
    }

    /// 获取所有绑定关系的快照
    pub fn get_all_bindings_snapshot(&self) -> std::collections::HashMap<String, String> {
        self.account_bindings
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect()
    }

    /// [HOT-RELOAD] Re-sync the in-memory DashMap from `config.account_bindings`.
    /// Called after `update_proxy_pool` so that a wholesale ProxyPoolConfig
    /// replacement (e.g. via `save_config`) does not leave the in-memory
    /// bindings stale or empty.
    pub async fn sync_bindings_from_config(&self) {
        let config = self.config.read().await;
        let snapshot = config.account_bindings.clone();
        drop(config);

        // Reset the DashMap: clear old entries, then insert fresh ones.
        self.account_bindings.clear();
        for (account_id, proxy_id) in &snapshot {
            self.account_bindings
                .insert(account_id.clone(), proxy_id.clone());
        }
        tracing::info!(
            "[ProxyPool] Re-synced {} account bindings from config (hot-reload)",
            snapshot.len()
        );
    }

    /// 持久化绑定关系到配置文件
    async fn persist_bindings(&self) {
        // 获取当前绑定快照
        let bindings = self.get_all_bindings_snapshot();

        // 更新配置中的绑定关系
        {
            let mut config = self.config.write().await;
            config.account_bindings = bindings;
        }

        // 保存到磁盘
        if let Ok(mut app_config) = crate::modules::config::load_app_config() {
            let config = self.config.read().await;
            app_config.proxy.proxy_pool = config.clone();
            if let Err(e) = crate::modules::config::save_app_config(&app_config) {
                tracing::error!("[ProxyPool] Failed to persist bindings: {}", e);
            }
        }
    }

    /// 全量健康检查（手动触发 / 低频巡检）：检查所有启用的代理
    pub async fn health_check(&self) -> Result<(), String> {
        self.health_check_filtered(false).await
    }

    /// 双速自愈：只重探当前标记为不健康（RED）的代理，用于快速回绿
    pub async fn health_check_unhealthy(&self) -> Result<(), String> {
        self.health_check_filtered(true).await
    }

    /// 健康检查（内部公共实现）
    /// - only_unhealthy == false: 全量扫描所有启用的代理
    /// - only_unhealthy == true:  只扫描当前 is_healthy == false 的红色节点（自愈路径）
    async fn health_check_filtered(&self, only_unhealthy: bool) -> Result<(), String> {
        // 由于需要异步并发检查，且不能锁住 config 太久，
        // 我们先复制一份需要检查的代理列表
        let proxies_to_check: Vec<_> = {
            let config = self.config.read().await;
            config
                .proxies
                .iter()
                .filter(|p| p.enabled && (!only_unhealthy || !p.is_healthy))
                .cloned()
                .collect()
        };

        if proxies_to_check.is_empty() {
            // 自愈路径下如果没有红色节点，直接返回，无需空转一轮
            return Ok(());
        }

        let concurrency_limit = 20usize;
        let results = stream::iter(proxies_to_check)
            .map(|proxy| async move {
                let (is_healthy, latency) = self.check_proxy_health(&proxy).await;

                let latency_msg = if let Some(ms) = latency {
                    format!("{}ms", ms)
                } else {
                    "-".to_string()
                };

                tracing::info!(
                    "Proxy {} ({}) health check: {} (Latency: {})",
                    proxy.name,
                    proxy.url,
                    if is_healthy { "✓ OK" } else { "✗ FAILED" },
                    latency_msg
                );

                (proxy.id, is_healthy, latency)
            })
            .buffer_unordered(concurrency_limit)
            .collect::<Vec<_>>()
            .await;

        // 统一更新状态
        {
            let mut config = self.config.write().await;
            for (id, probe_ok, latency) in &results {
                if let Some(proxy) = config.proxies.iter_mut().find(|p| &p.id == id) {
                    let was_healthy = proxy.is_healthy;
                    let prev_streak = self
                        .green_confirm_streak
                        .get(id)
                        .map(|v| *v)
                        .unwrap_or(0);
                    let (new_healthy, new_streak) =
                        next_health_state(was_healthy, *probe_ok, prev_streak);
                    if new_streak == 0 {
                        self.green_confirm_streak.remove(id);
                    } else {
                        self.green_confirm_streak.insert(id.clone(), new_streak);
                    }

                    proxy.is_healthy = new_healthy;
                    proxy.latency = *latency;
                    proxy.last_check_time = Some(chrono::Utc::now().timestamp());

                    // [NET-OBS] 网络波动观察：代理健康状态翻转日志（RED→GREEN 表示恢复；GREEN→RED 表示劣化）
                    if was_healthy && !new_healthy {
                        self.note_proxy_turned_red(id);
                        tracing::warn!(
                            proxy_id = %proxy.id,
                            proxy_name = %proxy.name,
                            proxy_url = %proxy.url,
                            latency_ms = latency.unwrap_or(0),
                            "[NET-OBS] proxy turned RED (health check): {} ({})",
                            proxy.name,
                            proxy.id
                        );
                    } else if !was_healthy && new_healthy {
                        self.note_proxy_turned_green(id);
                        tracing::info!(
                            proxy_id = %proxy.id,
                            proxy_name = %proxy.name,
                            proxy_url = %proxy.url,
                            latency_ms = latency.unwrap_or(0),
                            "[NET-OBS] proxy recovered GREEN after {} consecutive probes: {} ({})",
                            GREEN_CONFIRM_SUCCESSES,
                            proxy.name,
                            proxy.id
                        );
                    } else if !was_healthy && *probe_ok && !new_healthy {
                        tracing::info!(
                            proxy_id = %proxy.id,
                            proxy_name = %proxy.name,
                            confirm = new_streak,
                            required = GREEN_CONFIRM_SUCCESSES,
                            "[NET-OBS] proxy {} still RED, waiting for consecutive health successes ({}/{})",
                            proxy.name,
                            new_streak,
                            GREEN_CONFIRM_SUCCESSES
                        );
                    }
                }
            }
        }
        for (id, _, _) in &results {
            let healthy = {
                let config = self.config.read().await;
                config
                    .proxies
                    .iter()
                    .find(|p| &p.id == id)
                    .map(|p| p.is_healthy)
                    .unwrap_or(false)
            };
            self.egress_usage.insert(
                id.clone(),
                if healthy {
                    EgressUsageStatus::Ok
                } else {
                    EgressUsageStatus::Failed
                },
            );
        }
        self.persist_config().await;

        Ok(())
    }

    /// 检查单个代理健康状态
    async fn check_proxy_health(&self, entry: &ProxyEntry) -> (bool, Option<u64>) {
        let check_url = if let Some(url) = &entry.health_check_url {
            if url.trim().is_empty() {
                "http://cp.cloudflare.com/generate_204"
            } else {
                url.as_str()
            }
        } else {
            "http://cp.cloudflare.com/generate_204"
        };

        // 尝试构建 Client，如果失败直接视为不健康
        let proxy_res = self.build_proxy_config(entry);
        if let Err(e) = proxy_res {
            tracing::error!("Proxy {} build config failed: {}", entry.url, e);
            return (false, None);
        }
        let proxy_cfg = proxy_res.unwrap();

        // Probe timeout 8s; healthy threshold requires latency <= MAX_HEALTHY_LATENCY_MS (2500ms).
        let client_result = Client::builder()
            .proxy(proxy_cfg.proxy)
            .emulation(Emulation::Chrome132)
            .timeout(Duration::from_secs(8))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36")
            .build();

        let client = match client_result {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Proxy {} build client failed: {}", entry.url, e);
                return (false, None);
            }
        };

        let start = std::time::Instant::now();
        match client.get(check_url).send().await {
            Ok(resp) => {
                let latency = start.elapsed().as_millis() as u64;
                if resp.status().is_success() {
                    if latency <= MAX_HEALTHY_LATENCY_MS {
                        (true, Some(latency))
                    } else {
                        tracing::warn!(
                            "Proxy {} ({}) health check latency too high: {}ms > {}ms threshold (treated as UNHEALTHY)",
                            entry.name,
                            entry.url,
                            latency,
                            MAX_HEALTHY_LATENCY_MS
                        );
                        (false, Some(latency))
                    }
                } else {
                    tracing::warn!(
                        "Proxy {} health check status error: {} ({}ms)",
                        entry.url,
                        resp.status(),
                        latency
                    );
                    // Keep elapsed so UI can show "unreachable" with timing, not a blank "timeout".
                    (false, Some(latency))
                }
            }
            Err(e) => {
                let latency = start.elapsed().as_millis() as u64;
                tracing::warn!(
                    "Proxy {} health check request failed: {} ({}ms)",
                    entry.url,
                    e,
                    latency
                );
                (false, Some(latency))
            }
        }
    }

    /// 红色节点递增退避重探间隔（秒）：
    /// 变红后依次按 10s → 15s → 20s → 30s 重探，配合 GREEN_CONFIRM_SUCCESSES = 3（且每次 <= 2500ms），
    /// 确保代理在断流后必须经过至少 45s 的连续稳定低延迟探测才允许回绿，杜绝频繁闪断带病接单。
    const RED_RECHECK_BACKOFF_SECS: [u64; 4] = [10, 15, 20, 30];

    /// 启动健康检查循环（退避自愈探测）
    ///
    /// 自愈策略：
    /// - 池中有红色（不健康）节点 → 只快速重探红色节点，探测间隔按 3s→6s→9s→20s 递增退避，
    ///   探测成功后立即回绿、账号解冻并重置退避进度；探测失败则沿退避阶梯继续，直至全部恢复；
    /// - 全部节点健康 → 全量低频巡检（health_check_interval，默认 300s），
    ///   期间每 5s 检查一次是否有节点被业务请求拉红，一旦变红立即打断低频睡眠进入退避重探。
    pub fn start_health_check_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            tracing::info!("Starting proxy pool health check loop (backoff recheck)...");
            // 连续红色轮次：决定下一次退避间隔档位（0 = 无红色）
            let mut consecutive_red_rounds: usize = 0;

            loop {
                // 读取当前状态：代理池是否启用、红色节点数量、配置的巡检间隔
                let (enabled, red_count, base_interval) = {
                    let cfg = self.config.read().await;
                    let red_count = if cfg.enabled {
                        cfg.proxies
                            .iter()
                            .filter(|p| p.enabled && !p.is_healthy)
                            .count()
                    } else {
                        0
                    };
                    (cfg.enabled, red_count, cfg.health_check_interval)
                };

                if enabled {
                    if red_count > 0 {
                        // 退避自愈：只重探红色节点，快速回绿
                        if let Err(e) = self.health_check_unhealthy().await {
                            tracing::error!("Proxy pool red-node recheck failed: {}", e);
                        }
                    } else {
                        // 全绿：全量低频巡检
                        if let Err(e) = self.health_check().await {
                            tracing::error!("Proxy pool health check failed: {}", e);
                        }
                    }
                }

                // 检查后重新统计：是否仍有红色残留（决定退避递增 or 重置）
                let still_red = {
                    let cfg = self.config.read().await;
                    cfg.proxies
                        .iter()
                        .filter(|p| p.enabled && !p.is_healthy)
                        .count()
                };
                consecutive_red_rounds = if still_red > 0 {
                    (consecutive_red_rounds + 1).min(Self::RED_RECHECK_BACKOFF_SECS.len())
                } else {
                    0
                };

                // 计算下一次探测间隔（sleep AFTER check）
                let interval_secs = if !enabled {
                    60
                } else if consecutive_red_rounds > 0 {
                    // 退避阶梯：3s → 6s → 9s → 20s（封顶 20s，持续退避直至恢复）
                    Self::RED_RECHECK_BACKOFF_SECS[consecutive_red_rounds - 1]
                } else {
                    base_interval.max(30)
                };

                // 长档睡眠拆分为 5s 片段：感知到运行中代理被业务请求拉红时立即打断进入退避探测
                if interval_secs > 30 {
                    let mut slept: u64 = 0;
                    while slept < interval_secs {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        slept += 5;
                        let cfg = self.config.read().await;
                        let suddenly_red = cfg
                            .proxies
                            .iter()
                            .any(|p| p.enabled && !p.is_healthy);
                        if suddenly_red {
                            tracing::warn!(
                                "Proxy turned RED during low-frequency sleep; breaking into backoff recheck"
                            );
                            break;
                        }
                    }
                } else {
                    tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                }
            }
        });
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBindApplied {
    pub account_id: String,
    pub proxy_id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBindError {
    pub account_id: String,
    pub proxy_id: String,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBindResult {
    pub ok: bool,
    pub applied_count: usize,
    pub error_count: usize,
    pub applied: Vec<BatchBindApplied>,
    pub errors: Vec<BatchBindError>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnhealthyProxyInfo {
    pub id: String,
    pub name: Option<String>,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingOnUnhealthy {
    pub account_id: String,
    pub proxy_id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolHealthSnapshot {
    pub unbound_account_ids: Vec<String>,
    pub unhealthy_proxies: Vec<UnhealthyProxyInfo>,
    pub bindings_on_unhealthy: Vec<BindingOnUnhealthy>,
    pub bound_count: usize,
    pub proxy_count: usize,
    pub account_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::{ProxyEntry, ProxyPoolConfig};
    use tokio::sync::RwLock;

    fn test_pool(proxies: Vec<ProxyEntry>) -> ProxyPoolManager {
        let mut cfg = ProxyPoolConfig::default();
        cfg.enabled = true;
        cfg.proxies = proxies;
        ProxyPoolManager::new(Arc::new(RwLock::new(cfg)))
    }

    fn proxy(id: &str, max: Option<usize>) -> ProxyEntry {
        ProxyEntry {
            id: id.to_string(),
            name: id.to_string(),
            url: format!("http://127.0.0.1:{}", 9000),
            auth: None,
            enabled: true,
            priority: 0,
            tags: Vec::new(),
            max_accounts: max,
            health_check_url: None,
            last_check_time: None,
            is_healthy: true,
            latency: None,
        }
    }

    #[tokio::test]
    async fn test_batch_bind_partial_failure() {
        let pool = test_pool(vec![proxy("p1", Some(1))]);
        let result = pool
            .bind_accounts_batch(vec![
                ("a1".into(), "p1".into()),
                ("a2".into(), "missing".into()),
                ("a3".into(), "p1".into()), // may hit max
            ])
            .await;

        assert_eq!(result.applied_count, 1);
        assert_eq!(result.error_count, 2);
        assert!(!result.ok);
        assert_eq!(pool.get_account_binding("a1").as_deref(), Some("p1"));
        assert!(pool.get_account_binding("a2").is_none());
    }

    #[tokio::test]
    async fn test_pool_health_unbound_list() {
        let mut unhealthy = proxy("bad", None);
        unhealthy.is_healthy = false;
        let pool = test_pool(vec![proxy("good", None), unhealthy]);
        let _ = pool
            .bind_account_to_proxy_inner("bound".into(), "bad".into(), false)
            .await;

        let snap = pool
            .pool_health_snapshot(&["bound".into(), "free".into()])
            .await;
        assert_eq!(snap.unbound_account_ids, vec!["free".to_string()]);
        assert_eq!(snap.bound_count, 1);
        assert_eq!(snap.unhealthy_proxies.len(), 1);
        assert_eq!(snap.bindings_on_unhealthy.len(), 1);
        assert_eq!(snap.bindings_on_unhealthy[0].account_id, "bound");
    }

    #[tokio::test]
    async fn test_unbound_ops_use_same_pool_picker_as_ai() {
        let pool = test_pool(vec![proxy("pool-node", None), proxy("spare", None)]);
        let ai = pool.get_proxy_for_account("unbound-acc").await.unwrap();
        assert!(ai.is_some());

        let key = pool.egress_key_for_account("unbound-acc").await;
        assert!(key.starts_with("proxy:"), "unbound with healthy pool should get proxy key, got {key}");

        let _ = pool
            .bind_account_to_proxy_inner("a1".into(), "pool-node".into(), false)
            .await;
        assert_eq!(pool.egress_key_for_account("a1").await, "proxy:pool-node");
    }

    #[tokio::test]
    async fn test_resolve_and_pin_keeps_same_egress() {
        let pool = Arc::new(test_pool(vec![proxy("p1", None), proxy("p2", None)]));

        let (key, _guard) = pool.resolve_and_pin_egress("acc").await;
        assert!(key.starts_with("proxy:"));
        let first = pool.get_proxy_for_account("acc").await.unwrap();
        let second = pool.get_proxy_for_account("acc").await.unwrap();
        assert_eq!(
            first.as_ref().map(|p| p.entry_id.as_str()),
            second.as_ref().map(|p| p.entry_id.as_str())
        );
        assert_eq!(key, format!("proxy:{}", first.as_ref().unwrap().entry_id));
    }

    #[test]
    fn test_egress_route_error_prefix() {
        assert_eq!(
            EgressRoute::Pool {
                entry_id: "node-1".into()
            }
            .error_prefix(),
            "[代理池:node-1]"
        );
        assert_eq!(EgressRoute::Upstream.error_prefix(), "[上游代理]");
        assert_eq!(EgressRoute::Direct.error_prefix(), "[直连]");
    }

    #[tokio::test]
    async fn test_standard_client_with_route_returns_pool() {
        let pool = test_pool(vec![proxy("pool-node", None)]);
        let (_client, route) = pool
            .get_effective_standard_client_with_route(Some("unbound-acc"), 15)
            .await;
        match route {
            EgressRoute::Pool { entry_id } => {
                assert_eq!(entry_id, "pool-node");
            }
            other => panic!("expected Pool route, got {:?}", other),
        }
    }

    #[test]
    fn test_next_health_state_requires_consecutive_success() {
        assert_eq!(next_health_state(true, true, 0), (true, 0));
        assert_eq!(next_health_state(true, false, 0), (false, 0));
        assert_eq!(next_health_state(false, false, 1), (false, 0));
        assert_eq!(next_health_state(false, true, 0), (false, 1));
        assert_eq!(next_health_state(false, true, 1), (false, 2));
        assert_eq!(next_health_state(false, true, 2), (true, 0));
        assert_eq!(next_health_state(false, false, 2), (false, 0));
        assert_eq!(next_health_state(false, false, 0), (false, 0));
    }

    #[tokio::test]
    async fn test_bound_account_does_not_use_pool_or_upstream_when_red() {
        let mut bad = proxy("bound-node", None);
        bad.is_healthy = false;
        let pool = test_pool(vec![bad, proxy("other-healthy", None)]);
        let _ = pool
            .bind_account_to_proxy_inner("pinned".into(), "bound-node".into(), false)
            .await;

        assert!(pool.has_dedicated_binding("pinned"));
        assert!(!pool.has_dedicated_binding("free"));

        let route = pool.get_proxy_for_account("pinned").await.unwrap();
        assert!(
            route.is_none(),
            "bound RED must not pick another pool node"
        );

        let (_client, egress) = pool
            .get_effective_standard_client_with_route(Some("pinned"), 5)
            .await;
        match egress {
            EgressRoute::Direct => {}
            other => panic!("bound RED must fail closed, got {:?}", other),
        }
    }
}
