mod dashboard;
mod log;
mod logs_cmd;
mod proxy;
mod usage;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::log::Logger;
use crate::proxy::ProxyState;

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
    /// Live web dashboard of usage (reads ~/.lite/logs)
    Dashboard(DashboardArgs),
    /// Print / tail the latest session log
    Logs(LogsArgs),
}

#[derive(Args)]
struct ClaudeArgs {
    /// Upstream base URL (default: $ANTHROPIC_BASE_URL or api.anthropic.com)
    #[arg(long)]
    upstream: Option<String>,
    /// Fixed proxy port (default: ephemeral)
    #[arg(long)]
    port: Option<u16>,
    /// Log directory (default: ~/.lite/logs)
    #[arg(long)]
    log_dir: Option<PathBuf>,
    /// Log full request + response bodies
    #[arg(long)]
    bodies: bool,
    /// Also start the web dashboard and open it in the browser
    #[arg(long)]
    dashboard: bool,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Claude(args) => run_claude(args).await,
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
    let upstream = args
        .upstream
        .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string())
        .trim_end_matches('/')
        .to_string();

    let log_dir = args.log_dir.unwrap_or_else(default_log_dir);
    let session_ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let logger = Arc::new(Logger::new(&log_dir, &session_ts, args.bodies)?);

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
    });

    let app = axum::Router::new()
        .fallback(proxy::handle)
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Optional dashboard.
    if args.dashboard {
        let dash_dir = log_dir.clone();
        tokio::spawn(async move {
            let _ = dashboard::serve(DEFAULT_DASHBOARD_PORT, dash_dir).await;
        });
        let url = format!("http://localhost:{DEFAULT_DASHBOARD_PORT}");
        open_browser(&url);
        eprintln!("lite dashboard: {url}");
    }

    eprintln!(
        "lite: proxy on http://127.0.0.1:{proxy_port} -> {upstream}\nlite: logging to {}",
        logger.session_path.display()
    );

    // Launch claude pointed at the proxy. Claude applies settings.json `env` over the process
    // environment, so a plain env override is not enough — inject the base URL via `--settings`,
    // which has higher precedence than user settings. Auth token is left untouched so it flows
    // through unchanged.
    let base_url = format!("http://127.0.0.1:{proxy_port}");
    let settings = format!("{{\"env\":{{\"ANTHROPIC_BASE_URL\":\"{base_url}\"}}}}");
    let status = tokio::process::Command::new("claude")
        .arg("--settings")
        .arg(&settings)
        .args(&args.claude_args)
        .env("ANTHROPIC_BASE_URL", &base_url)
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
    if !s.by_model.is_empty() {
        eprintln!("  by model:");
        for (model, t) in &s.by_model {
            eprintln!(
                "    {model}: {} req, {} in, {} out",
                t.requests, t.input_tokens, t.output_tokens
            );
        }
    }
    eprintln!("  log: {}", logger.session_path.display());
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
