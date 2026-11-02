// OpenAI Stream Collector
// Used for auto-converting streaming responses to JSON for non-streaming requests

use super::models::*;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;

/// Collects an OpenAI SSE stream into a complete OpenAIResponse
pub async fn collect_stream_to_json<S, E>(mut stream: S) -> Result<OpenAIResponse, String>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut response = OpenAIResponse {
        id: "chatcmpl-unknown".to_string(),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: "unknown".to_string(),
        choices: Vec::new(),
        usage: None,
    };

    let mut role: Option<String> = None;
    let mut content_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut finish_reason: Option<String> = None;
    // Tool calls aggregation: index -> (id, type, name, arguments_parts)
    let mut tool_calls_map: HashMap<u32, (String, String, String, Vec<String>)> = HashMap::new();

    // 方案3：引入 Utf8ChunkBuffer 防止 TCP 切片截断多字节 UTF-8 字符（如中文、Emoji、思维链签名）
    let mut utf8_buf = crate::proxy::mappers::Utf8ChunkBuffer::new();
    let mut line_buffer = String::new();

    let mut process_line = |line: &str,
                            response: &mut OpenAIResponse,
                            role: &mut Option<String>,
                            content_parts: &mut Vec<String>,
                            reasoning_parts: &mut Vec<String>,
                            finish_reason: &mut Option<String>,
                            tool_calls_map: &mut HashMap<u32, (String, String, String, Vec<String>)>| {
        let line = line.trim();
        if line.starts_with("data: ") {
            let data_str = line.trim_start_matches("data: ").trim();
            if data_str == "[DONE]" {
                return;
            }

            if let Ok(json) = serde_json::from_str::<Value>(data_str) {
                // Update meta fields
                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                    response.id = id.to_string();
                }
                if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                    response.model = model.to_string();
                }
                if let Some(created) = json.get("created").and_then(|v| v.as_u64()) {
                    response.created = created;
                }

                // Collect Usage
                if let Some(usage) = json.get("usage") {
                    if let Ok(u) = serde_json::from_value::<OpenAIUsage>(usage.clone()) {
                        response.usage = Some(u);
                    }
                }

                // Collect Choices Delta
                if let Some(choices) = json.get("choices").and_then(|v| v.as_array()) {
                    if let Some(choice) = choices.first() {
                        if let Some(delta) = choice.get("delta") {
                            // Role
                            if let Some(r) = delta.get("role").and_then(|v| v.as_str()) {
                                *role = Some(r.to_string());
                            }

                            // Content
                            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                                content_parts.push(c.to_string());
                            }

                            // Reasoning Content
                            if let Some(rc) =
                                delta.get("reasoning_content").and_then(|v| v.as_str())
                            {
                                reasoning_parts.push(rc.to_string());
                            }

                            // Tool Calls
                            if let Some(tool_calls) =
                                delta.get("tool_calls").and_then(|v| v.as_array())
                            {
                                for tc in tool_calls {
                                    let raw_index = tc
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as u32;

                                    // 智能推断工具索引，防止多个工具混在 index: 0
                                    let index = if raw_index == 0 && !tool_calls_map.is_empty() {
                                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                            if !id.is_empty() {
                                                let mut found = None;
                                                for (&existing_idx, entry) in tool_calls_map.iter() {
                                                    if entry.0 == id {
                                                        found = Some(existing_idx);
                                                        break;
                                                    }
                                                }
                                                found.unwrap_or_else(|| {
                                                    tool_calls_map.keys().max().map(|&m| m + 1).unwrap_or(0)
                                                })
                                            } else {
                                                raw_index
                                            }
                                        } else {
                                            raw_index
                                        }
                                    } else {
                                        raw_index
                                    };

                                    let entry =
                                        tool_calls_map.entry(index).or_insert_with(|| {
                                            (
                                                String::new(),
                                                String::from("function"),
                                                String::new(),
                                                Vec::new(),
                                            )
                                        });

                                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                        if !id.is_empty() {
                                            entry.0 = id.to_string();
                                        }
                                    }

                                    if let Some(tc_type) =
                                        tc.get("type").and_then(|v| v.as_str())
                                    {
                                        if !tc_type.is_empty() {
                                            entry.1 = tc_type.to_string();
                                        }
                                    }

                                    if let Some(func) = tc.get("function") {
                                        if let Some(name) =
                                            func.get("name").and_then(|v| v.as_str())
                                        {
                                            if !name.is_empty() {
                                                entry.2 = name.to_string();
                                            }
                                        }
                                        if let Some(args) =
                                            func.get("arguments").and_then(|v| v.as_str())
                                        {
                                            entry.3.push(args.to_string());
                                        }
                                    }
                                }
                            }
                        }

                        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                            *finish_reason = Some(fr.to_string());
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
            process_line(
                &line,
                &mut response,
                &mut role,
                &mut content_parts,
                &mut reasoning_parts,
                &mut finish_reason,
                &mut tool_calls_map,
            );
        }
    }

    // 遇到 EOF (流结束) 时做最后的 flush_lossy 保证下游绝不挂起
    if let Some(lossy_tail) = utf8_buf.flush_lossy() {
        line_buffer.push_str(&lossy_tail);
    }
    if !line_buffer.is_empty() {
        process_line(
            &line_buffer,
            &mut response,
            &mut role,
            &mut content_parts,
            &mut reasoning_parts,
            &mut finish_reason,
            &mut tool_calls_map,
        );
        line_buffer.clear();
    }

    // Construct final message
    let full_content = content_parts.join("");
    let full_reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join(""))
    };

    // Build aggregated tool_calls
    let final_tool_calls: Option<Vec<ToolCall>> = if tool_calls_map.is_empty() {
        None
    } else {
        let mut calls: Vec<(u32, ToolCall)> = tool_calls_map
            .into_iter()
            .map(|(index, (id, tc_type, name, args_parts))| {
                (
                    index,
                    ToolCall {
                        id,
                        r#type: tc_type,
                        function: Some(ToolFunction {
                            name,
                            arguments: args_parts.join(""),
                        }),
                        status: None,
                        call_id: None,
                        operation: None,
                    },
                )
            })
            .collect();
        calls.sort_by_key(|(index, _)| *index);
        Some(calls.into_iter().map(|(_, tc)| tc).collect())
    };

    let message = OpenAIMessage {
        role: role.unwrap_or("assistant".to_string()),
        content: Some(OpenAIContent::String(full_content)),
        reasoning_content: full_reasoning,
        tool_calls: final_tool_calls,
        tool_call_id: None,
        name: None,
        refusal: None,
    };

    response.choices.push(Choice {
        index: 0,
        message,
        finish_reason: finish_reason.or(Some("stop".to_string())),
    });

    Ok(response)
}


#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::io;

    #[tokio::test]
    async fn test_openai_collector_utf8_fragmentation() {
        let raw_sse = "data: {\"id\":\"chatcmpl-123\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"你好世界，测试中文切片！\",\"reasoning_content\":\"深度思考中...\"}}]}\n\ndata: [DONE]\n";
        let raw_bytes = raw_sse.as_bytes();

        // 模拟在中文 "测试" 内部切断
        let split_pos = raw_sse.find("测试").unwrap() + 2;
        let chunk1 = &raw_bytes[..split_pos];
        let chunk2 = &raw_bytes[split_pos..];

        let byte_stream = stream::iter(vec![
            Ok::<Bytes, io::Error>(Bytes::copy_from_slice(chunk1)),
            Ok::<Bytes, io::Error>(Bytes::copy_from_slice(chunk2)),
        ]);

        let result = collect_stream_to_json(byte_stream).await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.id, "chatcmpl-123");
        let choice = resp.choices.first().unwrap();
        assert_eq!(
            choice.message.content,
            Some(crate::proxy::mappers::openai::models::OpenAIContent::String("你好世界，测试中文切片！".to_string()))
        );
        assert_eq!(choice.message.reasoning_content.as_deref(), Some("深度思考中..."));
    }
}
