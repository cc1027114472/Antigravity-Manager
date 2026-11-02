// UTF-8 Chunk Buffer for SSE Streaming
// Solves TCP fragmentation where multi-byte UTF-8 characters (e.g. Chinese 3-byte, Emoji 4-byte)
// or SSE lines are truncated across chunk boundaries.

use bytes::{Bytes, BytesMut};

/// 紧凑的 UTF-8 残缺字节缓冲区 (方案3)
/// 专门解决 TCP 切片导致的多字节 UTF-8 字符（特别是中文字符 3 字节、Emoji 4 字节）被截断在两个 TCP Packet 的问题
#[derive(Debug, Default, Clone)]
pub struct Utf8ChunkBuffer {
    /// 存储待拼接的原始字节，末尾可包含 1~3 个未闭合的残缺多字节 UTF-8 序列
    buffer: BytesMut,
}

impl Utf8ChunkBuffer {
    /// 创建新的 Utf8ChunkBuffer
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
        }
    }

    /// 接收新的字节切片并追加到 buffer 中，
    /// 使用 std::str::from_utf8 配合 e.valid_up_to() 提取出当前最大连续合法 UTF-8 字符串；
    /// 未闭合的 1~3 个残缺字节留在 buffer 中等待下一个 chunk
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        while !self.buffer.is_empty() {
            match std::str::from_utf8(&self.buffer) {
                Ok(_) => {
                    let bytes = self.buffer.split().freeze();
                    // SAFETY: 已通过 std::str::from_utf8 验证整段字节合法
                    let s = unsafe { String::from_utf8_unchecked(bytes.to_vec()) };
                    output.push_str(&s);
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        let valid_bytes = self.buffer.split_to(valid_up_to);
                        // SAFETY: valid_up_to 范围内的字节已经由 std::str::from_utf8 验证通过
                        let s = unsafe { String::from_utf8_unchecked(valid_bytes.to_vec()) };
                        output.push_str(&s);
                    }
                    if let Some(error_len) = e.error_len() {
                        // 遇到明确的非法字节序列（非末尾残缺），消费并替换为 replacement 字符，避免死循环
                        let _ = self.buffer.split_to(error_len);
                        output.push(std::char::REPLACEMENT_CHARACTER);
                    } else {
                        // error_len 为 None：说明末尾是未闭合的 1~3 个残缺字节，留在 buffer 中等待下一个 chunk
                        break;
                    }
                }
            }
        }
        output
    }

    /// 遇到 EOF (流结束) 时做最后的 flush_lossy 保证下游绝不挂起
    pub fn flush_lossy(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            None
        } else {
            let remaining = self.buffer.split().freeze();
            Some(String::from_utf8_lossy(&remaining).into_owned())
        }
    }

    /// 判断内部缓冲区是否为空
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 返回当前内部缓冲区残留的残缺字节数
    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}

/// SSE UTF-8 chunk buffer with support for incomplete multi-byte sequences.
#[derive(Debug, Default, Clone)]
pub struct Utf8SseChunkBuffer {
    /// Holds incomplete trailing UTF-8 bytes (1~3 bytes) waiting for subsequent TCP chunks
    pending_bytes: BytesMut,
    /// Holds decoded valid UTF-8 text waiting for newline delimiter '\n'
    text_buffer: String,
}

impl Utf8SseChunkBuffer {
    /// Creates a new, empty `Utf8SseChunkBuffer`.
    pub fn new() -> Self {
        Self {
            pending_bytes: BytesMut::new(),
            text_buffer: String::new(),
        }
    }

    /// Appends incoming TCP chunk bytes, inspects valid UTF-8 prefix using `valid_up_to()`,
    /// keeps 1~3 incomplete trailing bytes in `pending_bytes`, and extracts complete SSE lines.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending_bytes.extend_from_slice(chunk);
        self.decode_pending_to_text();
        self.extract_lines()
    }

    /// Decodes valid UTF-8 prefix from `pending_bytes` into `text_buffer`.
    /// Incomplete multi-byte sequence (1~3 bytes) remains in `pending_bytes` for the next packet.
    fn decode_pending_to_text(&mut self) {
        while !self.pending_bytes.is_empty() {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(valid_str) => {
                    self.text_buffer.push_str(valid_str);
                    self.pending_bytes.clear();
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        let valid_slice = self.pending_bytes.split_to(valid_up_to);
                        if let Ok(valid_str) = std::str::from_utf8(&valid_slice) {
                            self.text_buffer.push_str(valid_str);
                        }
                    }

                    if let Some(error_len) = e.error_len() {
                        // Truly invalid byte sequence encountered (not truncation at buffer end).
                        // Discard/replace invalid bytes to avoid infinite loop.
                        let _ = self.pending_bytes.split_to(error_len);
                        self.text_buffer.push(std::char::REPLACEMENT_CHARACTER);
                    } else {
                        // error_len is None: truncated UTF-8 character at end of buffer.
                        // Leave the incomplete 1~3 bytes in pending_bytes for the next TCP chunk.
                        break;
                    }
                }
            }
        }
    }

    /// Extracts all complete lines delimited by '\n' from `text_buffer`.
    /// Strips trailing '\r' if present.
    fn extract_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(pos) = self.text_buffer.find('\n') {
            let mut line: String = self.text_buffer.drain(..=pos).collect();
            // Pop trailing '\n'
            if line.ends_with('\n') {
                line.pop();
            }
            // Pop trailing '\r' (CRLF)
            if line.ends_with('\r') {
                line.pop();
            }
            lines.push(line);
        }
        lines
    }

    /// Flushes any remaining bytes and text upon stream EOF.
    /// Guarantees that no trailing line or bytes are dropped or cause downstream hangs.
    pub fn flush(&mut self) -> Vec<String> {
        // If there are still pending bytes at EOF (stream terminated prematurely),
        // decode them lossily to prevent data loss.
        if !self.pending_bytes.is_empty() {
            let lossy = String::from_utf8_lossy(&self.pending_bytes);
            self.text_buffer.push_str(&lossy);
            self.pending_bytes.clear();
        }

        let mut lines = self.extract_lines();
        // If there is any remaining un-terminated line in text_buffer, flush it
        if !self.text_buffer.is_empty() {
            lines.push(std::mem::take(&mut self.text_buffer));
        }
        lines
    }

    /// Returns true if both pending_bytes and text_buffer are empty.
    pub fn is_empty(&self) -> bool {
        self.pending_bytes.is_empty() && self.text_buffer.is_empty()
    }

    /// Returns the length of pending incomplete bytes.
    pub fn pending_len(&self) -> usize {
        self.pending_bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf8_chunk_buffer_chinese_split() {
        let mut buf = Utf8ChunkBuffer::new();
        // "你好世界" 的 UTF-8:
        // "你" -> [0xe4, 0xbd, 0xa0] (3 bytes)
        // "好" -> [0xe5, 0xa5, 0xbd] (3 bytes)
        // "世" -> [0xe4, 0xb8, 0x96] (3 bytes)
        // "界" -> [0xe7, 0x95, 0x8c] (3 bytes)
        let full = "data: 你好世界
";
        let bytes = full.as_bytes();

        // 在 "好" 的第 2 个字节后截断 (切片包含 "你" 全 3 字节 + "好" 的前 2 字节)
        let chunk1 = &bytes[..11];
        let chunk2 = &bytes[11..];

        let s1 = buf.push(chunk1);
        assert_eq!(s1, "data: 你");
        assert_eq!(buf.len(), 2); // 留下 "好" 的前两个残缺字节

        let s2 = buf.push(chunk2);
        assert_eq!(s2, "好世界
");
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());

        let tail = buf.flush_lossy();
        assert_eq!(tail, None);
    }

    #[test]
    fn test_utf8_chunk_buffer_emoji_split_and_eof_lossy() {
        let mut buf = Utf8ChunkBuffer::new();
        // Emoji '🎉' 是 4 字节: [0xf0, 0x9f, 0x8e, 0x89]
        let emoji_bytes = "🎉".as_bytes();
        assert_eq!(emoji_bytes.len(), 4);

        // 第一个 chunk 只有前 2 个字节
        let s1 = buf.push(&emoji_bytes[..2]);
        assert_eq!(s1, "");
        assert_eq!(buf.len(), 2);

        // 如果流突然 EOF，flush_lossy 保证下游绝不挂起并给出 lossy 替换字符
        let tail = buf.flush_lossy();
        assert!(tail.is_some());
        assert!(buf.is_empty());
    }

    #[test]
    fn test_normal_sse_lines() {
        let mut buffer = Utf8SseChunkBuffer::new();
        let chunk = b"data: {\"msg\": \"hello\"}\n\ndata: {\"msg\": \"world\"}\n";
        let lines = buffer.push_chunk(chunk);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], r#"data: {"msg": "hello"}"#);
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], r#"data: {"msg": "world"}"#);
    }

    #[test]
    fn test_chinese_multibyte_truncation() {
        let mut buffer = Utf8SseChunkBuffer::new();

        // "你好世界" in UTF-8:
        // "你" -> [0xe4, 0xbd, 0xa0]
        // "好" -> [0xe5, 0xa5, 0xbd]
        // "世" -> [0xe4, 0xb8, 0x96]
        // "界" -> [0xe7, 0x95, 0x8c]
        let full = "data: 你好世界\n";
        let bytes = full.as_bytes();

        // Split "好" in the middle:
        // "data: 你" has len 6 + 3 = 9.
        // "好" is at index 9, 10, 11.
        // Chunk 1 contains up to index 10 (bytes[..11] contains first 2 bytes of "好").
        let chunk1 = &bytes[..11];
        let chunk2 = &bytes[11..];

        let lines1 = buffer.push_chunk(chunk1);
        assert!(lines1.is_empty(), "Chunk 1 does not have complete line");
        assert_eq!(buffer.pending_len(), 2, "2 bytes of '好' should remain pending");

        let lines2 = buffer.push_chunk(chunk2);
        assert_eq!(lines2.len(), 1);
        assert_eq!(lines2[0], "data: 你好世界");
        assert_eq!(buffer.pending_len(), 0);
    }

    #[test]
    fn test_emoji_4byte_truncation() {
        let mut buffer = Utf8SseChunkBuffer::new();
        // Emoji '🎉' is [0xf0, 0x9f, 0x8e, 0x89] (4 bytes)
        let full = "data: {\"emoji\": \"🎉\"}\n";
        let bytes = full.as_bytes();
        let emoji_pos = full.find('🎉').unwrap();

        // Split in the middle of emoji (first 2 bytes in chunk 1, last 2 in chunk 2)
        let split_pos = emoji_pos + 2;
        let chunk1 = &bytes[..split_pos];
        let chunk2 = &bytes[split_pos..];

        let lines1 = buffer.push_chunk(chunk1);
        assert!(lines1.is_empty());
        assert_eq!(buffer.pending_len(), 2);

        let lines2 = buffer.push_chunk(chunk2);
        assert_eq!(lines2.len(), 1);
        assert_eq!(lines2[0], "data: {\"emoji\": \"🎉\"}");
        assert_eq!(buffer.pending_len(), 0);
    }

    #[test]
    fn test_flush_without_trailing_newline() {
        let mut buffer = Utf8SseChunkBuffer::new();
        let chunk = b"data: [DONE]";
        let lines1 = buffer.push_chunk(chunk);
        assert!(lines1.is_empty());

        let lines_flush = buffer.flush();
        assert_eq!(lines_flush.len(), 1);
        assert_eq!(lines_flush[0], "data: [DONE]");
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_flush_with_residual_broken_bytes() {
        let mut buffer = Utf8SseChunkBuffer::new();
        // Send broken byte at the very end
        buffer.push_chunk(&[b'd', b'a', b't', b'a', b':', b' ', 0xe4, 0xbd]); // Incomplete '你'
        let lines = buffer.flush();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("data: "));
        assert!(buffer.is_empty());
    }
}
