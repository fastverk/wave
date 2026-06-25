//! Concrete registry datasources (HTTP).
//!
//! The [`Datasource`](wave_core::Datasource) trait + the pure discovery
//! orchestration live in `wave-core`; the network implementations live here,
//! next to the REST repo-enumeration in [`enumerate`](crate::enumerate), so
//! `wave-core` stays I/O-light and unit-testable with mock datasources.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use wave_core::{Datasource, EdgeKind, VersionInfo};

/// The public npm registry — reads `dist-tags.latest` from a package's
/// (abbreviated) packument. Internal scopes (`@aion/`, `@savvi-studio/`) are
/// filtered out upstream by the discovery partition, so this never has to reach
/// a private GitLab registry.
pub struct NpmDatasource {
    http: reqwest::Client,
    /// Registry base URL (no trailing slash). Default: `https://registry.npmjs.org`.
    registry: String,
}

impl NpmDatasource {
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            registry: "https://registry.npmjs.org".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Packument {
    #[serde(rename = "dist-tags", default)]
    dist_tags: DistTags,
}

#[derive(Deserialize, Default)]
struct DistTags {
    #[serde(default)]
    latest: Option<String>,
}

#[async_trait]
impl Datasource for NpmDatasource {
    fn kind(&self) -> EdgeKind {
        EdgeKind::Npm
    }

    async fn latest(&self, package: &str) -> Result<Option<VersionInfo>> {
        // Scoped names (`@scope/name`) percent-encode their slash for the
        // packument path; bare names pass through.
        let path = if package.starts_with('@') {
            package.replacen('/', "%2F", 1)
        } else {
            package.to_string()
        };
        let url = format!("{}/{path}", self.registry);
        let resp = self
            .http
            .get(&url)
            // Abbreviated metadata: same dist-tags, far smaller than the full
            // packument (which carries every version's manifest).
            .header("Accept", "application/vnd.npm.install-v1+json")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("npm packument for {package}"))?;
        let pack: Packument = resp
            .json()
            .await
            .with_context(|| format!("parse npm packument for {package}"))?;
        Ok(pack.dist_tags.latest.map(|version| VersionInfo {
            version,
            digest: None,
            released_at: None,
        }))
    }
}
