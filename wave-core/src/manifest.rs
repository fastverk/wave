//! A [`ManifestSource`] backed by a [`forge::Forge`]: reads each repo's manifest
//! from its default branch over the forge API, so the graph engine can assemble
//! [`RepoNode`](crate::graph::RepoNode)s from live repos rather than fixtures.

use anyhow::Result;
use async_trait::async_trait;
use forge::{Forge, RepoRef};

use crate::edge::ManifestSource;

/// Reads manifests through a borrowed [`Forge`]. `F: ?Sized` so a
/// `&dyn Forge` works as well as a concrete adapter.
pub struct ForgeManifestSource<'a, F: Forge + ?Sized> {
    forge: &'a F,
}

impl<'a, F: Forge + ?Sized> ForgeManifestSource<'a, F> {
    #[must_use]
    pub fn new(forge: &'a F) -> Self {
        Self { forge }
    }
}

#[async_trait]
impl<F: Forge + ?Sized> ManifestSource for ForgeManifestSource<'_, F> {
    async fn read(&self, repo: &RepoRef, path: &str) -> Result<Option<String>> {
        // Empty ref = the repo's default branch. forge's error is its own anyhow
        // (a distinct crate instance under Bazel), so re-wrap it into ours.
        let blob = self
            .forge
            .read_file(repo, path, "")
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        Ok(blob.map(|b| b.content))
    }
}
