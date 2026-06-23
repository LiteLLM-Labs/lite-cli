//! Session logging: one JSON line per proxied API call, plus an in-memory aggregate.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::pricing::Pricing;
use crate::usage::Usage;

/// One proxied request, serialized as a JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub stream: bool,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<serde_json::Value>,
}

impl RequestRecord {
    pub fn from_usage(
        ts: String,
        method: String,
        path: String,
        status: u16,
        stream: bool,
        latency_ms: u64,
        usage: Usage,
    ) -> Self {
        Self {
            ts,
            method,
            path,
            status,
            stream,
            latency_ms,
            model: usage.model,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cost_usd: 0.0,
            request_body: None,
            response_body: None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ModelTotals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Summary {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub by_model: BTreeMap<String, ModelTotals>,
}

struct Inner {
    file: File,
    summary: Summary,
}

pub struct Logger {
    inner: Mutex<Inner>,
    pricing: Pricing,
    pub session_path: PathBuf,
    pub log_bodies: bool,
}

impl Logger {
    /// Create a new session logger under `log_dir`, naming the file by timestamp.
    pub fn new(log_dir: &Path, session_ts: &str, log_bodies: bool, pricing: Pricing) -> Result<Self> {
        fs::create_dir_all(log_dir)
            .with_context(|| format!("creating log dir {}", log_dir.display()))?;
        let session_path = log_dir.join(format!("session-{session_ts}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&session_path)
            .with_context(|| format!("opening session file {}", session_path.display()))?;
        // Write a `latest` pointer for the dashboard / `lite logs`.
        let _ = fs::write(log_dir.join("latest"), session_path.to_string_lossy().as_bytes());
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                summary: Summary::default(),
            }),
            pricing,
            session_path,
            log_bodies,
        })
    }

    pub fn log(&self, mut record: RequestRecord) {
        // Compute spend from current pricing before persisting/aggregating.
        record.cost_usd = self.pricing.cost(
            record.model.as_deref(),
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.cache_creation_tokens,
        );

        let mut inner = self.inner.lock().unwrap();
        // Append JSONL line.
        if let Ok(line) = serde_json::to_string(&record) {
            let _ = writeln!(inner.file, "{line}");
            let _ = inner.file.flush();
        }
        // Update aggregate.
        inner.summary.requests += 1;
        inner.summary.input_tokens += record.input_tokens;
        inner.summary.output_tokens += record.output_tokens;
        inner.summary.cost_usd += record.cost_usd;
        let model = record.model.clone().unwrap_or_else(|| "unknown".to_string());
        let entry = inner.summary.by_model.entry(model).or_default();
        entry.requests += 1;
        entry.input_tokens += record.input_tokens;
        entry.output_tokens += record.output_tokens;
        entry.cost_usd += record.cost_usd;
    }

    pub fn summary(&self) -> Summary {
        self.inner.lock().unwrap().summary.clone()
    }
}
