//! Transparent logging proxy: forwards every request to the upstream Anthropic-compatible
//! API unchanged, while tapping token usage out of the response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use serde_json::Value;

use crate::classifier::{classify, Tier};
use crate::log::{Logger, RequestRecord};
use crate::settings::Settings;
use crate::usage::{self, StreamParser, Usage};

pub struct ProxyState {
    pub client: reqwest::Client,
    pub upstream: String, // no trailing slash
    pub logger: Arc<Logger>,
    /// Tracks in-flight stream-logging tasks so they can be drained on shutdown.
    pub tracker: tokio_util::task::TaskTracker,
    /// `Some` in autorouter mode: rewrite the request `model` by complexity and inject gateway auth.
    /// `None` keeps the proxy fully transparent (verbatim forward).
    pub routing: Option<RoutingConfig>,
}

/// Autorouter state: the tier→model config, the gateway api key, and a per-session tier lock.
pub struct RoutingConfig {
    settings: Settings,
    api_key: String,
    /// session id → locked tier. Classify-once-lock: the first turn of a session decides the tier,
    /// which is then held for the whole session to keep Anthropic prompt caching stable.
    session_tiers: Mutex<HashMap<String, Tier>>,
}

impl RoutingConfig {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            settings: s.clone(),
            api_key: s.api_key.clone().unwrap_or_default(),
            session_tiers: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the model to route this request to. Returns `None` if no tier model is configured.
    fn target_model(&self, session_id: Option<&str>, body: &Value) -> Option<String> {
        let requested = body.get("model").and_then(|m| m.as_str()).unwrap_or("");
        // Claude Code's background small-fast slot (haiku) always goes to the cheapest model and
        // never participates in the session tier lock.
        if requested.contains("haiku") {
            return self.settings.simple_model.clone();
        }
        let key = session_id.unwrap_or("").to_string();
        let mut map = self.session_tiers.lock().unwrap();
        let tier = match map.get(&key) {
            Some(t) => *t,
            None => {
                // NOTE: do not print here. The proxy shares the terminal with Claude Code's
                // full-screen TUI; any stderr write during a session corrupts its input line.
                // Routing decisions belong in the session log, not the terminal.
                let t = classify_body(body);
                map.insert(key, t);
                t
            }
        };
        tier.model(&self.settings).map(String::from)
    }
}

/// Flatten an Anthropic `content` value (string, or array of blocks) to its text.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Pull the classifier inputs out of an Anthropic `/v1/messages` body and classify. We score the
/// user conversation only — never the system prompt (CC's system prompt is a large tool catalog
/// that would dominate keyword scoring). `<system-reminder>` blocks CC injects into user turns are
/// stripped via `classifier::strip_noise`.
fn classify_body(body: &Value) -> Tier {
    let mut user_msg = String::new();
    let mut context_chars = 0usize;
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            // Score the user conversation only. Some clients (and hooks) put the system prompt as a
            // `role:"system"` message inside `messages`; counting it would let CC boilerplate drive
            // the token-size signal. Skip everything that isn't a user turn.
            if m.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            let text = crate::classifier::strip_noise(&content_text(
                m.get("content").unwrap_or(&Value::Null),
            ));
            if text.is_empty() {
                continue;
            }
            context_chars += text.len();
            user_msg = text; // forward iteration → ends on the most recent user message
        }
    }
    let tool_count = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    classify(&user_msg, tool_count, context_chars)
}

const MAX_REQ_BYTES: usize = 128 * 1024 * 1024;

/// Headers we must not forward verbatim.
fn is_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "content-length" | "connection" | "accept-encoding" | "transfer-encoding"
    )
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            api_base: Some("http://gw".into()),
            api_key: Some("k".into()),
            simple_model: Some("simple-m".into()),
            medium_model: Some("medium-m".into()),
            complex_model: Some("complex-m".into()),
            reasoning_model: Some("reason-m".into()),
        }
    }

    fn body(model: &str, user: &str) -> Value {
        serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": user }],
        })
    }

    // Code keywords (→0.30) + 3 technical terms (→0.125) = 0.425 ⇒ COMPLEX, no reasoning markers.
    const COMPLEX_ASK: &str = "Refactor the async database query function to optimize api endpoint \
         latency in our distributed architecture";

    #[test]
    fn sticky_lock_holds_across_turns() {
        let rc = RoutingConfig::from_settings(&settings());
        // First turn is trivial → locks the session to the simple tier.
        assert_eq!(
            rc.target_model(Some("sess-1"), &body("claude-opus", "hi")).unwrap(),
            "simple-m"
        );
        // A clearly-complex second turn in the SAME session keeps the locked tier.
        assert_eq!(
            rc.target_model(Some("sess-1"), &body("claude-opus", COMPLEX_ASK)).unwrap(),
            "simple-m",
            "classify-once-lock must hold the first turn's tier"
        );
        // A different session classifies independently.
        assert_eq!(
            rc.target_model(Some("sess-2"), &body("claude-opus", COMPLEX_ASK)).unwrap(),
            "complex-m"
        );
    }

    #[test]
    fn param_error_extracts_name() {
        // Shape seen from the gateway: outer error.message wraps the inner provider error.
        let body = br#"{"error":{"message":"litellm.BadRequestError: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"'claude-sonnet-4-6' does not support the `speed` parameter. This feature is only available on supported models.\"}}"}}"#;
        assert_eq!(param_error(body).as_deref(), Some("speed"));
    }

    #[test]
    fn param_error_extracts_quoted_thinking() {
        // Bedrock/converse shape: token quoted, phrased "is not supported" (no "parameter").
        let body = br#"{"error":{"message":"BedrockException - {\"message\":\"The model returned the following errors: \\\"thinking.type.enabled\\\" is not supported for this model.\"}"}}"#;
        assert_eq!(param_error(body).as_deref(), Some("thinking.type.enabled"));
    }

    #[test]
    fn param_error_ignores_unrelated_400() {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"server is overloaded"}}"#;
        assert_eq!(param_error(body), None);
    }

    #[test]
    fn haiku_bypasses_to_simple_without_locking() {
        let rc = RoutingConfig::from_settings(&settings());
        // The background small-fast slot always routes to simple, regardless of content...
        assert_eq!(
            rc.target_model(Some("sess-3"), &body("claude-haiku-4-5", COMPLEX_ASK)).unwrap(),
            "simple-m"
        );
        // ...and it must not have set the session lock: a real turn still classifies fresh.
        assert_eq!(
            rc.target_model(Some("sess-3"), &body("claude-opus", COMPLEX_ASK)).unwrap(),
            "complex-m"
        );
    }
}

pub async fn handle(State(state): State<Arc<ProxyState>>, req: axum::extract::Request) -> Response<Body> {
    match proxy(state, req).await {
        Ok(resp) => resp,
        Err(e) => (StatusCode::BAD_GATEWAY, format!("lite proxy error: {e}")).into_response(),
    }
}

async fn proxy(state: Arc<ProxyState>, req: axum::extract::Request) -> Result<Response<Body>> {
    let start = Instant::now();
    let ts = now_rfc3339();
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    // Claude Code tags each request with its session id; capture it for per-session spend.
    let session_id = req
        .headers()
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Forward headers minus hop-by-hop.
    let mut fwd_headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if !is_hop_header(name) {
            fwd_headers.insert(name.clone(), value.clone());
        }
    }

    let mut body_bytes = axum::body::to_bytes(req.into_body(), MAX_REQ_BYTES).await?;

    // Autorouter mode: rewrite the request `model` by complexity (classify-once per session) and
    // swap in the gateway's auth. In transparent mode this whole block is skipped — no body parse.
    let mut routed_model: Option<String> = None;
    if let Some(routing) = &state.routing {
        if let Ok(mut v) = serde_json::from_slice::<Value>(&body_bytes) {
            if let Some(model) = routing.target_model(session_id.as_deref(), &v) {
                routed_model = Some(model.clone());
                v["model"] = Value::String(model);
                if let Ok(nb) = serde_json::to_vec(&v) {
                    body_bytes = Bytes::from(nb);
                }
            }
        }
        // The gateway owns auth now: drop Claude Code's credentials and present the stored key.
        fwd_headers.remove("authorization");
        fwd_headers.remove("x-api-key");
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", routing.api_key)) {
            fwd_headers.insert(HeaderName::from_static("authorization"), val);
        }
    }

    let request_json: Option<serde_json::Value> = if state.logger.log_bodies {
        serde_json::from_slice(&body_bytes).ok()
    } else {
        None
    };

    let url = format!("{}{}", state.upstream, path_and_query);
    let upstream_resp = state
        .client
        .request(method.clone(), &url)
        .headers(fwd_headers)
        .body(body_bytes.to_vec())
        .send()
        .await?;

    let status = upstream_resp.status();
    let is_stream = upstream_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("event-stream"))
        .unwrap_or(false);

    // Copy response headers (minus framing/encoding we manage ourselves).
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers() {
        match name.as_str() {
            "transfer-encoding" | "content-length" | "content-encoding" | "connection" => {}
            _ => {
                builder = builder.header(name, value);
            }
        }
    }

    let path_log = path_and_query.split('?').next().unwrap_or("").to_string();
    let method_s = method.to_string();
    let status_u = status.as_u16();
    let session_id_stream = session_id.clone();

    if is_stream {
        // Tee the SSE stream: forward each chunk to the client, feed a copy to the parser,
        // and log once the stream completes.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
        let logger = state.logger.clone();
        state.tracker.spawn(async move {
            let mut parser = StreamParser::new();
            let mut stream = upstream_resp.bytes_stream();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        parser.feed(&chunk);
                        if tx.send(Ok(chunk)).await.is_err() {
                            break; // client hung up
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
                            .await;
                        break;
                    }
                }
            }
            let usage = parser.finish();
            let mut rec = RequestRecord::from_usage(
                ts,
                method_s,
                path_log,
                status_u,
                true,
                start.elapsed().as_millis() as u64,
                usage,
            );
            rec.session_id = session_id_stream;
            if logger.log_bodies {
                rec.request_body = request_json;
            }
            logger.log(rec);
        });

        let body_stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(builder.body(Body::from_stream(body_stream))?)
    } else {
        // Buffer the full response, parse usage, log, return.
        let resp_bytes = upstream_resp.bytes().await?;
        let usage: Usage = usage::parse_non_stream(&resp_bytes);
        let mut rec = RequestRecord::from_usage(
            ts,
            method_s,
            path_log,
            status_u,
            false,
            start.elapsed().as_millis() as u64,
            usage,
        );
        rec.session_id = session_id;
        if state.logger.log_bodies {
            rec.request_body = request_json;
            rec.response_body = serde_json::from_slice(&resp_bytes).ok();
        }
        state.logger.log(rec);

        // Autorouter param-mismatch recovery: Claude Code builds params for the model it *thinks*
        // it's using (e.g. opus-only `speed`/`thinking`). When the routed model rejects one with a
        // 400, point CC at the routed model so it sends compatible params next time, and replace the
        // raw upstream error with a clear "retry" message in the TUI.
        let out_bytes = match (status.as_u16(), &routed_model) {
            (400, Some(model)) => match param_error(&resp_bytes) {
                Some(param) => {
                    // Don't print to stderr — it would corrupt CC's TUI. The recovery notice is
                    // delivered through the response body below, which CC renders correctly.
                    let _ = crate::settings::set_claude_model(model);
                    let msg = format!(
                        "lite autorouter: the routed model `{model}` rejected the `{param}` \
                         parameter that Claude Code sent for its previous model. I've switched \
                         Claude Code's model to `{model}` — please send your message again. If it \
                         still fails, restart Claude Code so it picks up the new model."
                    );
                    let err = serde_json::json!({
                        "type": "error",
                        "error": { "type": "invalid_request_error", "message": msg },
                    });
                    Bytes::from(serde_json::to_vec(&err).unwrap_or_default())
                }
                None => resp_bytes,
            },
            _ => resp_bytes,
        };
        Ok(builder.body(Body::from(out_bytes))?)
    }
}

/// Detect an upstream "this model doesn't support feature/param X" 400 and extract the offending
/// name. Covers both shapes we've seen: "does not support the `speed` parameter" and
/// `"thinking.type.enabled" is not supported for this model`. Works on the raw body so it tolerates
/// litellm's wrapped/escaped error string. Returns the param name (best-effort) when matched.
fn param_error(body: &[u8]) -> Option<String> {
    let raw = std::str::from_utf8(body).ok()?;
    // Un-escape JSON-embedded quotes so the inner provider message reads naturally.
    let norm = raw.replace("\\\"", "\"");
    let lower = norm.to_lowercase();
    if !(lower.contains("not support") || lower.contains("unsupported") || lower.contains("parameter"))
    {
        return None;
    }
    // Strip whitespace and the stray backslashes/quotes left by double-escaped JSON.
    let clean = |s: &str| s.trim().trim_matches(|c| c == '\\' || c == '"').trim().to_string();
    // Prefer a backticked token (`speed`); else the quoted token just before "not supported".
    if let Some(tok) = norm.split('`').nth(1).map(clean).filter(|s| !s.is_empty() && !s.contains(' '))
    {
        return Some(tok);
    }
    if let Some(idx) = lower.find("not support") {
        if let Some(tok) = norm[..idx]
            .rsplit('"')
            .nth(1)
            .map(clean)
            .filter(|s| !s.is_empty() && !s.contains(' '))
        {
            return Some(tok);
        }
    }
    Some("an unsupported parameter".to_string())
}
