//! Transparent logging proxy: forwards every request to the upstream Anthropic-compatible
//! API unchanged, while tapping token usage out of the response.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::StreamExt;

use crate::log::{Logger, RequestRecord};
use crate::usage::{self, StreamParser, Usage};

pub struct ProxyState {
    pub client: reqwest::Client,
    pub upstream: String, // no trailing slash
    pub logger: Arc<Logger>,
    /// Tracks in-flight stream-logging tasks so they can be drained on shutdown.
    pub tracker: tokio_util::task::TaskTracker,
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

    // Forward headers minus hop-by-hop.
    let mut fwd_headers = HeaderMap::new();
    for (name, value) in req.headers() {
        if !is_hop_header(name) {
            fwd_headers.insert(name.clone(), value.clone());
        }
    }

    let body_bytes = axum::body::to_bytes(req.into_body(), MAX_REQ_BYTES).await?;

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
        if state.logger.log_bodies {
            rec.request_body = request_json;
            rec.response_body = serde_json::from_slice(&resp_bytes).ok();
        }
        state.logger.log(rec);
        Ok(builder.body(Body::from(resp_bytes))?)
    }
}
