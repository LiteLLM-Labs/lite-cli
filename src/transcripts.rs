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

fn projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

fn u(v: &serde_json::Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
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
        let Some(msg) = o.get("message") else { continue };
        let Some(usage) = msg.get("usage") else { continue };
        let id = msg
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();

        let (eph_5m, eph_1h) = match usage.get("cache_creation") {
            Some(cc) => (
                u(cc, "ephemeral_5m_input_tokens"),
                u(cc, "ephemeral_1h_input_tokens"),
            ),
            // No split provided: treat all cache-creation as 5m (Claude Code's default TTL).
            None => (u(usage, "cache_creation_input_tokens"), 0),
        };

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
            input_tokens: u(usage, "input_tokens"),
            output_tokens: u(usage, "output_tokens"),
            cache_read_tokens: u(usage, "cache_read_input_tokens"),
            cache_creation_5m: eph_5m,
            cache_creation_1h: eph_1h,
            service_tier: usage
                .get("service_tier")
                .and_then(|s| s.as_str())
                .map(String::from),
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

/// Read all transcripts. If `project` is set, only sessions whose cwd matches are returned.
pub fn read_all(project: Option<&str>) -> Vec<Turn> {
    let root = projects_dir();
    let mut turns = Vec::new();
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return turns;
    };
    for dir in dirs.filter_map(|e| e.ok()) {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for f in files.filter_map(|e| e.ok()) {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                turns.extend(parse_file(&p));
            }
        }
    }
    if let Some(proj) = project {
        turns.retain(|t| t.project == proj);
    }
    turns.sort_by(|a, b| a.ts.cmp(&b.ts));
    turns
}
