//! `lite dashboard` — local web UI showing usage, polled live from the JSONL logs.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Json;
use serde::Serialize;

use crate::log::RequestRecord;
use crate::logs_cmd::latest_session;

struct DashState {
    log_dir: PathBuf,
}

#[derive(Serialize, Default)]
struct ModelRow {
    model: String,
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Serialize, Default)]
struct SeriesPoint {
    i: usize,
    input: u64,
    output: u64,
}

#[derive(Serialize, Default)]
struct UsageResponse {
    session: String,
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    by_model: Vec<ModelRow>,
    recent: Vec<RequestRecord>,
    series: Vec<SeriesPoint>,
}

pub async fn serve(port: u16, log_dir: PathBuf) -> Result<()> {
    let state = Arc::new(DashState { log_dir });
    let app = axum::Router::new()
        .route("/", get(root))
        .route("/api/usage", get(api_usage))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding dashboard port {port}"))?;
    eprintln!("lite dashboard: http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

async fn api_usage(
    State(state): State<Arc<DashState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<UsageResponse> {
    let session = params
        .get("session")
        .map(PathBuf::from)
        .or_else(|| latest_session(&state.log_dir));

    let mut resp = UsageResponse::default();
    let Some(path) = session else {
        return Json(resp);
    };
    resp.session = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let Ok(content) = std::fs::read_to_string(&path) else {
        return Json(resp);
    };

    let mut by_model: BTreeMap<String, ModelRow> = BTreeMap::new();
    let mut records: Vec<RequestRecord> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let Ok(rec) = serde_json::from_str::<RequestRecord>(line) else {
            continue;
        };
        resp.requests += 1;
        resp.input_tokens += rec.input_tokens;
        resp.output_tokens += rec.output_tokens;
        resp.cache_read_tokens += rec.cache_read_tokens;
        resp.cache_creation_tokens += rec.cache_creation_tokens;

        let key = rec.model.clone().unwrap_or_else(|| "unknown".to_string());
        let row = by_model.entry(key.clone()).or_insert_with(|| ModelRow {
            model: key,
            ..Default::default()
        });
        row.requests += 1;
        row.input_tokens += rec.input_tokens;
        row.output_tokens += rec.output_tokens;

        resp.series.push(SeriesPoint {
            i,
            input: rec.input_tokens,
            output: rec.output_tokens,
        });
        records.push(rec);
    }

    resp.by_model = by_model.into_values().collect();
    resp.by_model.sort_by(|a, b| b.input_tokens.cmp(&a.input_tokens));
    // Most recent first, capped.
    records.reverse();
    records.truncate(100);
    resp.recent = records;
    Json(resp)
}
