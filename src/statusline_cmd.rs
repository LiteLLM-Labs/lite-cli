//! `lite statusline` — prints one compact line for Claude Code's status bar (configured via the
//! `statusLine` setting lite injects when launching CC). Shows the dashboard URL and this session's
//! proxied spend, so the dashboard is discoverable from inside the TUI without lite writing to the
//! terminal directly (which would corrupt CC's screen).

use std::io::Read;

use anyhow::Result;

pub fn run() -> Result<()> {
    // Claude Code pipes a session JSON blob on stdin; we don't need it, but drain it so CC's write
    // completes cleanly. Best-effort — never fail the status line over input.
    let mut _input = String::new();
    let _ = std::io::stdin().read_to_string(&mut _input);

    let url = std::env::var("LITE_DASHBOARD_URL")
        .unwrap_or_else(|_| "http://localhost:4097".to_string());
    let (spend, model) = latest_session().unwrap_or((0.0, None));
    // The session's locked tier serves one model; show it (short form) so the routed model is
    // visible without leaving the TUI. Before the first request lands, show "routing…".
    let model = model.as_deref().map(short_model).unwrap_or("routing…");

    println!("◆ lite · {model} · ${spend:.4} this session · dashboard → {url}");
    Ok(())
}

/// Drop a provider prefix for a compact status line (`anthropic/claude-sonnet-4-6` → the tail).
fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// From the newest proxy session log in `~/.lite/logs`, return this session's total `cost_usd` and
/// the most recently served `model`. Best-effort; any problem returns `None`.
fn latest_session() -> Option<(f64, Option<String>)> {
    let dir = dirs::home_dir()?.join(".lite").join("logs");
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            newest = Some((modified, path));
        }
    }
    let (_, path) = newest?;
    let content = std::fs::read_to_string(path).ok()?;
    let mut sum = 0.0;
    let mut model: Option<String> = None;
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(c) = v.get("cost_usd").and_then(|c| c.as_f64()) {
                sum += c;
            }
            if let Some(m) = v.get("model").and_then(|m| m.as_str()).filter(|s| !s.is_empty()) {
                model = Some(m.to_string());
            }
        }
    }
    Some((sum, model))
}
