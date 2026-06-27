//! Read Claude Code's own session transcripts (`~/.claude/projects/<enc-cwd>/<session>.jsonl`)
//! as the source of truth for spend. Each assistant message carries full `usage` — including the
//! 5m/1h cache-creation split and service tier — which is richer than the proxy stream.
//!
//! Pure reader: walks the transcript tree and returns one `Turn` per billable assistant response.
//! Streaming writes the same `message.id` multiple times with a growing `output_tokens`, so turns
//! are de-duplicated by message id, keeping the maximal (final) output count.

use std::collections::HashMap;
use std::path::PathBuf;

/// One billable assistant response.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub ts: String,
    pub session_id: String,
    pub project: String, // the cwd the session ran in
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_5m: u64,
    pub cache_creation_1h: u64,
    pub service_tier: Option<String>,
}

impl Turn {
    pub fn cache_creation_total(&self) -> u64 {
        self.cache_creation_5m + self.cache_creation_1h
    }
}

/// One displayable event from a Claude Code session transcript.
#[derive(Debug, Clone, Default)]
pub struct SessionMessage {
    pub ts: String,
    pub role: String,
    pub kind: String,
    pub text: String,
    pub has_usage: bool,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_5m: u64,
    pub cache_creation_1h: u64,
    pub service_tier: Option<String>,
}

impl SessionMessage {
    pub fn cache_creation_total(&self) -> u64 {
        self.cache_creation_5m + self.cache_creation_1h
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionTranscript {
    pub session_id: String,
    pub title: String,
    pub project: String,
    pub messages: Vec<SessionMessage>,
}

fn projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

fn u(v: &serde_json::Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn transcript_files() -> Vec<PathBuf> {
    let root = projects_dir();
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for dir in dirs.filter_map(|e| e.ok()) {
        let Ok(entries) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for f in entries.filter_map(|e| e.ok()) {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                files.push(p);
            }
        }
    }
    files
}

fn usage_fields(usage: &serde_json::Value) -> (u64, u64, u64, u64, u64, Option<String>) {
    let (eph_5m, eph_1h) = match usage.get("cache_creation") {
        Some(cc) => (
            u(cc, "ephemeral_5m_input_tokens"),
            u(cc, "ephemeral_1h_input_tokens"),
        ),
        // No split provided: treat all cache-creation as 5m (Claude Code's default TTL).
        None => (u(usage, "cache_creation_input_tokens"), 0),
    };
    (
        u(usage, "input_tokens"),
        u(usage, "output_tokens"),
        u(usage, "cache_read_input_tokens"),
        eph_5m,
        eph_1h,
        usage
            .get("service_tier")
            .and_then(|s| s.as_str())
            .map(String::from),
    )
}

/// Parse one transcript file into de-duplicated turns.
fn parse_file(path: &PathBuf) -> Vec<Turn> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // message.id -> Turn, keeping the entry with the largest output_tokens (final stream frame).
    let mut by_id: HashMap<String, Turn> = HashMap::new();
    for line in content.lines() {
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if o.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = o.get("message") else {
            continue;
        };
        let Some(usage) = msg.get("usage") else {
            continue;
        };
        let id = msg
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();

        let (input_tokens, output_tokens, cache_read_tokens, eph_5m, eph_1h, service_tier) =
            usage_fields(usage);

        let turn = Turn {
            ts: o
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            session_id: o
                .get("sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            project: o
                .get("cwd")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            model: msg.get("model").and_then(|m| m.as_str()).map(String::from),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_5m: eph_5m,
            cache_creation_1h: eph_1h,
            service_tier,
        };

        match by_id.get(&id) {
            Some(existing) if existing.output_tokens >= turn.output_tokens && !id.is_empty() => {}
            _ => {
                by_id.insert(id, turn);
            }
        }
    }
    by_id.into_values().collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn json_preview(v: &serde_json::Value) -> String {
    serde_json::to_string(v)
        .map(|s| truncate_chars(&s, 800))
        .unwrap_or_else(|_| "[unrenderable json]".to_string())
}

fn content_preview(content: &serde_json::Value) -> (String, String) {
    const MAX_TEXT: usize = 1600;
    if let Some(s) = content.as_str() {
        return ("text".to_string(), truncate_chars(s.trim(), MAX_TEXT));
    }

    let Some(blocks) = content.as_array() else {
        return ("content".to_string(), json_preview(content));
    };

    let mut kind = "text".to_string();
    let mut parts = Vec::new();
    for block in blocks {
        let ty = block
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("content");
        match ty {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(text.trim().to_string());
                    }
                }
            }
            "tool_use" => {
                kind = "tool_use".to_string();
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let input = block
                    .get("input")
                    .map(json_preview)
                    .unwrap_or_else(|| "{}".to_string());
                parts.push(format!("{name} {input}"));
            }
            "tool_result" => {
                kind = "tool_result".to_string();
                let content = block
                    .get("content")
                    .map(|v| {
                        v.as_str()
                            .map(|s| truncate_chars(s.trim(), 800))
                            .unwrap_or_else(|| json_preview(v))
                    })
                    .unwrap_or_default();
                parts.push(if content.is_empty() {
                    "tool result".to_string()
                } else {
                    format!("tool result: {content}")
                });
            }
            "thinking" => {
                if parts.is_empty() {
                    kind = "thinking".to_string();
                    parts.push("[thinking]".to_string());
                }
            }
            other => {
                kind = other.to_string();
                parts.push(format!("[{other}]"));
            }
        }
    }

    let text = parts.join("\n\n");
    (kind, truncate_chars(text.trim(), MAX_TEXT))
}

fn parse_session_file(path: &PathBuf, session_id: &str) -> Option<SessionTranscript> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return None;
    };

    let mut found = false;
    let mut title = String::new();
    let mut project = String::new();
    let mut messages = Vec::new();
    let mut assistant_by_id: HashMap<String, SessionMessage> = HashMap::new();

    for line in content.lines() {
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if o.get("sessionId").and_then(|s| s.as_str()) != Some(session_id) {
            continue;
        }
        found = true;

        if project.is_empty() {
            project = o
                .get("cwd")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
        }

        match o.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                title = o
                    .get("aiTitle")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                continue;
            }
            Some("user") | Some("assistant") => {}
            _ => continue,
        }

        let Some(msg) = o.get("message") else {
            continue;
        };
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .or_else(|| o.get("type").and_then(|t| t.as_str()))
            .unwrap_or("message")
            .to_string();
        let content = msg.get("content").unwrap_or(&serde_json::Value::Null);
        let (kind, text) = content_preview(content);
        let usage = msg.get("usage");
        let (
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_5m,
            cache_creation_1h,
            service_tier,
        ) = usage.map(usage_fields).unwrap_or_default();
        let sm = SessionMessage {
            ts: o
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            role,
            kind,
            text,
            has_usage: usage.is_some(),
            model: msg.get("model").and_then(|m| m.as_str()).map(String::from),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_5m,
            cache_creation_1h,
            service_tier,
        };

        let id = msg.get("id").and_then(|i| i.as_str()).unwrap_or("");
        if !id.is_empty() && sm.role == "assistant" {
            match assistant_by_id.get(id) {
                Some(existing)
                    if existing.output_tokens > sm.output_tokens
                        || (existing.output_tokens == sm.output_tokens
                            && existing.text.len() >= sm.text.len()) => {}
                _ => {
                    assistant_by_id.insert(id.to_string(), sm);
                }
            }
        } else {
            messages.push(sm);
        }
    }

    if !found {
        return None;
    }

    messages.extend(assistant_by_id.into_values());
    messages.sort_by(|a, b| a.ts.cmp(&b.ts));

    Some(SessionTranscript {
        session_id: session_id.to_string(),
        title,
        project,
        messages,
    })
}

/// Read all transcripts. If `project` is set, only sessions whose cwd matches are returned.
pub fn read_all(project: Option<&str>) -> Vec<Turn> {
    let mut turns = Vec::new();
    for p in transcript_files() {
        turns.extend(parse_file(&p));
    }
    if let Some(proj) = project {
        turns.retain(|t| t.project == proj);
    }
    turns.sort_by(|a, b| a.ts.cmp(&b.ts));
    turns
}

/// Read one transcript session, with message previews for dashboard drill-down.
pub fn read_session(session_id: &str, project: Option<&str>) -> Option<SessionTranscript> {
    for p in transcript_files() {
        let Some(session) = parse_session_file(&p, session_id) else {
            continue;
        };
        if project.map(|proj| session.project == proj).unwrap_or(true) {
            return Some(session);
        }
    }
    None
}
