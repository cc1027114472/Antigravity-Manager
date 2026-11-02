use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use lru::LruCache;
use parking_lot::Mutex;

// Node.js proxy uses 2 hours TTL
const SIGNATURE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MIN_SIGNATURE_LENGTH: usize = 50;

// Bounded hard limits for different cache layers (guaranteeing O(1) eviction)
const TOOL_CACHE_LIMIT: usize = 1000;    // Layer 1: Tool-specific signatures
const FAMILY_CACHE_LIMIT: usize = 1000;  // Layer 2: Model family mappings
const SESSION_CACHE_LIMIT: usize = 2000; // Layer 3 & 4: Session-based signatures & reasonings

/// Cache entry with timestamp for TTL
#[derive(Clone, Debug)]
struct CacheEntry<T> {
    data: T,
    timestamp: SystemTime,
}

/// Specialized entry for session-based signatures to track message count
#[derive(Clone, Debug)]
struct SessionSignatureEntry {
    signature: String,
    message_count: usize,
}

impl<T> CacheEntry<T> {
    fn new(data: T) -> Self {
        Self {
            data,
            timestamp: SystemTime::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.timestamp.elapsed().unwrap_or(Duration::ZERO) > SIGNATURE_TTL
    }
}

/// Quad-layer bounded LRU signature cache to handle:
/// 1. Signature recovery for tool calls (when clients strip them)
/// 2. Cross-model compatibility checks (preventing Claude signatures on Gemini models)
/// 3. Session-based signature tracking (preventing cross-session pollution)
/// 4. Session-based reasoning history tracking
pub struct SignatureCache {
    /// Layer 1: Tool Use ID -> Thinking Signature
    tool_signatures: Mutex<LruCache<String, CacheEntry<String>>>,

    /// Layer 2: Signature -> Model Family
    thinking_families: Mutex<LruCache<String, CacheEntry<String>>>,

    /// Layer 3: Session ID -> Map of Message Count -> Thinking Signature
    session_signatures: Mutex<LruCache<String, CacheEntry<HashMap<usize, SessionSignatureEntry>>>>,

    /// Layer 4: Session ID -> Assistant Reasoning Text History
    session_reasonings: Mutex<LruCache<String, CacheEntry<Vec<String>>>>,
}

impl SignatureCache {
    fn new() -> Self {
        Self {
            tool_signatures: Mutex::new(LruCache::new(NonZeroUsize::new(TOOL_CACHE_LIMIT).unwrap())),
            thinking_families: Mutex::new(LruCache::new(NonZeroUsize::new(FAMILY_CACHE_LIMIT).unwrap())),
            session_signatures: Mutex::new(LruCache::new(NonZeroUsize::new(SESSION_CACHE_LIMIT).unwrap())),
            session_reasonings: Mutex::new(LruCache::new(NonZeroUsize::new(SESSION_CACHE_LIMIT).unwrap())),
        }
    }

    /// Global singleton instance
    pub fn global() -> &'static SignatureCache {
        static INSTANCE: OnceLock<SignatureCache> = OnceLock::new();
        INSTANCE.get_or_init(SignatureCache::new)
    }

    /// Store a tool call signature with strict LRU hard-cap eviction
    pub fn cache_tool_signature(&self, tool_use_id: &str, signature: String) {
        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        let mut cache = self.tool_signatures.lock();
        tracing::debug!(
            "[SignatureCache] Caching tool signature for id: {}",
            tool_use_id
        );
        cache.put(tool_use_id.to_string(), CacheEntry::new(signature));
    }

    /// Retrieve a signature for a tool_use_id with O(1) access and TTL verification
    pub fn get_tool_signature(&self, tool_use_id: &str) -> Option<String> {
        let mut cache = self.tool_signatures.lock();
        if let Some(entry) = cache.get(tool_use_id) {
            if !entry.is_expired() {
                tracing::debug!(
                    "[SignatureCache] Hit tool signature for id: {}",
                    tool_use_id
                );
                return Some(entry.data.clone());
            } else {
                tracing::debug!(
                    "[SignatureCache] Tool signature expired for id: {}",
                    tool_use_id
                );
            }
        } else {
            return None;
        }
        // If expired, pop from LRU
        cache.pop(tool_use_id);
        None
    }

    /// Store model family for a signature with strict LRU hard-cap eviction
    pub fn cache_thinking_family(&self, signature: String, family: String) {
        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        let mut cache = self.thinking_families.lock();
        tracing::debug!(
            "[SignatureCache] Caching thinking family for sig (len={}): {}",
            signature.len(),
            family
        );
        cache.put(signature, CacheEntry::new(family));
    }

    /// Get model family for a signature with O(1) access and TTL verification
    pub fn get_signature_family(&self, signature: &str) -> Option<String> {
        let mut cache = self.thinking_families.lock();
        if let Some(entry) = cache.get(signature) {
            if !entry.is_expired() {
                return Some(entry.data.clone());
            } else {
                tracing::debug!("[SignatureCache] Signature family entry expired");
            }
        } else {
            return None;
        }
        // If expired, pop from LRU
        cache.pop(signature);
        None
    }

    // ===== Layer 3: Session-based Signature Storage =====

    /// Store the thinking signature for a session at a specific message count.
    /// This is the preferred method for tracking signatures across tool loops.
    pub fn cache_session_signature(
        &self,
        session_id: &str,
        signature: String,
        message_count: usize,
    ) {
        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        let mut cache = self.session_signatures.lock();
        if let Some(entry) = cache.get_mut(session_id) {
            // Update timestamp to refresh TTL
            entry.timestamp = SystemTime::now();

            // Detect if a rewind happened: remove future signatures
            entry.data.retain(|&mc, _| {
                if mc > message_count {
                    tracing::info!(
                        "[SignatureCache] Rewind detected for {} at count {}: removing future signature at count {}.",
                        session_id,
                        message_count,
                        mc
                    );
                    false
                } else {
                    true
                }
            });

            let should_store = match entry.data.get(&message_count) {
                None => true,
                Some(existing) => signature.len() > existing.signature.len(),
            };

            if should_store {
                tracing::debug!(
                    "[SignatureCache] Session {} (msg_count={}) -> storing signature (len={})",
                    session_id,
                    message_count,
                    signature.len()
                );
                entry.data.insert(
                    message_count,
                    SessionSignatureEntry {
                        signature,
                        message_count,
                    },
                );
            }
        } else {
            // Not in cache yet, create new entry and put
            let mut data = HashMap::new();
            data.insert(
                message_count,
                SessionSignatureEntry {
                    signature,
                    message_count,
                },
            );
            cache.put(session_id.to_string(), CacheEntry::new(data));
        }
    }

    /// Retrieve the latest thinking signature for a session.
    /// Returns None if not found or expired.
    pub fn get_session_signature(&self, session_id: &str) -> Option<String> {
        let mut cache = self.session_signatures.lock();
        if let Some(entry) = cache.get(session_id) {
            if !entry.is_expired() {
                if let Some(sig_entry) = entry.data.values().max_by_key(|e| e.message_count) {
                    tracing::debug!(
                        "[SignatureCache] Session {} (latest, msg_count={}) -> HIT (len={})",
                        session_id,
                        sig_entry.message_count,
                        sig_entry.signature.len()
                    );
                    return Some(sig_entry.signature.clone());
                }
            } else {
                tracing::debug!("[SignatureCache] Session {} -> EXPIRED", session_id);
            }
        } else {
            return None;
        }
        // If expired, pop from LRU
        cache.pop(session_id);
        None
    }

    /// Retrieve the thinking signature for a session at a specific message count.
    /// Returns None if not found or expired.
    pub fn get_session_signature_at(
        &self,
        session_id: &str,
        message_count: usize,
    ) -> Option<String> {
        let mut cache = self.session_signatures.lock();
        if let Some(entry) = cache.get(session_id) {
            if !entry.is_expired() {
                if let Some(sig_entry) = entry.data.get(&message_count) {
                    tracing::debug!(
                        "[SignatureCache] Session {} (msg_count={}) -> HIT (len={})",
                        session_id,
                        message_count,
                        sig_entry.signature.len()
                    );
                    return Some(sig_entry.signature.clone());
                }
            } else {
                tracing::debug!("[SignatureCache] Session {} -> EXPIRED", session_id);
            }
        } else {
            return None;
        }
        // If expired, pop from LRU
        cache.pop(session_id);
        None
    }

    // ===== Layer 4: Session Reasoning Text Storage =====

    /// Store reasoning text for a specific assistant turn in a session
    pub fn cache_session_reasoning(&self, session_id: &str, reasoning: String, turn_index: usize) {
        if reasoning.trim().is_empty() {
            return;
        }

        let mut cache = self.session_reasonings.lock();
        if let Some(entry) = cache.get_mut(session_id) {
            entry.timestamp = SystemTime::now();
            if turn_index >= entry.data.len() {
                entry.data.resize(turn_index + 1, String::new());
            }
            let old_len = entry.data[turn_index].len();
            if reasoning.len() > old_len {
                tracing::debug!(
                    "[SignatureCache] Session {} (turn={}) -> caching reasoning text (len: {} -> {})",
                    session_id,
                    turn_index,
                    old_len,
                    reasoning.len()
                );
                entry.data[turn_index] = reasoning;
            }
        } else {
            let mut data = Vec::new();
            data.resize(turn_index + 1, String::new());
            data[turn_index] = reasoning;
            cache.put(session_id.to_string(), CacheEntry::new(data));
        }
    }

    /// Retrieve reasoning text for a specific assistant turn in a session
    pub fn get_session_reasoning(&self, session_id: &str, turn_index: usize) -> Option<String> {
        let mut cache = self.session_reasonings.lock();
        if let Some(entry) = cache.get(session_id) {
            if !entry.is_expired() && turn_index < entry.data.len() {
                let text = &entry.data[turn_index];
                if !text.trim().is_empty() {
                    tracing::debug!(
                        "[SignatureCache] Session {} (turn={}) -> Hit reasoning text cache (len: {})",
                        session_id,
                        turn_index,
                        text.len()
                    );
                    return Some(text.clone());
                }
            }
        } else {
            return None;
        }
        // If expired, pop from LRU
        cache.pop(session_id);
        None
    }

    /// 删除指定会话的缓存签名
    #[allow(dead_code)] // 预留给管理接口或调试使用
    pub fn delete_session_signature(&self, session_id: &str) {
        let mut cache = self.session_signatures.lock();
        if cache.pop(session_id).is_some() {
            tracing::debug!(
                "[SignatureCache] Deleted session signature for: {}",
                session_id
            );
        }
    }

    /// Clear all caches (for testing or manual reset)
    #[allow(dead_code)] // Used in tests
    pub fn clear(&self) {
        self.tool_signatures.lock().clear();
        self.thinking_families.lock().clear();
        self.session_signatures.lock().clear();
        self.session_reasonings.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_signature_cache() {
        let cache = SignatureCache::new();
        let sig = "x".repeat(60); // Valid length

        cache.cache_tool_signature("tool_1", sig.clone());
        assert_eq!(cache.get_tool_signature("tool_1"), Some(sig));
        assert_eq!(cache.get_tool_signature("tool_2"), None);
    }

    #[test]
    fn test_min_length() {
        let cache = SignatureCache::new();
        cache.cache_tool_signature("tool_short", "short".to_string());
        assert_eq!(cache.get_tool_signature("tool_short"), None);
    }

    #[test]
    fn test_thinking_family() {
        let cache = SignatureCache::new();
        let sig = "y".repeat(60);

        cache.cache_thinking_family(sig.clone(), "claude".to_string());
        assert_eq!(cache.get_signature_family(&sig), Some("claude".to_string()));
    }

    #[test]
    fn test_session_signature() {
        let cache = SignatureCache::new();
        let sig1 = "a".repeat(60);
        let sig2 = "b".repeat(80); // Longer, should replace
        let sig3 = "c".repeat(40); // Too short, should be ignored

        // Initially empty
        assert!(cache.get_session_signature("sid-test123").is_none());

        // Store first signature
        cache.cache_session_signature("sid-test123", sig1.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig1.clone())
        );

        // Longer signature should replace (same msg count)
        cache.cache_session_signature("sid-test123", sig2.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig2.clone())
        );

        // Shorter valid signature should NOT replace (same msg count)
        cache.cache_session_signature("sid-test123", sig1.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig2.clone())
        );

        // Rewind: Shorter signature MUST replace if message count is lower
        cache.cache_session_signature("sid-test123", sig1.clone(), 3);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig1.clone())
        );

        // Too short signature should be ignored entirely (even if rewind)
        cache.cache_session_signature("sid-test123", sig3, 1);
        assert_eq!(cache.get_session_signature("sid-test123"), Some(sig1));

        // Different session should be isolated
        assert!(cache.get_session_signature("sid-other").is_none());
    }

    #[test]
    fn test_clear_all_caches() {
        let cache = SignatureCache::new();
        let sig = "x".repeat(60);

        cache.cache_tool_signature("tool_1", sig.clone());
        cache.cache_thinking_family(sig.clone(), "model".to_string());
        cache.cache_session_signature("sid-1", sig.clone(), 1);

        assert!(cache.get_tool_signature("tool_1").is_some());
        assert!(cache.get_signature_family(&sig).is_some());
        assert!(cache.get_session_signature("sid-1").is_some());

        cache.clear();

        assert!(cache.get_tool_signature("tool_1").is_none());
        assert!(cache.get_signature_family(&sig).is_none());
        assert!(cache.get_session_signature("sid-1").is_none());
    }

    #[test]
    fn test_lru_hard_capacity_eviction() {
        let cache = SignatureCache::new();
        let sig = "s".repeat(60);

        // Fill tool cache up to TOOL_CACHE_LIMIT + 10
        for i in 0..(TOOL_CACHE_LIMIT + 10) {
            cache.cache_tool_signature(&format!("tool_{}", i), sig.clone());
        }

        // Check that older items were strictly evicted in O(1)
        assert_eq!(cache.get_tool_signature("tool_0"), None);
        assert_eq!(cache.get_tool_signature("tool_9"), None);
        // Latest items must still exist
        assert_eq!(cache.get_tool_signature(&format!("tool_{}", TOOL_CACHE_LIMIT + 9)), Some(sig));
    }
}
