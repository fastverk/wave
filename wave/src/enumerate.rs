//! Repo enumeration + graph-node assembly — fvkit-free.
//!
//! Lists a group/org's repos through the forge REST API (GitLab
//! `/groups/:id/projects`, GitHub `/orgs/:org/repos`), then reads each repo's
//! manifest through the forge to build [`RepoNode`]s. A repo whose manifest none
//! of the providers recognize is skipped.
//!
//! `owner` is the enumerated group/org — correct for a flat group; nested GitLab
//! subgroups would need `path_with_namespace` (a follow-up).

use anyhow::{bail, Context, Result};
use forge::{Forge, ForgeKind, RepoRef};
use futures::stream::StreamExt;
use serde::Deserialize;
use wave_core::{ForgeManifestSource, ManifestSource, ProviderChain, RepoNode};

/// A repo to consider for the cascade.
pub struct RepoSpec {
    pub name: String,
}

/// List every repo in `group` on `host` via the forge REST API.
pub async fn enumerate(
    kind: ForgeKind,
    host: &str,
    group: &str,
    token: &str,
) -> Result<Vec<RepoSpec>> {
    let http = reqwest::Client::builder()
        .user_agent("wave")
        .build()
        .context("build http client")?;
    match kind {
        ForgeKind::Gitlab => enumerate_gitlab(&http, host, group, token).await,
        ForgeKind::Github => enumerate_github(&http, host, group, token).await,
        other => bail!("unsupported forge kind: {other:?}"),
    }
}

#[derive(Deserialize)]
struct GlProject {
    path: String,
}

async fn enumerate_gitlab(
    http: &reqwest::Client,
    host: &str,
    group: &str,
    token: &str,
) -> Result<Vec<RepoSpec>> {
    let group_enc = urlencoding::encode(group);
    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!(
            "https://{host}/api/v4/groups/{group_enc}/projects?include_subgroups=true&archived=false&per_page=100&page={page}"
        );
        let resp = http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("GitLab projects for {group} on {host}"))?;
        let next = resp
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let projects: Vec<GlProject> = resp.json().await.context("parse GitLab projects")?;
        out.extend(projects.into_iter().map(|p| RepoSpec { name: p.path }));
        match next.parse::<u32>() {
            Ok(n) if n > 0 => page = n,
            _ => break,
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct GhRepo {
    name: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    fork: bool,
}

async fn enumerate_github(
    http: &reqwest::Client,
    host: &str,
    org: &str,
    token: &str,
) -> Result<Vec<RepoSpec>> {
    // github.com → api.github.com; Enterprise → https://<host>/api/v3.
    let api = if host.is_empty() || host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    };
    let mut out = Vec::new();
    let mut page = 1u32;
    loop {
        let url = format!("{api}/orgs/{org}/repos?per_page=100&page={page}");
        let repos: Vec<GhRepo> = http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("GitHub repos for org {org}"))?
            .json()
            .await
            .context("parse GitHub repos")?;
        if repos.is_empty() {
            break;
        }
        // Skip archived repos and forks — discovery doesn't manage a vendored
        // fork's upstream deps (e.g. a next.js fork with hundreds of npm deps).
        out.extend(
            repos
                .into_iter()
                .filter(|r| !r.archived && !r.fork)
                .map(|r| RepoSpec { name: r.name }),
        );
        page += 1;
    }
    Ok(out)
}

/// Max concurrent per-repo manifest reads. Keeps an org-wide scan from
/// serializing on forge-API latency without tripping secondary rate limits.
const REPO_CONCURRENCY: usize = 12;

/// Build graph nodes for `specs` by reading each repo's manifest through `forge`,
/// concurrently. A repo whose manifest can't be read (or none of the providers
/// recognize) is skipped, not fatal — one bad repo shouldn't sink an org scan.
pub async fn assemble_nodes<F: Forge + ?Sized>(
    forge: &F,
    specs: &[RepoSpec],
    host: &str,
    owner: &str,
    chain: &ProviderChain,
) -> Result<Vec<RepoNode>> {
    let src = ForgeManifestSource::new(forge);
    let src_ref = &src;
    let nodes: Vec<RepoNode> = futures::stream::iter(specs)
        .map(|spec| {
            let repo = RepoRef {
                forge: forge.kind() as i32,
                host: host.to_string(),
                owner: owner.to_string(),
                name: spec.name.clone(),
            };
            async move {
                match node_for(src_ref, &repo, chain).await {
                    Ok(node) => node,
                    Err(e) => {
                        tracing::warn!("skip {}: {e:#}", repo.name);
                        None
                    }
                }
            }
        })
        .buffer_unordered(REPO_CONCURRENCY)
        .filter_map(|n| async move { n })
        .collect()
        .await;
    Ok(nodes)
}

async fn node_for<F: Forge + ?Sized>(
    src: &ForgeManifestSource<'_, F>,
    repo: &RepoRef,
    chain: &ProviderChain,
) -> Result<Option<RepoNode>> {
    // Union edges across every manifest the repo has — a repo commonly carries
    // both MODULE.bazel and Cargo.toml (and sometimes package.json), and
    // discovery wants all of their external deps, not just the first match. The
    // published name is taken from the first provider that declares one (Bazel,
    // then npm, then Cargo) — its canonical registry artifact.
    let mut edges = Vec::new();
    let mut published: Option<String> = None;
    let mut matched = false;
    for p in chain.providers() {
        let Some(text) = src.read(repo, p.manifest_name()).await? else {
            continue;
        };
        let Some(mut es) = p.parse_edges(&text) else {
            continue;
        };
        matched = true;
        if published.is_none() {
            published = p.published_name(&text);
        }
        edges.append(&mut es);
    }
    Ok(matched.then(|| RepoNode {
        repo: repo.clone(),
        published,
        edges,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use forge::{
        BranchOutcome, ChangeRef, ChangeState, FileBlob, OpenedChange, PipelineStatus,
    };
    use std::collections::HashMap;

    struct ReadOnlyForge {
        files: HashMap<String, String>,
    }

    #[async_trait]
    impl Forge for ReadOnlyForge {
        fn kind(&self) -> ForgeKind {
            ForgeKind::Gitlab
        }
        async fn default_branch(&self, _: &RepoRef) -> forge::ForgeResult<String> {
            Ok("main".into())
        }
        async fn read_file(&self, repo: &RepoRef, path: &str, _: &str) -> forge::ForgeResult<Option<FileBlob>> {
            Ok(self
                .files
                .get(&format!("{}/{path}", repo.name))
                .map(|c| FileBlob {
                    path: path.into(),
                    content: c.clone(),
                    blob_sha: "sha".into(),
                }))
        }
        async fn create_branch(&self, _: &RepoRef, _: &str, _: &str) -> forge::ForgeResult<BranchOutcome> {
            unimplemented!()
        }
        async fn commit_file(
            &self,
            _: &RepoRef,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> forge::ForgeResult<String> {
            unimplemented!()
        }
        async fn open_change(
            &self,
            _: &RepoRef,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: bool,
        ) -> forge::ForgeResult<OpenedChange> {
            unimplemented!()
        }
        async fn enable_auto_merge(&self, _: &RepoRef, _: &ChangeRef) -> forge::ForgeResult<bool> {
            unimplemented!()
        }
        async fn pipeline_status(&self, _: &RepoRef, _: &ChangeRef) -> forge::ForgeResult<PipelineStatus> {
            unimplemented!()
        }
        async fn merge(&self, _: &RepoRef, _: &ChangeRef) -> forge::ForgeResult<String> {
            unimplemented!()
        }
        async fn change_state(&self, _: &RepoRef, _: &ChangeRef) -> forge::ForgeResult<ChangeState> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn assembles_recognized_repos_and_skips_the_rest() {
        let mut files = HashMap::new();
        files.insert(
            "foundation/package.json".to_string(),
            r#"{"name":"@s/foundation","version":"0.1.0","dependencies":{"@s/modules":"^0.1.0"}}"#
                .to_string(),
        );
        let forge = ReadOnlyForge { files };
        let specs = vec![
            RepoSpec { name: "foundation".into() },
            RepoSpec { name: "docs".into() },
        ];
        let chain = ProviderChain::default_chain();
        let nodes = assemble_nodes(&forge, &specs, "gitlab.savvifi.com", "studio", &chain)
            .await
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].repo.name, "foundation");
        assert_eq!(nodes[0].published.as_deref(), Some("@s/foundation"));
        assert_eq!(nodes[0].edges.len(), 1);
    }
}
