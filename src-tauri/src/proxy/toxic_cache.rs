use std::sync::Arc;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use axum::http::StatusCode;
use once_cell::sync::Lazy;

#[derive(Clone, Debug)]
pub struct ToxicEntry {
    pub status: u16,
    pub error_body: String,
    pub expired_at: i64,
}

pub static TOXIC_CACHE: Lazy<Arc<DashMap<String, ToxicEntry>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

pub const DEFAULT_TOXIC_TTL_SECS: u64 = 1800; // 30 分钟

/// 获取 toxic_cache 内存映射表
pub fn get_toxic_cache() -> Arc<DashMap<String, ToxicEntry>> {
    TOXIC_CACHE.clone()
}

/// 计算请求体与模型的 SHA-256 毒丸指纹
pub fn calculate_fingerprint(model: &str, body_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update(b"::");
    hasher.update(body_bytes);
    format!("{:x}", hasher.finalize())
}

/// 检查请求是否命中 403 毒丸黑名单（TTL 30 分钟）
/// 若命中且未过期，返回缓存的 StatusCode 和错误响应体；若过期自动清除
pub fn check_toxic(fingerprint: &str) -> Option<(StatusCode, String)> {
    let now = chrono::Utc::now().timestamp();
    if let Some(entry) = TOXIC_CACHE.get(fingerprint) {
        if entry.expired_at > now {
            let fp_prefix = if fingerprint.len() >= 12 {
                &fingerprint[..12]
            } else {
                fingerprint
            };
            tracing::warn!(
                "🛑 [ToxicCache] Request hit toxic cache (Fingerprint: {}), fast early rejection (TTL left: {}s)",
                fp_prefix,
                entry.expired_at - now
            );
            let code = StatusCode::from_u16(entry.status).unwrap_or(StatusCode::FORBIDDEN);
            return Some((code, entry.error_body.clone()));
        }
    }
    // 已过期，自动清理
    TOXIC_CACHE.remove_if(fingerprint, |_, entry| entry.expired_at <= now);
    None
}

/// 记录 403 毒丸指纹，默认 TTL 为 1800 秒（30 分钟）
pub fn record_toxic(fingerprint: &str, status: u16, error_body: String, ttl_secs: u64) {
    let now = chrono::Utc::now().timestamp();
    let ttl = if ttl_secs == 0 {
        DEFAULT_TOXIC_TTL_SECS
    } else {
        ttl_secs
    };
    let expired_at = now + ttl as i64;
    let fp_prefix = if fingerprint.len() >= 12 {
        &fingerprint[..12]
    } else {
        fingerprint
    };
    tracing::warn!(
        "☣️ [ToxicCache] Poison payload recorded! Fingerprint: {}, TTL: {}s. Subsequent identical requests will be rejected immediately.",
        fp_prefix,
        ttl
    );
    TOXIC_CACHE.insert(
        fingerprint.to_string(),
        ToxicEntry {
            status,
            error_body,
            expired_at,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_fingerprint() {
        let fp1 = calculate_fingerprint("gemini-2.5-flash", br#"{"contents":[]}"#);
        let fp2 = calculate_fingerprint("gemini-2.5-flash", br#"{"contents":[]}"#);
        let fp3 = calculate_fingerprint("gemini-2.5-pro", br#"{"contents":[]}"#);
        assert_eq!(fp1, fp2);
        assert_ne!(fp1, fp3);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn test_record_and_check_toxic() {
        let fp = "test_fp_1234567890";
        assert_eq!(check_toxic(fp), None);

        record_toxic(fp, 403, "TOS_VIOLATION: Account suspended".to_string(), 1800);
        let hit = check_toxic(fp);
        assert!(hit.is_some());
        let (status, body) = hit.unwrap();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "TOS_VIOLATION: Account suspended");
    }

    #[test]
    fn test_toxic_expiration() {
        let fp = "test_fp_expired";
        let past = chrono::Utc::now().timestamp() - 10;
        TOXIC_CACHE.insert(
            fp.to_string(),
            ToxicEntry {
                status: 403,
                error_body: "Expired".to_string(),
                expired_at: past,
            },
        );

        assert_eq!(check_toxic(fp), None);
        assert!(!TOXIC_CACHE.contains_key(fp));
    }
}
