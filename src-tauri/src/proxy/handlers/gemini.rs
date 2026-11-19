// Gemini Handler
use axum::{
    extract::State,
    extract::{Json, Path},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{json, Value};
use tracing::{debug, error, info};

use crate::proxy::common::client_adapter::CLIENT_ADAPTERS;
use crate::proxy::debug_logger;
use crate::proxy::handlers::common::{
    apply_retry_strategy, determine_retry_strategy, should_rotate_account,
};
use crate::proxy::mappers::gemini::{unwrap_response, wrap_request, wrap_request_v2};
use crate::proxy::server::AppState;
use crate::proxy::session_manager::SessionManager;
use crate::proxy::upstream::client::mask_email;
use axum::http::HeaderMap;

const MAX_RETRY_ATTEMPTS: usize = 3;

/// 处理 generateContent 和 streamGenerateContent
/// 路径参数: model_name, method (e.g. "gemini-pro", "generateContent")
pub async fn handle_generate(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,          // [NEW] Extract headers for adapter detection
    Json(mut body): Json<Value>, // 改为 mut 以支持修复提示词注入
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // 解析 model:method
    let (model_name, method) = if let Some((m, action)) = model_action.rsplit_once(':') {
        (m.to_string(), action.to_string())
    } else {
        (model_action, "generateContent".to_string())
    };

    crate::modules::logger::log_info(&format!(
        "Received Gemini request: {}/{}",
        model_name, method
    ));
    let trace_id = format!("req_{}", chrono::Utc::now().timestamp_subsec_millis());
    let debug_cfg = state.debug_logging.read().await.clone();

    // [NEW] Detect Client Adapter
    let client_adapter = CLIENT_ADAPTERS
        .iter()
        .find(|a| a.matches(&headers))
        .cloned();
    if client_adapter.is_some() {
        debug!("[{}] Client Adapter detected", trace_id);
    }

    // 1. 验证方法
    if method != "generateContent" && method != "streamGenerateContent" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unsupported method: {}", method),
        ));
    }
    if debug_logger::is_enabled(&debug_cfg) {
        let original_payload = json!({
            "kind": "original_request",
            "protocol": "gemini",
            "trace_id": trace_id,
            "original_model": model_name,
            "method": method,
            "request": body.clone(),
        });
        debug_logger::write_debug_payload(
            &debug_cfg,
            Some(&trace_id),
            "original_request",
            &original_payload,
        )
        .await;
    }
    // [TOXIC CACHE] 入口毒丸拦截：检查是否命中近 30 分钟内导致过 403 的毒丸请求
    let body_bytes_for_fp = serde_json::to_vec(&body).unwrap_or_default();
    let toxic_fp = crate::proxy::toxic_cache::calculate_fingerprint(&model_name, &body_bytes_for_fp);
    if let Some((toxic_status, toxic_body)) = crate::proxy::toxic_cache::check_toxic(&toxic_fp) {
        let fp_prefix = if toxic_fp.len() >= 12 { &toxic_fp[..12] } else { &toxic_fp };
        tracing::warn!("🚫 [Gemini] Fast early rejection on toxic request fingerprint: {}", fp_prefix);
        return Ok((toxic_status, [("content-type", "application/json")], toxic_body).into_response());
    }
    // [REQUEST 503 BREAKER] 同一请求累计 4 次 503 后本地冷却 45s，防止客户端死循环重发打爆多个账号
    if let Some((retry_secs, err_body)) = crate::proxy::toxic_cache::check_request_503_cooldown(&toxic_fp) {
        let fp_prefix = if toxic_fp.len() >= 12 { &toxic_fp[..12] } else { &toxic_fp };
        tracing::warn!(
            "🛑 [Gemini] Request-level 503 cooldown active for fingerprint {} (retry_after={}s)",
            fp_prefix,
            retry_secs
        );
        let retry_str = retry_secs.to_string();
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [
                ("content-type", "application/json"),
                ("retry-after", retry_str.as_str()),
            ],
            err_body,
        )
            .into_response());
    }

    let client_wants_stream = method == "streamGenerateContent";
    // [AUTO-CONVERSION] 强制内部流式化
    let force_stream_internally = !client_wants_stream;
    let is_stream = client_wants_stream || force_stream_internally;

    if force_stream_internally {
        // debug!("[AutoConverter] Converting non-stream request to stream");
    }

    // [TPM Protection] 预估输入 Token 规模，用于拟人化流控与大上下文阶梯延时降频
    let estimated_prompt_tokens =
        crate::proxy::mappers::context_manager::ContextManager::estimate_gemini_token_usage(&body);

    // 2. 获取 UpstreamClient 和 TokenManager
    let upstream = state.upstream.clone();
    let token_manager = state.token_manager;
    let pool_size = token_manager.len();
    let max_attempts = MAX_RETRY_ATTEMPTS.min(pool_size.saturating_add(1)).max(2);

    let mut last_error = String::new();
    let mut last_email: Option<String> = None;
    let mut last_status = StatusCode::SERVICE_UNAVAILABLE;
    let mut force_rotate = false;
    let mut inflight_guard: Option<crate::proxy::account_inflight::AccountInflightGuard> = None;
    let mut lease_guard: Option<crate::proxy::SlotLeaseGuard> = None;
    let raw_body_len: usize = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| serde_json::to_vec(&body).map(|b| b.len()).unwrap_or(0));
    let footprint = crate::proxy::RequestFootprint::new(estimated_prompt_tokens, raw_body_len);

    for attempt in 0..max_attempts {
        lease_guard = None;
        // 3. 模型路由解析
        let mapped_model = crate::proxy::common::model_mapping::resolve_model_route(
            &model_name,
            &*state.custom_mapping.read().await,
        );
        // 提取 tools 列表以进行联网探测 (Gemini 风格可能是嵌套的)
        let tools_val: Option<Vec<Value>> =
            body.get("tools").and_then(|t| t.as_array()).map(|arr| {
                let mut flattened = Vec::new();
                for tool_entry in arr {
                    if let Some(decls) = tool_entry
                        .get("functionDeclarations")
                        .and_then(|v| v.as_array())
                    {
                        flattened.extend(decls.iter().cloned());
                    } else {
                        flattened.push(tool_entry.clone());
                    }
                }
                flattened
            });

        let config = crate::proxy::mappers::common_utils::resolve_request_config(
            &model_name,
            &mapped_model,
            &tools_val,
            None,        // size (not applicable for Gemini native protocol)
            None,        // quality
            None,        // [NEW] image_size
            Some(&body), // [NEW] Pass request body for imageConfig parsing
        );

        // 4. 获取 Token (使用准确的 request_type)
        // 提取 SessionId (粘性指纹)
        let session_id = SessionManager::extract_gemini_session_id(&body, &model_name);

        // 关键：根据 force_rotate 标志与容量感知租约决定选号与切号
        let acquired = match token_manager
            .acquire_token_with_lease(
                &config.request_type,
                force_rotate,
                Some(&session_id),
                &config.final_model,
                footprint,
            )
            .await
        {
            Ok(acq) => acq,
            Err(crate::proxy::TokenAcquireError::PoolCoolingDown {
                retry_after_secs,
                reason,
            }) => {
                tracing::warn!(
                    "⏳ [Capacity Router] Pool cooling down ({} tokens, {}B): {} (Retry-After: {}s)",
                    estimated_prompt_tokens,
                    raw_body_len,
                    reason,
                    retry_after_secs
                );
                let err_json = json!({
                    "error": {
                        "code": 429,
                        "message": format!("All pool accounts are currently cooling down ({}). Please retry in {} seconds.", reason, retry_after_secs),
                        "status": "RESOURCE_EXHAUSTED",
                        "details": [{
                            "@type": "type.googleapis.com/google.rpc.RetryInfo",
                            "retryDelay": format!("{}s", retry_after_secs)
                        }]
                    }
                });
                let builder = axum::response::Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("Content-Type", "application/json")
                    .header("Retry-After", retry_after_secs.to_string());
                return Ok(builder
                    .body(axum::body::Body::from(
                        serde_json::to_string(&err_json).unwrap(),
                    ))
                    .unwrap()
                    .into_response());
            }
            Err(crate::proxy::TokenAcquireError::Unavailable(e)) => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Token error: {}", e),
                ));
            }
        };

        let access_token = acquired.access_token;
        let project_id = acquired.project_id;
        let email = acquired.email;
        let account_id = acquired.account_id;
        lease_guard = Some(acquired.lease_guard);

        let mapped_model = token_manager
            .resolve_dynamic_model_for_account(&account_id, &mapped_model)
            .await;

        last_email = Some(email.clone());
        info!(
            "✓ Using account: {} (type: {}, tier: {:?})",
            email, config.request_type, footprint.tier
        );

        let need_new = match inflight_guard.as_ref() {
            Some(g) => g.account_id() != account_id.as_str(),
            None => true,
        };
        if need_new {
            token_manager.replace_inflight(&mut inflight_guard, &account_id);
        }

        // 5. 包装请求 (project injection)
        // [FIX #765] Pass session_id to wrap_request for signature injection
        // [NEW] 获取完整 Token 对象以注入动态规格 (dynamic > static default > 65535)
        let token_obj = token_manager.get_token_by_id(&account_id);
        let wrapped_body = wrap_request_v2(
            &body,
            &project_id,
            &mapped_model,
            Some(account_id.as_str()),
            Some(&session_id),
            token_obj.as_ref(),
            Some(&token_manager),
        );

        if debug_logger::is_enabled(&debug_cfg) {
            let payload = json!({
                "kind": "v1internal_request",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "request_type": config.request_type,
                "attempt": attempt,
                "v1internal_request": wrapped_body.clone(),
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "v1internal_request",
                &payload,
            )
            .await;
        }

        // 5. 上游调用
        let query_string = if is_stream { Some("alt=sse") } else { None };
        let upstream_method = if is_stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };

        // [FIXED] Official Antigravity client never sends anthropic-beta header to Google v1internal
        let extra_headers = std::collections::HashMap::new();

        let call_result = match upstream
            .call_v1_internal_with_headers(
                upstream_method,
                &access_token,
                wrapped_body,
                query_string,
                extra_headers.clone(),
                Some(account_id.as_str()),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = e.clone();
                debug!(
                    "Gemini Request failed on attempt {}/{}: {}",
                    attempt + 1,
                    max_attempts,
                    e
                );
                continue;
            }
        };

        // [NEW] 记录端点降级日志到 debug 文件
        if !call_result.fallback_attempts.is_empty() && debug_logger::is_enabled(&debug_cfg) {
            let fallback_entries: Vec<serde_json::Value> = call_result
                .fallback_attempts
                .iter()
                .map(|a| {
                    json!({
                        "endpoint_url": a.endpoint_url,
                        "status": a.status,
                        "error": a.error,
                    })
                })
                .collect();
            let payload = json!({
                "kind": "endpoint_fallback",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "attempt": attempt,
                "account": mask_email(&email),
                "fallback_attempts": fallback_entries,
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "endpoint_fallback",
                &payload,
            )
            .await;
        }

        let response = call_result.response;
        // [NEW] 提取实际请求的上游端点 URL，用于日志记录和排查
        let upstream_url = response.url().to_string();
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        // [NEW] 提取官方 TraceID
        let cloud_code_trace_id = response
            .headers()
            .get("x-cloudaicompanion-trace-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        if status.is_success() {
            // 6. 响应处理
            if is_stream {
                use axum::body::Body;
                use futures::StreamExt;

                let meta = json!({
                    "protocol": "gemini",
                    "trace_id": trace_id,
                    "original_model": model_name,
                    "mapped_model": mapped_model,
                    "request_type": config.request_type,
                    "attempt": attempt,
                    "status": status.as_u16(),
                    "upstream_url": upstream_url,
                });
                let mut response_stream = debug_logger::wrap_stream_with_debug(
                    Box::pin(response.bytes_stream()),
                    debug_cfg.clone(),
                    trace_id.clone(),
                    "upstream_response",
                    meta,
                );
                let s_id = session_id.clone();

                if !client_wants_stream {
                    // 非流式请求（内部强转流）：必须在重试循环内完成整条流的收集与完整性校验，
                    // 绝不在收到首包时就提前宣告成功；缺失输出/缺失 finishReason/流中途出错统一走换号重试。
                    let timed_stream = stream_with_idle_timeout(
                        response_stream,
                        std::time::Duration::from_secs(300),
                    );
                    match crate::proxy::mappers::gemini::collector::collect_stream_to_json(
                        Box::pin(timed_stream),
                        &s_id,
                    )
                    .await
                    {
                        Ok(mut gemini_resp) => {
                            let has_finish_reason = gemini_resp
                                .pointer("/candidates/0/finishReason")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .map_or(false, |s| !s.is_empty());
                            if !has_finish_reason {
                                tracing::warn!(
                                    "[{}] Non-stream collected Gemini response missing finishReason on account {}, retrying...",
                                    session_id,
                                    email
                                );
                                last_error =
                                    "Upstream Gemini stream ended without a finishReason".to_string();
                                last_status = StatusCode::BAD_GATEWAY;
                                force_rotate = true;
                                continue;
                            }

                            if let Some(g) = lease_guard.as_mut() {
                                g.mark_upstream_accepted();
                            }
                            crate::proxy::toxic_cache::clear_request_503(&toxic_fp);
                            crate::proxy::mappers::gemini::wrapper::inject_ids_to_response(
                                &mut gemini_resp,
                                &mapped_model,
                            );
                            if let Some(mut guard) = lease_guard.take() {
                                guard.complete();
                            }
                            info!(
                                "[{}] ✓ Stream collected and converted to JSON (Gemini)",
                                session_id
                            );
                            let unwrapped = unwrap_response(&gemini_resp);
                            let peak = crate::proxy::account_inflight::sample_peak(
                                &mut inflight_guard,
                            );
                            let headers = crate::proxy::account_inflight::account_headers(
                                &email,
                                Some(&account_id),
                                Some(mapped_model.as_str()),
                                peak,
                            );
                            return Ok((StatusCode::OK, headers, Json(unwrapped)).into_response());
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[{}] Non-stream Gemini collection failed on account {}: {}, retrying...",
                                session_id,
                                email,
                                e
                            );
                            last_error = e;
                            last_status = StatusCode::BAD_GATEWAY;
                            force_rotate = true;
                            continue;
                        }
                    }
                }

                // 流式请求（streamGenerateContent）：
                // Peek 阶段按 SSE 完整行解析（跳过心跳、对纯思考加 1500ms 短观察窗防瞬断），
                // 若瞬时出现“只有思考/空响应/无 finishReason”或流内错误帧，在发 HTTP 200 前截停重试。
                let peek_outcome = match peek_gemini_stream_start(
                    &mut response_stream,
                    &s_id,
                    &mapped_model,
                    std::time::Duration::from_secs(300),
                    std::time::Duration::from_millis(1500),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(fail) => {
                        tracing::warn!(
                            "[Gemini] Peek rejected stream on account {}: {}, retrying...",
                            email,
                            fail.message
                        );
                        last_error = fail.message;
                        last_status = fail.status;
                        force_rotate = true;
                        continue;
                    }
                };

                if let Some(g) = lease_guard.as_mut() {
                    g.mark_upstream_accepted();
                }
                crate::proxy::toxic_cache::clear_request_503(&toxic_fp);

                let stream_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stream = build_gemini_sse_relay_stream(
                    response_stream,
                    peek_outcome,
                    cloud_code_trace_id.clone(),
                    std::time::Duration::from_secs(300),
                    stream_failed.clone(),
                );

                let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
                let mut builder = axum::response::Response::builder()
                    .header("Content-Type", "text/event-stream")
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "keep-alive")
                    .header("X-Accel-Buffering", "no")
                    .header("X-Account-Email", &email)
                    .header("X-Account-Id", &account_id)
                    .header("X-Mapped-Model", &mapped_model)
                    .header(
                        crate::proxy::account_inflight::HEADER_RESPONSE_SOURCE,
                        crate::proxy::account_inflight::SOURCE_REMOTE,
                    );
                if let Some(p) = peak {
                    builder = builder.header("X-In-Flight-Peak", p.to_string());
                }
                let lease_arc = std::sync::Arc::new(std::sync::Mutex::new(lease_guard.take()));
                let lease_clone = lease_arc.clone();
                let inflight_arc =
                    std::sync::Arc::new(std::sync::Mutex::new(inflight_guard.take()));
                let inflight_clone = inflight_arc.clone();
                let stream_failed_for_lease = stream_failed.clone();
                let stream_with_pacer = async_stream::stream! {
                    let _guard_hold = inflight_clone;
                    let _lease_hold = lease_clone;
                    let mut inner = Box::pin(stream);
                    while let Some(item) = inner.next().await {
                        yield item;
                    }
                    if let Ok(mut g) = _lease_hold.lock() {
                        if let Some(mut guard) = g.take() {
                            if stream_failed_for_lease.load(std::sync::atomic::Ordering::Acquire) {
                                guard.complete_on_upstream_overload();
                            } else {
                                guard.complete();
                            }
                        }
                    }
                    drop(_guard_hold);
                };
                let body = Body::from_stream(stream_with_pacer);
                return Ok(builder.body(body).unwrap().into_response());
            }

            if let Some(g) = lease_guard.as_mut() {
                g.mark_upstream_accepted();
            }
            crate::proxy::toxic_cache::clear_request_503(&toxic_fp);
            let mut gemini_resp: Value = response
                .json()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

            // [FIX #1522] Inject Tool ID into Non-streaming Response
            crate::proxy::mappers::gemini::wrapper::inject_ids_to_response(
                &mut gemini_resp,
                &mapped_model,
            );

            // [FIX #765] Extract thoughtSignature from non-streaming response
            let inner_val = if gemini_resp.get("response").is_some() {
                gemini_resp.get("response")
            } else {
                Some(&gemini_resp)
            };

            if let Some(resp) = inner_val {
                if let Some(candidates) = resp.get("candidates").and_then(|c| c.as_array()) {
                    for cand in candidates {
                        if let Some(parts) = cand
                            .get("content")
                            .and_then(|c| c.get("parts"))
                            .and_then(|p| p.as_array())
                        {
                            for part in parts {
                                if let Some(sig) =
                                    part.get("thoughtSignature").and_then(|s| s.as_str())
                                {
                                    crate::proxy::SignatureCache::global().cache_session_signature(
                                        &session_id,
                                        sig.to_string(),
                                        1,
                                    );
                                    debug!("[Gemini-Response] Cached signature (len: {}) for session: {}", sig.len(), session_id);
                                }
                            }
                        }
                    }
                }
            }

            let unwrapped = unwrap_response(&gemini_resp);
            if let Some(mut guard) = lease_guard.take() {
                guard.complete();
            }
            let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
            let headers = crate::proxy::account_inflight::account_headers(
                &email,
                Some(&account_id),
                Some(mapped_model.as_str()),
                peak,
            );
            return Ok((StatusCode::OK, headers, Json(unwrapped)).into_response());
        }

        // 处理错误并重试
        let status_code = status.as_u16();
        last_status = status;
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| format!("HTTP {}", status_code));
        last_error = format!("HTTP {}: {}", status_code, error_text);
        if debug_logger::is_enabled(&debug_cfg) {
            let payload = json!({
                "kind": "upstream_response_error",
                "protocol": "gemini",
                "trace_id": trace_id,
                "original_model": model_name,
                "mapped_model": mapped_model,
                "request_type": config.request_type,
                "attempt": attempt,
                "status": status_code,
                "upstream_url": upstream_url,
                "account": mask_email(&email),
                "error_text": error_text,
            });
            debug_logger::write_debug_payload(
                &debug_cfg,
                Some(&trace_id),
                "upstream_response_error",
                &payload,
            )
            .await;
        }

        // 标记限流状态(用于 UI 显示与 429 自动禁用/退避自愈)
        if status_code == 429
            || status_code == 500
            || status_code == 404
        {
            let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
            token_manager
                .mark_rate_limited_async(
                    &email,
                    status_code,
                    retry_after.as_deref(),
                    &error_text,
                    Some(&mapped_model),
                    peak,
                )
                .await;
        } else if status_code == 503 || status_code == 529 {
            // [503/529 双层防重压熔断]
            // 1. 保留本次请求已消耗的槽位冷却、60s RPM 计数与预占热度，绝不回滚
            if let Some(g) = lease_guard.as_mut() {
                g.complete_on_upstream_overload();
            }
            // 2. 记录请求指纹级 503 计数（满 4 次本地冷却该请求 45s）
            crate::proxy::toxic_cache::record_request_503(&toxic_fp, &error_text);
            // 3. 记录账号级连续 503 计数（满 3 次或出现 PER_STUB 则解绑粘性会话、降权并本地冷却 150s/300s）
            let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
            token_manager
                .record_account_503_async(
                    &email,
                    status_code,
                    &error_text,
                    Some(&mapped_model),
                    peak,
                )
                .await;
        }

        // 遇到 403 Forbidden 立即硬熔断：记录 403 毒丸指纹(TTL 30分钟)，标记反代已禁用、移出可用号池并持久化，彻底禁止任何重试，立刻返回响应
        if status_code == 403 {
            // [TOXIC CACHE] 若包含 TOS_VIOLATION 或 disabled，记录毒丸指纹 1800 秒，严禁重试换号
            let is_tos_or_disabled = error_text.contains("TOS_VIOLATION")
                || error_text.to_uppercase().contains("TOS_VIOLATION")
                || error_text.contains("disabled")
                || error_text.to_lowercase().contains("disabled");
            if is_tos_or_disabled {
                crate::proxy::toxic_cache::record_toxic(&toxic_fp, 403, error_text.clone(), 1800);
                let fp_prefix = if toxic_fp.len() >= 12 { &toxic_fp[..12] } else { &toxic_fp };
                tracing::warn!(
                    "🚫 [ToxicCache] Recorded toxic payload for Gemini (fingerprint: {}): TOS_VIOLATION / disabled detected",
                    fp_prefix
                );
            }
            let target_acc_id = token_manager
                .get_account_id_by_email(&email)
                .unwrap_or_else(|| account_id.clone());

            if let Err(e) = token_manager.set_forbidden(&target_acc_id, &error_text).await {
                tracing::error!("Failed to set forbidden status for {}: {}", email, e);
            } else {
                tracing::warn!("🚫 [Gemini] Account {} marked as forbidden (403), proxy disabled immediately", email);
            }

            if error_text.contains("VALIDATION_REQUIRED")
                || error_text.contains("verify your account")
                || error_text.contains("validation_url")
            {
                let block_minutes = 10i64;
                let block_until = chrono::Utc::now().timestamp() + (block_minutes * 60);
                let _ = token_manager
                    .set_validation_block_public(&target_acc_id, block_until, &error_text)
                    .await;
            }

            // 严禁后续任何重试，立刻向客户端返回 403 响应
            let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
            let headers = crate::proxy::account_inflight::account_headers(
                &email,
                Some(&target_acc_id),
                Some(mapped_model.as_str()),
                peak,
            );
            return Ok((status, headers, error_text).into_response());
        }

        if status_code == 401 {
            token_manager.invalidate_access_token(&account_id, &access_token);
        }

        // 确定重试策略
        let strategy = determine_retry_strategy(status_code, &error_text, false);
        let trace_id = format!("gemini_{}", session_id);

        // 执行退避
        if apply_retry_strategy(
            strategy.clone(),
            attempt,
            max_attempts,
            status_code,
            &trace_id,
        )
        .await
        {
            // [NEW] Apply Client Adapter "let_it_crash" strategy
            if let Some(adapter) = &client_adapter {
                if adapter.let_it_crash() && attempt > 0 {
                    tracing::warn!(
                        "[Gemini] let_it_crash active: Aborting retries after attempt {}",
                        attempt
                    );
                    break;
                }
            }

            // 判断是否需要轮换账号
            if !should_rotate_account(status_code, Some(&strategy)) {
                debug!(
                "[{}] Keeping same account for status {} (Gemini server-side issue or Grace Retry)",
                trace_id, status_code
            );
                force_rotate = false;
            } else {
                force_rotate = true;
            }

            continue;
        }

        // [NEW] 处理 400 错误 (Thinking 签名失效)
        if status_code == 400
            && (error_text.contains("Invalid `signature`")
                || error_text.contains("thinking.signature")
                || error_text.contains("Invalid signature")
                || error_text.contains("Corrupted thought signature"))
        {
            tracing::warn!(
                "[Gemini] Signature error detected on account {}, retrying without thinking",
                email
            );

            // 追加修复提示词到请求体的最后一条内容
            if let Some(contents) = body.get_mut("contents").and_then(|v| v.as_array_mut()) {
                if let Some(last_content) = contents.last_mut() {
                    if let Some(parts) =
                        last_content.get_mut("parts").and_then(|v| v.as_array_mut())
                    {
                        parts.push(json!({
                            "text": "\n\n[System Recovery] Your previous output contained an invalid signature. Please regenerate the response without the corrupted signature block."
                        }));
                        tracing::debug!("[Gemini] Appended repair prompt to last content");
                    }
                }
            }

            continue; // 重试
        }

        // 停止重试时直接透传真实上游 HTTP 状态码与错误内容（绝不将 503 伪装成 429 All accounts exhausted）
        error!(
            "Gemini Upstream error {} (returning without further retry): {}",
            status_code, error_text
        );
        let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
        let headers = crate::proxy::account_inflight::account_headers(
            &email,
            Some(&account_id),
            Some(mapped_model.as_str()),
            peak,
        );
        let err_json = serde_json::from_str::<Value>(&error_text)
            .ok()
            .filter(|v| v.get("error").is_some())
            .unwrap_or_else(|| {
                let status_label = match status_code {
                    429 => "RESOURCE_EXHAUSTED",
                    503 | 529 => "UNAVAILABLE",
                    404 => "NOT_FOUND",
                    400 => "INVALID_ARGUMENT",
                    _ => "UPSTREAM_ERROR",
                };
                json!({
                    "error": {
                        "code": status_code,
                        "message": error_text,
                        "status": status_label
                    }
                })
            });
        return Ok((status, headers, Json(err_json)).into_response());
    }

    let final_code = last_status.as_u16();
    let final_status_label = match final_code {
        429 => "RESOURCE_EXHAUSTED",
        503 | 529 => "UNAVAILABLE",
        _ => "UPSTREAM_ERROR",
    };
    if let Some(email) = last_email {
        let peak = crate::proxy::account_inflight::sample_peak(&mut inflight_guard);
        let headers =
            crate::proxy::account_inflight::account_headers_with_source(
                &email,
                None,
                None,
                peak,
                crate::proxy::account_inflight::SOURCE_REMOTE,
            );
        Ok((
            last_status,
            headers,
            Json(json!({
                "error": {
                    "code": final_code,
                    "message": last_error,
                    "status": final_status_label
                }
            })),
        )
            .into_response())
    } else {
        Ok((
            last_status,
            Json(json!({
                "error": {
                    "code": final_code,
                    "message": last_error,
                    "status": final_status_label
                }
            })),
        )
            .into_response())
    }
}

pub async fn handle_list_models(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    use crate::proxy::common::model_mapping::get_all_dynamic_models;

    // 获取所有动态模型列表（与 /v1/models 一致）
    let model_ids = get_all_dynamic_models(&state.custom_mapping, Some(&state.token_manager)).await;

    // 转换为 Gemini API 格式
    let models: Vec<_> = model_ids
        .into_iter()
        .map(|id| {
            json!({
                "name": format!("models/{}", id),
                "version": "001",
                "displayName": id.clone(),
                "description": "",
                "inputTokenLimit": 128000,
                "outputTokenLimit": 8192,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "temperature": 1.0,
                "topP": 0.95,
                "topK": 64
            })
        })
        .collect();

    Ok(Json(json!({ "models": models })))
}

pub async fn handle_get_model(Path(model_name): Path<String>) -> impl IntoResponse {
    Json(json!({
        "name": format!("models/{}", model_name),
        "displayName": model_name
    }))
}

pub async fn handle_count_tokens(
    State(state): State<AppState>,
    Path(_model_name): Path<String>,
    Json(_body): Json<Value>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let model_group = "gemini";
    let (_access_token, _project_id, _, _, _wait_ms) = state
        .token_manager
        .get_token(model_group, false, None, "gemini")
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Token error: {}", e),
            )
        })?;

    Ok(Json(json!({"totalTokens": 0})))
}

#[derive(Default)]
struct GeminiSseRelayState {
    saw_data_event: bool,
    saw_output: bool,
    saw_thought: bool,
    finish_reason: Option<String>,
    saw_error: bool,
    error_message: Option<String>,
}

struct GeminiSseParser {
    utf8_buf: crate::proxy::mappers::Utf8ChunkBuffer,
    line_buf: String,
    session_id: String,
    model_name: String,
    state: GeminiSseRelayState,
}

impl GeminiSseParser {
    fn new(session_id: String, model_name: String) -> Self {
        Self {
            utf8_buf: crate::proxy::mappers::Utf8ChunkBuffer::new(),
            line_buf: String::new(),
            session_id,
            model_name,
            state: GeminiSseRelayState::default(),
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<bytes::Bytes> {
        let text = self.utf8_buf.push(bytes);
        self.line_buf.push_str(&text);
        self.drain_complete_lines()
    }

    fn flush(&mut self) -> Vec<bytes::Bytes> {
        if let Some(tail) = self.utf8_buf.flush_lossy() {
            self.line_buf.push_str(&tail);
        }
        let mut frames = self.drain_complete_lines();
        if !self.line_buf.is_empty() {
            let tail = std::mem::take(&mut self.line_buf);
            if let Some(frame) = self.process_line(&tail) {
                frames.push(frame);
            }
        }
        frames
    }

    fn drain_complete_lines(&mut self) -> Vec<bytes::Bytes> {
        let mut frames = Vec::new();
        while let Some(pos) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=pos).collect();
            if let Some(frame) = self.process_line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    fn process_line(&mut self, raw_line: &str) -> Option<bytes::Bytes> {
        let line = raw_line.trim();
        if line.is_empty() {
            return None;
        }

        if let Some(json_part) = line.strip_prefix("data:").map(str::trim) {
            if json_part == "[DONE]" {
                return Some(bytes::Bytes::from("data: [DONE]\n\n"));
            }

            match serde_json::from_str::<Value>(json_part) {
                Ok(mut json) => {
                    self.state.saw_data_event = true;
                    {
                        let inner_val = json.get("response").unwrap_or(&json);
                        if let Some(err_obj) = inner_val.get("error") {
                            self.state.saw_error = true;
                            self.state.error_message = Some(
                                err_obj
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| err_obj.to_string()),
                            );
                        }
                        if let Some(candidates) =
                            inner_val.get("candidates").and_then(Value::as_array)
                        {
                            for cand in candidates {
                                if let Some(fr) = cand
                                    .get("finishReason")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    self.state.finish_reason = Some(fr.to_string());
                                }
                                if let Some(parts) = cand
                                    .get("content")
                                    .and_then(|c| c.get("parts"))
                                    .and_then(Value::as_array)
                                {
                                    for part in parts {
                                        if let Some(sig) =
                                            part.get("thoughtSignature").and_then(Value::as_str)
                                        {
                                            crate::proxy::SignatureCache::global()
                                                .cache_session_signature(
                                                    &self.session_id,
                                                    sig.to_string(),
                                                    1,
                                                );
                                            debug!(
                                                "[Gemini-SSE] Cached signature (len: {}) for session: {}",
                                                sig.len(),
                                                self.session_id
                                            );
                                        }
                                        if part
                                            .get("thought")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false)
                                        {
                                            self.state.saw_thought = true;
                                        }
                                        if crate::proxy::mappers::gemini::collector::is_meaningful_output_part(part) {
                                            self.state.saw_output = true;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    crate::proxy::mappers::gemini::wrapper::inject_ids_to_response(
                        &mut json,
                        &self.model_name,
                    );

                    if let Some(inner) = json.get_mut("response").map(|v| v.take()) {
                        let new_line = format!(
                            "data: {}\n\n",
                            serde_json::to_string(&inner).unwrap_or_default()
                        );
                        Some(bytes::Bytes::from(new_line))
                    } else {
                        Some(bytes::Bytes::from(format!(
                            "data: {}\n\n",
                            serde_json::to_string(&json).unwrap_or_default()
                        )))
                    }
                }
                Err(e) => {
                    debug!("[Gemini-SSE] JSON parse error: {}, passing raw line", e);
                    Some(bytes::Bytes::from(format!("{line}\n\n")))
                }
            }
        } else {
            Some(bytes::Bytes::from(format!("{line}\n\n")))
        }
    }

    fn has_terminal_incomplete_error(&self) -> Option<&'static str> {
        if self.state.saw_error {
            return None;
        }
        if self.state.finish_reason.is_none() {
            if self.state.saw_thought && !self.state.saw_output {
                return Some(
                    "Upstream Gemini stream ended with thinking only and no output or finishReason",
                );
            }
            if !self.state.saw_output {
                return Some("Upstream Gemini stream ended empty without finishReason");
            }
            return Some("Upstream Gemini stream ended unexpectedly without finishReason");
        }
        if self.state.finish_reason.as_deref() == Some("STOP") && !self.state.saw_output {
            return Some("Upstream Gemini stream ended with STOP but produced no output content");
        }
        None
    }
}

struct GeminiPeekOutcome {
    parser: GeminiSseParser,
    prebuffered_frames: Vec<bytes::Bytes>,
    stream_ended: bool,
}

#[derive(Debug)]
struct GeminiPeekFailure {
    status: StatusCode,
    message: String,
}

fn build_gemini_sse_error_bytes(code: u16, status: &str, message: &str) -> bytes::Bytes {
    let payload = json!({
        "error": {
            "code": code,
            "message": message,
            "status": status
        }
    });
    bytes::Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(&payload).unwrap_or_default()
    ))
}

fn stream_with_idle_timeout<S, E>(
    mut stream: S,
    chunk_timeout: std::time::Duration,
) -> impl futures::Stream<Item = Result<bytes::Bytes, String>> + Send + 'static
where
    S: futures::Stream<Item = Result<bytes::Bytes, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    use futures::StreamExt;
    async_stream::stream! {
        loop {
            match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(Some(Ok(bytes))) => yield Ok(bytes),
                Ok(Some(Err(e))) => {
                    yield Err(format!("Stream error: {}", e));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    yield Err(format!("Upstream stream idle timeout after {}s", chunk_timeout.as_secs()));
                    break;
                }
            }
        }
    }
}

async fn peek_gemini_stream_start<S, E>(
    stream: &mut S,
    session_id: &str,
    model_name: &str,
    first_chunk_timeout: std::time::Duration,
    thinking_grace_window: std::time::Duration,
) -> Result<GeminiPeekOutcome, GeminiPeekFailure>
where
    S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures::StreamExt;

    let mut parser = GeminiSseParser::new(session_id.to_string(), model_name.to_string());
    let mut prebuffered_frames: Vec<bytes::Bytes> = Vec::new();
    let mut grace_deadline: Option<tokio::time::Instant> = None;

    loop {
        let next_res = if let Some(deadline) = grace_deadline {
            tokio::time::timeout_at(deadline, stream.next()).await
        } else {
            tokio::time::timeout(first_chunk_timeout, stream.next()).await
        };

        match next_res {
            Ok(Some(Ok(bytes))) => {
                if bytes.is_empty() {
                    continue;
                }
                prebuffered_frames.extend(parser.push_bytes(&bytes));

                if parser.state.saw_error {
                    return Err(GeminiPeekFailure {
                        status: StatusCode::BAD_GATEWAY,
                        message: parser
                            .state
                            .error_message
                            .unwrap_or_else(|| "Upstream error event during peek".to_string()),
                    });
                }

                if parser.state.saw_output {
                    return Ok(GeminiPeekOutcome {
                        parser,
                        prebuffered_frames,
                        stream_ended: false,
                    });
                }

                if let Some(ref fr) = parser.state.finish_reason {
                    if fr != "STOP" {
                        return Ok(GeminiPeekOutcome {
                            parser,
                            prebuffered_frames,
                            stream_ended: false,
                        });
                    }
                }

                if parser.state.saw_data_event && grace_deadline.is_none() {
                    grace_deadline = Some(tokio::time::Instant::now() + thinking_grace_window);
                }
            }
            Ok(Some(Err(e))) => {
                return Err(GeminiPeekFailure {
                    status: StatusCode::BAD_GATEWAY,
                    message: format!("Stream error during peek: {}", e),
                });
            }
            Ok(None) => {
                prebuffered_frames.extend(parser.flush());
                if parser.state.saw_error {
                    return Err(GeminiPeekFailure {
                        status: StatusCode::BAD_GATEWAY,
                        message: parser
                            .state
                            .error_message
                            .unwrap_or_else(|| "Upstream error event during peek".to_string()),
                    });
                }
                if let Some(msg) = parser.has_terminal_incomplete_error() {
                    return Err(GeminiPeekFailure {
                        status: StatusCode::BAD_GATEWAY,
                        message: msg.to_string(),
                    });
                }
                return Ok(GeminiPeekOutcome {
                    parser,
                    prebuffered_frames,
                    stream_ended: true,
                });
            }
            Err(_) => {
                if !parser.state.saw_data_event {
                    return Err(GeminiPeekFailure {
                        status: StatusCode::GATEWAY_TIMEOUT,
                        message: format!(
                            "First chunk timeout after {}s",
                            first_chunk_timeout.as_secs()
                        ),
                    });
                }
                // Grace window elapsed while upstream is still streaming preliminary/thinking events:
                // release buffered frames immediately to protect client TTFB.
                return Ok(GeminiPeekOutcome {
                    parser,
                    prebuffered_frames,
                    stream_ended: false,
                });
            }
        }
    }
}

fn build_gemini_sse_relay_stream<S, E>(
    mut response_stream: S,
    peek_outcome: GeminiPeekOutcome,
    cloud_code_trace_id: Option<String>,
    idle_timeout: std::time::Duration,
    stream_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> impl futures::Stream<Item = Result<bytes::Bytes, String>> + Send + 'static
where
    S: futures::Stream<Item = Result<bytes::Bytes, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    use futures::StreamExt;
    use std::sync::atomic::Ordering;

    async_stream::stream! {
        let mut parser = peek_outcome.parser;
        let prebuffered_frames = peek_outcome.prebuffered_frames;
        let stream_ended_in_peek = peek_outcome.stream_ended;

        if let Some(tid) = &cloud_code_trace_id {
            let meta_pkg = json!({
                "__cloudCodeMeta": {
                    "traceId": tid
                }
            });
            yield Ok::<bytes::Bytes, String>(bytes::Bytes::from(format!(
                "data: {}\n\n",
                serde_json::to_string(&meta_pkg).unwrap_or_default()
            )));
        }

        for frame in prebuffered_frames {
            yield Ok::<bytes::Bytes, String>(frame);
        }

        if stream_ended_in_peek {
            return;
        }

        loop {
            match tokio::time::timeout(idle_timeout, response_stream.next()).await {
                Ok(Some(Ok(bytes))) => {
                    debug!("[Gemini-SSE] Received chunk: {} bytes", bytes.len());
                    for frame in parser.push_bytes(&bytes) {
                        yield Ok::<bytes::Bytes, String>(frame);
                    }
                }
                Ok(Some(Err(e))) => {
                    let msg = format!("[Stream Error] {}", e);
                    error!("[Gemini-SSE] Stream error: {}", e);
                    stream_failed.store(true, Ordering::Release);
                    yield Ok::<bytes::Bytes, String>(build_gemini_sse_error_bytes(
                        502,
                        "BAD_GATEWAY",
                        &msg,
                    ));
                    break;
                }
                Ok(None) => {
                    for frame in parser.flush() {
                        yield Ok::<bytes::Bytes, String>(frame);
                    }
                    if parser.state.saw_error {
                        stream_failed.store(true, Ordering::Release);
                    } else if let Some(msg) = parser.has_terminal_incomplete_error() {
                        tracing::warn!("[Gemini-SSE] {}", msg);
                        stream_failed.store(true, Ordering::Release);
                        yield Ok::<bytes::Bytes, String>(build_gemini_sse_error_bytes(
                            502,
                            "INTERNAL",
                            msg,
                        ));
                    }
                    break;
                }
                Err(_) => {
                    let msg = format!(
                        "Upstream stream idle timeout after {}s",
                        idle_timeout.as_secs()
                    );
                    error!("[Gemini-SSE] {}", msg);
                    stream_failed.store(true, Ordering::Release);
                    yield Ok::<bytes::Bytes, String>(build_gemini_sse_error_bytes(
                        504,
                        "DEADLINE_EXCEEDED",
                        &msg,
                    ));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use std::io;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[tokio::test]
    async fn test_peek_rejects_thought_only_immediate_eof_and_heartbeats() {
        // 1. Thought-only + usageMetadata + immediate EOF -> rejected before HTTP 200
        let thought_only = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"pondering\",\"thought\":true}]}}]}}\n\n",
            "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":0}}}\n\n",
        );
        let mut s1 = stream::iter(vec![Ok::<Bytes, io::Error>(Bytes::from(thought_only))]);
        let err1 = peek_gemini_stream_start(
            &mut s1,
            "s1",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(50),
        )
        .await
        .err()
        .expect("thought-only stream without finishReason must be rejected in peek");
        assert!(err1.message.contains("thinking only"));

        // 2. Heartbeat comment only + EOF -> rejected
        let mut s2 = stream::iter(vec![Ok::<Bytes, io::Error>(Bytes::from(": ping\n\n"))]);
        assert!(peek_gemini_stream_start(
            &mut s2,
            "s2",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(50),
        )
        .await
        .is_err());

        // 3. Embedded error frame during peek -> rejected
        let mut s3 = stream::iter(vec![Ok::<Bytes, io::Error>(Bytes::from(
            "data: {\"error\":{\"code\":503,\"message\":\"overloaded\"}}\n\n",
        ))]);
        let err3 = peek_gemini_stream_start(
            &mut s3,
            "s3",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(50),
        )
        .await
        .err()
        .expect("embedded error frame must reject peek");
        assert!(err3.message.contains("overloaded"));
    }

    #[tokio::test]
    async fn test_peek_releases_slow_thinking_after_grace_window_without_blocking_ttfb() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(4);
        tx.send(Ok(Bytes::from(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"step 1\",\"thought\":true}]}}]}}\n\n",
        )))
        .await
        .unwrap();

        let mut s = tokio_stream::wrappers::ReceiverStream::new(rx);
        let outcome = peek_gemini_stream_start(
            &mut s,
            "s-slow-think",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(25),
        )
        .await
        .expect("slow thinking should proceed cleanly after grace window");
        assert!(!outcome.stream_ended);
        assert_eq!(outcome.prebuffered_frames.len(), 1);
    }

    #[tokio::test]
    async fn test_relay_emits_gemini_error_json_never_openai_chunk_on_mid_stream_failures() {
        // 1. Mid-stream IO error -> native Gemini error JSON, no OpenAI choices/chat.completion.chunk, no [DONE]
        let s_err = stream::iter(vec![
            Ok::<Bytes, io::Error>(Bytes::from(
                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n\n",
            )),
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "tcp reset")),
        ]);
        let mut s_err_box = Box::pin(s_err);
        let outcome = peek_gemini_stream_start(
            &mut s_err_box,
            "s-err",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(50),
        )
        .await
        .ok()
        .expect("initial text chunk passes peek");

        let failed_flag = Arc::new(AtomicBool::new(false));
        let relay = build_gemini_sse_relay_stream(
            s_err_box,
            outcome,
            None,
            Duration::from_secs(300),
            failed_flag.clone(),
        );
        let chunks: Vec<String> = relay
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await;
        let joined = chunks.join("");
        assert!(failed_flag.load(Ordering::Acquire));
        assert!(!joined.contains("chat.completion.chunk"));
        assert!(!joined.contains("\"choices\""));
        assert!(!joined.contains("data: [DONE]"));
        assert!(joined.contains("\"error\""));
        assert!(joined.contains("tcp reset"));
        assert!(joined.contains("BAD_GATEWAY"));

        // 2. Live thinking stream released after grace window that later terminates with usageMetadata & NO finishReason
        //    + trailing chunk lacking trailing '\n' must still be processed cleanly.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(4);
        tx.send(Ok(Bytes::from(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"deep thought\",\"thought\":true}]}}]}}\n\n",
        )))
        .await
        .unwrap();
        let mut rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let outcome2 = peek_gemini_stream_start(
            &mut rx_stream,
            "s-trunc",
            "gemini-3.8-flash-high",
            Duration::from_secs(300),
            Duration::from_millis(20),
        )
        .await
        .ok()
        .unwrap();
        // Upstream sends usage without trailing newline and closes
        tx.send(Ok(Bytes::from(
            "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":0}}}",
        )))
        .await
        .unwrap();
        drop(tx);

        let failed2 = Arc::new(AtomicBool::new(false));
        let relay2 = build_gemini_sse_relay_stream(
            rx_stream,
            outcome2,
            None,
            Duration::from_secs(300),
            failed2.clone(),
        );
        let chunks2: Vec<String> = relay2
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await;
        let joined2 = chunks2.join("");
        assert!(failed2.load(Ordering::Acquire));
        // Trailing usage line without '\n' was flushed, AND terminal incomplete Gemini error emitted
        assert!(joined2.contains("\"usageMetadata\""));
        assert!(joined2.contains("\"error\""));
        assert!(joined2.contains("thinking only and no output or finishReason"));
    }
}
