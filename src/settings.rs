//! `~/.lite/settings.json`: gateway credentials + the four autorouter tier models.
//!
//! When all six fields are present (`routing_enabled`), `lite claude` switches from a transparent
//! proxy into autorouter mode: it points upstream at the gateway, picks a model per session by
//! complexity, and injects the gateway api key. With the file absent or incomplete, `lite claude`
//! stays a verbatim proxy — the routing fields are purely additive.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persisted config. Every field is optional so a partially-filled file (e.g. after `lite login`
/// but before `lite autorouter`) still loads cleanly.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simple_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complex_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_model: Option<String>,
}

impl Settings {
    /// `~/.lite/settings.json` (sibling of the existing `~/.lite/logs` and `model_prices.json`).
    pub fn path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(home.join(".lite").join("settings.json"))
    }

    /// Load settings, or `Default` (all `None`) if the file is missing or unparseable.
    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Persist to `~/.lite/settings.json` with owner-only (0600) permissions — the file holds the
    /// gateway api key.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating ~/.lite")?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Self::restrict_perms(&path)?;
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_perms(path: &std::path::Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("chmod 0600 settings.json")
    }

    #[cfg(not(unix))]
    fn restrict_perms(_path: &std::path::Path) -> Result<()> {
        Ok(())
    }

    /// True when both gateway creds and all four tier models are set — the precondition for the
    /// proxy to route instead of forwarding verbatim.
    pub fn routing_enabled(&self) -> bool {
        let nonempty = |o: &Option<String>| o.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
        nonempty(&self.api_base)
            && nonempty(&self.api_key)
            && nonempty(&self.simple_model)
            && nonempty(&self.medium_model)
            && nonempty(&self.complex_model)
            && nonempty(&self.reasoning_model)
    }
}

/// Point Claude Code at `model` by writing the `model` field of `~/.claude/settings.json`, so CC
/// builds requests (and their params) for that model instead of its previous one. All other fields
/// are preserved. Used to recover from a routed model rejecting a param CC only sends for its old
/// model (e.g. opus-only `speed`/`thinking`).
pub fn set_claude_model(model: &str) -> Result<()> {
    let path = dirs::home_dir()
        .context("no home directory")?
        .join(".claude")
        .join("settings.json");
    let mut json: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    json["model"] = serde_json::Value::String(model.to_string());
    let out = serde_json::to_string_pretty(&json)?;
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Redact a secret for display: keep a hint of the tail, hide the rest.
pub fn redact(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 4 {
        return "*".repeat(n);
    }
    let tail: String = secret.chars().skip(n.saturating_sub(4)).collect();
    format!("…{tail}")
}
