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

/// An npm registry — reads `dist-tags.latest` from a package's (abbreviated)
/// packument.
///
/// Defaults to the public registry. Scope overrides mirror `.npmrc`'s own
/// `@scope:registry=…` model, so a first-party scope can resolve from a private
/// registry: GitLab's group npm endpoint
/// (`/api/v4/groups/<id>/-/packages/npm/`) is packument-compatible, serving the
/// same `dist-tags`, so it needs only a base URL + a token — no separate
/// datasource. Scope routing (rather than a second `EdgeKind::Npm` datasource)
/// is required because discovery picks a datasource by `kind()` alone, so a
/// second npm datasource would shadow this one for *every* package.
///
/// Only reachable for first-party scopes when
/// [`DiscoverConfig::include_internal`](wave_core::DiscoverConfig) is set;
/// otherwise the partition filters them out before any lookup.
pub struct NpmDatasource {
    http: reqwest::Client,
    /// Registry base URL (no trailing slash). Default: `https://registry.npmjs.org`.
    registry: String,
    /// Per-scope registry overrides, longest-prefix-first.
    scopes: Vec<ScopeRegistry>,
}

/// One `@scope:registry=…` override, with the token that authorizes it.
struct ScopeRegistry {
    /// Scope prefix including the trailing slash (e.g. `@aion/`).
    prefix: String,
    /// Registry base URL (no trailing slash).
    registry: String,
    /// GitLab PAT (`read_api`) sent as `PRIVATE-TOKEN`, when the registry needs one.
    token: Option<String>,
}

impl NpmDatasource {
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            registry: "https://registry.npmjs.org".to_string(),
            scopes: Vec::new(),
        }
    }

    /// Route `prefix`-scoped packages at `registry` instead of the default.
    /// `prefix` is normalized to end in `/` so `@aion` and `@aion/` behave alike
    /// (and `@aion` can never match `@aion-other/…`).
    #[must_use]
    pub fn with_scope(
        mut self,
        prefix: impl Into<String>,
        registry: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        let mut prefix = prefix.into();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        self.scopes.push(ScopeRegistry {
            prefix,
            registry: registry.into().trim_end_matches('/').to_string(),
            token,
        });
        // Longest prefix first, so a more specific scope override wins.
        self.scopes
            .sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        self
    }

    /// The registry + token that should serve `package`.
    fn route(&self, package: &str) -> (&str, Option<&str>) {
        self.scopes
            .iter()
            .find(|s| package.starts_with(&s.prefix))
            .map_or((self.registry.as_str(), None), |s| {
                (s.registry.as_str(), s.token.as_deref())
            })
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
        let (registry, token) = self.route(package);
        let url = format!("{registry}/{path}");
        let mut req = self
            .http
            .get(&url)
            // Abbreviated metadata: same dist-tags, far smaller than the full
            // packument (which carries every version's manifest).
            .header("Accept", "application/vnd.npm.install-v1+json");
        if let Some(token) = token {
            req = req.header("PRIVATE-TOKEN", token);
        }
        let resp = req
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

/// crates.io via the **sparse index** (`index.crates.io`) — static per-crate
/// files, no rate limit (the API would throttle a fleet of concurrent polls).
/// Each line is one published version; the latest non-yanked stable wins.
pub struct CargoDatasource {
    http: reqwest::Client,
    /// Sparse-index base URL (no trailing slash). Default: `https://index.crates.io`.
    index: String,
}

impl CargoDatasource {
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            index: "https://index.crates.io".to_string(),
        }
    }

    /// The sparse-index path for `name` (lowercased): `1/{n}`, `2/{n}`,
    /// `3/{a}/{n}`, else `{ab}/{cd}/{n}`.
    fn index_path(name: &str) -> String {
        let n = name.to_lowercase();
        match n.len() {
            0 => n,
            1 => format!("1/{n}"),
            2 => format!("2/{n}"),
            3 => format!("3/{}/{n}", &n[0..1]),
            _ => format!("{}/{}/{n}", &n[0..2], &n[2..4]),
        }
    }
}

#[derive(Deserialize)]
struct CrateVersionRow {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

#[async_trait]
impl Datasource for CargoDatasource {
    fn kind(&self) -> EdgeKind {
        EdgeKind::Cargo
    }

    async fn latest(&self, package: &str) -> Result<Option<VersionInfo>> {
        let url = format!("{}/{}", self.index, Self::index_path(package));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp
            .error_for_status()
            .with_context(|| format!("crates.io index for {package}"))?
            .text()
            .await
            .with_context(|| format!("read crates.io index for {package}"))?;

        // Newline-delimited JSON, one row per version. Pick the max non-yanked
        // stable semver. (find_candidates also filters prerelease, but skipping
        // them here keeps the "latest" honest.)
        let mut best: Option<semver::Version> = None;
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<CrateVersionRow>(line) else {
                continue;
            };
            if row.yanked {
                continue;
            }
            let Ok(v) = semver::Version::parse(&row.vers) else {
                continue;
            };
            if !v.pre.is_empty() {
                continue;
            }
            if best.as_ref().is_none_or(|b| v > *b) {
                best = Some(v);
            }
        }
        Ok(best.map(|v| VersionInfo {
            version: v.to_string(),
            digest: None,
            released_at: None,
        }))
    }
}
