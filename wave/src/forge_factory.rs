//! Construct an authenticated [`forge::Forge`] from an environment token —
//! no internal auth tooling, so `wave` stays a generic, standalone public tool.
//!
//! Token resolution (first non-empty wins):
//! - GitLab: `GITLAB_TOKEN`, then `FORGE_TOKEN`;
//! - GitHub: `GITHUB_TOKEN`, then `FORGE_TOKEN`.

use anyhow::{bail, Result};
use forge::{Forge, ForgeKind};

/// Parse a `--forge` value.
pub fn parse_forge_kind(s: &str) -> Result<ForgeKind> {
    match s {
        "github" => Ok(ForgeKind::Github),
        "gitlab" => Ok(ForgeKind::Gitlab),
        other => bail!("unknown forge {other} (use github|gitlab)"),
    }
}

/// The effective host for `kind` (`host` empty → the forge default).
#[must_use]
pub fn effective_host(kind: ForgeKind, host: &str) -> String {
    if !host.is_empty() {
        host.to_string()
    } else if kind == ForgeKind::Github {
        "github.com".to_string()
    } else {
        "gitlab.com".to_string()
    }
}

/// Resolve the API token for `kind` from the environment.
pub fn token_for(kind: ForgeKind) -> Result<String> {
    let candidates: &[&str] = match kind {
        ForgeKind::Gitlab => &["GITLAB_TOKEN", "FORGE_TOKEN"],
        ForgeKind::Github => &["GITHUB_TOKEN", "FORGE_TOKEN"],
        _ => &["FORGE_TOKEN"],
    };
    for var in candidates {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    bail!("no token in env — set {} (or FORGE_TOKEN)", candidates[0]);
}

/// Build a forge adapter for `kind` on `host` with `token`.
pub fn build_forge(kind: ForgeKind, host: &str, token: &str) -> Result<Box<dyn Forge>> {
    match kind {
        ForgeKind::Github => Ok(Box::new(forge::github::GitHubForge::new(token.to_string())?)),
        ForgeKind::Gitlab => Ok(Box::new(forge::gitlab::GitLabForge::new(
            host.to_string(),
            token.to_string(),
        )?)),
        other => bail!("unsupported forge kind: {other:?}"),
    }
}
