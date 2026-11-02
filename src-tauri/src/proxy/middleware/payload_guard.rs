// 请求包体与上下文防护中间件 (Payload Guard)
// 用于在网关层前置拦截超大上下文请求（如 >150k tokens），防止单账号突增高额算力被 Google/上游风控封号。

use crate::proxy::mappers::context_manager::ContextManager;
use crate::proxy::server::AppState;
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceProtocol {
    Gemini,
    Claude,
    OpenAI,
}

/// 识别推理请求协议类型；若不是推理接口（如健康检查、countTokens、模型列表）则返回 None
pub fn detect_inference_protocol(method: &str, path: &str) -> Option<InferenceProtocol> {
    if method != "POST" {
        return None;
    }

    // 排除非推理端点
    if path.contains("countTokens")
        || path.contains("count_tokens")
        || path.starts_with("/internal/")
        || path.contains("event_logging")
        || path.starts_with("/health")
        || path.starts_with("/mcp/")
        || path == "/v1/models/detect"
    {
        return None;
    }

    if path.starts_with("/v1beta/models/") {
        Some(InferenceProtocol::Gemini)
    } else if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        Some(InferenceProtocol::Claude)
    } else if path == "/v1/chat/completions" || path == "/v1/completions" {
        Some(InferenceProtocol::OpenAI)
    } else {
        None
    }
}

/// 请求包体与上下文安全卫士中间件
pub async fn payload_guard_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let guard_cfg = state.payload_guard.read().await.clone();

    // 1. 若功能未启用或阈值设为 0（表示不限），直接放行
    if !guard_cfg.enabled || guard_cfg.max_input_tokens == 0 {
        return next.run(request).await;
    }

    let method = request.method().as_str().to_string();
    let uri_path = request.uri().path().to_string();

    // 2. 仅拦截实际的 AI 推理端点
    let proto = match detect_inference_protocol(&method, &uri_path) {
        Some(p) => p,
        None => return next.run(request).await,
    };

    let max_tokens = guard_cfg.max_input_tokens;
    // 物理最低换算比 (2.5 字节/Token)：低于此大小在物理上绝不可能超过 max_tokens
    // 比如 150_000 * 2.5 = 375_000 字节 (~366 KB)
    let min_token_calc_threshold = (max_tokens as f32 * 2.5) as usize;

    // 3. 第一级轻量检查：如果存在 Content-Length 请求头且明显很小，零开销秒级放行
    let content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    if let Some(cl) = content_length {
        if cl < min_token_calc_threshold {
            return next.run(request).await;
        }
    }

    // 4. 第二级精确检查：读取 Body 并根据多模态 / 文本解析真实 Token 数量
    let (parts, body) = request.into_parts();
    // 限制单次读取上限 50MB
    let bytes = match to_bytes(body, 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("[PayloadGuard] Failed to read request body: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "code": 400,
                        "status": "INVALID_ARGUMENT",
                        "message": format!("Failed to read request body: {}", e)
                    }
                })),
            )
                .into_response();
        }
    };

    // 如果实际读取的字节数依然小于下限阈值，直接还原并放行
    if bytes.len() < min_token_calc_threshold {
        let req = Request::from_parts(parts, Body::from(bytes));
        return next.run(req).await;
    }

    // 执行精确 Token 估算（含多模态 inlineData / 文本）
    let estimated_tokens = estimate_tokens_from_bytes(proto, &bytes);

    if let Some(tokens) = estimated_tokens {
        if tokens > max_tokens {
            let client_ip = extract_client_ip_from_parts(&parts);
            tracing::warn!(
                "🚫 [PayloadGuard] 拦截超大上下文请求: 预估 ~{}k tokens (安全阈值: {}k tokens), 包体: {} 字节, 客户端IP: {}, 路径: {}",
                tokens / 1000,
                max_tokens / 1000,
                bytes.len(),
                client_ip.as_deref().unwrap_or("unknown"),
                uri_path
            );

            return make_payload_too_large_response(proto, tokens, max_tokens);
        }
    }

    // 检验通过，安全还原 Request 继续后续处理
    let req = Request::from_parts(parts, Body::from(bytes));
    next.run(req).await
}

/// 解析各种协议请求体中的 Token 数量（已包含多模态折算）
fn estimate_tokens_from_bytes(proto: InferenceProtocol, bytes: &[u8]) -> Option<u32> {
    match proto {
        InferenceProtocol::Gemini => {
            let val: Value = serde_json::from_slice(bytes).ok()?;
            Some(ContextManager::estimate_gemini_token_usage(&val))
        }
        InferenceProtocol::Claude => {
            if let Ok(req) =
                serde_json::from_slice::<crate::proxy::mappers::claude::ClaudeRequest>(bytes)
            {
                Some(ContextManager::estimate_token_usage(&req))
            } else if let Ok(val) = serde_json::from_slice::<Value>(bytes) {
                Some(ContextManager::estimate_gemini_token_usage(&val))
            } else {
                None
            }
        }
        InferenceProtocol::OpenAI => {
            if let Ok(req) =
                serde_json::from_slice::<crate::proxy::mappers::openai::OpenAIRequest>(bytes)
            {
                Some(ContextManager::estimate_openai_token_usage(&req))
            } else if let Ok(val) = serde_json::from_slice::<Value>(bytes) {
                Some(ContextManager::estimate_gemini_token_usage(&val))
            } else {
                None
            }
        }
    }
}

/// 构建针对各客户端协议友好的 413 Payload Too Large 响应
fn make_payload_too_large_response(
    proto: InferenceProtocol,
    estimated_tokens: u32,
    max_tokens: u32,
) -> Response {
    let msg = format!(
        "Request context (~{}k tokens) exceeds gateway limit ({}k tokens) to protect upstream accounts from suspension. Please reduce prompt or context size.",
        estimated_tokens / 1000,
        max_tokens / 1000
    );

    let (status, body_val) = match proto {
        InferenceProtocol::Gemini => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "error": {
                    "code": 413,
                    "message": msg,
                    "status": "INVALID_ARGUMENT",
                    "details": [
                        {
                            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                            "reason": "PAYLOAD_TOO_LARGE",
                            "domain": "antigravity-gateway",
                            "metadata": {
                                "estimated_tokens": estimated_tokens.to_string(),
                                "max_tokens": max_tokens.to_string(),
                            }
                        }
                    ]
                }
            }),
        ),
        InferenceProtocol::Claude => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": msg,
                }
            }),
        ),
        InferenceProtocol::OpenAI => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "error": {
                    "message": msg,
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "context_length_exceeded"
                }
            }),
        ),
    };

    let mut resp = Json(body_val).into_response();
    *resp.status_mut() = status;
    resp
}

/// 从请求头部中提取客户端 IP
fn extract_client_ip_from_parts(parts: &axum::http::request::Parts) -> Option<String> {
    parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .or_else(|| {
            parts
                .headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_inference_protocol() {
        assert_eq!(
            detect_inference_protocol("POST", "/v1beta/models/gemini-3.8-flash-medium:streamGenerateContent"),
            Some(InferenceProtocol::Gemini)
        );
        assert_eq!(
            detect_inference_protocol("POST", "/v1/messages"),
            Some(InferenceProtocol::Claude)
        );
        assert_eq!(
            detect_inference_protocol("POST", "/v1/chat/completions"),
            Some(InferenceProtocol::OpenAI)
        );

        // GET 请求均不拦截
        assert_eq!(detect_inference_protocol("GET", "/v1beta/models"), None);
        assert_eq!(detect_inference_protocol("GET", "/v1/models"), None);

        // countTokens 端点不拦截
        assert_eq!(
            detect_inference_protocol("POST", "/v1beta/models/gemini-1.5-pro:countTokens"),
            None
        );
        assert_eq!(
            detect_inference_protocol("POST", "/v1/messages/count_tokens"),
            None
        );

        // 管理与内部端点不拦截
        assert_eq!(detect_inference_protocol("POST", "/internal/warmup"), None);
        assert_eq!(detect_inference_protocol("POST", "/api/config"), None);
    }

    #[test]
    fn test_estimate_tokens_gemini() {
        // 构造一个约 200k 字符的请求体
        let long_text = "Hello world! ".repeat(20000); // 240,000 字符 (~60,000 tokens)
        let body = json!({
            "contents": [
                {
                    "role": "user",
                    "parts": [
                        { "text": long_text }
                    ]
                }
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let tokens = estimate_tokens_from_bytes(InferenceProtocol::Gemini, &bytes).unwrap();
        assert!(tokens > 40_000 && tokens < 80_000);
    }

    #[test]
    fn test_multimodal_image_tokens_not_overcounted() {
        // 构造一个包含大 Base64 图片 (300KB) 但文本很少的请求体
        let fake_base64 = "A".repeat(300_000);
        let body = json!({
            "contents": [
                {
                    "role": "user",
                    "parts": [
                        { "text": "Describe this picture" },
                        {
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": fake_base64
                            }
                        }
                    ]
                }
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let tokens = estimate_tokens_from_bytes(InferenceProtocol::Gemini, &bytes).unwrap();
        // 即使请求体有 300KB，图片只被折算为约 258 tokens，总 tokens 绝不超过 1000
        assert!(tokens < 2_000, "Image tokens was: {}", tokens);
    }

    #[test]
    fn test_make_payload_too_large_response() {
        let resp = make_payload_too_large_response(InferenceProtocol::Gemini, 180_000, 150_000);
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let resp_claude = make_payload_too_large_response(InferenceProtocol::Claude, 180_000, 150_000);
        assert_eq!(resp_claude.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let resp_openai = make_payload_too_large_response(InferenceProtocol::OpenAI, 180_000, 150_000);
        assert_eq!(resp_openai.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
