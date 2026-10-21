# Lossless Streaming Token Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Fix streaming token extraction in `monitor.rs` and OpenAI/Gemini streaming mappers so that Claude `message_start` input/cached tokens, `message_delta` output tokens, and trailing Gemini standalone `usageMetadata` chunks are 100% captured without loss or overwrite.

**Architecture:** 
1. In `monitor.rs`, support Anthropic SSE `message_start` (`json["message"]["usage"]`) and `message_delta` (`json["usage"]`), and make the stream token accumulator monotonic (preventing later chunks with partial usage from wiping out previously recorded input/cache tokens with `None`).
2. In `openai/streaming.rs`, ensure standalone Gemini trailing chunks with `usageMetadata` (and empty/missing `candidates`) emit a final usage chunk (`choices: [], usage: ...`) before `data: [DONE]\n\n`.
3. In `monitor.rs` tail scanner and JSON handlers, broaden usage extraction paths to include `message.usage` and `delta.usage`.

**Tech Stack:** Rust, Axum, Tokio, Serde JSON, SSE (Server-Sent Events)

---

### Task 1: Add Unit Tests for Streaming Token Extraction in `monitor.rs`

**Files:**
- Modify: `src-tauri/src/proxy/middleware/monitor.rs`

- [x] **Step 1: Write failing unit tests for `message_start` and non-overwriting accumulation**

Add a `mod tests` block at the bottom of `src-tauri/src/proxy/middleware/monitor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_claude_message_start_and_delta() {
        // Step 1: Simulate message_start chunk
        let start_chunk = json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "usage": {
                    "input_tokens": 1500,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 500,
                    "output_tokens": 1
                }
            }
        });

        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut cached_tokens = None;

        let usage_start = start_chunk
            .get("usage")
            .or(start_chunk.get("usageMetadata"))
            .or(start_chunk.get("message").and_then(|m| m.get("usage")))
            .or(start_chunk.get("delta").and_then(|d| d.get("usage")));

        assert!(usage_start.is_some());
        if let Some(u) = usage_start {
            if let Some(tok) = extract_input_tokens(u) { input_tokens = Some(tok); }
            if let Some(tok) = extract_output_tokens(u) { output_tokens = Some(tok); }
            if let Some(tok) = extract_cached_tokens(u) { cached_tokens = Some(tok); }
        }

        assert_eq!(input_tokens, Some(1500));
        assert_eq!(cached_tokens, Some(500));

        // Step 2: Simulate message_delta chunk arriving at stream end with only output_tokens
        let delta_chunk = json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "stop_sequence": null
            },
            "usage": {
                "output_tokens": 350
            }
        });

        let usage_delta = delta_chunk
            .get("usage")
            .or(delta_chunk.get("usageMetadata"))
            .or(delta_chunk.get("message").and_then(|m| m.get("usage")))
            .or(delta_chunk.get("delta").and_then(|d| d.get("usage")));

        assert!(usage_delta.is_some());
        if let Some(u) = usage_delta {
            // Crucial: do not overwrite input_tokens with None!
            if let Some(tok) = extract_input_tokens(u) { input_tokens = Some(tok); }
            if let Some(tok) = extract_output_tokens(u) { output_tokens = Some(tok); }
            if let Some(tok) = extract_cached_tokens(u) { cached_tokens = Some(tok); }
        }

        assert_eq!(input_tokens, Some(1500), "input_tokens must not be wiped by message_delta");
        assert_eq!(cached_tokens, Some(500), "cached_tokens must not be wiped by message_delta");
        assert_eq!(output_tokens, Some(350), "output_tokens must be updated to 350");
    }
}
```

- [x] **Step 2: Run `cargo check` to verify test syntax compiles**

Run:
```powershell
$env:CMAKE_GENERATOR="MinGW Makefiles"; $env:BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I C:/Users/Administrator/scoop/apps/mingw-winlibs/current/x86_64-w64-mingw32/include"; cargo check
```
Expected: PASS

---

### Task 2: Fix Token Extraction and Accumulation in `monitor.rs`

**Files:**
- Modify: `src-tauri/src/proxy/middleware/monitor.rs:645-810`
- Modify: `src-tauri/src/proxy/middleware/monitor.rs:910-975`

- [x] **Step 1: Update `match msg_type` to handle `message_start` and enhance `message_delta`**

In `src-tauri/src/proxy/middleware/monitor.rs`, add `Some("message_start")` branch:
```rust
Some("message_start") => {
    if let Some(msg) = json.get("message") {
        if let Some(usage) = msg.get("usage") {
            if let Some(in_tok) = extract_input_tokens(usage) {
                log.input_tokens = Some(in_tok);
            }
            if let Some(out_tok) = extract_output_tokens(usage) {
                log.output_tokens = Some(out_tok);
            }
            if let Some(c_tok) = extract_cached_tokens(usage) {
                cached_tokens = Some(c_tok);
                log.cached_tokens = Some(c_tok);
            }
        }
    }
}
```
And in `Some("message_delta")`:
```rust
Some("message_delta") => {
    let usage = json.get("usage").or_else(|| json.get("delta").and_then(|d| d.get("usage")));
    if let Some(usage) = usage {
        if let Some(out_tok) = extract_output_tokens(usage) {
            log.output_tokens = Some(out_tok);
        }
    }
}
```

- [x] **Step 2: Fix the generic SSE token extraction loop to prevent overwriting with `None`**

Replace lines 781-802 with:
```rust
                        // Token usage extraction (OpenAI, Gemini, Claude, Responses API)
                        if let Some(usage) = json
                            .get("usage")
                            .or(json.get("usageMetadata"))
                            .or(json.get("message").and_then(|m| m.get("usage")))
                            .or(json.get("delta").and_then(|d| d.get("usage")))
                            .or(json.get("response").and_then(|r| r.get("usage")))
                            .or(json.get("response").and_then(|r| r.get("usageMetadata")))
                        {
                            if let Some(tok) = extract_input_tokens(usage) {
                                log.input_tokens = Some(tok);
                            }
                            if let Some(tok) = extract_output_tokens(usage) {
                                log.output_tokens = Some(tok);
                            }
                            if let Some(tok) = extract_cached_tokens(usage) {
                                cached_tokens = Some(tok);
                                log.cached_tokens = Some(tok);
                            }
                            if let Some(tok) = extract_reasoning_tokens(usage) {
                                reasoning_tokens = Some(tok);
                            }

                            if log.input_tokens.is_none() && log.output_tokens.is_none() {
                                log.output_tokens = usage
                                    .get("total_tokens")
                                    .or(usage.get("totalTokenCount"))
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32);
                            }
                        }
```

- [x] **Step 3: Fix trailing tail scanner and non-streaming response extraction**

In tail scanner (around line 920):
```rust
                                if let Some(usage) = json
                                    .get("usage")
                                    .or(json.get("usageMetadata"))
                                    .or(json.get("message").and_then(|m| m.get("usage")))
                                    .or(json.get("delta").and_then(|d| d.get("usage")))
                                    .or(json.get("response").and_then(|r| r.get("usage")))
                                    .or(json.get("response").and_then(|r| r.get("usageMetadata")))
                                {
                                    if let Some(tok) = extract_input_tokens(usage) {
                                        log.input_tokens = Some(tok);
                                    }
                                    if let Some(tok) = extract_output_tokens(usage) {
                                        log.output_tokens = Some(tok);
                                    }
                                    if let Some(tok) = extract_cached_tokens(usage) {
                                        log.cached_tokens = log.cached_tokens.or(Some(tok));
                                    }
                                    break;
                                }
```
And in non-streaming response handler (around line 958):
Add `.or(json.get("message").and_then(|m| m.get("usage")))` and do not wipe existing values.

- [x] **Step 4: Run `cargo check` to verify**

Run:
```powershell
$env:CMAKE_GENERATOR="MinGW Makefiles"; $env:BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I C:/Users/Administrator/scoop/apps/mingw-winlibs/current/x86_64-w64-mingw32/include"; cargo check
```
Expected: PASS

---

### Task 3: Fix Trailing Standalone `usageMetadata` Chunk in `openai/streaming.rs`

**Files:**
- Modify: `src-tauri/src/proxy/mappers/openai/streaming.rs:145-370`
- Modify: `src-tauri/src/proxy/mappers/openai/streaming.rs:415-450`

- [x] **Step 1: Ensure trailing `usageMetadata` without candidates is emitted in `create_openai_sse_stream`**

In `create_openai_sse_stream`, after loop / before `data: [DONE]\n\n`:
If `final_usage.is_some()`, yield an OpenAI SSE chunk with `choices: []` containing `"usage"`:
```rust
        if !error_occurred {
            if let Some(usage) = final_usage.take() {
                let usage_chunk = json!({
                    "id": &stream_id,
                    "object": "chat.completion.chunk",
                    "created": created_ts,
                    "model": &model,
                    "choices": [],
                    "usage": serde_json::to_value(&usage).unwrap_or_default()
                });
                yield Ok::<Bytes, String>(Bytes::from(format!("data: {}\n\n", serde_json::to_string(&usage_chunk).unwrap_or_default())));
            }
            yield Ok::<Bytes, String>(Bytes::from("data: [DONE]\n\n"));
        }
```

- [x] **Step 2: Ensure trailing `usageMetadata` is emitted in `create_legacy_sse_stream`**

In `create_legacy_sse_stream`, before `data: [DONE]\n\n`:
```rust
        if !error_occurred {
            if let Some(usage) = final_usage.take() {
                let legacy_chunk = json!({
                    "id": &stream_id,
                    "object": "text_completion",
                    "created": created_ts,
                    "model": &model,
                    "choices": [],
                    "usage": serde_json::to_value(&usage).unwrap_or_default()
                });
                yield Ok::<Bytes, String>(Bytes::from(format!("data: {}\n\n", serde_json::to_string(&legacy_chunk).unwrap_or_default())));
            }
            yield Ok::<Bytes, String>(Bytes::from("data: [DONE]\n\n"));
        }
```

- [x] **Step 3: Run `cargo check` to verify**

Run:
```powershell
$env:CMAKE_GENERATOR="MinGW Makefiles"; $env:BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I C:/Users/Administrator/scoop/apps/mingw-winlibs/current/x86_64-w64-mingw32/include"; cargo check
```
Expected: PASS

---

### Task 4: Add Unit Test for Standalone Trailing `usageMetadata` Chunk

**Files:**
- Modify: `src-tauri/src/proxy/mappers/openai/streaming.rs`

- [x] **Step 1: Add unit test `test_openai_streaming_standalone_usage_chunk`**

In `src-tauri/src/proxy/mappers/openai/streaming.rs` under `mod tests`:
```rust
    #[tokio::test]
    async fn test_openai_streaming_standalone_usage_chunk() {
        // Chunk 1: Content with finish reason STOP but NO usage
        let chunk1_json = json!({
            "candidates": [{
                "finishReason": "STOP",
                "content": {
                    "parts": [{ "text": "Hello world" }]
                }
            }]
        });

        // Chunk 2: Standalone usageMetadata with NO candidates
        let chunk2_json = json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });

        let items: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from(format!("data: {}\n\n", chunk1_json))),
            Ok(Bytes::from(format!("data: {}\n\n", chunk2_json))),
        ];

        let gemini_stream = Box::pin(futures_util::stream::iter(items));

        let mut openai_stream = create_openai_sse_stream(
            gemini_stream,
            "gemini-2.5-flash".to_string(),
            "test-session-standalone".to_string(),
            0,
            None,
        );

        let mut chunks = Vec::new();
        while let Some(result) = openai_stream.next().await {
            if let Ok(bytes) = result {
                let s = String::from_utf8_lossy(&bytes).to_string();
                for line in s.lines() {
                    if line.starts_with("data: ") && !line.contains("[DONE]") {
                        chunks.push(line.to_string());
                    }
                }
            }
        }

        let mut found_usage = false;
        for c in &chunks {
            let json_str = c.trim_start_matches("data: ");
            if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                if let Some(usage) = json.get("usage") {
                    found_usage = true;
                    assert_eq!(usage["prompt_tokens"], 10);
                    assert_eq!(usage["completion_tokens"], 5);
                    assert_eq!(usage["total_tokens"], 15);
                }
            }
        }

        assert!(found_usage, "Standalone trailing usage must be emitted before [DONE]");
    }
```

- [x] **Step 2: Run `cargo check` to verify**

Run:
```powershell
$env:CMAKE_GENERATOR="MinGW Makefiles"; $env:BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -I C:/Users/Administrator/scoop/apps/mingw-winlibs/current/x86_64-w64-mingw32/include"; cargo check
```
Expected: PASS
