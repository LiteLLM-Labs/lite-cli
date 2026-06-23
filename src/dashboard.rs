//! `lite dashboard` — local web UI of Claude Code spend, sourced from `~/.claude` transcripts.
//!
//! Spend is read from Claude's own session logs (see `transcripts.rs`), not the proxy — they are
//! complete (every session, retroactive) and richer (5m/1h cache split, service tier). Per
//! AGENTS.md this is a read-only presenter: it reads transcripts, prices each turn via `pricing`,
//! and aggregates. No proxy state involved.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Json;
use serde::Serialize;

use crate::pricing::Pricing;
use crate::transcripts::{self, Turn};

struct DashState {
    pricing: Pricing,
    current_project: Option<String>,
}

#[derive(Serialize, Default)]
struct GroupRow {
    key: String,
    project: String,
    model: String,
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    last_ts: String,
}

#[derive(Serialize, Default)]
struct RecentTurn {
    ts: String,
    project: String,
    session_id: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

#[derive(Serialize, Default)]
struct SeriesPoint {
    i: usize,
    cost: f64,
}

#[derive(Serialize, Default)]
struct UsageResponse {
    scope: String,
    project: String,
    turns: u64,
    sessions: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
    cache_savings_usd: f64,
    hit_rate: f64,
    by_session: Vec<GroupRow>,
    by_project: Vec<GroupRow>,
    by_model: Vec<GroupRow>,
    by_day: Vec<GroupRow>,
    recent: Vec<RecentTurn>,
    series: Vec<SeriesPoint>,
}

pub async fn serve(port: u16, _log_dir: std::path::PathBuf) -> Result<()> {
    let pricing = Pricing::load().await;
    let current_project = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    let state = Arc::new(DashState {
        pricing,
        current_project,
    });
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
    let mut resp = UsageResponse::default();

    // Scope: "project" filters to the dashboard's launch cwd; otherwise all projects.
    let project_filter = if params.get("scope").map(|s| s == "project").unwrap_or(false) {
        state.current_project.clone()
    } else {
        None
    };
    resp.scope = if project_filter.is_some() {
        "project".into()
    } else {
        "all".into()
    };
    resp.project = project_filter.clone().unwrap_or_default();

    let turns = transcripts::read_all(project_filter.as_deref());

    let mut by_session: BTreeMap<String, GroupRow> = BTreeMap::new();
    let mut by_project: BTreeMap<String, GroupRow> = BTreeMap::new();
    let mut by_model: BTreeMap<String, GroupRow> = BTreeMap::new();
    let mut by_day: BTreeMap<String, GroupRow> = BTreeMap::new();
    let mut recent: Vec<RecentTurn> = Vec::new();

    for t in &turns {
        let cost = state.pricing.cost_detailed(
            t.model.as_deref(),
            t.input_tokens,
            t.output_tokens,
            t.cache_read_tokens,
            t.cache_creation_5m,
            t.cache_creation_1h,
            t.service_tier.as_deref(),
        );
        let model = t.model.clone().unwrap_or_else(|| "unknown".to_string());

        resp.turns += 1;
        resp.input_tokens += t.input_tokens;
        resp.output_tokens += t.output_tokens;
        resp.cache_read_tokens += t.cache_read_tokens;
        resp.cache_creation_tokens += t.cache_creation_total();
        resp.cost_usd += cost;
        resp.cache_savings_usd += state
            .pricing
            .cache_savings(t.model.as_deref(), t.cache_read_tokens);

        accumulate(by_session.entry(t.session_id.clone()).or_default(), t, &model, cost);
        // Label session rows by short id + project basename.
        let srow = by_session.get_mut(&t.session_id).unwrap();
        srow.key = t.session_id.clone();
        srow.project = t.project.clone();

        accumulate(by_project.entry(t.project.clone()).or_default(), t, &model, cost);
        by_project.get_mut(&t.project).unwrap().key = t.project.clone();

        accumulate(by_model.entry(model.clone()).or_default(), t, &model, cost);
        by_model.get_mut(&model).unwrap().key = model.clone();

        // Day bucket from the ISO timestamp (YYYY-MM-DD).
        let day = t.ts.get(..10).unwrap_or("").to_string();
        let drow = by_day.entry(day.clone()).or_default();
        accumulate(drow, t, &model, cost);
        drow.key = day;

        resp.series.push(SeriesPoint {
            i: resp.turns as usize,
            cost,
        });
        recent.push(RecentTurn {
            ts: t.ts.clone(),
            project: t.project.clone(),
            session_id: t.session_id.clone(),
            model,
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cost_usd: cost,
        });
    }

    resp.sessions = by_session.len() as u64;
    let total_in = resp.input_tokens + resp.cache_read_tokens + resp.cache_creation_tokens;
    if total_in > 0 {
        resp.hit_rate = resp.cache_read_tokens as f64 / total_in as f64 * 100.0;
    }

    resp.by_session = sorted_by_cost(by_session);
    resp.by_project = sorted_by_cost(by_project);
    resp.by_model = sorted_by_cost(by_model);
    // Days stay chronological for a time-series chart.
    resp.by_day = by_day.into_values().collect();
    recent.reverse();
    recent.truncate(100);
    resp.recent = recent;
    Json(resp)
}

fn accumulate(row: &mut GroupRow, t: &Turn, model: &str, cost: f64) {
    row.turns += 1;
    row.input_tokens += t.input_tokens;
    row.output_tokens += t.output_tokens;
    row.cost_usd += cost;
    if t.ts > row.last_ts {
        row.last_ts = t.ts.clone();
        row.model = model.to_string(); // most recent model in the group
    }
}

fn sorted_by_cost(map: BTreeMap<String, GroupRow>) -> Vec<GroupRow> {
    let mut v: Vec<GroupRow> = map.into_values().collect();
    v.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}
