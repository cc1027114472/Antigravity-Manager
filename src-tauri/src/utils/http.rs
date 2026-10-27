use crate::modules::config::load_app_config;
use once_cell::sync::Lazy;
use rquest::{Client, Proxy};
use rquest_util::Emulation;
use std::time::Duration;

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

/// Global shared standard HTTP client (15s timeout, NO JA3 Emulation)
pub static SHARED_STANDARD_CLIENT: Lazy<Client> = Lazy::new(|| create_standard_client(15));

/// Global shared standard HTTP client (Long timeout: 60s, NO JA3 Emulation)
pub static SHARED_STANDARD_CLIENT_LONG: Lazy<Client> = Lazy::new(|| create_standard_client(60));

/// Base client creation logic with JA3 Emulation
fn create_base_client(timeout_secs: u64) -> Client {
    let mut builder = Client::builder()
        .emulation(Emulation::Chrome132)
        .timeout(Duration::from_secs(timeout_secs));

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
                        tracing::info!("Initialized JA3/TLS Impersonation (Chrome123)");
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

/// Base client creation logic strictly WITHOUT JA3 Emulation (Pure Native)
fn create_standard_client(timeout_secs: u64) -> Client {
    let mut builder = Client::builder()
        // No .emulation(Emulation::Chrome123) here!
        .timeout(Duration::from_secs(timeout_secs));

    match load_app_config() {
        Ok(config) => {
            let proxy_config = config.proxy.upstream_proxy;
            if proxy_config.enabled && !proxy_config.url.is_empty() {
                let normalized = crate::proxy::config::normalize_proxy_url(&proxy_config.url);
                match Proxy::all(&normalized) {
                    Ok(proxy) => {
                        builder = builder.proxy(proxy);
                        tracing::info!(
                            "HTTP standard client enabled upstream proxy: {}",
                            normalized
                        );
                        tracing::info!("Initialized Pure Native Standard Client");
                        return builder.build().unwrap_or_else(|e| {
                            build_blackhole_client(&format!(
                                "shared standard client build failed: {}",
                                e
                            ))
                        });
                    }
                    Err(e) => {
                        return build_blackhole_client(&format!(
                            "invalid_proxy_url: {}, error: {}. Strict policy: refuse direct leak",
                            normalized, e
                        ));
                    }
                }
            } else {
                return build_blackhole_client(
                    "upstream proxy not enabled or URL empty in app config (standard client)",
                );
            }
        }
        Err(e) => {
            return build_blackhole_client(&format!(
                "failed to load app config for shared standard client: {}",
                e
            ));
        }
    }
}

/// Get standard HTTP client without JA3 Emulation (15s timeout)
pub fn get_standard_client() -> Client {
    SHARED_STANDARD_CLIENT.clone()
}

/// Get long timeout standard HTTP client without JA3 Emulation (60s timeout)
pub fn get_long_standard_client() -> Client {
    SHARED_STANDARD_CLIENT_LONG.clone()
}