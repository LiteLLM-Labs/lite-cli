# lite-cli

A Rust CLI that wraps [Claude Code](https://github.com/anthropics/claude-code) with a
transparent **logging proxy** — see every request, model, token count, and latency without
changing how you use `claude`. Inspired by [headroom](https://github.com/headroomlabs-ai/headroom),
but observe-only: it logs traffic, it does not transform it.

```
claude  ──>  lite proxy (localhost)  ──>  upstream API (Anthropic / LiteLLM gateway)
                  │
                  └── JSONL logs + live web dashboard
```

## Install

```sh
./install.sh                 # builds release, installs to ~/.local/bin, re-signs on macOS
PREFIX=/usr/local/bin ./install.sh   # custom install location
```

Or manually:

```sh
cargo build --release
cp target/release/lite ~/.local/bin/lite
codesign -s - -f ~/.local/bin/lite   # macOS only — see note below
```

> **macOS note:** `cp` invalidates the binary's ad-hoc code signature on Apple Silicon, after
> which the kernel kills it on launch (`zsh: killed`, exit 137). Re-sign with
> `codesign -s - -f <path>` after copying. `install.sh` does this automatically.

## Usage

```sh
lite claude                      # launch Claude Code through the proxy, log everything
lite claude --dashboard          # also open the live web dashboard
lite claude -- --model opus      # args after `--` are forwarded to claude

lite dashboard                   # live web UI at http://localhost:4097
lite logs                        # latest session as a table
lite logs --follow               # live tail
```

### Flags (`lite claude`)

| Flag | Default | Description |
|------|---------|-------------|
| `--upstream <url>` | `$ANTHROPIC_BASE_URL` or `api.anthropic.com` | upstream base URL |
| `--port <n>` | ephemeral | fixed proxy port |
| `--log-dir <path>` | `~/.lite/logs` | log directory |
| `--bodies` | off | log full request + response bodies |
| `--dashboard` | off | also start the web dashboard + open browser |

## Spend tracking

Per-request USD cost is computed from LiteLLM's
[`model_prices_and_context_window.json`](https://github.com/BerriAI/litellm/blob/litellm_internal_staging/model_prices_and_context_window.json)
(fetched once, cached to `~/.lite/model_prices.json`, refreshed every 24h). The math is a faithful
port of litellm's `generic_cost_per_token` token path — including separate input / output /
cache-read / cache-write rates and long-context (`_above_Nk_tokens`) tiered pricing, where the
threshold is the **total** context (input + cache read + cache write). Verified to match litellm's
function exactly. Spend shows in the session summary, the `lite logs` table, and the dashboard
SPEND card.

## Where logs live

`~/.lite/logs/session-<timestamp>.jsonl` — one JSON object per API call (model, input/output
tokens, cache reads, latency, status). `~/.lite/logs/latest` points at the active session.

## How it redirects Claude Code

Claude Code reads `ANTHROPIC_BASE_URL` from `~/.claude/settings.json` (`env` block), which
overrides the process environment. So `lite` injects the proxy URL via
`claude --settings '{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:<port>"}}'`, which has higher
precedence. Your auth token is left untouched and forwarded verbatim by the proxy.

## License

MIT
