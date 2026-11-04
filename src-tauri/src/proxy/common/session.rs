// [NEW v4.1.24] Tools for deriving stable session identifiers
use sha2::{Digest, Sha256};

/// From account ID string to a stable negative signed integer session ID
/// Implements FNV-1a hash which matches the official client behavior of sending
/// a large negative integer for `sessionId`.
pub fn derive_session_id(account_id: &str) -> String {
    let mut hash: i64 = -3750763034362895579_i64; // FNV offset basis
    for byte in account_id.bytes() {
        hash = hash.wrapping_mul(1099511628211_i64);
        hash ^= byte as i64;
    }
    hash.to_string()
}

/// Derives a deterministic RFC 4122 v4 UUID from an arbitrary seed string using SHA-256
pub fn derive_uuid_from_seed(seed: &str) -> uuid::Uuid {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&result[0..16]);
    // Set version to 4 (bits 4-7 of time_hi_and_version to 0100)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set variant to RFC 4122 (bits 6-7 of clock_seq_hi_and_reserved to 10)
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

/// Generates a 5-segment requestId matching official Antigravity IDE:
/// agent/{cascade_uuid}/{timestamp_ms}/{trajectory_uuid}/{step_index}
pub fn generate_official_request_id(
    account_id: Option<&str>,
    session_hint: &str,
    step_index: usize,
) -> String {
    let ts = chrono::Utc::now().timestamp_millis();
    let acc = account_id.unwrap_or("anon");
    let cascade_seed = format!("cascade:{}:{}", acc, session_hint);
    // Trajectory is refreshed hourly or per conversation segment
    let trajectory_seed = format!("trajectory:{}:{}:{}", acc, session_hint, ts / 3_600_000);

    let cascade_uuid = derive_uuid_from_seed(&cascade_seed);
    let trajectory_uuid = derive_uuid_from_seed(&trajectory_seed);
    let step = step_index.max(1);

    format!("agent/{}/{}/{}/{}", cascade_uuid, ts, trajectory_uuid, step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_session_id() {
        let x = derive_session_id("my_account@gmail.com");
        let y = derive_session_id("my_account@gmail.com");
        assert_eq!(x, y);
    }

    #[test]
    fn test_generate_official_request_id() {
        let req_id = generate_official_request_id(Some("acc_1"), "sess_abc", 3);
        let parts: Vec<&str> = req_id.split('/').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], "agent");
        assert_eq!(parts[4], "3");
        // parts[1] and parts[3] must be valid UUIDs
        assert!(uuid::Uuid::parse_str(parts[1]).is_ok());
        assert!(uuid::Uuid::parse_str(parts[3]).is_ok());
    }
}
