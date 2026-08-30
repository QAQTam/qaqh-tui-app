//! 手写 SSE 帧解码器（对照 `qaqh-client/src/sse_decoder.rs` 的行为契约）。
//!
//! - 按 `\n` 定位行（O(n) 总体、无搬移），字节层面切分；
//! - 整行严格 UTF-8 解码（非法行跳过，绝不 lossy——保护中文/emoji）；
//! - 空行定界事件帧；注释行（`:` 开头，keepalive）不产出帧；
//! - 多 `data:` 行聚合为单帧（SSE 规范以单个 `\n` 连接）；
//! - CRLF 兼容；流结束不冲刷残帧。
//!
//! 与 daemon 发送端 `id:`/`event:`/`data:` + 空行的帧格式完全对齐。

#[allow(dead_code)] // id 字段在 cursor 校验中使用；保留完整帧语义
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub id: String,
    pub event_type: String,
    pub data: String,
}

/// 游标式 SSE 帧解码器。`push` 追加字节，`next_frame` 逐帧产出。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    /// 已消费前缀长度（超过阈值时统一搬移一次，摊销 O(n)）。
    consumed: usize,
    pending: Option<SseFrame>,
}

/// 搬移阈值：超过后一次性 drain 已消费前缀。
const COMPACT_THRESHOLD: usize = 64 * 1024;

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.consumed >= COMPACT_THRESHOLD {
            self.buf.drain(..self.consumed);
            self.consumed = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// `Some(Some(frame))` = 完整帧；`Some(None)` = 流中存在不可解析行（跳过）；
    /// `None` = 暂无完整帧。
    pub fn next_frame(&mut self) -> Option<Result<SseFrame, ()>> {
        loop {
            let rel = self.buf[self.consumed..].iter().position(|&b| b == b'\n')?;
            let end = self.consumed + rel;
            let raw = &self.buf[self.consumed..end];
            self.consumed = end + 1;

            let line = match std::str::from_utf8(raw) {
                Ok(line) => line.trim_end(),
                Err(_) => continue, // 非法 UTF-8 行：跳过，绝不 lossy
            };

            if line.is_empty() {
                if let Some(frame) = self.pending.take() {
                    return Some(Ok(frame));
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // 注释行（keepalive）
            }

            let frame = self.pending.get_or_insert_with(SseFrame::default);
            if let Some(id) = line.strip_prefix("id:") {
                frame.id = id.trim().to_string();
            } else if let Some(event) = line.strip_prefix("event:") {
                frame.event_type = event.trim().to_string();
            } else if let Some(data) = line.strip_prefix("data:") {
                if !frame.data.is_empty() {
                    frame.data.push('\n');
                }
                frame.data.push_str(data.trim());
            }
            // 其余字段（retry 等）忽略。
        }
    }
}

/// 解析流帧 id `<epoch>:<stream>:<seq>`，校验 stream 段匹配后返回 seq。
/// epoch 或段不匹配 → None（按 seq 0 处理，与后端 `parse_sse_cursor` 对偶）。
pub fn frame_seq(id: &str, expect_stream: &str) -> Option<u64> {
    let mut parts = id.split(':');
    let _epoch = parts.next()?;
    let stream = parts.next()?;
    let seq = parts.next()?;
    if parts.next().is_some() || stream != expect_stream {
        return None;
    }
    seq.parse::<u64>().ok()
}

/// 组装 `Last-Event-ID`：`<epoch>:<channel>:<seq>`。
pub fn last_event_id(epoch: &str, stream: &str, seq: u64) -> String {
    format!("{epoch}:{stream}:{seq}")
}

/// 指数退避：1s ×2 → 30s 封顶，附带少量抖动（winui 参照实现无抖动，这里补上）。
pub fn backoff_delay(attempt: u32) -> std::time::Duration {
    let base = 1000u64.saturating_mul(1u64 << attempt.min(5));
    let capped = base.min(30_000);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64 % 250)
        .unwrap_or(0);
    std::time::Duration::from_millis(capped + jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(decoder: &mut SseDecoder) -> SseFrame {
        decoder.next_frame().expect("frame").expect("utf-8")
    }

    #[test]
    fn parses_id_event_data_frame() {
        let mut d = SseDecoder::new();
        d.push(b"id: epoch-1:conversation:7\nevent: turn_started\ndata: {\"x\":1}\n\n");
        let f = frame(&mut d);
        assert_eq!(f.id, "epoch-1:conversation:7");
        assert_eq!(f.event_type, "turn_started");
        assert_eq!(f.data, "{\"x\":1}");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn frame_split_across_chunks_is_reassembled() {
        let mut d = SseDecoder::new();
        d.push(b"id: e:tool:1\nevent: tool_star");
        assert!(d.next_frame().is_none());
        d.push(b"ted\ndata: {}\n\n");
        let f = frame(&mut d);
        assert_eq!(f.id, "e:tool:1");
        assert_eq!(f.event_type, "tool_started");
        assert_eq!(f.data, "{}");
    }

    #[test]
    fn utf8_char_split_across_chunks_is_not_corrupted() {
        let mut d = SseDecoder::new();
        // "中" = E4 B8 AD，切到两个 push。
        d.push(b"data: {\"t\":\"\xe4\xb8");
        assert!(d.next_frame().is_none());
        d.push(b"\xad\"}\n\n");
        assert_eq!(frame(&mut d).data, "{\"t\":\"中\"}");
    }

    #[test]
    fn invalid_utf8_line_is_skipped() {
        let mut d = SseDecoder::new();
        // 坏行被跳过、绝不 lossy；解析器继续存活并产出后续完整帧。
        d.push(b"data: \xff\xfe broken\n\ndata: fine\n\n");
        assert_eq!(frame(&mut d).data, "fine");
    }

    #[test]
    fn multiple_data_lines_aggregate() {
        let mut d = SseDecoder::new();
        d.push(b"data: first\ndata: second\n\n");
        assert_eq!(frame(&mut d).data, "first\nsecond");
    }

    #[test]
    fn keepalive_comments_emit_nothing() {
        let mut d = SseDecoder::new();
        d.push(b": keep-alive\n\n");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let mut d = SseDecoder::new();
        d.push(b"data: {\"a\":1}\r\n\r\n");
        assert_eq!(frame(&mut d).data, "{\"a\":1}");
    }

    #[test]
    fn cursor_parsing() {
        assert_eq!(frame_seq("ep:conversation:42", "conversation"), Some(42));
        assert_eq!(frame_seq("ep:timeline:9", "timeline"), Some(9));
        assert_eq!(frame_seq("ep:tool:42", "conversation"), None);
        assert_eq!(frame_seq("ep:conversation:x", "conversation"), None);
        assert_eq!(frame_seq("junk", "conversation"), None);
        assert_eq!(last_event_id("ep", "tool", 3), "ep:tool:3");
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff_delay(0).as_millis() >= 1000, true);
        assert_eq!(backoff_delay(10).as_millis() <= 30_250, true);
    }
}
