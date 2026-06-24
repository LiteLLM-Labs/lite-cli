//! `lite autorouter` — pull the gateway's model list and interactively assign one model to each
//! complexity tier (simple / medium / complex / reasoning), writing the picks to
//! `~/.lite/settings.json`. Once all four are set, `lite claude` routes by complexity.

use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::settings::Settings;

pub async fn run() -> Result<()> {
    let mut settings = Settings::load();

    let (Some(api_base), Some(api_key)) = (settings.api_base.clone(), settings.api_key.clone())
    else {
        anyhow::bail!("no gateway credentials — run `lite login` first");
    };

    let models = match fetch_models(&api_base, &api_key).await {
        Ok(m) if !m.is_empty() => {
            eprintln!("lite: pulled {} models from {api_base}", m.len());
            m
        }
        Ok(_) => {
            eprintln!("lite: gateway returned no models; enter model names manually.");
            Vec::new()
        }
        Err(e) => {
            eprintln!("lite: could not list models ({e}); enter model names manually.");
            Vec::new()
        }
    };

    settings.simple_model = Some(pick("simple", settings.simple_model.as_deref(), &models)?);
    settings.medium_model = Some(pick("medium", settings.medium_model.as_deref(), &models)?);
    settings.complex_model = Some(pick("complex", settings.complex_model.as_deref(), &models)?);
    settings.reasoning_model =
        Some(pick("reasoning", settings.reasoning_model.as_deref(), &models)?);

    settings.save()?;

    eprintln!("\nlite: autorouter configured:");
    eprintln!("  simple    -> {}", settings.simple_model.as_deref().unwrap_or(""));
    eprintln!("  medium    -> {}", settings.medium_model.as_deref().unwrap_or(""));
    eprintln!("  complex   -> {}", settings.complex_model.as_deref().unwrap_or(""));
    eprintln!("  reasoning -> {}", settings.reasoning_model.as_deref().unwrap_or(""));
    eprintln!("\nrun `lite claude` to route by complexity.");
    Ok(())
}

/// GET `{api_base}/v1/models` (OpenAI-compatible list) → `data[].id`.
async fn fetch_models(api_base: &str, api_key: &str) -> Result<Vec<String>> {
    let url = format!("{}/v1/models", api_base.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .context("requesting model list")?;
    if !resp.status().is_success() {
        anyhow::bail!("gateway returned {}", resp.status());
    }
    let text = resp.text().await.context("reading model list")?;
    let json: serde_json::Value = serde_json::from_str(&text).context("parsing model list")?;
    let ids = json
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ids)
}

/// Pick a model for a tier. On a terminal with a model list, use a type-to-filter fuzzy picker
/// (488-model lists are unusable as a static menu). Otherwise fall back to a numbered menu /
/// free-text entry so the command stays scriptable.
fn pick(tier: &str, current: Option<&str>, models: &[String]) -> Result<String> {
    use std::io::IsTerminal;
    if !models.is_empty() && std::io::stdin().is_terminal() {
        // Pre-highlight the currently-configured model in the list (no need to also name it in the
        // prompt — that collides with dialoguer's "<prompt>: <selection>" echo and reads as two
        // values).
        let default_idx = current
            .and_then(|c| models.iter().position(|m| m == c))
            .unwrap_or(0);
        let idx = dialoguer::FuzzySelect::new()
            .with_prompt(format!("{tier} model"))
            .items(models)
            .default(default_idx)
            // Cap the visible window — without this the filtered list (hundreds of models) renders
            // taller than the terminal and scrolls the screen on every keystroke.
            .max_length(8)
            .interact()
            .context("selecting model")?;
        return Ok(models[idx].clone());
    }
    pick_fallback(tier, current, models)
}

/// Non-interactive picker: numbered menu, or free-text when no list is available.
fn pick_fallback(tier: &str, current: Option<&str>, models: &[String]) -> Result<String> {
    if models.is_empty() {
        let line = prompt(&format!(
            "{tier} model{}: ",
            current.map(|c| format!(" [{c}]")).unwrap_or_default()
        ))?;
        let line = line.trim();
        if line.is_empty() {
            if let Some(c) = current {
                return Ok(c.to_string());
            }
            anyhow::bail!("no model entered for {tier} tier");
        }
        return Ok(line.to_string());
    }

    eprintln!("\nselect {tier} model:");
    for (i, m) in models.iter().enumerate() {
        let marker = if Some(m.as_str()) == current { " (current)" } else { "" };
        eprintln!("  {:>2}) {}{}", i + 1, m, marker);
    }
    let default_hint = current
        .and_then(|c| models.iter().position(|m| m == c))
        .map(|i| format!(" [{}]", i + 1))
        .unwrap_or_default();

    loop {
        let line = prompt(&format!("{tier} (number or model name){default_hint}: "))?;
        let line = line.trim();
        if line.is_empty() {
            if let Some(c) = current {
                return Ok(c.to_string());
            }
            eprintln!("  enter a number 1-{} or a model name", models.len());
            continue;
        }
        // A bare number selects from the list; anything else is taken as a literal model name
        // (lets you type a name directly without scanning the menu).
        match line.parse::<usize>() {
            Ok(n) if n >= 1 && n <= models.len() => return Ok(models[n - 1].clone()),
            Ok(_) => eprintln!("  number out of range 1-{}", models.len()),
            Err(_) => return Ok(line.to_string()),
        }
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).context("reading input")?;
    Ok(buf)
}
