// 预热处理器 - 内部预热 API
//
// 提供 /internal/warmup 端点，支持：
// - 指定账号（通过 email）
// - 指定模型（不做映射，直接使用原始模型名称）
// - 复用代理的所有基础设施（UpstreamClient、TokenManager）

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::proxy::mappers::gemini::wrapper::wrap_request;
use crate::proxy::monitor::ProxyRequestLog;
use crate::proxy::server::AppState;

/// 预热请求体
#[derive(Debug, Deserialize)]
pub struct WarmupRequest {
    /// 账号邮箱
    pub email: String,
    /// 模型名称（原始名称，不做映射）
    pub model: String,
    /// 可选：账号 ID（确保内部预热保留专属代理绑定、sessionId 与设备 UA）
    #[serde(default)]
    pub account_id: Option<String>,
    /// 可选：直接提供 Access Token（用于不在 TokenManager 中的账号）
    pub access_token: Option<String>,
    /// 可选：直接提供 Project ID
    pub project_id: Option<String>,
}

fn resolve_warmup_account_id(provided: Option<&str>, email: &str) -> String {
    provided
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| crate::modules::account::find_account_id_by_email(email))
        .unwrap_or_default()
}

fn build_warmup_payload(
    model: &str,
    project_id: &str,
    acc_opt: Option<&str>,
) -> Result<Value, String> {
    let lower = model.to_lowercase();
    let is_claude = lower.contains("claude");
    let is_image = lower.contains("image");

    if is_claude {
        let session_id = format!(
            "warmup_{}_{}",
            chrono::Utc::now().timestamp_millis(),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let claude_request = crate::proxy::mappers::claude::models::ClaudeRequest {
            model: model.to_string(),
            messages: vec![crate::proxy::mappers::claude::models::Message {
                role: "user".to_string(),
                content: crate::proxy::mappers::claude::models::MessageContent::String(
                    "ping".to_string(),
                ),
            }],
            max_tokens: None,
            stream: true,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            tools: None,
            metadata: Some(crate::proxy::mappers::claude::models::Metadata {
                user_id: Some(session_id.clone()),
            }),
            thinking: None,
            output_config: None,
            size: None,
            quality: None,
        };

        crate::proxy::mappers::claude::transform_claude_request_in(
            &claude_request,
            project_id,
            false,
            acc_opt,
            &session_id,
            None,
        )
    } else {
        let base_request = if is_image {
            json!({
                "contents": [{"role": "user", "parts": [{"text": "Say hi"}]}],
                "generationConfig": {
                    "maxOutputTokens": 10,
                    "responseModalities": ["TEXT"]
                }
            })
        } else {
            json!({
                "contents": [{"role": "user", "parts": [{"text": "Say hi"}]}],
                "generationConfig": {
                    "maxOutputTokens": 128
                }
            })
        };

        Ok(wrap_request(
            &base_request,
            project_id,
            model,
            acc_opt,
            None,
            None,
        ))
    }
}

/// 预热响应
#[derive(Debug, Serialize)]
pub struct WarmupResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 处理预热请求
pub async fn handle_warmup(
    State(state): State<AppState>,
    Json(req): Json<WarmupRequest>,
) -> Response {
    let start_time = std::time::Instant::now();

    // ===== 前置检查：跳过 gemini-2.5-* 家族模型 =====
    let model_lower = req.model.to_lowercase();
    if model_lower.contains("2.5-") || model_lower.contains("2-5-") {
        info!(
            "[Warmup-API] SKIP: gemini-2.5-* model not supported for warmup: {} / {}",
            req.email, req.model
        );
        return (
            StatusCode::OK,
            Json(WarmupResponse {
                success: true,
                message: format!(
                    "Skipped warmup for {} (2.5 models not supported)",
                    req.model
                ),
                error: None,
            }),
        )
            .into_response();
    }

    info!(
        "[Warmup-API] ========== START: email={}, model={} ==========",
        req.email, req.model
    );

    // ===== 步骤 1: 获取 Token =====
    let (access_token, project_id, account_id) =
        if let (Some(at), Some(pid)) = (&req.access_token, &req.project_id) {
            (
                at.clone(),
                pid.clone(),
                resolve_warmup_account_id(req.account_id.as_deref(), &req.email),
            )
        } else {
            match state.token_manager.get_token_by_email(&req.email).await {
                Ok((at, pid, _, acc_id, _wait_ms)) => (at, pid, acc_id),
                Err(e) => {
                    warn!(
                        "[Warmup-API] Step 1 FAILED: Token error for {}: {}",
                        req.email, e
                    );
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(WarmupResponse {
                            success: false,
                            message: format!("Failed to get token for {}", req.email),
                            error: Some(e),
                        }),
                    )
                        .into_response();
                }
            }
        };

    let acc_opt = (!account_id.trim().is_empty()).then_some(account_id.as_str());

    // ===== 步骤 2: 根据模型类型构建请求体 =====
    let body: Value = match build_warmup_payload(&req.model, &project_id, acc_opt) {
        Ok(transformed) => transformed,
        Err(e) => {
            warn!("[Warmup-API] Step 2 FAILED: Warmup payload build error: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WarmupResponse {
                    success: false,
                    message: format!("Transform error: {}", e),
                    error: Some(e),
                }),
            )
                .into_response();
        }
    };

    // ===== 步骤 3: 调用 UpstreamClient =====
    // 对齐官方抓包：requestType="agent" 100% 走 streamGenerateContent?alt=sse，严禁回退 generateContent 或隐藏重发
    let result = state
        .upstream
        .call_v1_internal(
            "streamGenerateContent",
            &access_token,
            body,
            Some("alt=sse"),
            acc_opt,
        )
        .await;

    let duration = start_time.elapsed().as_millis() as u64;

    // ===== 步骤 4: 处理响应并记录流量日志 =====
    match result {
        Ok(call_result) => {
            let response = call_result.response;
            let status = response.status();
            let status_code = status.as_u16();

            // 记录预热请求到流量日志
            let log = ProxyRequestLog {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now().timestamp_millis(),
                method: "POST".to_string(),
                url: format!("/internal/warmup -> {}", req.model),
                status: status_code,
                duration,
                model: Some(req.model.clone()),
                mapped_model: Some(req.model.clone()),
                account_email: Some(req.email.clone()),
                account_id: acc_opt.map(str::to_string),
                client_ip: Some("127.0.0.1".to_string()),
                error: if status.is_success() {
                    None
                } else {
                    Some(format!("HTTP {}", status_code))
                },
                request_body: Some(format!(
                    "{{\"type\": \"warmup\", \"model\": \"{}\"}}",
                    req.model
                )),
                response_body: None,
                input_tokens: Some(0),
                output_tokens: Some(0),
                cached_tokens: None,
                protocol: Some("warmup".to_string()),
                username: None,
                in_flight_peak: None,
                response_source: Some("remote".to_string()),
            };
            state.monitor.log_request(log).await;

            let mut response = if status.is_success() {
                // 完整消费响应流，避免提前 Drop 连接触发 HTTP/2 RST_STREAM
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(25),
                    response.bytes(),
                )
                .await;
                info!(
                    "[Warmup-API] ========== SUCCESS: {} / {} ({}ms) ==========",
                    req.email, req.model, duration
                );
                (
                    StatusCode::OK,
                    Json(WarmupResponse {
                        success: true,
                        message: format!("Warmup triggered for {}", req.model),
                        error: None,
                    }),
                )
                    .into_response()
            } else {
                let error_text = response.text().await.unwrap_or_default();

                // [FIX] 预热阶段检测到 403 时，标记账号为 forbidden 并立即从内存池移除，避免无效账号继续参与轮询
                // 如果 account_id 为空（直接传入 access_token 的场景），通过 email 从索引中找到 ID
                if status_code == 403 {
                    let resolved_account_id = if !account_id.is_empty() {
                        account_id.clone()
                    } else {
                        // 尝试通过 email 查找账号 ID
                        crate::modules::account::find_account_id_by_email(&req.email)
                            .unwrap_or_default()
                    };

                    if !resolved_account_id.is_empty() {
                        warn!(
                            "[Warmup-API] 403 Forbidden detected for {}, marking account as forbidden and removing from memory pool",
                            req.email
                        );
                        let _ = crate::modules::account::mark_account_forbidden(
                            &resolved_account_id,
                            &error_text,
                        );
                        state.token_manager.remove_account(&resolved_account_id);
                    } else {
                        warn!(
                            "[Warmup-API] 403 Forbidden detected for {} but could not resolve account_id, skipping mark",
                            req.email
                        );
                    }
                }

                (
                    StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(WarmupResponse {
                        success: false,
                        message: format!("Warmup failed: HTTP {}", status_code),
                        error: Some(error_text),
                    }),
                )
                    .into_response()
            };

            // 添加响应头，让监控中间件捕获账号信息
            if let Ok(email_val) = axum::http::HeaderValue::from_str(&req.email) {
                response.headers_mut().insert("X-Account-Email", email_val);
            }
            if let Ok(model_val) = axum::http::HeaderValue::from_str(&req.model) {
                response.headers_mut().insert("X-Mapped-Model", model_val);
            }

            response
        }
        Err(e) => {
            warn!(
                "[Warmup-API] ========== ERROR: {} / {} - {} ({}ms) ==========",
                req.email, req.model, e, duration
            );

            // 记录失败的预热请求到流量日志
            let log = ProxyRequestLog {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now().timestamp_millis(),
                method: "POST".to_string(),
                url: format!("/internal/warmup -> {}", req.model),
                status: 500,
                duration,
                model: Some(req.model.clone()),
                mapped_model: Some(req.model.clone()),
                account_email: Some(req.email.clone()),
                account_id: acc_opt.map(str::to_string),
                client_ip: Some("127.0.0.1".to_string()),
                error: Some(e.clone()),
                request_body: Some(format!(
                    "{{\"type\": \"warmup\", \"model\": \"{}\"}}",
                    req.model
                )),
                response_body: Some(e.clone()),
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                protocol: Some("warmup".to_string()),
                username: None,
                in_flight_peak: None,
                response_source: Some("local".to_string()),
            };
            state.monitor.log_request(log).await;

            let mut response = (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WarmupResponse {
                    success: false,
                    message: "Warmup request failed".to_string(),
                    error: Some(e),
                }),
            )
                .into_response();

            // 即使失败也添加响应头，以便监控
            if let Ok(email_val) = axum::http::HeaderValue::from_str(&req.email) {
                response.headers_mut().insert("X-Account-Email", email_val);
            }
            if let Ok(model_val) = axum::http::HeaderValue::from_str(&req.model) {
                response.headers_mut().insert("X-Mapped-Model", model_val);
            }

            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_warmup_request_parses_optional_account_id() {
        let with_id: WarmupRequest = serde_json::from_value(json!({
            "email": "user@example.com",
            "model": "gemini-3.8-flash-medium",
            "account_id": "acc-123"
        }))
        .unwrap();
        assert_eq!(with_id.account_id.as_deref(), Some("acc-123"));
        assert_eq!(
            resolve_warmup_account_id(with_id.account_id.as_deref(), &with_id.email),
            "acc-123"
        );

        let without_id: WarmupRequest = serde_json::from_value(json!({
            "email": "user@example.com",
            "model": "gemini-3.8-flash-medium"
        }))
        .unwrap();
        assert!(without_id.account_id.is_none());
    }

    #[test]
    fn test_build_warmup_payload_includes_session_id_and_account_seed() {
        let gemini_body =
            build_warmup_payload("gemini-3.5-flash-lite", "proj-1", Some("acc-xyz")).unwrap();
        assert_eq!(gemini_body["requestType"], "agent");
        assert!(gemini_body["request"].get("sessionId").is_some());
        assert_eq!(
            gemini_body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "LOW"
        );

        let claude_body =
            build_warmup_payload("claude-sonnet-5-5-medium", "proj-1", Some("acc-xyz")).unwrap();
        assert_eq!(claude_body["requestType"], "agent");
        assert!(claude_body["request"].get("sessionId").is_some());
    }
}
