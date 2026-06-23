//! `lite logs` — print or tail the latest session log.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::log::RequestRecord;

/// Resolve the session file to read: explicit `latest` pointer, else newest `session-*.jsonl`.
pub fn latest_session(log_dir: &Path) -> Option<PathBuf> {
    let pointer = log_dir.join("latest");
    if let Ok(content) = std::fs::read_to_string(&pointer) {
        let p = PathBuf::from(content.trim());
        if p.exists() {
            return Some(p);
        }
    }
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("session-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect();
    sessions.sort();
    sessions.pop()
}

pub async fn run(log_dir: PathBuf, session: Option<PathBuf>, follow: bool) -> Result<()> {
    let path = match session.or_else(|| latest_session(&log_dir)) {
        Some(p) => p,
        None => bail!("no session logs found in {}", log_dir.display()),
    };
    println!("# {}", path.display());
    print_header();

    let mut file = std::fs::File::open(&path)?;
    let mut reader = BufReader::new(&mut file);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            if !follow {
                break;
            }
            // Wait for more data, then continue from the current offset.
            let pos = reader.stream_position()?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            reader.seek(SeekFrom::Start(pos))?;
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<RequestRecord>(line.trim()) {
            print_row(&rec);
        }
    }
    Ok(())
}

fn print_header() {
    println!(
        "{:<12}  {:<28}  {:>7}  {:>7}  {:>9}  {:>7}  {:>4}  {}",
        "time", "model", "in", "out", "cost", "ms", "code", "path"
    );
}

fn print_row(r: &RequestRecord) {
    let time = r.ts.split('T').nth(1).unwrap_or(&r.ts);
    let time = time.trim_end_matches('Z');
    let model = r.model.as_deref().unwrap_or("-");
    let cost = if r.cost_usd > 0.0 {
        format!("${:.4}", r.cost_usd)
    } else {
        "-".to_string()
    };
    println!(
        "{:<12}  {:<28}  {:>7}  {:>7}  {:>9}  {:>7}  {:>4}  {}",
        time, model, r.input_tokens, r.output_tokens, cost, r.latency_ms, r.status, r.path
    );
}
