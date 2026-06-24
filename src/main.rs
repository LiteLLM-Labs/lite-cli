mod autorouter_cmd;
mod classifier;
mod dashboard;
mod log;
mod login_cmd;
mod logs_cmd;
mod pricing;
mod proxy;
mod settings;
mod statusline_cmd;
mod transcripts;
mod usage;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::log::Logger;
use crate::proxy::{ProxyState, RoutingConfig};
use crate::settings::Settings;

const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
const DEFAULT_DASHBOARD_PORT: u16 = 4097;

#[derive(Parser)]
#[command(
    name = "lite",
    version,
    about = "Wrap Claude Code with a transparent logging proxy"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch Claude Code through the logging proxy
    Claude(ClaudeArgs),
    /// Store LiteLLM gateway base URL + api key in ~/.lite/settings.json
    Login,
    /// Pick a model per complexity tier from the gateway (enables autorouting)
    Autorouter,
    /// Print one status-bar line for Claude Code (used internally via injected statusLine)
    Statusline,
    /// Live web dashboard of usage (reads ~/.lite/logs)
    Dashboard(DashboardArgs),
    /// Print / tail the latest session log
    Logs(LogsArgs),
}

#[derive(Args)]
struct ClaudeArgs {
    /// Upstream base URL (default: $ANTHROPIC_BASE_URL or api.anthropic.com)
    #[arg(long = "litellm_upstream")]
    upstream: Option<String>,
    /// Fixed proxy port (default: ephemeral)
    #[arg(long = "litellm_port")]
    port: Option<u16>,
    /// Log directory (default: ~/.lite/logs)
    #[arg(long = "litellm_log_dir")]
    log_dir: Option<PathBuf>,
    /// Log full request + response bodies
    #[arg(long = "litellm_bodies")]
    bodies: bool,
    /// Also start the web dashboard and open it in the browser
    #[arg(long = "litellm_dashboard")]
    dashboard: bool,
    /// Enable rtk for this session: inject rtk's PreToolUse hook so Bash commands are
    /// rewritten to token-saving `rtk` equivalents (requires `rtk` on PATH).
    #[arg(long = "litellm_enable_rtk")]
    litellm_enable_rtk: bool,
    /// Arguments forwarded verbatim to `claude` (after `--`)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    claude_args: Vec<String>,
}

#[derive(Args)]
struct DashboardArgs {
    #[arg(long, default_value_t = DEFAULT_DASHBOARD_PORT)]
    port: u16,
    #[arg(long)]
    log_dir: Option<PathBuf>,
}

#[derive(Args)]
struct LogsArgs {
    /// Follow the log (live tail)
    #[arg(long, short)]
    follow: bool,
    /// Specific session file (default: latest)
    #[arg(long)]
    session: Option<PathBuf>,
    #[arg(long)]
    log_dir: Option<PathBuf>,
}

fn default_log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lite")
        .join("logs")
}

/// Hoist `--litellm_*` flags (lite's reserved namespace) to right after the `claude`
/// subcommand so they parse as lite flags no matter where the user typed them — otherwise
/// clap's trailing-arg capture would forward them to `claude`. Tokens after a lone `--`
/// are left untouched (those are explicitly claude's).
fn reorder_litellm_flags(args: Vec<String>) -> Vec<String> {
    let Some(claude_idx) = args.iter().position(|a| a == "claude") else {
        return args;
    };
    let sep = args.iter().position(|a| a == "--").unwrap_or(args.len());

    let mut hoist = Vec::new();
    let mut rest = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if i > claude_idx && i < sep && a.starts_with("--litellm_") {
            hoist.push(a.clone());
        } else if i > claude_idx && i < sep {
            rest.push(a.clone());
        }
    }
    if hoist.is_empty() {
        return args;
    }
    let mut out = args[..=claude_idx].to_vec();
    out.extend(hoist);
    out.extend(rest);
    out.extend(args[sep..].iter().cloned());
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_from(reorder_litellm_flags(std::env::args().collect()));
    match cli.command {
        Commands::Claude(args) => run_claude(args).await,
        Commands::Login => login_cmd::run(),
        Commands::Autorouter => autorouter_cmd::run().await,
        Commands::Statusline => statusline_cmd::run(),
        Commands::Dashboard(args) => {
            let log_dir = args.log_dir.unwrap_or_else(default_log_dir);
            dashboard::serve(args.port, log_dir).await
        }
        Commands::Logs(args) => {
            let log_dir = args.log_dir.unwrap_or_else(default_log_dir);
            logs_cmd::run(log_dir, args.session, args.follow).await
        }
    }
}

async fn run_claude(args: ClaudeArgs) -> Result<()> {
    // Fail fast before binding anything if rtk was requested but isn't installed.
    if args.litellm_enable_rtk && !rtk_available() {
        anyhow::bail!(
            "--litellm_enable_rtk needs `rtk` on PATH. Install it:\n  \
             brew install rtk   (or: curl -fsSL https://raw.githubusercontent.com/rtk-ai/rtk/master/install.sh | sh)\n\
             then re-run. See https://github.com/rtk-ai/rtk"
        );
    }

    // Autorouter mode: if ~/.lite/settings.json carries gateway creds + all four tier models, the
    // proxy routes by complexity instead of forwarding verbatim. Absent/partial config → transparent
    // proxy, exactly as before.
    let lite_settings = Settings::load();
    let routing = lite_settings
        .routing_enabled()
        .then(|| RoutingConfig::from_settings(&lite_settings));

    // Resolve upstream the same way claude resolves it: explicit flag, then (in routing mode) the
    // gateway api_base, then process env, then ~/.claude/settings.json `env`, then the public
    // default. Pulling from settings keeps the upstream and the auth token in sync when neither is
    // exported in the shell.
    let upstream = args
        .upstream
        .filter(|s| !s.is_empty())
        .or_else(|| {
            routing
                .as_ref()
                .and_then(|_| lite_settings.api_base.clone())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok().filter(|s| !s.is_empty()))
        .or_else(|| settings_env("ANTHROPIC_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string())
        .trim_end_matches('/')
        .to_string();

    let log_dir = args.log_dir.unwrap_or_else(default_log_dir);
    let session_ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let pricing = pricing::Pricing::load().await;
    let logger = Arc::new(Logger::new(&log_dir, &session_ts, args.bodies, pricing)?);

    // Bind proxy listener (ephemeral unless --port given).
    let bind_port = args.port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", bind_port))
        .await
        .context("binding proxy port")?;
    let proxy_port = listener.local_addr()?.port();

    let client = reqwest::Client::builder()
        .build()
        .context("building http client")?;
    let tracker = tokio_util::task::TaskTracker::new();
    let state = Arc::new(ProxyState {
        client,
        upstream: upstream.clone(),
        logger: logger.clone(),
        tracker: tracker.clone(),
        routing,
    });

    let app = axum::Router::new()
        .fallback(proxy::handle)
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Always start the dashboard server in the background so the in-TUI status-line link works.
    // (Bind failure — e.g. another lite already serving it — is fine; the URL still resolves.)
    let dash_url = format!("http://localhost:{DEFAULT_DASHBOARD_PORT}");
    {
        let dash_dir = log_dir.clone();
        tokio::spawn(async move {
            let _ = dashboard::serve(DEFAULT_DASHBOARD_PORT, dash_dir).await;
        });
    }
    // Only pop the browser when explicitly asked.
    if args.dashboard {
        open_browser(&dash_url);
        eprintln!("lite dashboard: {dash_url}");
    }

    eprintln!(
        "lite: proxy on http://127.0.0.1:{proxy_port} -> {upstream}\nlite: logging to {}",
        logger.session_path.display()
    );
    if lite_settings.routing_enabled() {
        eprintln!(
            "lite: autorouter ON (simple={}, medium={}, complex={}, reasoning={})",
            lite_settings.simple_model.as_deref().unwrap_or(""),
            lite_settings.medium_model.as_deref().unwrap_or(""),
            lite_settings.complex_model.as_deref().unwrap_or(""),
            lite_settings.reasoning_model.as_deref().unwrap_or(""),
        );
    }

    // Point claude at the proxy. claude reads ANTHROPIC_BASE_URL from settings.json `env`, which
    // overrides the process environment — so inject the override via `--settings` (higher
    // precedence, and it *merges* with the user's `env`, preserving their auth token). claude
    // sends its own Authorization header; the proxy forwards it verbatim, so we do no auth work.
    let base_url = format!("http://127.0.0.1:{proxy_port}");
    let mut settings = serde_json::json!({ "env": { "ANTHROPIC_BASE_URL": base_url } });

    // Surface the dashboard inside CC via its status bar (a persistent bottom line fed by a command)
    // — the only way to show info in the TUI without lite writing to the terminal and corrupting it.
    let lite_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "lite".to_string());
    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!("{lite_exe} statusline"),
    });
    if args.litellm_enable_rtk {
        // rtk's own PreToolUse hook — injected for this session only (merged via --settings),
        // so the user's global ~/.claude/settings.json is never modified.
        settings["hooks"] = serde_json::json!({
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "rtk hook claude" }]
            }]
        });
        eprintln!("lite: rtk enabled (Bash commands rewritten to rtk for this session)");
    }
    let settings = settings.to_string();
    let status = tokio::process::Command::new("claude")
        .env("LITE_DASHBOARD_URL", &dash_url)
        .arg("--settings")
        .arg(&settings)
        .args(&args.claude_args)
        .status()
        .await
        .context("launching `claude` (is it on PATH?)")?;

    // Drain in-flight stream-logging tasks (bounded) before reporting.
    server.abort();
    tracker.close();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), tracker.wait()).await;
    print_summary(&logger);
    std::process::exit(status.code().unwrap_or(0));
}

fn print_summary(logger: &Logger) {
    let s = logger.summary();
    eprintln!("\nlite session summary");
    eprintln!("  requests:      {}", s.requests);
    eprintln!("  input tokens:  {}", s.input_tokens);
    eprintln!("  output tokens: {}", s.output_tokens);
    eprintln!("  spend:         ${:.4}", s.cost_usd);
    if !s.by_model.is_empty() {
        eprintln!("  by model:");
        for (model, t) in &s.by_model {
            eprintln!(
                "    {model}: {} req, {} in, {} out, ${:.4}",
                t.requests, t.input_tokens, t.output_tokens, t.cost_usd
            );
        }
    }
    eprintln!("  log: {}", logger.session_path.display());
}

/// Read a single key from the `env` block of ~/.claude/settings.json.
fn settings_env(key: &str) -> Option<String> {
    let settings_path = dirs::home_dir()?.join(".claude").join("settings.json");
    let content = std::fs::read_to_string(settings_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("env")?
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// True if the `rtk` binary is callable on PATH.
fn rtk_available() -> bool {
    std::process::Command::new("rtk")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}
