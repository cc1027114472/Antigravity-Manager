// Gemini Stream Collector
// Used for auto-converting streaming responses to JSON for non-streaming requests

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use tracing::debug;

use crate::proxy::mappers::Utf8ChunkBuffer;
use crate::proxy::SignatureCache; // Assuming this is available at crate root or re-exported

/// Collects a Gemini SSE stream into a complete Gemini Response Value
/// ALSO performs signature caching side-effect
pub async fn collect_stream_to_json<S, E>(mut stream: S, session_id: &str) -> Result<Value, String>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut collected_response = json!({
        "candidates": [
            {
                "content": {
                    "parts": [],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }
        ]
    });

    let mut content_parts: Vec<Value> = Vec::new(); // To accumulate parts
    let mut usage_metadata: Option<Value> = None;
    let mut finish_reason: Option<String> = None;

    // 方案3：引入 Utf8ChunkBuffer 防止 TCP 切片截断多字节 UTF-8 字符（如中文、Emoji、思维链签名）
    let mut utf8_buf = Utf8ChunkBuffer::new();
    let mut line_buffer = String::new();

    let mut process_line = |line: &str,
                            content_parts: &mut Vec<Value>,
                            usage_metadata: &mut Option<Value>,
                            finish_reason: &mut Option<String>| {
        let line = line.trim();
        if line.starts_with("data: ") {
            let json_part = line.trim_start_matches("data: ").trim();
            if json_part == "[DONE]" {
                return;
            }

            if let Ok(mut json) = serde_json::from_str::<Value>(json_part) {
                // Unwrap v1internal response wrapper similar to handler
                let actual_data =
                    if let Some(inner) = json.get_mut("response").map(|v| v.take()) {
                        inner
                    } else {
                        json
                    };

                // 1. Capture Usage
                if let Some(usage) = actual_data.get("usageMetadata") {
                    *usage_metadata = Some(usage.clone());
                }

                // 2. Capture Content & Signature
                if let Some(candidates) =
                    actual_data.get("candidates").and_then(|c| c.as_array())
                {
                    if let Some(candidate) = candidates.first() {
                        // Update finish reason if present
                        if let Some(fr) = candidate.get("finishReason").and_then(|v| v.as_str())
                        {
                            *finish_reason = Some(fr.to_string());
                        }

                        if let Some(parts) = candidate
                            .get("content")
                            .and_then(|c| c.get("parts"))
                            .and_then(|p| p.as_array())
                        {
                            for part in parts {
                                // Signature Caching
                                if let Some(sig) =
                                    part.get("thoughtSignature").and_then(|s| s.as_str())
                                {
                                    // Cache it!
                                    SignatureCache::global().cache_session_signature(
                                        session_id,
                                        sig.to_string(),
                                        1,
                                    );
                                    debug!("[Gemini-AutoConverter] Cached signature (len: {}) for session: {}", sig.len(), session_id);
                                }

                                // Collect part
                                // Simple aggregation: if text, append to last text part? Or just push all parts?
                                // Gemini stream sends separate parts. We can just accumulate them.
                                // Optimization: Merge adjacent text parts.

                                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                    if let Some(last) = content_parts.last_mut() {
                                        if last.get("text").is_some()
                                            && part.get("thought").is_none()
                                            && last.get("thought").is_none()
                                        {
                                            // Merge text
                                            if let Some(last_text) =
                                                last.get_mut("text").and_then(|v| v.as_str())
                                            {
                                                let new_text = format!("{}{}", last_text, text);
                                                *last = json!({"text": new_text});
                                                continue;
                                            }
                                        }
                                    }
                                    content_parts.push(part.clone());
                                } else {
                                    // Other parts (images, thoughts, function calls), just push
                                    content_parts.push(part.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| format!("Stream error: {}", e))?;
        let text_chunk = utf8_buf.push(&chunk);
        line_buffer.push_str(&text_chunk);

        while let Some(pos) = line_buffer.find('\n') {
            let line: String = line_buffer.drain(..=pos).collect();
            process_line(&line, &mut content_parts, &mut usage_metadata, &mut finish_reason);
        }
    }

    // 遇到 EOF (流结束) 时做最后的 flush_lossy 保证下游绝不挂起
    if let Some(lossy_tail) = utf8_buf.flush_lossy() {
        line_buffer.push_str(&lossy_tail);
    }
    if !line_buffer.is_empty() {
        process_line(&line_buffer, &mut content_parts, &mut usage_metadata, &mut finish_reason);
        line_buffer.clear();
    }

    // Construct final response
    collected_response["candidates"][0]["content"]["parts"] = json!(content_parts);
    if let Some(fr) = finish_reason {
        collected_response["candidates"][0]["finishReason"] = json!(fr);
    }
    if let Some(usage) = usage_metadata {
        collected_response["usageMetadata"] = usage;
    }

    Ok(collected_response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::io;

    #[tokio::test]
    async fn test_gemini_collector_utf8_fragmentation() {
        // 测试中文字符与思维链签名跨 TCP 切片截断时的恢复
        let raw_sse = r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"你好世界，思维链正常！","thoughtSignature":"sig_abcdef123456"}]}}]}}

"#;
        let raw_bytes = raw_sse.as_bytes();

        // 模拟在 "思维链" 中间以及签名中间发生 TCP Packet 切片
        let mid = raw_sse.find("思维链").unwrap() + 4; // 切断在汉字多字节内部
        let chunk1 = &raw_bytes[..mid];
        let chunk2 = &raw_bytes[mid..];

        let byte_stream = stream::iter(vec![
            Ok::<Bytes, io::Error>(Bytes::copy_from_slice(chunk1)),
            Ok::<Bytes, io::Error>(Bytes::copy_from_slice(chunk2)),
        ]);

        let result = collect_stream_to_json(byte_stream, "test_session").await;
        assert!(result.is_ok());
        let res = result.unwrap();
        let text = res["candidates"][0]["content"]["parts"][0]["text"].as_str().unwrap();
        assert_eq!(text, "你好世界，思维链正常！");
    }
}
