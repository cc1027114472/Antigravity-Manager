use crate::proxy::config::{ProxyEntry, ProxyPoolConfig, ProxySelectionStrategy};
use dashmap::DashMap;
use futures::{stream, StreamExt};
use rquest::Client;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use rquest_util::Emulation;
use std::sync::OnceLock;

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

/// 代理池管理器
pub struct ProxyPoolManager {
    config: Arc<RwLock<ProxyPoolConfig>>,

    /// 代理使用计数 (proxy_id -> count)
    usage_counter: Arc<DashMap<String, usize>>,

    /// 账号到代理的绑定 (account_id -> proxy_id)
    account_bindings: Arc<DashMap<String, String>>,

    /// 轮询索引 (用于 RoundRobin 策略)
    round_robin_index: Arc<AtomicUsize>,
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
        }
    }

    /// [NEW] 为指定账号获取“最终生效”的 HttpClient
    /// 逻辑：
    /// 1. 账号显式绑定代理优先 (Account-Proxy Binding)
    /// 2. 如果无绑定，且开启了“自动全局”，取池中第一个节点
    /// 3. 如果以上均无，则检查全局上游代理 (Upstream Proxy) [由调用方 fallback]
    pub async fn get_effective_client(
        &self,
        account_id: Option<&str>,
        timeout_secs: u64,
    ) -> Client {
        let mut builder = Client::builder()
            .emulation(Emulation::Chrome123)
            .timeout(Duration::from_secs(timeout_secs));

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
                    tracing::warn!("[Proxy] Route: Generic Request -> No available proxy in pool, falling back to upstream or direct");
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
        } else {
            // Fallback 到应用配置的单上游代理
            if let Ok(app_cfg) = crate::modules::config::load_app_config() {
                let up = app_cfg.proxy.upstream_proxy;
                if up.enabled && !up.url.is_empty() {
                    if let Ok(p) = rquest::Proxy::all(&up.url) {
                        tracing::info!(
                            "[Proxy] Route: {:?} -> Upstream: {} (AppConfig)",
                            account_id.unwrap_or("Generic"),
                            up.url
                        );
                        builder = builder.proxy(p);
                    }
                } else {
                    tracing::info!(
                        "[Proxy] Route: {:?} -> Direct",
                        account_id.unwrap_or("Generic")
                    );
                }
            }
        }

        builder.build().unwrap_or_else(|_| Client::new())
    }

    /// [NEW] 为指定账号获取“最终生效”的无特征 Standard HttpClient (专门用于纯净场景，如 OAuth 退还)
    pub async fn get_effective_standard_client(
        &self,
        account_id: Option<&str>,
        timeout_secs: u64,
    ) -> Client {
        let mut builder = Client::builder()
            // 无 Emulation 设置，走纯正的基础 TLS 指纹
            .timeout(Duration::from_secs(timeout_secs));

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

        if let Some(proxy_cfg) = proxy_opt {
            builder = builder.proxy(proxy_cfg.proxy);
        } else {
            // Fallback 到应用配置的单上游代理
            if let Ok(app_cfg) = crate::modules::config::load_app_config() {
                let up = app_cfg.proxy.upstream_proxy;
                if up.enabled && !up.url.is_empty() {
                    if let Ok(p) = rquest::Proxy::all(&up.url) {
                        tracing::info!(
                            "[Proxy] Route: {:?} (Standard Client) -> Upstream: {} (AppConfig)",
                            account_id.unwrap_or("Generic"),
                            up.url
                        );
                        builder = builder.proxy(p);
                    }
                } else {
                    tracing::info!(
                        "[Proxy] Route: {:?} (Standard Client) -> Direct",
                        account_id.unwrap_or("Generic")
                    );
                }
            }
        }

        builder.build().unwrap_or_else(|_| Client::new())
    }

    /// 账号运维面（额度/token refresh）出口：仅绑定，禁止未绑定蹭池
    pub async fn resolve_account_ops_proxy(
        &self,
        account_id: &str,
    ) -> Option<(AccountOpsRoute, PoolProxyConfig)> {
        let config = self.config.read().await;
        if !config.enabled || config.proxies.is_empty() {
            return None;
        }
        match self.get_bound_proxy(account_id, &config).await {
            Ok(Some(proxy)) => Some((AccountOpsRoute::Bound, proxy)),
            _ => None,
        }
    }

    /// 额度/OAuth refresh 等账号运维请求的 Standard Client：
    /// `bound → upstream → direct`，**绝不** `select_proxy_from_pool`。
    pub async fn get_effective_standard_client_for_account_ops(
        &self,
        account_id: &str,
        timeout_secs: u64,
    ) -> Client {
        let mut builder = Client::builder().timeout(Duration::from_secs(timeout_secs));

        if let Some((route, proxy_cfg)) = self.resolve_account_ops_proxy(account_id).await {
            tracing::info!(
                "[Proxy] AccountOps Route: {} -> {:?} ({})",
                account_id,
                route,
                proxy_cfg.entry_id
            );
            builder = builder.proxy(proxy_cfg.proxy);
            return builder.build().unwrap_or_else(|_| Client::new());
        }

        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                if let Ok(p) = rquest::Proxy::all(&up.url) {
                    tracing::info!(
                        "[Proxy] AccountOps Route: {} -> Upstream ({})",
                        account_id,
                        up.url
                    );
                    builder = builder.proxy(p);
                    return builder.build().unwrap_or_else(|_| Client::new());
                }
            }
        }

        tracing::info!("[Proxy] AccountOps Route: {} -> Direct", account_id);
        builder.build().unwrap_or_else(|_| Client::new())
    }

    /// 批量额度刷新用的出口键：同 key 串行，避免多号同 IP 并发
    pub async fn egress_key_for_account(&self, account_id: &str) -> String {
        let config = self.config.read().await;
        if config.enabled {
            if let Ok(Some(proxy)) = self.get_bound_proxy(account_id, &config).await {
                return format!("proxy:{}", proxy.entry_id);
            }
        }
        drop(config);

        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                return "upstream".to_string();
            }
        }
        "direct".to_string()
    }

    /// 为账号获取代理
    pub async fn get_proxy_for_account(
        &self,
        account_id: &str,
    ) -> Result<Option<PoolProxyConfig>, String> {
        let config = self.config.read().await;

        if !config.enabled || config.proxies.is_empty() {
            return Ok(None);
        }

        // 1. 优先使用账号绑定 (专属 IP)
        if let Some(proxy) = self.get_bound_proxy(account_id, &config).await? {
            tracing::info!(
                "[Proxy] Route: Account {} -> Proxy {} (Bound)",
                account_id,
                proxy.entry_id
            );
            return Ok(Some(proxy));
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
                    // 如果开启了自动故障转移且代理不健康，则返回 None (将回退到其他策略或失败)
                    if config.auto_failover && !entry.is_healthy {
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

    /// 健康检查
    pub async fn health_check(&self) -> Result<(), String> {
        // 由于需要异步并发检查，且不能锁住 config 太久，
        // 我们先复制一份需要检查的代理列表
        let proxies_to_check: Vec<_> = {
            let config = self.config.read().await;
            config
                .proxies
                .iter()
                .filter(|p| p.enabled)
                .cloned()
                .collect()
        };

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
        let mut config = self.config.write().await;
        for (id, is_healthy, latency) in results {
            if let Some(proxy) = config.proxies.iter_mut().find(|p| p.id == id) {
                proxy.is_healthy = is_healthy;
                proxy.latency = latency;
                proxy.last_check_time = Some(chrono::Utc::now().timestamp());
            }
        }

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

        // Residential proxies often need >10s; keep a bit of headroom for CONNECT + TLS.
        let client_result = Client::builder()
            .proxy(proxy_cfg.proxy)
            .emulation(Emulation::Chrome123)
            .timeout(Duration::from_secs(20))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36")
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
                    (true, Some(latency))
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

    /// 启动健康检查循环
    pub fn start_health_check_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            tracing::info!("Starting proxy pool health check loop...");
            loop {
                // Perform check only if enabled
                let enabled = self.config.read().await.enabled;
                if enabled {
                    if let Err(e) = self.health_check().await {
                        tracing::error!("Proxy pool health check failed: {}", e);
                    }
                }

                // Get interval and sleep AFTER check
                let interval_secs = {
                    let cfg = self.config.read().await;
                    if !cfg.enabled {
                        60 // check every minute if disabled
                    } else {
                        cfg.health_check_interval.max(30) // Back to default min 30s
                    }
                };

                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountOpsRoute {
    Bound,
    Upstream,
    Direct,
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
    async fn test_account_ops_no_pool_scrape_when_unbound() {
        let pool = test_pool(vec![proxy("pool-node", None), proxy("spare", None)]);
        let ops = pool.resolve_account_ops_proxy("unbound-acc").await;
        assert!(ops.is_none());

        let key = pool.egress_key_for_account("unbound-acc").await;
        assert!(key == "upstream" || key == "direct");

        let ai = pool.get_proxy_for_account("unbound-acc").await.unwrap();
        assert!(ai.is_some());

        let _ = pool
            .bind_account_to_proxy_inner("a1".into(), "pool-node".into(), false)
            .await;
        let ops = pool.resolve_account_ops_proxy("a1").await;
        assert!(matches!(ops, Some((AccountOpsRoute::Bound, _))));
        assert_eq!(pool.egress_key_for_account("a1").await, "proxy:pool-node");
    }
}
