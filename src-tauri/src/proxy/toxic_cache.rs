use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use axum::http::StatusCode;
use once_cell::sync::Lazy;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToxicEntry {
    pub status: u16,
    pub error_body: String,
    pub expired_at: i64,
}

#[derive(Clone, Debug)]
pub struct Request503State {
    pub count: u32,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub cooldown_until: Option<i64>,
    pub last_error_body: String,
}

fn toxic_cache_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::modules::account::get_data_dir()
        .ok()
        .map(|d| d.join("toxic_cache.json"))
}

pub fn load_toxic_cache_from_path(path: &Path, target: &DashMap<String, ToxicEntry>, now: i64) -> usize {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return 0;
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, ToxicEntry>>(&raw) else {
        return 0;
    };
    let mut restored = 0usize;
    for (k, v) in map {
        if v.expired_at > now {
            target.insert(k, v);
            restored += 1;
        }
    }
    restored
}

pub fn save_toxic_cache_to_path(path: &Path, source: &DashMap<String, ToxicEntry>, now: i64) {
    let snapshot: HashMap<String, ToxicEntry> = source
        .iter()
        .filter(|e| e.value().expired_at > now)
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            if std::fs::rename(&tmp, path).is_err() {
                let _ = std::fs::write(path, json);
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

pub static TOXIC_CACHE: Lazy<Arc<DashMap<String, ToxicEntry>>> = Lazy::new(|| {
    let map = DashMap::new();
    if let Some(path) = toxic_cache_path() {
        let now = chrono::Utc::now().timestamp();
        let restored = load_toxic_cache_from_path(&path, &map, now);
        if restored > 0 {
            tracing::info!("[ToxicCache] Restored {} unexpired toxic payload fingerprint(s) from disk", restored);
        }
    }
    Arc::new(map)
});

pub static REQUEST_503_TRACKER: Lazy<Arc<DashMap<String, Request503State>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

pub const DEFAULT_TOXIC_TTL_SECS: u64 = 1800; // 30 分钟
pub const REQUEST_503_WINDOW_SECS: i64 = 120; // 2 分钟滑动统计窗口
pub const REQUEST_503_MAX_COUNT: u32 = 4; // 同一请求累计 4 次 503 触发本地冷却拦截
pub const REQUEST_503_COOLDOWN_SECS: u64 = 45; // 触发后本地冷却拦截 45 秒

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
    if let Some(path) = toxic_cache_path() {
        save_toxic_cache_to_path(&path, &TOXIC_CACHE, now);
    }
}

/// 检查同一请求指纹是否因累计 4 次 503 处于本地冷却拦截窗口中
/// 若处于冷却期，返回 `Some((剩余冷却秒数, 最近一次 503 响应体))`
pub fn check_request_503_cooldown(fingerprint: &str) -> Option<(u64, String)> {
    let now = chrono::Utc::now().timestamp();
    if let Some(entry) = REQUEST_503_TRACKER.get(fingerprint) {
        if let Some(until) = entry.cooldown_until {
            if until > now {
                let remaining = (until - now).max(1) as u64;
                let fp_prefix = if fingerprint.len() >= 12 {
                    &fingerprint[..12]
                } else {
                    fingerprint
                };
                tracing::warn!(
                    "⏳ [Request503Breaker] Identical request hit 503 cooldown lock (Fingerprint: {}, count: {}, retry after {}s) — rejecting locally to protect account pool",
                    fp_prefix,
                    entry.count,
                    remaining
                );
                return Some((remaining, entry.last_error_body.clone()));
            }
        }
    }
    // 若冷却已到期或超过滑动时间窗，清理过期状态
    REQUEST_503_TRACKER.remove_if(fingerprint, |_, entry| {
        if let Some(until) = entry.cooldown_until {
            until <= now
        } else {
            now - entry.last_seen_at > REQUEST_503_WINDOW_SECS
        }
    });
    None
}

/// 记录同一请求指纹发生了一次上游 503/529：
/// - 120s 滑动窗口内累加计数；
/// - 当累计达到 `REQUEST_503_MAX_COUNT` (4 次，即换号后依然 503) 时，自动开启 `REQUEST_503_COOLDOWN_SECS` (45 秒) 本地冷却拦截锁。
/// 返回 `(当前累计次数, 若刚触发冷却则返回 Some(cooldown_secs))`
pub fn record_request_503(fingerprint: &str, error_body: &str) -> (u32, Option<u64>) {
    let now = chrono::Utc::now().timestamp();
    let fp_prefix = if fingerprint.len() >= 12 {
        &fingerprint[..12]
    } else {
        fingerprint
    };

    let mut entry = REQUEST_503_TRACKER
        .entry(fingerprint.to_string())
        .or_insert_with(|| Request503State {
            count: 0,
            first_seen_at: now,
            last_seen_at: now,
            cooldown_until: None,
            last_error_body: error_body.to_string(),
        });

    // 超过滑动时间窗或上一次冷却已过期，重新从 1 开始计数
    let window_expired = now - entry.last_seen_at > REQUEST_503_WINDOW_SECS;
    let cooldown_expired = entry.cooldown_until.map_or(false, |u| u <= now);
    if window_expired || cooldown_expired {
        entry.count = 0;
        entry.first_seen_at = now;
        entry.cooldown_until = None;
    }

    entry.count += 1;
    entry.last_seen_at = now;
    if !error_body.is_empty() {
        entry.last_error_body = error_body.to_string();
    }

    if entry.count >= REQUEST_503_MAX_COUNT && entry.cooldown_until.is_none() {
        let until = now + REQUEST_503_COOLDOWN_SECS as i64;
        entry.cooldown_until = Some(until);
        tracing::warn!(
            "🛑 [Request503Breaker] Request fingerprint {} hit {} consecutive 503s across accounts within {}s! Locking request locally for {}s to prevent pool exhaustion.",
            fp_prefix,
            entry.count,
            now - entry.first_seen_at,
            REQUEST_503_COOLDOWN_SECS
        );
        return (entry.count, Some(REQUEST_503_COOLDOWN_SECS));
    }

    (entry.count, None)
}

/// 当请求成功返回 200 OK 时，清除该请求指纹的 503 失败计数
pub fn clear_request_503(fingerprint: &str) {
    REQUEST_503_TRACKER.remove(fingerprint);
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

    #[test]
    fn test_request_503_breaker() {
        let fp = "test_req_503_fp";
        clear_request_503(fp);
        assert_eq!(check_request_503_cooldown(fp), None);

        // 第 1 ~ 3 次 503：仅计数，不锁请求
        assert_eq!(record_request_503(fp, "503 #1"), (1, None));
        assert_eq!(check_request_503_cooldown(fp), None);
        assert_eq!(record_request_503(fp, "503 #2"), (2, None));
        assert_eq!(check_request_503_cooldown(fp), None);
        assert_eq!(record_request_503(fp, "503 #3"), (3, None));
        assert_eq!(check_request_503_cooldown(fp), None);

        // 第 4 次 503：触发 45s 本地请求冷却锁
        let (cnt, cd) = record_request_503(fp, "503 #4");
        assert_eq!(cnt, 4);
        assert_eq!(cd, Some(REQUEST_503_COOLDOWN_SECS));

        let hit = check_request_503_cooldown(fp);
        assert!(hit.is_some());
        let (rem, body) = hit.unwrap();
        assert!(rem > 0 && rem <= REQUEST_503_COOLDOWN_SECS);
        assert_eq!(body, "503 #4");

        // 成功后清理
        clear_request_503(fp);
        assert_eq!(check_request_503_cooldown(fp), None);
    }

    #[test]
    fn test_toxic_cache_disk_roundtrip_filters_expired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toxic_cache.json");
        let now = 1_700_000_000i64;

        let src = DashMap::new();
        src.insert(
            "active_fp".to_string(),
            ToxicEntry {
                status: 403,
                error_body: "TOS_VIOLATION".to_string(),
                expired_at: now + 600,
            },
        );
        src.insert(
            "expired_fp".to_string(),
            ToxicEntry {
                status: 403,
                error_body: "Old".to_string(),
                expired_at: now - 10,
            },
        );

        save_toxic_cache_to_path(&path, &src, now);
        let dst = DashMap::new();
        let restored = load_toxic_cache_from_path(&path, &dst, now);
        assert_eq!(restored, 1);
        assert!(dst.contains_key("active_fp"));
        assert!(!dst.contains_key("expired_fp"));
    }
}
