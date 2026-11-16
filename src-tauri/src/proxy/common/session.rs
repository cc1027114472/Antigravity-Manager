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

/// Maps a model ID to the official Antigravity IDE `labels.model_enum`
pub fn resolve_official_model_enum(model: &str) -> &'static str {
    // Ordered most-specific first so variants like -low/-medium/-lite/-thinking never collide.
    const MODEL_ENUM_TABLE: &[(&str, &str)] = &[
        ("claude-opus-5-5-low", "MODEL_PLACEHOLDER_M400"),
        ("claude-opus-5-5-medium", "MODEL_PLACEHOLDER_M401"),
        ("claude-opus-5", "MODEL_PLACEHOLDER_M402"),
        ("claude-opus", "MODEL_PLACEHOLDER_M402"),
        ("claude-sonnet-5-5-low", "MODEL_PLACEHOLDER_M403"),
        ("claude-sonnet-5-5-medium", "MODEL_PLACEHOLDER_M404"),
        ("claude-sonnet-5", "MODEL_PLACEHOLDER_M405"),
        ("claude", "MODEL_PLACEHOLDER_M405"),
        ("gemini-3.8-flash-tiered", "MODEL_PLACEHOLDER_M322"),
        ("gemini-3.8-flash-low", "MODEL_PLACEHOLDER_M320"),
        ("gemini-3.8-flash-medium", "MODEL_PLACEHOLDER_M319"),
        ("gemini-3.8-flash", "MODEL_PLACEHOLDER_M318"),
        ("gemini-4-argon-low", "MODEL_PLACEHOLDER_M419"),
        ("gemini-4-argon-medium", "MODEL_PLACEHOLDER_M418"),
        ("gemini-4-argon", "MODEL_PLACEHOLDER_M417"),
        ("gemini-3.5-flash", "MODEL_PLACEHOLDER_M198"),
        ("gemini-3.1-flash-image", "MODEL_PLACEHOLDER_M21"),
        ("-image", "MODEL_PLACEHOLDER_M21"),
        ("gemini-3.1-flash-lite", "MODEL_PLACEHOLDER_M50"),
        ("gemini-3-flash", "MODEL_PLACEHOLDER_M18"),
        ("gemini-2.5-flash-lite", "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE"),
        ("gemini-2.5-flash-thinking", "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING"),
        ("gemini-2.5-flash", "MODEL_GOOGLE_GEMINI_2_5_FLASH"),
        ("gemini-2.5-pro", "MODEL_GOOGLE_GEMINI_2_5_PRO"),
        ("gpt-oss-120b", "MODEL_OPENAI_GPT_OSS_120B_MEDIUM"),
        ("tab_jump_flash_lite_preview", "MODEL_PLACEHOLDER_M28"),
        ("tab_flash_lite_preview", "MODEL_PLACEHOLDER_M19"),
        ("chat_20706", "MODEL_CHAT_20706"),
        ("chat_23310", "MODEL_CHAT_23310"),
    ];

    let lower = model.to_lowercase();
    MODEL_ENUM_TABLE
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, enum_name)| *enum_name)
        .unwrap_or("MODEL_PLACEHOLDER_M318")
}

/// Generates a 5-segment requestId AND matching `request.labels` object
/// aligned with official Antigravity IDE captures:
/// agent/{cascade_uuid}/{timestamp_ms}/{trajectory_uuid}/{step_index}
pub fn generate_official_request_id_and_labels(
    account_id: Option<&str>,
    session_hint: &str,
    step_index: usize,
    final_model: &str,
) -> (String, serde_json::Value) {
    let ts = chrono::Utc::now().timestamp_millis();
    let acc = account_id.unwrap_or("anon");
    let cascade_seed = format!("cascade:{}:{}", acc, session_hint);
    // Trajectory is refreshed hourly or per conversation segment
    let trajectory_seed = format!("trajectory:{}:{}:{}", acc, session_hint, ts / 3_600_000);

    let cascade_uuid = derive_uuid_from_seed(&cascade_seed);
    let trajectory_uuid = derive_uuid_from_seed(&trajectory_seed);
    let step = step_index.max(1);
    let last_step = step.saturating_sub(1);
    let lower = final_model.to_lowercase();
    let is_claude = lower.contains("claude");
    let is_non_gemini = !lower.contains("gemini");
    let model_enum = resolve_official_model_enum(final_model);

    let request_id = format!("agent/{}/{}/{}/{}", cascade_uuid, ts, trajectory_uuid, step);
    let mut labels_map = serde_json::Map::new();
    labels_map.insert("cascade_id".to_string(), serde_json::json!(cascade_uuid.to_string()));
    if last_step > 0 {
        let last_exec_seed = format!("exec:{}:{}:{}", acc, session_hint, last_step);
        let last_exec_uuid = derive_uuid_from_seed(&last_exec_seed);
        labels_map.insert(
            "last_execution_id".to_string(),
            serde_json::json!(last_exec_uuid.to_string()),
        );
    }
    labels_map.insert("last_step_index".to_string(), serde_json::json!(last_step.to_string()));
    labels_map.insert("model_enum".to_string(), serde_json::json!(model_enum));
    labels_map.insert(
        "request_id".to_string(),
        serde_json::json!(format!("{}-{}", trajectory_uuid, last_step)),
    );
    labels_map.insert(
        "root_cascade_id".to_string(),
        serde_json::json!(cascade_uuid.to_string()),
    );
    labels_map.insert(
        "trajectory_id".to_string(),
        serde_json::json!(trajectory_uuid.to_string()),
    );
    labels_map.insert(
        "used_claude".to_string(),
        serde_json::json!(if is_claude { "true" } else { "false" }),
    );
    labels_map.insert(
        "used_claude_conservative".to_string(),
        serde_json::json!(if is_claude { "true" } else { "false" }),
    );
    labels_map.insert(
        "used_non_gemini_model".to_string(),
        serde_json::json!(if is_non_gemini { "true" } else { "false" }),
    );
    let labels = serde_json::Value::Object(labels_map);

    (request_id, labels)
}

/// Generates a 5-segment requestId matching official Antigravity IDE:
/// agent/{cascade_uuid}/{timestamp_ms}/{trajectory_uuid}/{step_index}
pub fn generate_official_request_id(
    account_id: Option<&str>,
    session_hint: &str,
    step_index: usize,
) -> String {
    let (request_id, _) = generate_official_request_id_and_labels(
        account_id,
        session_hint,
        step_index,
        "gemini-3.8-flash-high",
    );
    request_id
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

        // Step 1 omits last_execution_id; step > 1 includes last_execution_id and maps Claude 5.5 enum
        let (_, labels_step1) = generate_official_request_id_and_labels(
            Some("acc_1"),
            "sess_abc",
            1,
            "claude-sonnet-5-5-high",
        );
        assert!(labels_step1.get("last_execution_id").is_none());
        assert_eq!(labels_step1["model_enum"], "MODEL_PLACEHOLDER_M405");
        assert_eq!(labels_step1["used_claude"], "true");

        let (_, labels_step2) = generate_official_request_id_and_labels(
            Some("acc_1"),
            "sess_abc",
            2,
            "gemini-3.5-flash-lite",
        );
        assert!(labels_step2.get("last_execution_id").is_some());
        assert_eq!(labels_step2["model_enum"], "MODEL_PLACEHOLDER_M198");
        assert_eq!(labels_step2["used_claude"], "false");
    }

    #[test]
    fn test_resolve_official_model_enum_matches_all_26_fetch_models() {
        let expected = [
            ("chat_20706", "MODEL_CHAT_20706"),
            ("chat_23310", "MODEL_CHAT_23310"),
            ("claude-opus-5-5-high", "MODEL_PLACEHOLDER_M402"),
            ("claude-opus-5-5-low", "MODEL_PLACEHOLDER_M400"),
            ("claude-opus-5-5-medium", "MODEL_PLACEHOLDER_M401"),
            ("claude-sonnet-5-5-high", "MODEL_PLACEHOLDER_M405"),
            ("claude-sonnet-5-5-low", "MODEL_PLACEHOLDER_M403"),
            ("claude-sonnet-5-5-medium", "MODEL_PLACEHOLDER_M404"),
            ("gemini-2.5-flash", "MODEL_GOOGLE_GEMINI_2_5_FLASH"),
            ("gemini-2.5-flash-lite", "MODEL_GOOGLE_GEMINI_2_5_FLASH_LITE"),
            ("gemini-2.5-flash-thinking", "MODEL_GOOGLE_GEMINI_2_5_FLASH_THINKING"),
            ("gemini-2.5-pro", "MODEL_GOOGLE_GEMINI_2_5_PRO"),
            ("gemini-3-flash", "MODEL_PLACEHOLDER_M18"),
            ("gemini-3.1-flash-image", "MODEL_PLACEHOLDER_M21"),
            ("gemini-3.1-flash-lite", "MODEL_PLACEHOLDER_M50"),
            ("gemini-3.5-flash-lite", "MODEL_PLACEHOLDER_M198"),
            ("gemini-3.8-flash-high", "MODEL_PLACEHOLDER_M318"),
            ("gemini-3.8-flash-low", "MODEL_PLACEHOLDER_M320"),
            ("gemini-3.8-flash-medium", "MODEL_PLACEHOLDER_M319"),
            ("gemini-3.8-flash-tiered", "MODEL_PLACEHOLDER_M322"),
            ("gemini-4-argon-high", "MODEL_PLACEHOLDER_M417"),
            ("gemini-4-argon-low", "MODEL_PLACEHOLDER_M419"),
            ("gemini-4-argon-medium", "MODEL_PLACEHOLDER_M418"),
            ("gpt-oss-120b-medium", "MODEL_OPENAI_GPT_OSS_120B_MEDIUM"),
            ("tab_flash_lite_preview", "MODEL_PLACEHOLDER_M19"),
            ("tab_jump_flash_lite_preview", "MODEL_PLACEHOLDER_M28"),
        ];

        for (model_id, expected_enum) in expected {
            assert_eq!(
                resolve_official_model_enum(model_id),
                expected_enum,
                "mismatch for model_id={}",
                model_id
            );
        }
    }
}
