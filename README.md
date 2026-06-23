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
cargo build --release
cp target/release/lite ~/.local/bin/    # or anywhere on your PATH
```

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
