# lite-cli — architecture & conventions

Guidance for agents (and humans) working on this codebase. Read before adding features.

## What lite-cli is

A **thin, transparent logging proxy** that wraps Claude Code. It launches `claude` pointed at a
local proxy, forwards every request to the real upstream **unchanged**, and observes traffic
(tokens, model, latency, spend). It is an *observe/spend* tool — it never transforms requests or
responses.

## Prime directive: stay thin

Do the minimum needed to observe. Claude Code already sends its own credentials and request
bodies; the proxy forwards them verbatim. We only do two things claude can't do for us:

1. **Redirect** claude to the proxy (inject `ANTHROPIC_BASE_URL` via `--settings`).
2. **Know the real upstream** to forward to (resolve from env → `~/.claude/settings.json`).

If you're tempted to parse, rewrite, or re-inject something claude already provides, stop — that's
almost certainly the wrong layer. (We learned this the hard way: an auth-token re-injection was
added, then deleted once we confirmed claude sends `Authorization` itself.)

## Autorouter mode — the one sanctioned exception

`lite login` + `lite autorouter` write `~/.lite/settings.json` (gateway `api_base`/`api_key` + a
model per complexity tier: `simple`/`medium`/`complex`/`reasoning`). When **all six** fields are
present (`Settings::routing_enabled`), `lite claude` stops being verbatim and routes:

- Upstream is the gateway `api_base`.
- The proxy **parses the request body, rewrites `model`** to the tier model, and **injects the
  gateway api key** (`Authorization: Bearer …`, dropping claude's own creds). This is the *only*
  place we knowingly violate "stay thin" — it is gated entirely behind `state.routing.is_some()`;
  with no config the transparent path is byte-for-byte unchanged.
- **Classify-once-lock**: the first turn of a session (keyed by `x-claude-code-session-id`) decides
  the tier via `classifier.rs`, and that tier is held for the whole session to keep Anthropic
  prompt caching stable. The haiku small-fast slot always routes to `simple_model` and never sets
  the lock.
- `classifier.rs` is a faithful port of litellm's `router_strategy/complexity_router` (rule-based,
  local, zero API calls) plus three Claude Code signals: the `thinking` field (→ reasoning), tool
  count (gentle boost), and conversation context size.

Keep routing logic in `classifier.rs` (pure scoring) and `proxy.rs` (the gated rewrite). Don't
leak tier knowledge into pricing/log/presenters — they still just observe the *served* model.

## Layers — what goes where

Data flows: **claude → proxy → upstream**, with the proxy teeing usage into a log, which readers
render. Each module has one job:

| Module | Responsibility | Must NOT |
|---|---|---|
| `main.rs` | CLI parsing, process orchestration, env/settings resolution, launching `claude`, printing the session summary. | Contain HTTP, parsing, or pricing logic. |
| `proxy.rs` | Transparent HTTP forward. Extract raw signals from the wire (status, headers like `x-claude-code-session-id`, streaming-ness) and the token usage. In autorouter mode only (gated on `state.routing`), rewrite `model` + inject gateway auth. | Make business decisions beyond routing. No cost math, no aggregation. |
| `settings.rs` | Read/write `~/.lite/settings.json` (gateway creds + tier models, 0600). `routing_enabled` gate. | Network, pricing, HTTP. |
| `classifier.rs` | Pure complexity scoring (port of litellm `complexity_router` + CC signals) → `Tier`. | Touch network/disk/state. Stateless. |
| `login_cmd.rs` / `autorouter_cmd.rs` | Interactive setup: store creds; list gateway models and assign tier models. | Run during `lite claude`. Setup-only. |
| `usage.rs` | Pure parsing of token usage from responses (SSE `message_start`/`message_delta` + non-stream JSON). | Touch the network, disk, or pricing. |
| `pricing.rs` | The **pure cost model** — a faithful port of litellm's `generic_cost_per_token` (incl. tiered/threshold + service-tier + 5m/1h cache split via `cost_detailed`). Fetch/cache the price table; given tokens, return USD. | Know about requests, logs, or the dashboard. Stateless math + a cached table. |
| `transcripts.rs` | **Pure reader of Claude Code's own logs** (`~/.claude/projects/**/*.jsonl`). One `Turn` per billable assistant response, de-duped by `message.id`. The spend source of truth. | Price anything, render, or write. |
| `log.rs` | **Source of truth for the proxy's live log.** Computes `cost_usd` once (via `pricing`), writes one JSONL line per request, maintains the in-memory summary. | Render or present. |
| `logs_cmd.rs` | **Read-only presenter** of the proxy live log. Reads `~/.lite` JSONL, renders a table / tail. | Re-derive per-request cost. |
| `dashboard.rs` | **Read-only spend presenter.** Reads `transcripts`, prices each turn via `pricing`, aggregates by session / project / model. | Re-derive cost outside `pricing`; touch proxy state. |
| `dashboard.html` | Pure presentation. Visual transforms (cumulative sums, bar widths, formatting) live here. | Contain pricing or model knowledge. |

## Core rules

1. **Cost is computed once, at log time.** `log.rs` calls `pricing` and stamps `cost_usd` into the
   record before persisting. The number on disk is canonical.
2. **Readers trust stored values.** `dashboard.rs` / `logs_cmd.rs` read `cost_usd`; they do not
   recompute it. The *one* allowed exception is **backfilling a missing value** for legacy records
   written before a field existed — backfill only when absent, never override.
3. **Aggregation is a reader concern.** Summing cost, computing hit rate / latency / by-session
   breakdowns is presentation — it belongs in the presenters, not in `log.rs` or `proxy.rs`.
4. **Pricing parity lives in `pricing.rs` only.** Tiered/threshold pricing, cache-rate logic, etc.
   are all there, with unit tests cross-checked against litellm's actual function. Don't scatter
   cost arithmetic elsewhere.
5. **The proxy is dumb on purpose.** It extracts wire facts and forwards bytes. If a new datum is
   needed, capture it in `proxy.rs`, persist it in `log.rs` (`RequestRecord`), and consume it in a
   presenter.

## Where to add a new feature

- **New metric from data we already log** (e.g. p95 latency, error rate) → compute in the presenter
  (`dashboard.rs` / `logs_cmd.rs`). No proxy/log changes.
- **New datum from the wire** (e.g. a request header, a response field) → extract in `proxy.rs`,
  add a field to `RequestRecord` in `log.rs`, then consume in a presenter.
- **New cost behavior** (e.g. a new provider's pricing quirk) → `pricing.rs`, with a unit test.
- **New CLI command / flag** → `main.rs` (clap). `litellm_*` is the reserved namespace for future
  LiteLLM gateway commands.

## Two data sources — know which to use

1. **Proxy live log** (`~/.lite/logs/session-<ts>.jsonl`) — written by `log.rs` while `lite claude`
   runs. Real-time, only covers proxied sessions, has latency/status. Consumed by `lite logs`.
   Use for **live, low-level HTTP observation**.
2. **Claude transcripts** (`~/.claude/projects/<enc-cwd>/<session-id>.jsonl`) — written by Claude
   Code itself. Complete, retroactive, every session, with model + full `usage` (incl. 5m/1h cache
   split and service tier). Consumed by the **dashboard** via `transcripts.rs`.
   Use for **spend analytics** — it's strictly better than the proxy for cost.

Rule of thumb: *spend → transcripts; live wire detail → proxy log.* Don't try to make the proxy log
the spend source — it's partial and lacks the cache split.

## Testing & verification

- `pricing.rs` has unit tests; keep them cross-checked against litellm (`generic_cost_per_token`).
- Verify end-to-end with a real headless call: `lite claude -- -p "say hi"`, then inspect the
  JSONL and `lite logs`. The headless path exercises the same proxy/log code as the TUI.

## Platform note

macOS Apple Silicon: `cp` invalidates the ad-hoc code signature, so the kernel kills the binary
(`zsh: killed`). `install.sh` re-signs after copy. Don't remove that step.
