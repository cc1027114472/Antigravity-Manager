use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use tracing::{debug, info};

use crate::proxy::{audio::AudioProcessor, server::AppState};

/// 处理音频转录请求 (OpenAI Whisper API 兼容)
pub async fn handle_audio_transcription(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut audio_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut model = "gemini-2.5-flash".to_string();
    let mut prompt = "Generate a transcript of the speech.".to_string();

    // 1. 解析 multipart/form-data
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("解析表单失败: {}", e)))?
    {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                audio_data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取文件失败: {}", e)))?
                        .to_vec(),
                );
            }
            "model" => {
                model = field.text().await.unwrap_or(model);
            }
            "prompt" => {
                prompt = field.text().await.unwrap_or(prompt);
            }
            _ => {}
        }
    }

    let audio_bytes = audio_data.ok_or((StatusCode::BAD_REQUEST, "缺少音频文件".to_string()))?;

    let file_name = filename.ok_or((StatusCode::BAD_REQUEST, "无法获取文件名".to_string()))?;

    info!(
        "收到音频转录请求: 文件={}, 大小={} bytes, 模型={}",
        file_name,
        audio_bytes.len(),
        model
    );

    // 2. 检测 MIME 类型
    let mime_type =
        AudioProcessor::detect_mime_type(&file_name).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // 3. 验证文件大小
    if AudioProcessor::exceeds_size_limit(audio_bytes.len()) {
        let size_mb = audio_bytes.len() as f64 / (1024.0 * 1024.0);
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "音频文件过大 ({:.1} MB)。最大支持 15 MB (约 16 分钟 MP3)。建议: 1) 压缩音频质量 2) 分段上传",
                size_mb
            ),
        ));
    }

    // 4. 使用 Inline Data 方式
    debug!("使用 Inline Data 方式处理");
    let base64_audio = AudioProcessor::encode_to_base64(&audio_bytes);

    // 5. 获取 Token 和上游客户端
    let token_manager = state.token_manager;
    let (access_token, project_id, email, account_id, _wait_ms) = token_manager
        .get_token("text", false, None, &model)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;

    info!("使用账号: {}", email);

    // 6. 构建 Gemini 请求 (对齐官方 contents[].role + labels + sessionId + requestType)
    let (request_id, official_labels) =
        crate::proxy::common::session::generate_official_request_id_and_labels(
            Some(account_id.as_str()),
            "audio",
            1,
            &model,
        );
    let session_id = crate::proxy::common::session::derive_session_id(account_id.as_str());
    let gemini_request = json!({
        "contents": [{
            "role": "user",
            "parts": [
                {"text": prompt},
                {
                    "inlineData": {
                        "mimeType": mime_type,
                        "data": base64_audio
                    }
                }
            ]
        }],
        "labels": official_labels,
        "sessionId": session_id,
    });

    // 7. 包装请求为标准官方 v1internal 信封（含官方字段顺序与 fallback tools）
    let wrapped_body = crate::proxy::mappers::common_utils::build_v1internal_envelope(
        &project_id,
        &request_id,
        gemini_request,
        &model,
        "antigravity",
        "agent",
    );

    // 8. 发送请求到 Gemini（对齐官方抓包：requestType="agent" 一律走 streamGenerateContent?alt=sse）
    let upstream = state.upstream.clone();
    let response = upstream
        .call_v1_internal(
            "streamGenerateContent",
            &access_token,
            wrapped_body,
            Some("alt=sse"),
            Some(account_id.as_str()),
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("上游请求失败: {}", e)))?
        .response;

    if !response.status().is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Gemini API 错误: {}", error_text),
        ));
    }

    let raw_body = response
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("读取响应失败: {}", e)))?;

    // 9. 聚合 SSE 非思考正文响应
    let text = crate::proxy::mappers::common_utils::extract_v1internal_sse_text(&raw_body)
        .unwrap_or_default();

    info!("音频转录完成，返回 {} 字符", text.len());

    // 10. 返回标准格式响应
    Ok((
        StatusCode::OK,
        [("X-Account-Email", email.as_str())],
        Json(json!({
            "text": text
        })),
    )
        .into_response())
}
