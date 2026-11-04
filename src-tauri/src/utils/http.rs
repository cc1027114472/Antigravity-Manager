use crate::modules::config::load_app_config;
use once_cell::sync::Lazy;
use rquest::{header, Client, ClientBuilder, Proxy};
use rquest_util::{Emulation, EmulationOption};
use std::time::Duration;

/// Same TLS / HTTP2 identity as official Antigravity IDE (BoringSSL Chrome 132 TLS & HTTP/2).
/// Strips browser document navigation headers (sec-ch-ua, sec-fetch-*, etc.) via skip_headers(true).
pub fn apply_chrome_identity(builder: ClientBuilder) -> ClientBuilder {
    let emu = EmulationOption::builder()
        .emulation(Emulation::Chrome132)
        .skip_headers(true)
        .build();

    let mut default_headers = header::HeaderMap::new();
    default_headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("gzip"),
    );

    let builder = builder
        .emulation(emu)
        .default_headers(default_headers)
        .connect_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60));
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

/// 黑洞客户端：任何请求都在连接阶段立即失败（不可达地址 + 极短超时），
/// 用于 [STRICT PROXY] 策略——代理未启用/配置失败时禁止降级为无代理裸客户端，
/// 宁可让请求失败也绝不让真实出口 IP 暴露给外部 AI 服务。
pub fn build_blackhole_client(reason: &str) -> Client {
    tracing::error!(
        "STRICT PROXY: {} -> building BLACKHOLE client (all requests will fail instantly, NO direct egress)",
        reason
    );
    let blackhole = Client::builder()
        .timeout(Duration::from_millis(1))
        // 指向本地保留端口，连接必然失败；配合 1ms 超时双保险
        .proxy(
            Proxy::all("http://127.0.0.1:1")
                .expect("static blackhole proxy URL must be valid"),
        )
        .build()
        // 若连黑洞客户端都构建失败（几乎不可能），绝对不允许回退到 Client::new()，
        // 因为那是一个无代理裸客户端。
        .expect("blackhole client build must not fall back to a bare client");
    blackhole
}

/// Global shared HTTP client (15s timeout)
/// Client has a built-in connection pool; cloning it is light and shares the pool
pub static SHARED_CLIENT: Lazy<Client> = Lazy::new(|| create_base_client(15));

/// Global shared HTTP client (Long timeout: 60s, for warmup etc.)
pub static SHARED_CLIENT_LONG: Lazy<Client> = Lazy::new(|| create_base_client(60));

/// Base client: Chrome132 impersonation + Antigravity UA (same as AI upstream).
fn create_base_client(timeout_secs: u64) -> Client {
    let mut builder = apply_chrome_identity(Client::builder().timeout(Duration::from_secs(timeout_secs)));

    match load_app_config() {
        Ok(config) => {
            let proxy_config = config.proxy.upstream_proxy;
            if proxy_config.enabled && !proxy_config.url.is_empty() {
                let normalized = crate::proxy::config::normalize_proxy_url(&proxy_config.url);
                match Proxy::all(&normalized) {
                    Ok(proxy) => {
                        builder = builder.proxy(proxy);
                        tracing::info!(
                            "HTTP shared client enabled upstream proxy: {}",
                            normalized
                        );
                        tracing::info!("Initialized JA3/TLS Impersonation (Chrome132)");
                        return builder.build().unwrap_or_else(|e| {
                            build_blackhole_client(&format!(
                                "shared JA3 client build failed: {}",
                                e
                            ))
                        });
                    }
                    Err(e) => {
                        // [STRICT PROXY] 代理解析失败 = 黑洞，绝不裸连
                        return build_blackhole_client(&format!(
                            "invalid_proxy_url: {}, error: {}. Strict policy: refuse direct leak",
                            normalized, e
                        ));
                    }
                }
            } else {
                // [STRICT PROXY] 代理未启用/未配置 = 黑洞，绝不裸连
                return build_blackhole_client(
                    "upstream proxy not enabled or URL empty in app config",
                );
            }
        }
        Err(e) => {
            // 配置读取失败也视为无代理 = 黑洞
            return build_blackhole_client(&format!(
                "failed to load app config for shared client: {}",
                e
            ));
        }
    }
}

/// Get uniformly configured HTTP client (15s timeout)
pub fn get_client() -> Client {
    SHARED_CLIENT.clone()
}

/// Same impersonated client as [`get_client`]. Kept as a named alias for OAuth/quota call sites.
pub fn get_standard_client() -> Client {
    SHARED_CLIENT.clone()
}

/// Same impersonated client as the long-timeout shared pool (60s).
pub fn get_long_standard_client() -> Client {
    SHARED_CLIENT_LONG.clone()
}