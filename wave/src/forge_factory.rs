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
///
/// The adapter is wrapped in [`forge::runtime::pin`], which runs its HTTP on a
/// runtime the `forge` crate owns. That is not optional here: under Bazel `forge`
/// resolves its own `crate_universe`, so its hyper links against **forge's**
/// tokio while wave awaits on **wave's** — two reactor thread-locals, and the
/// first DNS resolution panics
///
/// ```text
/// there is no reactor running, must be called from the context of a Tokio 1.x runtime
/// ```
///
/// on `main`, killing the process. That is why every `wave-discover-*` CronJob
/// has failed on every run. See `forge::runtime` for the full account.
pub fn build_forge(kind: ForgeKind, host: &str, token: &str) -> Result<Box<dyn Forge>> {
    let inner: Box<dyn Forge> = match kind {
        ForgeKind::Github => Box::new(forge::github::GitHubForge::new(token.to_string())?),
        ForgeKind::Gitlab => Box::new(forge::gitlab::GitLabForge::new(
            host.to_string(),
            token.to_string(),
        )?),
        other => bail!("unsupported forge kind: {other:?}"),
    };
    Ok(Box::new(forge::runtime::pin_boxed(inner)))
}
