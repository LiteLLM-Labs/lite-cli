//! Extract token usage + model from Anthropic-style responses.
//!
//! Two shapes:
//!   - Non-stream JSON: `{ "model": ..., "usage": { "input_tokens", "output_tokens", ... } }`
//!   - SSE stream: `message_start` carries model + input tokens, `message_delta` carries the
//!     (cumulative) output token count.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

fn read_usage_obj(u: &serde_json::Value, out: &mut Usage) {
    if let Some(n) = u.get("input_tokens").and_then(|v| v.as_u64()) {
        out.input_tokens = n;
    }
    if let Some(n) = u.get("output_tokens").and_then(|v| v.as_u64()) {
        out.output_tokens = n;
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        out.cache_read_tokens = n;
    }
    if let Some(n) = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
        out.cache_creation_tokens = n;
    }
}

/// Parse a complete non-streaming JSON response body.
pub fn parse_non_stream(body: &[u8]) -> Usage {
    let mut usage = Usage::default();
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return usage;
    };
    if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
        usage.model = Some(m.to_string());
    }
    if let Some(u) = v.get("usage") {
        read_usage_obj(u, &mut usage);
    }
    usage
}

/// Incrementally parses an SSE byte stream, accumulating usage as events arrive.
#[derive(Default)]
pub struct StreamParser {
    buf: String,
    usage: Usage,
}

impl StreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw chunk of SSE bytes. Safe to call with arbitrary chunk boundaries.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        // Process complete lines; keep any trailing partial line in the buffer.
        while let Some(idx) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=idx).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("message_start") => {
                    if let Some(msg) = v.get("message") {
                        if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                            self.usage.model = Some(m.to_string());
                        }
                        if let Some(u) = msg.get("usage") {
                            read_usage_obj(u, &mut self.usage);
                        }
                    }
                }
                Some("message_delta") => {
                    if let Some(u) = v.get("usage") {
                        // output_tokens here is cumulative for the message.
                        read_usage_obj(u, &mut self.usage);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn finish(self) -> Usage {
        self.usage
    }
}
