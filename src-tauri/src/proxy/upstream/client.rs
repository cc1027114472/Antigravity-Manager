// 上游客户端实现
// 基于高性能通讯接口封装

use dashmap::DashMap;
use rquest::{header, Client, Response, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

/// 端点降级尝试的记录信息
#[derive(Debug, Clone)]
pub struct FallbackAttemptLog {
    /// 尝试的端点 URL
    pub endpoint_url: String,
    /// HTTP 状态码 (网络错误时为 None)
    pub status: Option<u16>,
    /// 错误描述
    pub error: String,
}

/// 上游调用结果，包含响应和降级尝试记录
pub struct UpstreamCallResult {
    /// 最终的 HTTP 响应
    pub response: Response,
    /// 降级过程中失败的端点尝试记录 (成功时为空)
    pub fallback_attempts: Vec<FallbackAttemptLog>,
}

/// 邮箱脱敏：只显示前3位 + *** + @域名前2位 + ***
/// 例: "userexample@gmail.com" → "use***@gm***"
pub fn mask_email(email: &str) -> String {
    if let Some(at_pos) = email.find('@') {
        let local = &email[..at_pos];
        let domain = &email[at_pos + 1..];
        let local_prefix: String = local.chars().take(3).collect();
        let domain_prefix: String = domain.chars().take(2).collect();
        format!("{}***@{}***", local_prefix, domain_prefix)
    } else {
        // 不是合法邮箱格式，直接截取前5位
        let prefix: String = email.chars().take(5).collect();
        format!("{}***", prefix)
    }
}

/// [NEW] 错误日志脱敏：抹除报错信息中的 access_token, proxy_url 等敏感凭证
pub fn sanitize_error_for_log(error_text: &str) -> String {
    // 抹除常见敏感 key 的值
    let re = regex::Regex::new(r#"(?i)(access_token|refresh_token|id_token|authorization|api_key|secret|password|proxy_url|http_proxy|https_proxy)\s*[:=]\s*[^"'\\\s,}\]]+"#).unwrap();
    let redacted = re.replace_all(error_text, "$1=<redacted>");

    // 抹除 Bearer token
    let re_bearer = regex::Regex::new(r#"(?i)(bearer\s+)[^"'\\\s,}\]]+"#).unwrap();
    let redacted = re_bearer.replace_all(&redacted, "$1<redacted>");

    // 限制长度防止日志炸弹
    if redacted.len() > 1000 {
        format!("{}... (truncated)", &redacted[..1000])
    } else {
        redacted.into_owned()
    }
}

// Cloud Code v1internal endpoints (fallback order: Daily → Sandbox)
// 剔除 100% 报 429 的 Prod 生产公共端点，切断导致账号误判限流的通道
const V1_INTERNAL_BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
const V1_INTERNAL_BASE_URL_SANDBOX: &str =
    "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal";

pub const V1_INTERNAL_BASE_URL_FALLBACKS: [&str; 2] = [
    V1_INTERNAL_BASE_URL_DAILY,   // 优先级 1: Daily (原生主力端点，毫秒级响应，稳定支持长流式与思维链)
    V1_INTERNAL_BASE_URL_SANDBOX, // 优先级 2: Sandbox (沙箱备用，极速响应，无缝容灾)
];

pub struct UpstreamClient {
    default_client: RwLock<Client>,
    proxy_pool: Option<Arc<crate::proxy::proxy_pool::ProxyPoolManager>>,
    client_cache: DashMap<String, Client>, // proxy_id -> Client
    client_cache_gen: DashMap<String, u64>, // proxy_id -> evict_generation
    user_agent_override: RwLock<Option<String>>,
}

impl UpstreamClient {
    pub fn new(
        proxy_config: Option<crate::proxy::config::UpstreamProxyConfig>,
        proxy_pool: Option<Arc<crate::proxy::proxy_pool::ProxyPoolManager>>,
    ) -> Self {
        let default_client = match Self::build_client_internal(proxy_config.clone()) {
            Ok(client) => client,
            Err(err_with_proxy) => {
                tracing::error!(
                    error = %err_with_proxy,
                    "CRITICAL: Failed to create default HTTP client with configured upstream proxy. Strict policy: REFUSE falling back to unproxied/bare client! Building BLACKHOLE client."
                );
                // [STRICT PROXY] 严禁在代理配置失败时降级重试无代理/裸客户端！
                // 宁可连接直接报错，绝不向外暴露真实 IP。
                crate::utils::http::build_blackhole_client(&format!(
                    "UpstreamClient default client build failed: {}",
                    err_with_proxy
                ))
            }
        };

        Self {
            default_client: RwLock::new(default_client),
            proxy_pool,
            client_cache: DashMap::new(),
            client_cache_gen: DashMap::new(),
            user_agent_override: RwLock::new(None),
        }
    }

    /// [WARMUP] 官方协议级 TLS / HTTP/2 预热：异步与上游端点建立 TLS 握手并保活长连接
    /// 零模型推理消耗，不产生空对话探针，消除高频冷启动指纹并降低用户首字延迟 (TTFT)
    pub async fn warmup_endpoints_async(&self) {
        let client = self.default_client.read().await.clone();
        tokio::spawn(async move {
            for endpoint in V1_INTERNAL_BASE_URL_FALLBACKS {
                let _ = client
                    .head(endpoint)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await;
            }
            tracing::info!("Upstream endpoints TLS & HTTP/2 connection pool prewarmed");
        });
    }

    /// [HOT-RELOAD] Rebuild the default HTTP client using the supplied upstream
    /// proxy config. Called from `update_proxy` so changes to the upstream proxy
    /// take effect without restarting the app.
    pub async fn rebuild_default_client(
        &self,
        proxy_config: Option<crate::proxy::config::UpstreamProxyConfig>,
    ) {
        let new_client = match Self::build_client_internal(proxy_config.clone()) {
            Ok(c) => c,
            Err(err_with_proxy) => {
                tracing::error!(
                    error = %err_with_proxy,
                    "Hot-reload: failed to rebuild default HTTP client with configured upstream proxy. Strict policy: keeping previous client and REFUSING fallback to unproxied client!"
                );
                return;
            }
        };
        let mut guard = self.default_client.write().await;
        *guard = new_client;
        tracing::info!("UpstreamClient default_client rebuilt (upstream proxy hot-reloaded)");
    }

    /// [HOT-RELOAD] Drop all per-proxy cached clients. Call after the pool
    /// configuration changes (proxy URL/credentials edited, proxy removed,
    /// bindings changed) so the next request rebuilds with fresh settings.
    pub fn clear_client_cache(&self) {
        let size = self.client_cache.len();
        self.client_cache.clear();
        self.client_cache_gen.clear();
        if size > 0 {
            tracing::info!("UpstreamClient cleared {} cached per-proxy clients", size);
        }
    }

    /// Internal helper to build a client with optional upstream proxy config.
    /// [STRICT PROXY] 若未传入启用的上游代理配置（None / 未启用 / URL 为空），
    /// 直接返回 Err —— 严禁构建无代理裸客户端作为 default_client。
    fn build_client_internal(
        proxy_config: Option<crate::proxy::config::UpstreamProxyConfig>,
    ) -> Result<Client, String> {
        let mut builder = crate::utils::http::apply_chrome_identity(
            Client::builder().timeout(Duration::from_secs(600)),
        );

        let config = proxy_config
            .ok_or_else(|| "strict proxy: missing upstream proxy config (None)".to_string())?;
        if !config.enabled || config.url.is_empty() {
            return Err(format!(
                "strict proxy: upstream proxy not enabled or URL empty (enabled={}, url='{}'): refuse building unproxied default client",
                config.enabled, config.url
            ));
        }
        let url = crate::proxy::config::normalize_proxy_url(&config.url);
        let proxy = rquest::Proxy::all(&url).map_err(|e| {
            format!(
                "strict proxy: invalid upstream proxy URL '{}': {} : refuse building unproxied default client",
                url, e
            )
        })?;
        builder = builder.proxy(proxy);
        tracing::info!("UpstreamClient enabled proxy: {}", url);

        builder
            .build()
            .map_err(|e| format!("failed to build proxied client: {}", e))
    }

    /// Build a client with a specific PoolProxyConfig (from ProxyPool)
    fn build_client_with_proxy(
        &self,
        proxy_config: crate::proxy::proxy_pool::PoolProxyConfig,
    ) -> Result<Client, rquest::Error> {
        // Reuse base settings similar to default client but with specific proxy
        crate::utils::http::apply_chrome_identity(
            Client::builder()
                .timeout(Duration::from_secs(600))
                .proxy(proxy_config.proxy),
        )
        .build()
    }

    fn apply_default_user_agent(builder: rquest::ClientBuilder) -> rquest::ClientBuilder {
        let ua = crate::constants::USER_AGENT.as_str();
        if header::HeaderValue::from_str(ua).is_ok() {
            builder.user_agent(ua)
        } else {
            tracing::warn!(
                user_agent = %ua,
                "Invalid default User-Agent value, using fallback"
            );
            builder.user_agent("antigravity")
        }
    }

    /// Set dynamic User-Agent override
    pub async fn set_user_agent_override(&self, ua: Option<String>) {
        let mut lock = self.user_agent_override.write().await;
        *lock = ua;
        tracing::debug!("UpstreamClient User-Agent override updated: {:?}", lock);
    }

    /// Get current User-Agent
    pub async fn get_user_agent(&self) -> String {
        let ua_override = self.user_agent_override.read().await;
        ua_override
            .as_ref()
            .cloned()
            .unwrap_or_else(|| crate::constants::USER_AGENT.clone())
    }

    /// Get client for a specific account (or default if no proxy bound).
    /// Returns `(client, Some(proxy_id))` when a pool proxy is used; `None` for upstream default.
    /// [STRICT POLICY] Direct connection is strictly forbidden for account requests!
    pub async fn get_client(&self, account_id: Option<&str>) -> Result<(Client, Option<String>), String> {
        if let Some(pool) = &self.proxy_pool {
            if let Some(acc_id) = account_id {
                // Try to get per-account proxy
                match pool.get_proxy_for_account(acc_id).await {
                    Ok(Some(proxy_cfg)) => {
                        let entry_id = proxy_cfg.entry_id.clone();
                        let cur_gen = pool.proxy_evict_generation(&entry_id);
                        let cached_gen = self.client_cache_gen.get(&entry_id).map(|g| *g.value());

                        // Check cache & verify evict_generation matches so stale/half-open SOCKS5/TLS sockets are discarded
                        if cached_gen == Some(cur_gen) {
                            if let Some(client) = self.client_cache.get(&entry_id) {
                                return Ok((client.clone(), Some(entry_id)));
                            }
                        } else if cached_gen.is_some() {
                            self.client_cache.remove(&entry_id);
                            tracing::info!(
                                "Evicted stale cached HTTP/TLS client for proxy {} (gen {:?} -> {})",
                                entry_id,
                                cached_gen,
                                cur_gen
                            );
                        }

                        // Build new client and cache it
                        match self.build_client_with_proxy(proxy_cfg.clone()) {
                            Ok(client) => {
                                self.client_cache
                                    .insert(entry_id.clone(), client.clone());
                                self.client_cache_gen.insert(entry_id.clone(), cur_gen);
                                tracing::info!(
                                    "Using ProxyPool proxy ID: {} (gen={}) for account: {}",
                                    entry_id,
                                    cur_gen,
                                    acc_id
                                );
                                return Ok((client, Some(entry_id)));
                            }
                            Err(e) => {
                                tracing::error!("Failed to build client for proxy {}: {}, strict policy: refuse direct", entry_id, e);
                                pool.record_egress_usage(
                                    &entry_id,
                                    crate::proxy::proxy_pool::EgressUsageStatus::Failed,
                                );
                                let pool_clone = Arc::clone(pool);
                                let eid = entry_id.clone();
                                tokio::spawn(async move {
                                    pool_clone.mark_proxy_unhealthy(&eid).await;
                                });
                                return Err(format!("代理节点 {} 构建失败且禁止直连: {}", entry_id, e));
                            }
                        }
                    }
                    Ok(None) => {
                        // 专属绑定账号必须钉在原出口：绑定节点 RED 时硬失败，禁止切到全局上游。
                        if pool.has_dedicated_binding(acc_id) {
                            let err_msg = format!(
                                "严格代理策略拦截：账号 {} 的专属代理不通 (RED)，禁止切换全局上游以免出口漂移。等待原代理回绿后再请求。",
                                acc_id
                            );
                            tracing::error!("[UpstreamClient] {}", err_msg);
                            return Err(err_msg);
                        }
                        // 未绑定账号：池中无健康节点时，仅允许已启用的全局上游代理（仍禁止直连）
                        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
                            let up = app_cfg.proxy.upstream_proxy;
                            if up.enabled && !up.url.is_empty() {
                                tracing::info!("[UpstreamClient] Unbound account {} fallback to global upstream proxy: {}", acc_id, up.url);
                                return Ok((self.default_client.read().await.clone(), None));
                            }
                        }
                        let err_msg = format!("严格代理策略拦截：账号 {} 未绑定健康代理或代理不通 (RED)，严禁本地直连！请先配置或修复代理。", acc_id);
                        tracing::error!("[UpstreamClient] {}", err_msg);
                        return Err(err_msg);
                    }
                    Err(e) => {
                        let err_msg = format!("获取账号 {} 的代理配置失败: {}", acc_id, e);
                        tracing::error!("[UpstreamClient] {}", err_msg);
                        return Err(err_msg);
                    }
                }
            }
        }

        // Generic request without account_id
        if let Ok(app_cfg) = crate::modules::config::load_app_config() {
            let up = app_cfg.proxy.upstream_proxy;
            if up.enabled && !up.url.is_empty() {
                return Ok((self.default_client.read().await.clone(), None));
            }
        }
        let err_msg = "严格代理策略拦截：未检测到任何健康代理或启用的全局上游代理，严禁本地直连！".to_string();
        tracing::error!("[UpstreamClient] {}", err_msg);
        Err(err_msg)
    }

    /// True when the error is an egress/transport failure (not an HTTP application status).
    fn is_egress_transport_error(err: &rquest::Error) -> bool {
        err.is_connect()
            || err.is_timeout()
            || err.is_request()
            || {
                let s = err.to_string().to_lowercase();
                s.contains("connect")
                    || s.contains("connection")
                    || s.contains("dns")
                    || s.contains("proxy")
                    || s.contains("timed out")
                    || s.contains("timeout")
            }
    }

    /// 细分传输层错误类型，用于 [NET-OBS] 网络波动观察日志。
    fn classify_egress_error(err: &rquest::Error) -> &'static str {
        if err.is_timeout() {
            "timeout"
        } else if err.is_connect() {
            "connect_failed"
        } else if err.is_request() {
            "request_malformed"
        } else {
            let s = err.to_string().to_lowercase();
            if s.contains("dns") || s.contains("name resolution") || s.contains("failed to lookup") {
                "dns"
            } else if s.contains("proxy") {
                "proxy_handshake"
            } else if s.contains("tls")
                || s.contains("ssl")
                || s.contains("handshake")
                || s.contains("certificate")
            {
                "tls_handshake"
            } else if s.contains("reset")
                || s.contains("closed")
                || s.contains("broken pipe")
                || s.contains("eof")
                || s.contains("connection aborted")
            {
                "conn_reset"
            } else {
                "other"
            }
        }
    }

    fn record_egress_outcome(&self, proxy_id: Option<&str>, status: crate::proxy::proxy_pool::EgressUsageStatus) {
        if let (Some(pool), Some(id)) = (&self.proxy_pool, proxy_id) {
            pool.record_egress_usage(id, status);
            if status == crate::proxy::proxy_pool::EgressUsageStatus::Failed {
                self.client_cache.remove(id);
            }
        }
    }

    /// Build v1internal URL
    fn build_url(base_url: &str, method: &str, query_string: Option<&str>) -> String {
        if let Some(qs) = query_string {
            format!("{}:{}?{}", base_url, method, qs)
        } else {
            format!("{}:{}", base_url, method)
        }
    }

    /// Determine if we should try next endpoint (fallback logic)
    fn should_try_next_endpoint(status: StatusCode) -> bool {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::NOT_FOUND
            || status.is_server_error()
    }

    /// Call v1internal API (Basic Method)
    ///
    /// Initiates a basic network request, supporting multi-endpoint auto-fallback.
    /// [UPDATED] Takes optional account_id for per-account proxy selection.
    pub async fn call_v1_internal(
        &self,
        method: &str,
        access_token: &str,
        body: Value,
        query_string: Option<&str>,
        account_id: Option<&str>, // [NEW] Account ID for proxy selection
    ) -> Result<UpstreamCallResult, String> {
        self.call_v1_internal_with_headers(
            method,
            access_token,
            body,
            query_string,
            std::collections::HashMap::new(),
            account_id,
        )
        .await
    }

    /// [FIX #765] 调用 v1internal API，支持透传额外的 Headers
    /// [ENHANCED] 返回 UpstreamCallResult，包含降级尝试记录，用于 debug 日志
    pub async fn call_v1_internal_with_headers(
        &self,
        method: &str,
        access_token: &str,
        mut body: Value,
        query_string: Option<&str>,
        extra_headers: std::collections::HashMap<String, String>,
        account_id: Option<&str>, // [NEW] Account ID
    ) -> Result<UpstreamCallResult, String> {
        // [DEFENSE] 全局终极防御拦截：清洗提示词中的计费伪 Header 与外部指纹，对齐官方原生 Antigravity 身份，防止 Google WAF 假性 429
        if method == "streamGenerateContent" || method == "generateContent" {
            if let Some(inner) = body.get_mut("request") {
                crate::proxy::mappers::prompt_sanitizer::PromptSanitizer::sanitize_gemini_payload(
                    inner,
                );
            } else {
                crate::proxy::mappers::prompt_sanitizer::PromptSanitizer::sanitize_gemini_payload(
                    &mut body,
                );
            }
        }

        // [OVERHEAD] 自适应上下文开销平衡：对 3.8 超短请求注入官方环境上下文骨架
        let model_owned = body.get("model").and_then(Value::as_str).map(|s| s.to_string());
        if method == "streamGenerateContent" || method == "generateContent" {
            if let Some(inner) = body.get_mut("request") {
                crate::proxy::mappers::prompt_sanitizer::PromptSanitizer::adapt_context_overhead(
                    inner,
                    model_owned.as_deref(),
                );
            } else {
                crate::proxy::mappers::prompt_sanitizer::PromptSanitizer::adapt_context_overhead(
                    &mut body,
                    model_owned.as_deref(),
                );
            }
        }

        // [DEFENSE] 3.8 网关严格校验：普通文本/Agent 请求绝不发送非官方包装字段（仅保留真实的 image_gen）
        if let Some(obj) = body.as_object_mut() {
            if obj.get("requestType").and_then(|v| v.as_str()) != Some("image_gen") {
                obj.remove("requestType");
            }
        }

        // [DEFENSE] 3.8 网关严格校验：工具链互斥物理收敛（functionDeclarations 存在时物理剥离 googleSearch）
        if let Some(inner) = body.get_mut("request") {
            crate::proxy::mappers::common_utils::enforce_tool_mutual_exclusion(inner);
        } else {
            crate::proxy::mappers::common_utils::enforce_tool_mutual_exclusion(&mut body);
        }

        // [NEW] Get client based on account (cached in proxy pool manager)
        let (client, egress_proxy_id) = self.get_client(account_id).await?;

        // 构建 Headers (所有端点复用)
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {}", access_token))
                .map_err(|e| e.to_string())?,
        );

        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(&self.get_user_agent().await).unwrap_or_else(|e| {
                tracing::warn!("Invalid User-Agent header value, using fallback: {}", e);
                header::HeaderValue::from_static("antigravity")
            }),
        );

        // [ENHANCED] 注入 Antigravity 官方客户端关键特征 Headers
        // 1. Client Identity
        headers.insert(
            "x-client-name",
            header::HeaderValue::from_static("antigravity"),
        );
        if let Ok(ver) = header::HeaderValue::from_str(&crate::constants::CURRENT_VERSION) {
            headers.insert("x-client-version", ver);
        }

        // 2. Device & Session Identity
        // Machine ID (Persistent)
        if let Ok(mid) = machine_uid::get() {
            if let Ok(mid_val) = header::HeaderValue::from_str(&mid) {
                headers.insert("x-machine-id", mid_val);
            }
        }
        // Session ID (Per App Launch)
        if let Ok(sess_val) = header::HeaderValue::from_str(&crate::constants::SESSION_ID) {
            headers.insert("x-vscode-sessionid", sess_val);
        }

        // [REMOVED v4.1.24] x-goog-api-client (gl-node/fire/grpc) header has been removed.
        // This header belongs to the IDE's JS layer, not the official client's egress.
        // Sending it creates a contradictory "Electron + Node.js" fingerprint.

        // [REMOVED] 不再注入 x-goog-user-project 配额项目头（对齐上游 v4.6.2 已验证方案）。
        // 官方 Antigravity 客户端在内容生成请求中从不发送该 Header（见抓包协议文档），
        // 一旦发送，Google 会对 body 中的 project（如 aicode-consumers）执行配额项目 IAM 校验，
        // 无 serviceusage.services.use 权限的账号一律返回 403 USER_PROJECT_DENIED。
        // 移除后鉴权回归正常，也无需再靠"摘头偷偷重试"的降级补丁兜底。

        // 注入额外的 Headers (如 anthropic-beta)
        for (k, v) in extra_headers {
            if let Ok(hk) = header::HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(hv) = header::HeaderValue::from_str(&v) {
                    headers.insert(hk, hv);
                }
            }
        }

        // [DEBUG] Log headers for verification
        tracing::debug!(?headers, "Final Upstream Request Headers");

        let mut last_err: Option<String> = None;
        let mut fallback_attempts: Vec<FallbackAttemptLog> = Vec::new();

        // 遍历所有端点，失败时自动切换
        for (idx, base_url) in V1_INTERNAL_BASE_URL_FALLBACKS.iter().enumerate() {
                let url = Self::build_url(base_url, method, query_string);
                let has_next = idx + 1 < V1_INTERNAL_BASE_URL_FALLBACKS.len();

                let body_bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;

                let mut req_builder = client.post(&url).headers(headers.clone());

                // [FIX] 仅对流式接口 (streamGenerateContent) 使用分块传输仿真
                // 对其他接口 (如 generateContent, loadCodeAssist) 发送正常的固定长度 Body
                // 否则图像生成会因为缺少 Content-Length 而被 Google 服务端拒绝或限流 (429)
                if method == "streamGenerateContent" {
                    let stream_bytes = body_bytes.clone();
                    req_builder = req_builder.body(rquest::Body::wrap_stream(
                        futures::stream::once(async move { Ok::<_, std::io::Error>(stream_bytes) }),
                    ));
                } else {
                    req_builder = req_builder.body(body_bytes.clone());
                }

                // [WATCHDOG] 端点级自适应防假死超时：流式接口保底 120s 满足大模型思维链与长上下文冷推理 (TTFT ~35s)，
                // 普通轻量请求保底 15s 快速探活，兼顾防上游静默挂起与避免长思考被误判假死导致 429 假阳性
                let watchdog_secs = if method == "streamGenerateContent" {
                    std::cmp::max(120, (body_bytes.len() as u64) / (64 * 1024) + 120)
                } else {
                    std::cmp::max(15, (body_bytes.len() as u64) / (256 * 1024) + 15)
                };
                let send_future = req_builder.send();
                let response = if has_next {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(watchdog_secs),
                        send_future,
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(_) => {
                            let err_msg = format!(
                                "HTTP request timed out after {}s waiting for response headers at {} (method={}, body_len={})",
                                watchdog_secs, base_url, method, body_bytes.len()
                            );
                            tracing::warn!(
                                account_id = %account_id.unwrap_or("generic"),
                                proxy_id = %egress_proxy_id.as_deref().unwrap_or("upstream"),
                                endpoint = %base_url,
                                method = %method,
                                err_kind = "timeout",
                                endpoint_idx = idx,
                                "[NET-OBS] endpoint timeout, failing over: {}",
                                err_msg
                            );
                            fallback_attempts.push(FallbackAttemptLog {
                                endpoint_url: base_url.to_string(),
                                status: None,
                                error: err_msg.clone(),
                            });
                            last_err = Some(err_msg);

                            if egress_proxy_id.is_some() {
                                self.record_egress_outcome(
                                    egress_proxy_id.as_deref(),
                                    crate::proxy::proxy_pool::EgressUsageStatus::Failed,
                                );
                                if let (Some(pool), Some(ref id)) = (&self.proxy_pool, &egress_proxy_id) {
                                    let pool_clone = Arc::clone(pool);
                                    let proxy_id_str = id.clone();
                                    tokio::spawn(async move {
                                        pool_clone.mark_proxy_unhealthy(&proxy_id_str).await;
                                    });
                                }
                                // 专属代理超时：立即熔断退出端点循环，严禁在同一条卡死代理上继续重试 sandbox 端点！
                                break;
                            }
                            continue;
                        }
                    }
                } else {
                    send_future.await
                };

                match response {
                    Ok(resp) => {
                        // Any HTTP response means the egress path worked.
                        self.record_egress_outcome(
                            egress_proxy_id.as_deref(),
                            crate::proxy::proxy_pool::EgressUsageStatus::Ok,
                        );

                        let status = resp.status();
                        if status.is_success() {
                            if idx > 0 {
                                tracing::info!(
                                    account_id = %account_id.unwrap_or("generic"),
                                    proxy_id = %egress_proxy_id.as_deref().unwrap_or("upstream"),
                                    endpoint = %base_url,
                                    method = %method,
                                    failover_idx = idx,
                                    status = %status.as_u16(),
                                    "[NET-OBS] upstream recovered via endpoint failover"
                                );
                            } else {
                                tracing::debug!(
                                    "✓ Upstream request succeeded | Endpoint: {} | Status: {}",
                                    base_url,
                                    status
                                );
                            }
                            return Ok(UpstreamCallResult {
                                response: resp,
                                fallback_attempts,
                            });
                        }

                        // 遇到 403 Forbidden 立即硬熔断返回，严禁任何内部重试或端点轮换，直接交由上层 Handler 标记账号禁用
                        if status == StatusCode::FORBIDDEN {
                            tracing::warn!(
                                "🚫 Upstream returned 403 Forbidden on endpoint {} (Account: {:?}), immediate abort without retry",
                                base_url,
                                account_id
                            );
                            return Ok(UpstreamCallResult {
                                response: resp,
                                fallback_attempts,
                            });
                        }

                        // 如果有下一个端点且当前错误可重试，则切换
                        if has_next && Self::should_try_next_endpoint(status) {
                            let err_msg = format!("Upstream {} returned {}", base_url, status);
                            tracing::warn!(
                                "Upstream endpoint returned {} at {} (method={}), trying next endpoint",
                                status,
                                base_url,
                                method
                            );
                            // [NEW] 记录降级尝试
                            fallback_attempts.push(FallbackAttemptLog {
                                endpoint_url: url.clone(),
                                status: Some(status.as_u16()),
                                error: err_msg.clone(),
                            });
                            last_err = Some(err_msg);
                            continue;
                        }

                        // 不可重试的错误或已是最后一个端点，直接返回
                        return Ok(UpstreamCallResult {
                            response: resp,
                            fallback_attempts,
                        });
                    }
                    Err(e) => {
                        if Self::is_egress_transport_error(&e) {
                            // [NET-OBS] 网络波动观察日志：传输层失败（连接/超时/DNS/TLS/代理握手/连接重置）
                            // 统一前缀便于在 logs/app.log 中 grep。仅观察，不改任何行为。
                            tracing::warn!(
                                account_id = %account_id.unwrap_or("generic"),
                                proxy_id = %egress_proxy_id.as_deref().unwrap_or("upstream"),
                                endpoint = %base_url,
                                method = %method,
                                err_kind = Self::classify_egress_error(&e),
                                endpoint_idx = idx,
                                "[NET-OBS] egress transport failure: {}",
                                e
                            );

                            self.record_egress_outcome(
                                egress_proxy_id.as_deref(),
                                crate::proxy::proxy_pool::EgressUsageStatus::Failed,
                            );
                            if let (Some(pool), Some(ref id)) = (&self.proxy_pool, &egress_proxy_id) {
                                let pool_clone = Arc::clone(pool);
                                let proxy_id_str = id.clone();
                                tokio::spawn(async move {
                                    pool_clone.mark_proxy_unhealthy(&proxy_id_str).await;
                                });
                            }
                        } else {
                            // [NET-OBS] 非传输层错误（例如连接建立成功但协议层失败），也记录以便对比
                            tracing::warn!(
                                account_id = %account_id.unwrap_or("generic"),
                                proxy_id = %egress_proxy_id.as_deref().unwrap_or("upstream"),
                                endpoint = %base_url,
                                method = %method,
                                err_kind = Self::classify_egress_error(&e),
                                endpoint_idx = idx,
                                "[NET-OBS] egress non-transport failure: {}",
                                e
                            );
                        }
                        let msg = format!("HTTP request failed at {}: {}", base_url, e);
                        tracing::debug!("{}", msg);
                        // [NEW] 记录网络错误的降级尝试
                        fallback_attempts.push(FallbackAttemptLog {
                            endpoint_url: url.clone(),
                            status: None,
                            error: msg.clone(),
                        });
                        last_err = Some(msg);

                        // 若是专属代理传输层故障，该代理已被置为 RED，严禁在同一条故障代理上继续重试 sandbox 端点
                        if egress_proxy_id.is_some() && Self::is_egress_transport_error(&e) {
                            break;
                        }

                        // 如果是最后一个端点，退出循环
                        if !has_next {
                            break;
                        }
                        continue;
                    }
                }
            }

        // 所有端点都尝试过，返回最后的错误
        Err(last_err.unwrap_or_else(|| "All endpoints failed".to_string()))
    }

    /// 调用 v1internal API（带 429 重试,支持闭包）
    ///
    /// 带容错和重试的核心请求逻辑
    ///
    /// # Arguments
    /// * `method` - API method (e.g., "generateContent")
    /// * `query_string` - Optional query string (e.g., "?alt=sse")
    /// * `get_credentials` - 闭包，获取凭证（支持账号轮换）
    /// * `build_body` - 闭包，接收 project_id 构建请求体
    /// * `max_attempts` - 最大重试次数
    ///
    /// # Returns
    /// HTTP Response
    // 已移除弃用的重试方法 (call_v1_internal_with_retry)

    // 已移除弃用的辅助方法 (parse_retry_delay)

    // 已移除弃用的辅助方法 (parse_duration_ms)

    /// 获取可用模型列表
    ///
    /// 获取远端模型列表，支持多端点自动 Fallback
    #[allow(dead_code)] // API ready for future model discovery feature
    pub async fn fetch_available_models(
        &self,
        access_token: &str,
        account_id: Option<&str>,
    ) -> Result<Value, String> {
        // 复用 call_v1_internal，然后解析 JSON
        let result = self
            .call_v1_internal(
                "fetchAvailableModels",
                access_token,
                serde_json::json!({}),
                None,
                account_id,
            )
            .await?;
        let json: Value = result
            .response
            .json()
            .await
            .map_err(|e| format!("Parse json failed: {}", e))?;
        Ok(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_url() {
        let base_url = "https://cloudcode-pa.googleapis.com/v1internal";

        let url1 = UpstreamClient::build_url(base_url, "generateContent", None);
        assert_eq!(
            url1,
            "https://cloudcode-pa.googleapis.com/v1internal:generateContent"
        );

        let url2 = UpstreamClient::build_url(base_url, "streamGenerateContent", Some("alt=sse"));
        assert_eq!(
            url2,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }
}
