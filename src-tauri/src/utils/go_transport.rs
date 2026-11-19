use once_cell::sync::Lazy;
use rquest::{Client, RequestBuilder};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

pub const HDR_TARGET_URL: &str = "X-Ag-Target-Url";
pub const HDR_PROXY_URL: &str = "X-Ag-Proxy";
pub const HDR_PROTO: &str = "X-Ag-Proto";
pub const HDR_CHUNKED: &str = "X-Ag-Chunked";
pub const HDR_TIMEOUT_MS: &str = "X-Ag-Timeout-Ms";
pub const HDR_EVICT_GEN: &str = "X-Ag-Evict-Gen";

struct GoSidecar {
    _child: Child,
    _stdin: ChildStdin,
    relay_url: String,
}

static SIDECAR: OnceLock<Mutex<Option<GoSidecar>>> = OnceLock::new();
static RELAY_URL_CACHE: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));

static LOOPBACK_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .no_proxy()
        .http1_only()
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .build()
        .expect("loopback go-transport client must build")
});

fn candidate_binary_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(explicit) = std::env::var("ABV_GO_TRANSPORT_BIN") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            paths.push(PathBuf::from(trimmed));
        }
    }
    let bin_name = if cfg!(windows) {
        "go-aidev-transport.exe"
    } else {
        "go-aidev-transport"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(bin_name));
        }
    }
    paths.push(PathBuf::from("/app/go-aidev-transport"));
    paths.push(PathBuf::from("go-transport").join(bin_name));
    paths
}

fn spawn_sidecar() -> Option<GoSidecar> {
    let bin_path = candidate_binary_paths().into_iter().find(|p| p.is_file())?;
    tracing::info!("[GoTransport] Starting sidecar binary: {:?}", bin_path);

    let mut cmd = Command::new(&bin_path);
    cmd.arg("-listen")
        .arg("127.0.0.1:0")
        .arg("-watch-stdin=true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[GoTransport] Failed to spawn {:?}: {}", bin_path, e);
            return None;
        }
    };

    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        tracing::warn!("[GoTransport] Failed to read READY line: {}", e);
        let _ = child.kill();
        return None;
    }

    let trimmed = line.trim();
    let Some(addr) = trimmed.strip_prefix("READY ") else {
        tracing::warn!("[GoTransport] Unexpected startup output: {:?}", trimmed);
        let _ = child.kill();
        return None;
    };

    let relay_url = format!("http://{}/relay", addr.trim());
    tracing::info!("[GoTransport] Go native TLS/HTTP sidecar ready at {}", relay_url);

    Some(GoSidecar {
        _child: child,
        _stdin: stdin,
        relay_url,
    })
}

/// Start the Go native TLS/HTTP transport sidecar if available (no-op in tests or if ABV_GO_TRANSPORT=0).
pub fn ensure_started() {
    if cfg!(test) {
        return;
    }
    if std::env::var("ABV_GO_TRANSPORT")
        .map(|v| v.trim() == "0" || v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        tracing::info!("[GoTransport] Disabled via ABV_GO_TRANSPORT=0; using Rust rquest fallback");
        return;
    }
    if let Ok(ext_url) = std::env::var("ABV_GO_TRANSPORT_URL") {
        let trimmed = ext_url.trim();
        if !trimmed.is_empty() {
            if let Ok(mut w) = RELAY_URL_CACHE.write() {
                *w = Some(trimmed.to_string());
            }
            tracing::info!("[GoTransport] Using external Go sidecar URL: {}", trimmed);
            return;
        }
    }

    let lock = SIDECAR.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        if guard.is_none() {
            if let Some(sidecar) = spawn_sidecar() {
                if let Ok(mut w) = RELAY_URL_CACHE.write() {
                    *w = Some(sidecar.relay_url.clone());
                }
                *guard = Some(sidecar);
            } else {
                tracing::debug!("[GoTransport] Sidecar binary not found; using Rust rquest fallback");
            }
        }
    }
}

/// Returns `Some("http://127.0.0.1:<port>/relay")` when the Go sidecar is active.
pub fn relay_url() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    RELAY_URL_CACHE.read().ok().and_then(|g| g.clone())
}

pub fn loopback_client() -> Client {
    LOOPBACK_CLIENT.clone()
}

pub fn prepare_relay_request(
    builder: RequestBuilder,
    target_url: &str,
    proxy_url: &str,
    proto: &str,
    chunked: bool,
    timeout_ms: Option<u64>,
    evict_gen: Option<u64>,
) -> RequestBuilder {
    let mut b = builder
        .header(HDR_TARGET_URL, target_url)
        .header(HDR_PROXY_URL, proxy_url)
        .header(HDR_PROTO, proto);
    if chunked {
        b = b.header(HDR_CHUNKED, "1");
    }
    if let Some(ms) = timeout_ms {
        b = b.header(HDR_TIMEOUT_MS, ms.to_string());
    }
    if let Some(gen_val) = evict_gen {
        b = b.header(HDR_EVICT_GEN, gen_val.to_string());
    }
    b
}

/// Resolve the strict-proxy egress URL for an account (or generic pool/upstream request)
/// for forwarding through the Go sidecar. Refuses Direct egress.
pub async fn resolve_strict_proxy_url(
    account_id: Option<&str>,
) -> Result<(String, Option<String>, Option<u64>), String> {
    if let Some(pool) = crate::proxy::proxy_pool::get_global_proxy_pool() {
        if let Some(acc_id) = account_id {
            match pool.get_proxy_for_account(acc_id).await {
                Ok(Some(cfg)) => {
                    let evict_gen = pool.proxy_evict_generation(&cfg.entry_id);
                    return Ok((cfg.url, Some(cfg.entry_id), Some(evict_gen)));
                }
                Ok(None) => {
                    if pool.has_dedicated_binding(acc_id) {
                        return Err(format!(
                            "严格代理策略拦截：账号 {} 的专属代理不通 (RED)，禁止切换全局上游或直连",
                            acc_id
                        ));
                    }
                }
                Err(e) => {
                    return Err(format!("获取账号 {} 代理失败: {}", acc_id, e));
                }
            }
        } else if let Ok(Some(cfg)) = pool.select_generic_proxy().await {
            let evict_gen = pool.proxy_evict_generation(&cfg.entry_id);
            return Ok((cfg.url, Some(cfg.entry_id), Some(evict_gen)));
        }
    }

    if let Ok(app_cfg) = crate::modules::config::load_app_config() {
        let up = app_cfg.proxy.upstream_proxy;
        if up.enabled && !up.url.trim().is_empty() {
            return Ok((crate::proxy::config::normalize_proxy_url(&up.url), None, None));
        }
    }

    Err(format!(
        "严格代理策略拦截：账号 {:?} 无健康池代理且未启用全局上游代理，严禁直连",
        account_id
    ))
}

/// Build a POST RequestBuilder routed through the Go native TLS/HTTP sidecar when active,
/// or falling back to the given Rust `rquest::Client` directly.
pub async fn post_with_sidecar_or_fallback(
    fallback_client: &Client,
    target_url: &str,
    account_id: Option<&str>,
    proto: &str,
    chunked: bool,
    timeout_ms: Option<u64>,
) -> Result<(RequestBuilder, Option<String>, bool), String> {
    if let Some(relay) = relay_url() {
        let (proxy_url, egress_id, evict_gen) = resolve_strict_proxy_url(account_id).await?;
        let rb = prepare_relay_request(
            loopback_client().post(&relay),
            target_url,
            &proxy_url,
            proto,
            chunked,
            timeout_ms,
            evict_gen,
        );
        Ok((rb, egress_id, true))
    } else {
        Ok((fallback_client.post(target_url), None, false))
    }
}

/// Build a GET RequestBuilder routed through the Go native TLS/HTTP sidecar when active,
/// or falling back to the given Rust `rquest::Client` directly.
pub async fn get_with_sidecar_or_fallback(
    fallback_client: &Client,
    target_url: &str,
    account_id: Option<&str>,
    proto: &str,
    timeout_ms: Option<u64>,
) -> Result<(RequestBuilder, Option<String>, bool), String> {
    if let Some(relay) = relay_url() {
        let (proxy_url, egress_id, evict_gen) = resolve_strict_proxy_url(account_id).await?;
        let rb = prepare_relay_request(
            loopback_client().get(&relay),
            target_url,
            &proxy_url,
            proto,
            false,
            timeout_ms,
            evict_gen,
        );
        Ok((rb, egress_id, true))
    } else {
        Ok((fallback_client.get(target_url), None, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_loopback_client_emits_no_unexpected_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
            raw
        });

        let relay = format!("http://{}/relay", addr);
        let rb = prepare_relay_request(
            loopback_client().post(&relay),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent",
            "http://127.0.0.1:8888",
            "h1",
            false,
            Some(60_000),
            Some(1),
        )
        .header("User-Agent", "antigravity/hub/2.31.0 (aidev_client; os_type=windows; arch=amd64; cl=1017595969)")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(r#"{"model":"gemini-2.5-flash"}"#);

        let resp = rb.send().await.unwrap();
        assert_eq!(resp.status(), 200);

        let raw = server.await.unwrap();
        let headers_section = raw.split("\r\n\r\n").next().unwrap_or("");
        let lower = headers_section.to_ascii_lowercase();
        assert!(!lower.contains("\naccept:"), "unexpected Accept header emitted: {}", headers_section);
        assert!(!lower.contains("\ncookie:"), "unexpected Cookie header emitted: {}", headers_section);
        assert!(!lower.contains("\nsec-"), "unexpected Sec-* browser header emitted: {}", headers_section);
    }
}
