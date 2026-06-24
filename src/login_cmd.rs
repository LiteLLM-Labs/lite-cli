//! `lite login` — store the LiteLLM gateway base URL + api key in `~/.lite/settings.json`.
//!
//! These feed autorouter mode: `lite autorouter` lists the gateway's models, and `lite claude`
//! points upstream at the gateway and authenticates with the stored key.

use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::settings::{redact, Settings};

pub fn run() -> Result<()> {
    let mut settings = Settings::load();

    let api_base = prompt_line("API base", settings.api_base.as_deref())?;
    let api_key = prompt_secret("API key", settings.api_key.is_some())?;

    if !api_base.is_empty() {
        settings.api_base = Some(api_base.trim_end_matches('/').to_string());
    }
    if !api_key.is_empty() {
        settings.api_key = Some(api_key);
    }

    if settings.api_base.is_none() || settings.api_key.is_none() {
        anyhow::bail!("both an API base and an API key are required");
    }

    settings.save()?;

    eprintln!(
        "lite: saved to {}\n  api_base: {}\n  api_key:  {}",
        Settings::path()?.display(),
        settings.api_base.as_deref().unwrap_or(""),
        settings.api_key.as_deref().map(redact).unwrap_or_default(),
    );
    eprintln!("next: run `lite autorouter` to pick tier models.");
    Ok(())
}

/// Prompt for a plain line; empty input keeps the current value (shown when present).
fn prompt_line(label: &str, current: Option<&str>) -> Result<String> {
    match current {
        Some(c) => print!("{label} [{c}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).context("reading input")?;
    Ok(buf.trim().to_string())
}

/// Prompt for a secret without echoing; empty input keeps the current value. Falls back to a plain
/// (unmasked) stdin read when stdin isn't a terminal, so the command stays scriptable.
fn prompt_secret(label: &str, has_current: bool) -> Result<String> {
    use std::io::IsTerminal;
    let suffix = if has_current { " [keep existing]" } else { "" };
    if std::io::stdin().is_terminal() {
        let prompt = format!("{label}{suffix}: ");
        let secret = rpassword::prompt_password(&prompt).context("reading secret")?;
        Ok(secret.trim().to_string())
    } else {
        prompt_line(&format!("{label}{suffix}"), None)
    }
}
