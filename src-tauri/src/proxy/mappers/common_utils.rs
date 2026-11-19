// Common utilities for request mapping across all protocols
// Provides unified grounding/networking logic

use serde_json::{json, Value};

/// Default Antigravity system identity prepended when no client Antigravity identity is provided.
pub const FALLBACK_ANTIGRAVITY_IDENTITY: &str =
    "<identity>\n\
    You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.\n\
    You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.\n\
    **Absolute paths only**\n\
    **Proactiveness**\n\
    </identity>";

/// Search-specific system identity used when `requestType == "web_search"`.
pub const FALLBACK_WEB_SEARCH_IDENTITY: &str =
    "You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar.";

/// Select the default identity prompt for a given v1internal `requestType`.
pub fn default_identity_for_request_type(request_type: &str) -> &'static str {
    if request_type == "web_search" {
        FALLBACK_WEB_SEARCH_IDENTITY
    } else {
        FALLBACK_ANTIGRAVITY_IDENTITY
    }
}

/// Merge non-empty system instruction text chunks into a single string so
/// `systemInstruction.parts` stays single-part (`len == 1`), matching official captures.
pub fn merge_system_instruction_chunks<S: AsRef<str>>(chunks: &[S]) -> Option<String> {
    let non_empty: Vec<&str> = chunks
        .iter()
        .map(|s| s.as_ref().trim_matches(|c| c == '\r' || c == '\n'))
        .filter(|s| !s.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        None
    } else {
        Some(non_empty.join("\n\n"))
    }
}

/// Base64 sentinel observed in official Antigravity captures (`"skip_thought_signature_validator"`).
pub const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str =
    "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=";

/// Model used by official Antigravity for standalone `web_search` requests.
pub const WEB_SEARCH_MODEL: &str = "gemini-3.5-flash-lite";

/// Build the official `googleSearch` tool declaration observed in captures.
pub fn build_official_google_search_tool() -> Value {
    json!({
        "googleSearch": {
            "enhancedContent": {
                "imageSearch": {
                    "maxResultCount": 5
                }
            }
        }
    })
}

/// Normalize any JSON value to a UTF-8 string matching official `functionResponse.response.{output,error}` captures.
pub fn stringify_function_response_value(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Normalize a Gemini `functionResponse` payload (`response` object) in-place:
/// - Legacy `"result"` key is migrated to `"output"` if `"output"` is absent
/// - Non-string values (`dict`, `array`, `number`, `bool`, `null`) are stringified so
///   upstream always receives `response.{output,error}` as strings (matching 314,134/314,134 captures)
/// - Missing/empty `response` object defaults to `{"output": ""}`
pub fn normalize_function_response_object(resp_val: &mut Value) {
    if let Value::Object(obj) = resp_val {
        if let Some(result_val) = obj.remove("result") {
            if !obj.contains_key("output") {
                obj.insert("output".to_string(), result_val);
            }
        }
        if obj.is_empty() {
            obj.insert("output".to_string(), Value::String(String::new()));
            return;
        }
        for (_k, v) in obj.iter_mut() {
            if !v.is_string() {
                *v = Value::String(stringify_function_response_value(v));
            }
        }
    } else {
        let s = stringify_function_response_value(resp_val);
        *resp_val = json!({ "output": s });
    }
}

/// Build a standardized Gemini `functionResponse` part using official `"output"` / `"error"` keys
/// with guaranteed UTF-8 string values (matching 314,134/314,134 captured upstream payloads).
pub fn build_function_response_part(
    name: &str,
    id: Option<&str>,
    response_val: Value,
    is_error: bool,
) -> Value {
    let key = if is_error { "error" } else { "output" };
    let val_str = stringify_function_response_value(&response_val);
    let mut fr = serde_json::Map::new();
    fr.insert("name".to_string(), json!(name));
    fr.insert("response".to_string(), json!({ key: val_str }));
    if let Some(id_str) = id.filter(|s| !s.is_empty()) {
        fr.insert("id".to_string(), json!(id_str));
    }
    json!({ "functionResponse": Value::Object(fr) })
}

/// Ensure every `functionCall` and `functionResponse` part across `contents` carries a
/// non-empty `"id"` and pairs unlabelled responses with the earliest pending call ID of the
/// same tool name (`call_{name}_{count}` fallback, matching official captures).
pub fn ensure_paired_tool_call_ids(contents: &mut [Value], log_prefix: &str) {
    let mut pending_call_ids: std::collections::HashMap<
        String,
        std::collections::VecDeque<String>,
    > = std::collections::HashMap::new();
    let mut global_name_counters: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for content in contents.iter_mut() {
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        for part in parts {
            let Some(obj) = part.as_object_mut() else {
                continue;
            };

            if let Some(fc) = obj.get_mut("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let existing_id = fc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let call_id = if let Some(id) = existing_id {
                    id
                } else {
                    let count = global_name_counters.entry(name.clone()).or_insert(0);
                    let call_id = format!("call_{}_{}", name, count);
                    *count += 1;

                    if let Some(fc_obj) = fc.as_object_mut() {
                        fc_obj.insert("id".to_string(), json!(call_id));
                    }
                    tracing::debug!(
                        "{} Request stage: Injected missing call_id '{}'",
                        log_prefix,
                        call_id
                    );
                    call_id
                };
                pending_call_ids.entry(name).or_default().push_back(call_id);
            }

            if let Some(fr) = obj.get_mut("functionResponse") {
                let name = fr
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let existing_id = fr
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                if let Some(existing) = existing_id {
                    if let Some(q) = pending_call_ids.get_mut(&name) {
                        if let Some(pos) = q.iter().position(|x| x == &existing) {
                            q.remove(pos);
                        }
                    }
                } else {
                    let matched_id = pending_call_ids
                        .get_mut(&name)
                        .and_then(|q| q.pop_front());
                    let call_id = if let Some(id) = matched_id {
                        id
                    } else {
                        let count = global_name_counters.entry(name.clone()).or_insert(0);
                        let generated = format!("call_{}_{}", name, count);
                        *count += 1;
                        generated
                    };

                    if let Some(fr_obj) = fr.as_object_mut() {
                        fr_obj.insert("id".to_string(), json!(call_id));
                    }
                    tracing::debug!(
                        "{} Request stage: Injected synced response_id '{}'",
                        log_prefix,
                        call_id
                    );
                }
            }
        }
    }
}

/// Normalize inner & outer v1internal envelope fields for `web_search` requests
/// to match official Antigravity captures (`generationConfig: {"candidateCount": 1}`,
/// and no `sessionId`, `labels`, `toolConfig`, or outer `requestId`).
pub fn apply_web_search_envelope_overrides(outer_body: &mut Value) {
    if let Some(inner) = outer_body.get_mut("request") {
        if let Some(inner_obj) = inner.as_object_mut() {
            inner_obj.insert("generationConfig".to_string(), json!({ "candidateCount": 1 }));
            inner_obj.remove("sessionId");
            inner_obj.remove("labels");
            inner_obj.remove("toolConfig");
        }
        reorder_inner_request_official(inner);
    }
    if let Some(outer_obj) = outer_body.as_object_mut() {
        // Avoid Map::remove (= IndexMap::swap_remove) scrambling relative key order
        let old = std::mem::take(outer_obj);
        for (k, v) in old {
            if k != "requestId" {
                outer_obj.insert(k, v);
            }
        }
    }
}

/// Minimum `maxOutputTokens` allowed on normal OpenAI/Claude `agent` calls before
/// normalizing small probe caps (`512`, `1024`, etc.) to official model defaults.
pub const MIN_AGENT_MAX_OUTPUT_TOKENS: i64 = 4096;

/// Normalize small client-provided `max_tokens` on `agent` calls so health-check
/// / probe caps (`max_tokens: 512`) do not leak upstream as bot fingerprints or
/// starve `thinkingLevel` reasoning before emitting text.
pub fn resolve_agent_max_output_tokens(
    raw_max_tokens: Option<i64>,
    default_max_output_tokens: u64,
    mapped_model: &str,
    is_agent_req: bool,
) -> Option<i64> {
    match raw_max_tokens {
        Some(max_tokens) => {
            if is_agent_req && max_tokens > 0 && max_tokens < MIN_AGENT_MAX_OUTPUT_TOKENS {
                if should_omit_default_max_output_tokens(mapped_model) {
                    None
                } else {
                    Some(default_max_output_tokens as i64)
                }
            } else {
                Some(max_tokens)
            }
        }
        None => {
            if !is_agent_req || should_omit_default_max_output_tokens(mapped_model) {
                None
            } else {
                Some(default_max_output_tokens as i64)
            }
        }
    }
}

/// Reorder inner `v1internal` `request` object keys in place to match official Antigravity captures
/// (`contents -> systemInstruction -> tools -> toolConfig -> labels -> generationConfig -> sessionId`,
/// followed by any remaining keys such as `metadata` or `cachedContent`).
pub fn reorder_inner_request_official(inner: &mut Value) {
    let Some(old_obj) = inner.as_object_mut() else {
        return;
    };
    let old = std::mem::take(old_obj);
    let mut ordered = serde_json::Map::with_capacity(old.len());
    const OFFICIAL_ORDER: &[&str] = &[
        "contents",
        "systemInstruction",
        "tools",
        "toolConfig",
        "labels",
        "generationConfig",
        "sessionId",
    ];
    let mut slots: [Option<Value>; OFFICIAL_ORDER.len()] = Default::default();
    let mut extras: Vec<(String, Value)> = Vec::new();
    for (k, v) in old {
        if let Some(pos) = OFFICIAL_ORDER.iter().position(|&target| target == k.as_str()) {
            slots[pos] = Some(v);
        } else {
            extras.push((k, v));
        }
    }
    for (idx, val_opt) in slots.into_iter().enumerate() {
        if let Some(val) = val_opt {
            ordered.insert(OFFICIAL_ORDER[idx].to_string(), val);
        }
    }
    for (k, v) in extras {
        ordered.insert(k, v);
    }
    *old_obj = ordered;
}

/// Official 14-tool Antigravity IDE schema captured verbatim from `v1internal` `agent` traffic (`3,024/3,024` captures).
pub fn official_default_agent_tools() -> &'static Value {
    static DEFAULT_TOOLS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    DEFAULT_TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("official_fallback_tools.json"))
            .expect("official_fallback_tools.json must be valid JSON")
    })
}

/// Inject official Antigravity IDE default `tools` when an `agent` request arrives without any client tools
/// (`3,024/3,024` official `agent` captures carry `tools`), without injecting fabricated XML tags into `systemInstruction`.
pub fn inject_fallback_agent_tools_if_missing(
    inner: &mut Value,
    request_type: &str,
    model: &str,
) -> bool {
    if request_type != "agent" || model.eq_ignore_ascii_case("loadCodeAssist") {
        return false;
    }
    let Some(obj) = inner.as_object_mut() else {
        return false;
    };
    if obj.get("metadata").is_some() {
        return false;
    }
    if obj
        .get("generationConfig")
        .and_then(|g| g.get("imageConfig"))
        .is_some()
    {
        return false;
    }
    let tools_missing = obj
        .get("tools")
        .and_then(Value::as_array)
        .map_or(true, |arr| arr.is_empty());
    if !tools_missing {
        return false;
    }
    obj.insert("tools".to_string(), official_default_agent_tools().clone());
    true
}

/// Construct the outer `v1internal` envelope in official capture field order
/// (`project -> requestId -> request -> model -> userAgent -> requestType`)
/// with officially ordered inner `request` keys (`web_search` omits `requestId` etc.).
pub fn build_v1internal_envelope(
    project: &str,
    request_id: &str,
    mut request: Value,
    model: &str,
    user_agent: &str,
    request_type: &str,
) -> Value {
    inject_fallback_agent_tools_if_missing(&mut request, request_type, model);
    reorder_inner_request_official(&mut request);
    let mut outer = json!({
        "project": project,
        "requestId": request_id,
        "request": request,
        "model": model,
        "userAgent": user_agent,
        "requestType": request_type,
    });
    if request_type == "web_search" {
        apply_web_search_envelope_overrides(&mut outer);
    }
    outer
}

/// Request configuration after grounding resolution
#[derive(Debug, Clone)]
pub struct RequestConfig {
    /// The request type: "agent", "web_search", or "image_gen"
    pub request_type: String,
    /// Whether to inject the googleSearch tool
    pub inject_google_search: bool,
    /// The final model name (with suffixes stripped)
    pub final_model: String,
    /// Image generation configuration (if request_type is image_gen)
    pub image_config: Option<Value>,
}

pub fn resolve_request_config(
    original_model: &str,
    mapped_model: &str,
    tools: &Option<Vec<Value>>,
    size: Option<&str>,       // [NEW] Image size parameter
    quality: Option<&str>,    // [NEW] Image quality parameter
    image_size: Option<&str>, // [NEW] Direct imageSize parameter (e.g. "4K")
    body: Option<&Value>,     // [NEW] Request body for Gemini native imageConfig
) -> RequestConfig {
    // 1. Image Generation Check (Priority)
    // Detect via the original requested alias OR the account-resolved model name, because the
    // dynamic model rewrite may turn "gemini-3-pro-image" into e.g. "gemini-3.1-flash-image".
    if original_model.to_lowercase().contains("-image") || mapped_model.contains("-image") {
        // [RESOLVE #1694] Improved priority logic:
        // 1. First parse inferred config from model suffix and OpenAI parameters
        let (mut inferred_config, parsed_base_model) =
            parse_image_config_with_params(original_model, size, quality, image_size);

        // 2. Then merge with imageConfig from Gemini request body (if exists)
        if let Some(body_val) = body {
            if let Some(gen_config) = body_val.get("generationConfig") {
                if let Some(body_image_config) = gen_config.get("imageConfig") {
                    tracing::info!(
                        "[Common-Utils] Found imageConfig in body, merging with inferred config from suffix/params"
                    );

                    if let Some(inferred_obj) = inferred_config.as_object_mut() {
                        if let Some(body_obj) = body_image_config.as_object() {
                            // Merge body_obj into inferred_obj
                            for (key, value) in body_obj {
                                // CRITICAL: Only allow body to override if inferred doesn't already have a high-priority value
                                // Specifically, if we inferred imageSize from -4k, don't let body downgrade it if it's missing or standard.
                                let is_size_downgrade = key == "imageSize"
                                    && (value.as_str() == Some("1K") || value.is_null())
                                    && inferred_obj.contains_key("imageSize");

                                if !is_size_downgrade {
                                    inferred_obj.insert(key.clone(), value.clone());
                                } else {
                                    tracing::debug!("[Common-Utils] Shielding inferred imageSize from body downgrade");
                                }
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            "[Common-Utils] Final Image Config for {}: {:?}",
            parsed_base_model,
            inferred_config
        );

        // Prefer the account-resolved concrete image model (mapped_model) for the upstream
        // call; fall back to the parsed base of the requested alias if it wasn't resolved.
        let upstream_model = if mapped_model.contains("-image") {
            mapped_model.to_string()
        } else {
            parsed_base_model
        };
        return RequestConfig {
            request_type: "image_gen".to_string(),
            inject_google_search: false,
            final_model: upstream_model,
            image_config: Some(inferred_config),
        };
    }

    // 检测是否有联网工具定义 (内置功能调用)
    let has_networking_tool = detects_networking_tool(tools);
    // 检测是否包含非联网工具 (如 MCP 本地工具)
    let _has_non_networking = contains_non_networking_tool(tools);

    // Strip -online suffix from original model if present (to detect networking intent)
    let is_online_suffix = original_model.ends_with("-online");

    // Gemini search allowlist: Claude/OpenAI/unlisted models do not support native googleSearch
    // grounding directly and must fall back to WEB_SEARCH_MODEL (gemini-3.5-flash-lite).
    let _is_high_quality_model = mapped_model == "gemini-2.5-flash"
        || mapped_model == "gemini-1.5-pro"
        || mapped_model.starts_with("gemini-1.5-pro-")
        || mapped_model.starts_with("gemini-2.5-flash-")
        || mapped_model.starts_with("gemini-2.0-flash")
        || mapped_model.starts_with("gemini-3-")
        || mapped_model.starts_with("gemini-3.")
        || mapped_model.starts_with("gemini-3.5-")
        || mapped_model.starts_with("gemini-pro-")
        || mapped_model.starts_with("gemini-3-flash")
        || mapped_model.starts_with("gemini-3.5-flash")
        || mapped_model.starts_with("gemini-4")
        || mapped_model.starts_with("agent");

    // Determine if we should enable networking
    // [FIX] 禁用基于模型的自动联网逻辑，防止图像请求被联网搜索结果覆盖。
    // 仅在用户显式请求联网时启用：1) -online 后缀 2) 携带联网工具定义
    let enable_networking = is_online_suffix || has_networking_tool;

    // The final model to send upstream should be the MAPPED model,
    // but if searching, we MUST ensure the model name is one the backend associates with search.
    // Force a stable search model for search requests.
    let mut final_model = mapped_model.trim_end_matches("-online").to_string();

    // Map explicit preview aliases that have stable physical counterparts.
    // Note: gemini-3-pro-preview / gemini-3.1-pro-preview are intentionally NOT forced
    // to *-high here; dynamic runtime rewrite is handled after account selection.
    final_model = match final_model.as_str() {
        "gemini-3-pro-image-preview" => "gemini-3-pro-image".to_string(),
        "gemini-3-flash-preview" => "gemini-3-flash".to_string(),
        _ => final_model,
    };

    // [FIX] Check allowlist before forcing downgrade
    // If networking is enabled but the model doesn't support search, fall back to official web_search model
    if enable_networking && !_is_high_quality_model {
        tracing::info!(
            "[Common-Utils] Downgrading {} to {} for web search (model not in search allowlist)",
            final_model,
            WEB_SEARCH_MODEL
        );
        final_model = WEB_SEARCH_MODEL.to_string();
    }

    RequestConfig {
        request_type: if enable_networking {
            "web_search".to_string()
        } else {
            "agent".to_string()
        },
        inject_google_search: enable_networking,
        final_model,
        image_config: None,
    }
}

/// 判断是否为仅使用官方大写 `thinkingLevel`（禁止同时携带 `thinkingBudget`）的新代际模型
pub fn is_modern_thinking_level_model(model: &str) -> bool {
    let m = model.to_lowercase();
    if m.contains("-image") {
        return false;
    }
    m.contains("gemini-2.5-flash")
        || m.contains("gemini-3-flash")
        || m.contains("gemini-3.1-flash-lite")
        || m.contains("gemini-3.5")
        || m.contains("gemini-3.8")
        || m.contains("gemini-4")
        || m.contains("claude-sonnet-5")
        || m.contains("claude-opus-5")
        || m.contains("claude-5")
}

/// 官方抓包中默认不发送 `maxOutputTokens` 的轻量/分层模型：
/// 仅在客户端显式指定 `max_tokens` / `maxOutputTokens` 时透传，未指定时保持省略。
pub fn should_omit_default_max_output_tokens(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("gemini-3.8-flash-tiered") || m.contains("gemini-3.5-flash-lite")
}

/// 推导官方大写 `thinkingLevel`（`"MINIMAL" | "LOW" | "MEDIUM" | "HIGH"`）
/// 优先级：显式参数 (`effort` / `thinkingLevel` / `reasoning_effort`) > 模型名后缀 (`-low` / `-medium` / `-high` / `flash-lite` / `-tiered`) > 模型默认等级 (`gemini-2.5-flash*` LOW, `gemini-3-flash` HIGH) > `fallback`
pub fn resolve_modern_thinking_level(
    model: &str,
    explicit_level: Option<&str>,
    fallback: &'static str,
) -> &'static str {
    if let Some(raw) = explicit_level {
        match raw.trim().to_lowercase().as_str() {
            "minimal" => return "MINIMAL",
            "low" => return "LOW",
            "medium" => return "MEDIUM",
            "high" | "max" => return "HIGH",
            _ => {}
        }
    }
    let m = model.to_lowercase();
    if m.contains("-low")
        || m.contains("flash-lite")
        || m.contains("-tiered")
        || m.contains("gemini-2.5-flash")
    {
        "LOW"
    } else if m.contains("-medium") {
        "MEDIUM"
    } else if m.contains("-high") || m.contains("-max") || m.contains("gemini-3-flash") {
        "HIGH"
    } else {
        fallback
    }
}

/// Legacy wrapper for backward compatibility and simple usage
#[allow(dead_code)]
pub fn parse_image_config(model_name: &str) -> (Value, String) {
    parse_image_config_with_params(model_name, None, None, None)
}

/// Extended version that accepts OpenAI size and quality parameters
///
/// This function supports parsing image configuration from:
/// 1. Direct imageSize parameter - takes highest priority
/// 2. OpenAI API parameters (size, quality) - medium priority
/// 3. Model name suffixes (e.g., -16x9, -4k) - fallback
///
/// # Arguments
/// * `model_name` - The model name (may contain suffixes like -16x9-4k)
/// * `size` - Optional OpenAI size parameter (e.g., "1280x720", "1792x1024")
/// * `quality` - Optional OpenAI quality parameter ("standard", "hd", "medium")
/// * `image_size` - Optional direct Gemini imageSize parameter ("2K", "4K")
///
/// # Returns
/// (image_config, clean_model_name) where image_config contains aspectRatio and optionally imageSize
pub fn parse_image_config_with_params(
    model_name: &str,
    size: Option<&str>,
    quality: Option<&str>,
    image_size: Option<&str>,
) -> (Value, String) {
    let mut aspect_ratio = "1:1";

    // 1. 优先从 size 参数解析宽高比
    if let Some(s) = size {
        aspect_ratio = calculate_aspect_ratio_from_size(s);
    } else {
        // 2. 回退到模型后缀解析（保持向后兼容）
        if model_name.contains("-21x9") || model_name.contains("-21-9") {
            aspect_ratio = "21:9";
        } else if model_name.contains("-16x9") || model_name.contains("-16-9") {
            aspect_ratio = "16:9";
        } else if model_name.contains("-9x16") || model_name.contains("-9-16") {
            aspect_ratio = "9:16";
        } else if model_name.contains("-4x3") || model_name.contains("-4-3") {
            aspect_ratio = "4:3";
        } else if model_name.contains("-3x4") || model_name.contains("-3-4") {
            aspect_ratio = "3:4";
        } else if model_name.contains("-3x2") || model_name.contains("-3-2") {
            aspect_ratio = "3:2";
        } else if model_name.contains("-2x3") || model_name.contains("-2-3") {
            aspect_ratio = "2:3";
        } else if model_name.contains("-5x4") || model_name.contains("-5-4") {
            aspect_ratio = "5:4";
        } else if model_name.contains("-4x5") || model_name.contains("-4-5") {
            aspect_ratio = "4:5";
        } else if model_name.contains("-1x1") || model_name.contains("-1-1") {
            aspect_ratio = "1:1";
        }
    }

    let mut config = serde_json::Map::new();
    config.insert("aspectRatio".to_string(), json!(aspect_ratio));

    // [NEW] 0. 最高优先级：直接使用 image_size 参数
    if let Some(is) = image_size {
        config.insert("imageSize".to_string(), json!(is.to_uppercase()));
    } else {
        // 3. 优先从 quality 参数解析分辨率
        if let Some(q) = quality {
            match q.to_lowercase().as_str() {
                "hd" | "4k" => {
                    config.insert("imageSize".to_string(), json!("4K"));
                }
                "medium" | "2k" => {
                    config.insert("imageSize".to_string(), json!("2K"));
                }
                "standard" | "1k" => {
                    config.insert("imageSize".to_string(), json!("1K"));
                }
                _ => {} // 其他值不设置，使用默认
            }
        } else {
            // 4. 回退到模型后缀解析（保持向后兼容）
            let is_hd = model_name.contains("-4k") || model_name.contains("-hd");
            let is_2k = model_name.contains("-2k");

            if is_hd {
                config.insert("imageSize".to_string(), json!("4K"));
            } else if is_2k {
                config.insert("imageSize".to_string(), json!("2K"));
            }
        }
    }

    let clean_model_name = clean_image_model_name(model_name);

    (serde_json::Value::Object(config), clean_model_name)
}

/// Helper function to clean image model names by removing resolution/aspect-ratio suffixes.
/// E.g., "gemini-3.1-flash-image-16x9-4k" -> "gemini-3.1-flash-image"
fn clean_image_model_name(model_name: &str) -> String {
    let mut clean_name = model_name.to_lowercase();

    // Ordered list of known suffixes to strip
    let suffixes = [
        "-4k",
        "-2k",
        "-1k",
        "-hd",
        "-standard",
        "-medium",
        "-21x9",
        "-21-9",
        "-16x9",
        "-16-9",
        "-9x16",
        "-9-16",
        "-4x3",
        "-4-3",
        "-3x4",
        "-3-4",
        "-3x2",
        "-3-2",
        "-2x3",
        "-2-3",
        "-5x4",
        "-5-4",
        "-4x5",
        "-4-5",
        "-1x1",
        "-1-1",
    ];

    // Repeatedly strip suffixes until no more are found
    let mut changed = true;
    while changed {
        changed = false;
        for suffix in &suffixes {
            if clean_name.ends_with(suffix) {
                clean_name.truncate(clean_name.len() - suffix.len());
                changed = true;
            }
        }
    }

    clean_name
}

/// 动态计算宽高比（解决硬编码问题）
///
/// 从 "WIDTHxHEIGHT" 格式的字符串解析并计算宽高比，
/// 使用容差匹配常见的标准比例。
///
/// # Arguments
/// * `size` - 尺寸字符串，格式为 "WIDTHxHEIGHT" (e.g., "1280x720", "1792x1024")
///
/// # Returns
/// 标准宽高比字符串 ("1:1", "16:9", "9:16", "4:3", "3:4", "21:9")
fn calculate_aspect_ratio_from_size(size: &str) -> &'static str {
    // 0. Explicitly check known aspect ratios first
    match size {
        "21:9" => return "21:9",
        "16:9" => return "16:9",
        "9:16" => return "9:16",
        "4:3" => return "4:3",
        "3:4" => return "3:4",
        "3:2" => return "3:2",
        "2:3" => return "2:3",
        "5:4" => return "5:4",
        "4:5" => return "4:5",
        "1:1" => return "1:1",
        _ => {}
    }

    if let Some((w_str, h_str)) = size.split_once('x') {
        if let (Ok(width), Ok(height)) = (w_str.parse::<f64>(), h_str.parse::<f64>()) {
            if width > 0.0 && height > 0.0 {
                let ratio = width / height;

                // 容差匹配常见比例（容差 0.05，避免 3:4 和 2:3 重叠）
                if (ratio - 21.0 / 9.0).abs() < 0.05 {
                    return "21:9";
                }
                if (ratio - 16.0 / 9.0).abs() < 0.05 {
                    return "16:9";
                }
                if (ratio - 4.0 / 3.0).abs() < 0.05 {
                    return "4:3";
                }
                if (ratio - 3.0 / 4.0).abs() < 0.05 {
                    return "3:4";
                }
                if (ratio - 9.0 / 16.0).abs() < 0.05 {
                    return "9:16";
                }
                if (ratio - 3.0 / 2.0).abs() < 0.05 {
                    return "3:2";
                }
                if (ratio - 2.0 / 3.0).abs() < 0.05 {
                    return "2:3";
                }
                if (ratio - 5.0 / 4.0).abs() < 0.05 {
                    return "5:4";
                }
                if (ratio - 4.0 / 5.0).abs() < 0.05 {
                    return "4:5";
                }
                if (ratio - 1.0).abs() < 0.05 {
                    return "1:1";
                }
            }
        }
    }

    "1:1" // 默认回退
}

/// Inject current googleSearch tool and ensure no duplicate legacy search tools
pub fn inject_google_search_tool(body: &mut Value, _mapped_model: Option<&str>) {
    if let Some(obj) = body.as_object_mut() {
        let tools_entry = obj.entry("tools").or_insert_with(|| json!([]));
        if let Some(tools_arr) = tools_entry.as_array_mut() {
            let has_functions = tools_arr.iter().any(|t| {
                t.as_object()
                    .map_or(false, |o| o.contains_key("functionDeclarations"))
            });

            // [FIX] v1internal (cloudcode-pa) does NOT support mixing googleSearch
            // with functionDeclarations — it lacks includeServerSideToolInvocations.
            // Skip googleSearch injection entirely when function tools are present.
            if has_functions {
                tracing::debug!(
                    "Skipping googleSearch injection: functionDeclarations present (v1internal incompatible)"
                );
                return;
            }

            // 首先清理掉已存在的 googleSearch 或 googleSearchRetrieval，以防重复产生冲突
            tools_arr.retain(|t| {
                if let Some(o) = t.as_object() {
                    !(o.contains_key("googleSearch") || o.contains_key("googleSearchRetrieval"))
                } else {
                    true
                }
            });

            // 注入统一的 googleSearch (v1internal 规范)
            tools_arr.push(build_official_google_search_tool());
        }
    }
}

/// [DEFENSE] 3.8 网关严格铁律：functionDeclarations 与 googleSearch 绝对互斥
/// 当 tools 中存在自定义函数调用时，物理剔除所有 googleSearch / googleSearchRetrieval 声明；
/// 若清洗后 tools 数组为空，物理移出 tools 键，杜绝上游报 400 格式错误或触发伪 429。
pub fn enforce_tool_mutual_exclusion(request: &mut Value) {
    if let Some(obj) = request.as_object_mut() {
        if let Some(tools_val) = obj.get_mut("tools") {
            if let Some(tools_arr) = tools_val.as_array_mut() {
                let has_functions = tools_arr.iter().any(|t| {
                    t.as_object().map_or(false, |o| {
                        o.get("functionDeclarations")
                            .and_then(|v| v.as_array())
                            .map_or(false, |arr| !arr.is_empty())
                    })
                });

                if has_functions {
                    tools_arr.retain(|t| {
                        if let Some(o) = t.as_object() {
                            !(o.contains_key("googleSearch") || o.contains_key("googleSearchRetrieval"))
                        } else {
                            true
                        }
                    });
                }

                // 移除空对象
                tools_arr.retain(|t| {
                    if let Some(o) = t.as_object() {
                        !o.is_empty()
                    } else {
                        true
                    }
                });
            }

            // 如果 tools 数组已经变空，直接移出 tools 键
            let is_empty = tools_val.as_array().map_or(false, |arr| arr.is_empty());
            if is_empty {
                obj.remove("tools");
            }
        }
    }
}

/// 深度迭代清理客户端发送的 [undefined] 脏字符串，防止 Gemini 接口校验失败
pub fn deep_clean_undefined(value: &mut Value, depth: usize) {
    if depth > 10 {
        return;
    }
    match value {
        Value::Object(map) => {
            // 移除值为 "[undefined]" 的键
            map.retain(|_, v| {
                if let Some(s) = v.as_str() {
                    s != "[undefined]"
                } else {
                    true
                }
            });
            // 递归处理嵌套
            for v in map.values_mut() {
                deep_clean_undefined(v, depth + 1);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                deep_clean_undefined(v, depth + 1);
            }
        }
        _ => {}
    }
}

/// Detects if the tool list contains a request for networking/web search.
/// Supported keywords: "web_search", "google_search", "web_search_20250305"
pub fn detects_networking_tool(tools: &Option<Vec<Value>>) -> bool {
    if let Some(list) = tools {
        for tool in list {
            // 1. 直发风格 (Claude/Simple OpenAI/Anthropic Builtin/Vertex): { "name": "..." } 或 { "type": "..." }
            if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                if n == "web_search"
                    || n == "google_search"
                    || n == "web_search_20250305"
                    || n == "google_search_retrieval"
                    || n == "builtin_web_search"
                {
                    return true;
                }
            }

            if let Some(t) = tool.get("type").and_then(|v| v.as_str()) {
                if t == "web_search_20250305"
                    || t == "google_search"
                    || t == "web_search"
                    || t == "google_search_retrieval"
                    || t == "builtin_web_search"
                {
                    return true;
                }
            }

            // 2. OpenAI 嵌套风格: { "type": "function", "function": { "name": "..." } }
            if let Some(func) = tool.get("function") {
                if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                    let keywords = [
                        "web_search",
                        "google_search",
                        "web_search_20250305",
                        "google_search_retrieval",
                        "builtin_web_search",
                    ];
                    if keywords.contains(&n) {
                        return true;
                    }
                }
            }

            // 3. Gemini 原生风格: { "functionDeclarations": [ { "name": "..." } ] }
            if let Some(decls) = tool.get("functionDeclarations").and_then(|v| v.as_array()) {
                for decl in decls {
                    if let Some(n) = decl.get("name").and_then(|v| v.as_str()) {
                        if n == "web_search"
                            || n == "google_search"
                            || n == "google_search_retrieval"
                            || n == "builtin_web_search"
                        {
                            return true;
                        }
                    }
                }
            }

            // 4. Gemini googleSearch 声明 (含 googleSearchRetrieval 变体)
            if tool.get("googleSearch").is_some() || tool.get("googleSearchRetrieval").is_some() {
                return true;
            }
        }
    }
    false
}

/// 探测是否包含非联网相关的本地函数工具
pub fn contains_non_networking_tool(tools: &Option<Vec<Value>>) -> bool {
    if let Some(list) = tools {
        for tool in list {
            let mut is_networking = false;

            // 简单逻辑：如果它是一个函数声明且名字不是联网关键词，则视为非联网工具
            if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                let keywords = [
                    "web_search",
                    "google_search",
                    "web_search_20250305",
                    "google_search_retrieval",
                    "builtin_web_search",
                ];
                if keywords.contains(&n) {
                    is_networking = true;
                }
            } else if let Some(func) = tool.get("function") {
                if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                    let keywords = [
                        "web_search",
                        "google_search",
                        "web_search_20250305",
                        "google_search_retrieval",
                        "builtin_web_search",
                    ];
                    if keywords.contains(&n) {
                        is_networking = true;
                    }
                }
            } else if tool.get("googleSearch").is_some()
                || tool.get("googleSearchRetrieval").is_some()
            {
                is_networking = true;
            } else if tool.get("functionDeclarations").is_some() {
                // 如果是 Gemini 风格的 functionDeclarations，进去看一眼
                if let Some(decls) = tool.get("functionDeclarations").and_then(|v| v.as_array()) {
                    for decl in decls {
                        if let Some(n) = decl.get("name").and_then(|v| v.as_str()) {
                            let keywords = [
                                "web_search",
                                "google_search",
                                "google_search_retrieval",
                                "builtin_web_search",
                            ];
                            if !keywords.contains(&n) {
                                return true; // 发现本地函数
                            }
                        }
                    }
                }
                is_networking = true; // 即使全是联网，外层也标记为联网
            }

            if !is_networking {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_high_quality_model_auto_grounding() {
        // Auto-grounding is currently disabled by default due to conflict with image gen
        let config =
            resolve_request_config("gpt-4o", "gemini-2.5-flash", &None, None, None, None, None);
        assert_eq!(config.request_type, "agent");
        assert!(!config.inject_google_search);
    }

    #[test]
    fn test_gemini_native_tool_detection() {
        let tools = Some(vec![json!({
            "functionDeclarations": [
                { "name": "web_search", "parameters": {} }
            ]
        })]);
        assert!(detects_networking_tool(&tools));
    }

    #[test]
    fn test_online_suffix_force_grounding() {
        let config = resolve_request_config(
            "gemini-3-flash-online",
            "gemini-3-flash",
            &None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(config.request_type, "web_search");
        assert!(config.inject_google_search);
        assert_eq!(config.final_model, "gemini-3-flash");
    }

    #[test]
    fn test_default_no_grounding() {
        let config = resolve_request_config(
            "claude-sonnet",
            "gemini-3-flash",
            &None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(config.request_type, "agent");
        assert!(!config.inject_google_search);
    }

    #[test]
    fn test_image_model_excluded() {
        let config = resolve_request_config(
            "gemini-3-pro-image",
            "gemini-3-pro-image",
            &None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(config.request_type, "image_gen");
        assert!(!config.inject_google_search);
    }

    #[test]
    fn test_alignment_web_search_model_tool_and_wrapper_and_schema() {
        // 1. Web search fallback model aligns with official gemini-3.5-flash-lite
        let ws_config = resolve_request_config(
            "claude-sonnet-4-6-online",
            "claude-sonnet-4-6",
            &None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(ws_config.request_type, "web_search");
        assert!(ws_config.inject_google_search);
        assert_eq!(ws_config.final_model, WEB_SEARCH_MODEL);

        // 2. Official googleSearch payload
        let gs = build_official_google_search_tool();
        assert_eq!(
            gs["googleSearch"]["enhancedContent"]["imageSearch"]["maxResultCount"],
            5
        );

        // 3. Official functionResponse keys ("output" / "error") & guaranteed string values
        let ok_fr = build_function_response_part("ls", Some("id_1"), json!("ok"), false);
        assert_eq!(ok_fr["functionResponse"]["response"]["output"], "ok");
        assert!(ok_fr["functionResponse"]["response"].get("result").is_none());
        let obj_fr = build_function_response_part(
            "ls",
            Some("id_obj"),
            json!({"files": ["a.rs"], "count": 1}),
            false,
        );
        assert!(obj_fr["functionResponse"]["response"]["output"].is_string());
        assert_eq!(
            obj_fr["functionResponse"]["response"]["output"],
            r#"{"files":["a.rs"],"count":1}"#
        );
        let null_fr = build_function_response_part("ls", Some("id_null"), Value::Null, false);
        assert_eq!(null_fr["functionResponse"]["response"]["output"], "");
        let err_fr = build_function_response_part("ls", Some("id_2"), json!(404), true);
        assert_eq!(err_fr["functionResponse"]["response"]["error"], "404");

        let mut legacy_resp = json!({ "result": { "ok": true } });
        normalize_function_response_object(&mut legacy_resp);
        assert_eq!(legacy_resp, json!({ "output": r#"{"ok":true}"# }));

        assert!(is_modern_thinking_level_model("gemini-3.1-flash-lite"));
        assert_eq!(
            resolve_modern_thinking_level("gemini-3.1-flash-lite", None, "MEDIUM"),
            "LOW"
        );
        assert!(is_modern_thinking_level_model("gemini-2.5-flash"));
        assert!(is_modern_thinking_level_model("gemini-2.5-flash-lite"));
        assert!(is_modern_thinking_level_model("gemini-2.5-flash-thinking"));
        assert!(is_modern_thinking_level_model("gemini-3-flash"));
        assert!(!is_modern_thinking_level_model("gemini-2.5-pro"));
        assert!(!is_modern_thinking_level_model("gemini-3.1-flash-image"));
        assert_eq!(
            resolve_modern_thinking_level("gemini-2.5-flash", None, "HIGH"),
            "LOW"
        );
        assert_eq!(
            resolve_modern_thinking_level("gemini-2.5-flash-thinking", None, "HIGH"),
            "LOW"
        );
        assert_eq!(
            resolve_modern_thinking_level("gemini-3-flash", None, "MEDIUM"),
            "HIGH"
        );
        assert_eq!(
            resolve_modern_thinking_level("gemini-3-flash", Some("low"), "MEDIUM"),
            "LOW"
        );

        let mut contents = vec![
            json!({
                "role": "model",
                "parts": [{ "functionCall": { "name": "ping", "args": {}, "id": "" } }]
            }),
            json!({
                "role": "user",
                "parts": [{ "functionResponse": { "name": "ping", "response": { "output": "ok" } } }]
            }),
        ];
        ensure_paired_tool_call_ids(&mut contents, "[Test]");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["id"], "call_ping_0");
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["id"], "call_ping_0");

        // 4. Official tool schema cleaning: uppercase types + minLength/minItems/minimum preserved
        let mut schema = json!({
            "properties": {
                "query": { "type": "string", "minLength": 2, "maxLength": 100 },
                "tags": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                "count": { "type": "integer", "minimum": 0 },
                "type": { "type": "string" }
            },
            "required": ["query"]
        });
        crate::proxy::common::json_schema::clean_official_tool_schema(&mut schema);
        assert_eq!(schema["type"], "OBJECT");
        assert_eq!(schema["properties"]["query"]["type"], "STRING");
        assert_eq!(schema["properties"]["query"]["minLength"], 2);
        assert!(schema["properties"]["query"].get("maxLength").is_none());
        assert_eq!(schema["properties"]["tags"]["type"], "ARRAY");
        assert_eq!(schema["properties"]["tags"]["minItems"], 1);
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "STRING");
        assert_eq!(schema["properties"]["count"]["type"], "INTEGER");
        assert_eq!(schema["properties"]["count"]["minimum"], 0);
        // Property named literally "type" must still have its own schema type uppercased
        assert_eq!(schema["properties"]["type"]["type"], "STRING");

        // 5. Gemini wrapper: Base64 sentinel for missing flash sig + official web_search envelope
        let gemini_tool_body = json!({
            "model": "gemini-3-flash",
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        { "functionCall": { "name": "ping", "args": {} } }
                    ]
                }
            ],
            "tools": [
                {
                    "functionDeclarations": [
                        {
                            "name": "ping",
                            "parameters": {
                                "type": "object",
                                "properties": { "host": { "type": "string", "minLength": 1 } }
                            }
                        }
                    ]
                }
            ]
        });
        let wrapped_tool = crate::proxy::mappers::gemini::wrapper::wrap_request_v2(
            &gemini_tool_body,
            "proj",
            "gemini-3-flash",
            Some("acc_1"),
            Some("sess_wrap_missing_sig"),
            None,
            None,
        );
        assert_eq!(
            wrapped_tool["request"]["contents"][0]["parts"][0]["thoughtSignature"],
            SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert_eq!(
            wrapped_tool["request"]["tools"][0]["functionDeclarations"][0]["parameters"]["type"],
            "OBJECT"
        );
        assert_eq!(
            wrapped_tool["request"]["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]
                ["host"]["minLength"],
            1
        );

        let gemini_ws_body = json!({
            "model": "gemini-2.5-flash-online",
            "generationConfig": { "temperature": 0.9, "maxOutputTokens": 4096 },
            "contents": [{ "role": "user", "parts": [{ "text": "search news" }] }]
        });
        let wrapped_ws = crate::proxy::mappers::gemini::wrapper::wrap_request_v2(
            &gemini_ws_body,
            "proj",
            "gemini-3-flash",
            Some("acc_1"),
            Some("sess_ws"),
            None,
            None,
        );
        assert_eq!(wrapped_ws["requestType"], "web_search");
        assert!(wrapped_ws.get("requestId").is_none());
        assert!(wrapped_ws["request"].get("sessionId").is_none());
        assert!(wrapped_ws["request"].get("labels").is_none());
        assert_eq!(
            wrapped_ws["request"]["generationConfig"],
            json!({ "candidateCount": 1 })
        );
        assert_eq!(
            wrapped_ws["request"]["tools"][0]["googleSearch"]["enhancedContent"]["imageSearch"]
                ["maxResultCount"],
            5
        );
    }

    #[test]
    fn test_image_2k_and_ultrawide_config() {
        // Test 2K
        let (config_2k, _) = parse_image_config("gemini-3-pro-image-2k");
        assert_eq!(config_2k["imageSize"], "2K");

        // Test 21:9
        let (config_21x9, _) = parse_image_config("gemini-3-pro-image-21x9");
        assert_eq!(config_21x9["aspectRatio"], "21:9");

        // Test Combined (if logic allows, though suffix parsing is greedy)
        let (config_combined, _) = parse_image_config("gemini-3-pro-image-2k-21x9");
        assert_eq!(config_combined["imageSize"], "2K");
        assert_eq!(config_combined["aspectRatio"], "21:9");

        // Test 4K + 21:9
        let (config_4k_wide, _) = parse_image_config("gemini-3-pro-image-4k-21x9");
        assert_eq!(config_4k_wide["imageSize"], "4K");
        assert_eq!(config_4k_wide["aspectRatio"], "21:9");
    }

    #[test]
    fn test_parse_image_config_with_openai_params() {
        // Test quality parameter mapping
        let (config_hd, model_hd) =
            parse_image_config_with_params("gemini-3-pro-image", None, Some("hd"), None);
        assert_eq!(config_hd["imageSize"], "4K");
        assert_eq!(config_hd["aspectRatio"], "1:1");
        assert_eq!(model_hd, "gemini-3-pro-image");

        let (config_medium, model_medium) =
            parse_image_config_with_params("gemini-3-pro-image", None, Some("medium"), None);
        assert_eq!(config_medium["imageSize"], "2K");
        assert_eq!(model_medium, "gemini-3-pro-image");

        let (config_standard, model_standard) =
            parse_image_config_with_params("gemini-3-pro-image", None, Some("standard"), None);
        assert_eq!(config_standard["imageSize"], "1K");
        assert_eq!(model_standard, "gemini-3-pro-image");

        // Test size parameter mapping with dynamic calculation
        let (config_16_9, model_16_9) =
            parse_image_config_with_params("gemini-3-pro-image", Some("1280x720"), None, None);
        assert_eq!(config_16_9["aspectRatio"], "16:9");
        assert_eq!(model_16_9, "gemini-3-pro-image");

        let (config_9_16, model_9_16) =
            parse_image_config_with_params("gemini-3-pro-image", Some("720x1280"), None, None);
        assert_eq!(config_9_16["aspectRatio"], "9:16");
        assert_eq!(model_9_16, "gemini-3-pro-image");

        let (config_4_3, model_4_3) =
            parse_image_config_with_params("gemini-3-pro-image", Some("800x600"), None, None);
        assert_eq!(config_4_3["aspectRatio"], "4:3");
        assert_eq!(model_4_3, "gemini-3-pro-image");

        // Test combined size + quality
        let (config_combined, model_combined) = parse_image_config_with_params(
            "gemini-3-pro-image",
            Some("1920x1080"),
            Some("hd"),
            None,
        );
        assert_eq!(config_combined["aspectRatio"], "16:9");
        assert_eq!(config_combined["imageSize"], "4K");
        assert_eq!(model_combined, "gemini-3-pro-image");

        // Test backward compatibility: model suffix takes precedence when no params
        let (config_compat, model_compat) =
            parse_image_config_with_params("gemini-3-pro-image-16x9-4k", None, None, None);
        assert_eq!(config_compat["aspectRatio"], "16:9");
        assert_eq!(config_compat["imageSize"], "4K");
        assert_eq!(model_compat, "gemini-3-pro-image");

        // Test parameter priority: params override model suffix
        let (config_override, model_override) = parse_image_config_with_params(
            "gemini-3-pro-image-1x1-2k",
            Some("1280x720"),
            Some("hd"),
            None,
        );
        assert_eq!(config_override["aspectRatio"], "16:9"); // from size param, not model suffix
        assert_eq!(config_override["imageSize"], "4K"); // from quality param, not model suffix
        assert_eq!(model_override, "gemini-3-pro-image");
    }

    #[test]
    fn test_clean_image_model_name() {
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-4k"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3-pro-image-16x9"),
            "gemini-3-pro-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3-pro-image-16x9-4k"),
            "gemini-3-pro-image"
        );
        // Test varying order
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-4k-16x9"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-16-9-hd"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-2k-9x16"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-1x1"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-standard"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-medium"),
            "gemini-3.1-flash-image"
        );
        assert_eq!(
            clean_image_model_name("gemini-3.1-flash-image-21-9-4k"),
            "gemini-3.1-flash-image"
        );
    }

    #[test]
    fn test_calculate_aspect_ratio_from_size() {
        // Test standard OpenAI sizes
        assert_eq!(calculate_aspect_ratio_from_size("1280x720"), "16:9");
        assert_eq!(calculate_aspect_ratio_from_size("1920x1080"), "16:9");
        assert_eq!(calculate_aspect_ratio_from_size("720x1280"), "9:16");
        assert_eq!(calculate_aspect_ratio_from_size("1080x1920"), "9:16");
        assert_eq!(calculate_aspect_ratio_from_size("1024x1024"), "1:1");
        assert_eq!(calculate_aspect_ratio_from_size("800x600"), "4:3");
        assert_eq!(calculate_aspect_ratio_from_size("600x800"), "3:4");
        assert_eq!(calculate_aspect_ratio_from_size("2560x1080"), "21:9");

        // [NEW] Test new aspect ratios
        assert_eq!(calculate_aspect_ratio_from_size("1500x1000"), "3:2");
        assert_eq!(calculate_aspect_ratio_from_size("1000x1500"), "2:3");
        assert_eq!(calculate_aspect_ratio_from_size("1250x1000"), "5:4");
        assert_eq!(calculate_aspect_ratio_from_size("1000x1250"), "4:5");

        // [NEW] Test direct aspect ratio strings
        assert_eq!(calculate_aspect_ratio_from_size("21:9"), "21:9");
        assert_eq!(calculate_aspect_ratio_from_size("16:9"), "16:9");
        assert_eq!(calculate_aspect_ratio_from_size("1:1"), "1:1");

        // Test edge cases
        assert_eq!(calculate_aspect_ratio_from_size("invalid"), "1:1");
        assert_eq!(calculate_aspect_ratio_from_size("1920x0"), "1:1");
        assert_eq!(calculate_aspect_ratio_from_size("0x1080"), "1:1");
        assert_eq!(calculate_aspect_ratio_from_size("abc x def"), "1:1");
    }

    #[test]
    fn test_image_config_merging_priority() {
        // Case 1: Body contains empty/default imageSize, suffix contains -4k
        // Expected: Should KEEP 4K from suffix
        let body = json!({
            "generationConfig": {
                "imageConfig": {
                    "aspectRatio": "1:1",
                    "imageSize": "1K" // Simulated downgrade from client
                }
            }
        });
        let config = resolve_request_config(
            "gemini-3-pro-image-4k",
            "gemini-3-pro-image",
            &None,
            None,
            None,
            None,
            Some(&body),
        );
        let image_config = config.image_config.unwrap();
        assert_eq!(
            image_config["imageSize"], "4K",
            "Should shield inferred 4K from body downgrade"
        );
        assert_eq!(
            image_config["aspectRatio"], "1:1",
            "Should take aspectRatio from body"
        );

        // Case 2: Suffix contains -16-9, Body contains aspectRatio: 1:1
        // Expected: Body overrides suffix for aspectRatio (since it's not a 'downgrade' shield case yet, only size is shielded)
        let body_2 = json!({
            "generationConfig": {
                "imageConfig": {
                    "aspectRatio": "1:1"
                }
            }
        });
        let config_2 = resolve_request_config(
            "gemini-3-pro-image-16x9",
            "gemini-3-pro-image",
            &None,
            None,
            None,
            None,
            Some(&body_2),
        );
        let image_config_2 = config_2.image_config.unwrap();
        assert_eq!(
            image_config_2["aspectRatio"], "1:1",
            "Body should be allowed to override aspectRatio"
        );
    }

    #[test]
    fn test_image_size_priority() {
        // Case 1: imageSize param overrides quality
        // Expected: "4K" from imageSize param
        let (config_1, _) = parse_image_config_with_params(
            "gemini-3-pro-image",
            None,
            Some("standard"), // would be 1K
            Some("4K"),       // should override
        );
        assert_eq!(config_1["imageSize"], "4K");

        // Case 2: imageSize param overrides suffix
        // Expected: "2K" from imageSize param
        let (config_2, _) = parse_image_config_with_params(
            "gemini-3-pro-image-4k", // would be 4K
            None,
            None,
            Some("2K"), // should override
        );
        assert_eq!(config_2["imageSize"], "2K");

        // Case 3: imageSize param + size param + quality param
        // Expected: "4K" from imageSize, "16:9" from size
        let (config_3, _) = parse_image_config_with_params(
            "gemini-3-pro-image",
            Some("1920x1080"), // 16:9
            Some("standard"),  // 1K (ignored)
            Some("4K"),        // 4K (priority)
        );
        assert_eq!(config_3["imageSize"], "4K");
        assert_eq!(config_3["aspectRatio"], "16:9");
    }
}

pub fn sanitize_system_prompt_for_tokens(text: &str) -> String {
    use regex::Regex;
    let mut cleaned = text.to_string();

    // [CACHE] Step 1: 剥离动态内容（时间戳、UUID），确保跨请求的前缀一致性
    // 这对 Gemini 隐式前缀缓存命中至关重要
    let time_patterns = [
        r"(?im)^Current (date|time)(\s+is)?\s*:.*$",
        r"(?im)^Today is\s*:.*$",
        r"(?im)^Date:\s+\d{4}-\d{2}-\d{2}.*$",
    ];
    for pat in &time_patterns {
        if let Ok(re) = Regex::new(pat) {
            cleaned = re.replace_all(&cleaned, "").into_owned();
        }
    }

    // 剥离 UUID
    if let Ok(re) = Regex::new(r"\b[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}\b")
    {
        cleaned = re.replace_all(&cleaned, "{uuid}").into_owned();
    }

    // 剥离随机 request/session/trace ID
    if let Ok(re) = Regex::new(r"\b(req|sid|trace)_[a-f0-9]{6,32}\b") {
        cleaned = re.replace_all(&cleaned, "{id}").into_owned();
    }

    // Step 2: Compress massive XML tags injected by thick clients to save tokens

    let tags_to_compress = [
        "skills_instructions",
        "skills",
        "plugins",
        "subagents",
        "customizations",
        "conversation_transcript",
        "guidelines",
    ];

    for tag in tags_to_compress.iter() {
        let pattern = format!(r"(?s)<{}>.*?</{}>", tag, tag);
        if let Ok(re) = Regex::new(&pattern) {
            let replacement = format!("<{}>\n[Omitted by Antigravity Proxy to save tokens. Tool definitions remain available.]\n</{}>", tag, tag);
            cleaned = re.replace_all(&cleaned, replacement).into_owned();
        }
    }

    cleaned
}

/// [FIX] Parse markdown base64 images from text and split into Gemini parts
/// This prevents base64 reflection bloat where generated images are sent back as huge text strings
pub fn parse_markdown_images_to_parts(text: &str) -> Vec<Value> {
    let mut parts = Vec::new();
    // Match ![...](data:image/...;base64,...)
    if let Ok(re) = regex::Regex::new(r"!\[.*?\]\(data:(image/[^;]+);base64,([a-zA-Z0-9+/=]+)\)") {
        let mut last_match = 0;

        for cap in re.captures_iter(text) {
            let m = cap.get(0).unwrap();

            // Add preceding text
            if m.start() > last_match {
                let preceding = &text[last_match..m.start()];
                if !preceding.trim().is_empty() {
                    parts.push(json!({"text": preceding}));
                }
            }

            // Add inlineData image
            let mime = cap.get(1).unwrap().as_str();
            let b64 = cap.get(2).unwrap().as_str();
            parts.push(json!({
                "inlineData": { "mimeType": mime, "data": b64 }
            }));

            last_match = m.end();
        }

        // Add remaining text
        if last_match < text.len() {
            let remaining = &text[last_match..];
            if !remaining.trim().is_empty() {
                parts.push(json!({"text": remaining}));
            }
        }

        if parts.is_empty() && !text.trim().is_empty() {
            parts.push(json!({"text": text}));
        }

        return parts;
    }

    if !text.trim().is_empty() {
        parts.push(json!({"text": text}));
    }

    parts
}

/// Re-encode decoded Claude thought signatures (`CAQS...`) back to the outer
/// Base64 wire format (`Q0FR...`) expected by upstream `v1internal`.
pub fn ensure_upstream_thought_signature(sig: &str) -> String {
    if sig.starts_with("CAQS") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(sig.as_bytes())
    } else {
        sig.to_string()
    }
}

/// Extract the first non-thought text part from a `v1internal:generateContent` (or raw Gemini) JSON response.
/// Skips `{"thought": true, ...}` parts and non-text parts (e.g. `functionCall`), falling back to
/// joining non-thought text parts if multiple text chunks exist.
pub fn extract_v1internal_primary_text(gemini_response: &Value) -> Option<String> {
    let payload = gemini_response.get("response").unwrap_or(gemini_response);
    let parts = payload
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())?;

    let mut collected = String::new();
    for part in parts {
        if part.get("thought").and_then(|t| t.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            collected.push_str(text);
        }
    }
    if collected.is_empty() {
        None
    } else {
        Some(collected)
    }
}

#[cfg(test)]
mod extract_text_tests {
    use super::*;

    #[test]
    fn test_extract_v1internal_primary_text_skips_thought_and_function_call() {
        let resp = json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            { "thought": true, "text": "internal reasoning..." },
                            { "functionCall": { "name": "read_file", "args": {} } },
                            { "text": "<state_snapshot>ok</state_snapshot>" }
                        ]
                    }
                }]
            }
        });
        assert_eq!(
            extract_v1internal_primary_text(&resp).as_deref(),
            Some("<state_snapshot>ok</state_snapshot>")
        );
    }
}
