//! The dependency-edge model: what one repo declares it depends on, and the
//! version-constraint policy that decides whether a published upstream version
//! warrants a downstream bump.

use anyhow::Result;
use async_trait::async_trait;
use forge::RepoRef;

/// Which manifest kind an edge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// `bazel_dep(name, version)` in MODULE.bazel.
    BazelDep,
    /// A `package.json` dependency.
    Npm,
}

/// A normalized version constraint on a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionConstraint {
    /// An exact pin (bazel_dep; an npm `"1.2.3"`).
    Exact(semver::Version),
    /// An npm caret range `^1.2.3`.
    Caret(semver::Version),
    /// Any other semver range (`~1.2`, `>=1, <2`).
    Range(semver::VersionReq),
    /// A non-semver / opaque spec (`"main"`, a URL, `workspace:*`).
    Other(String),
}

impl VersionConstraint {
    /// Parse a Bazel `bazel_dep` version (always an exact pin).
    #[must_use]
    pub fn parse_exact(s: &str) -> Self {
        match semver::Version::parse(s.trim()) {
            Ok(v) => Self::Exact(v),
            Err(_) => Self::Other(s.trim().to_string()),
        }
    }

    /// Parse an npm dependency spec.
    #[must_use]
    pub fn parse_npm(spec: &str) -> Self {
        let s = spec.trim();
        if let Some(rest) = s.strip_prefix('^') {
            if let Ok(v) = semver::Version::parse(rest.trim()) {
                return Self::Caret(v);
            }
        }
        if let Ok(v) = semver::Version::parse(s) {
            return Self::Exact(v);
        }
        if let Ok(req) = semver::VersionReq::parse(s) {
            return Self::Range(req);
        }
        Self::Other(s.to_string())
    }

    /// The version this constraint is anchored at, if any (for ordering/compare).
    #[must_use]
    pub fn anchor(&self) -> Option<&semver::Version> {
        match self {
            Self::Exact(v) | Self::Caret(v) => Some(v),
            _ => None,
        }
    }

    /// Does this constraint already admit `target` (no bump strictly needed)?
    #[must_use]
    pub fn admits(&self, target: &semver::Version) -> bool {
        match self {
            Self::Exact(v) => v == target,
            Self::Caret(v) => semver::VersionReq::parse(&format!("^{v}"))
                .map(|r| r.matches(target))
                .unwrap_or(false),
            Self::Range(req) => req.matches(target),
            Self::Other(_) => false,
        }
    }
}

/// One dependency edge declared by a repo's manifest.
#[derive(Debug, Clone)]
pub struct DepEdge {
    /// The dependency's published name (`"rules_lang"` | `"@savvi-studio/ui"`).
    pub module: String,
    pub current: VersionConstraint,
    /// The manifest the edge was parsed from (`"MODULE.bazel"` | `"package.json"`).
    pub manifest_path: String,
    pub kind: EdgeKind,
}

/// Whether a downstream edge needs a bump for a given target version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BumpDecision {
    /// Open an MR bumping this edge.
    NeedsBump,
    /// The constraint already admits the target — no MR needed.
    AlreadySatisfied,
    /// The consumer pins something incompatible (e.g. a newer version).
    Conflict(String),
}

/// Decide whether `current` warrants a bump to `target`. `bump_satisfied`
/// forces a bump even when a caret/range already admits the target (to advance
/// a pinned lockfile floor).
#[must_use]
pub fn decide(
    current: &VersionConstraint,
    target: &semver::Version,
    bump_satisfied: bool,
) -> BumpDecision {
    use VersionConstraint::{Caret, Exact, Other, Range};
    match current {
        Exact(v) => {
            if v == target {
                BumpDecision::AlreadySatisfied
            } else if v > target {
                BumpDecision::Conflict(format!("pinned {v} > target {target}"))
            } else {
                BumpDecision::NeedsBump
            }
        }
        Caret(v) => {
            if v > target {
                BumpDecision::Conflict(format!("caret floor {v} > target {target}"))
            } else if current.admits(target) && !bump_satisfied {
                BumpDecision::AlreadySatisfied
            } else {
                BumpDecision::NeedsBump
            }
        }
        Range(_) => {
            if current.admits(target) && !bump_satisfied {
                BumpDecision::AlreadySatisfied
            } else {
                BumpDecision::NeedsBump
            }
        }
        Other(_) => BumpDecision::NeedsBump,
    }
}

/// Read-only manifest access, abstracting whether the bytes come from a forge
/// API, a local checkout, or a fixture. Lets the graph engine stay pure.
#[async_trait]
pub trait ManifestSource: Send + Sync {
    /// Read `path` from `repo`'s default branch. `Ok(None)` = absent.
    async fn read(&self, repo: &RepoRef, path: &str) -> Result<Option<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_spec_classification() {
        assert!(matches!(
            VersionConstraint::parse_npm("^0.1.0"),
            VersionConstraint::Caret(_)
        ));
        assert!(matches!(
            VersionConstraint::parse_npm("0.1.0"),
            VersionConstraint::Exact(_)
        ));
        assert!(matches!(
            VersionConstraint::parse_npm(">=1, <2"),
            VersionConstraint::Range(_)
        ));
        assert!(matches!(
            VersionConstraint::parse_npm("workspace:*"),
            VersionConstraint::Other(_)
        ));
    }

    #[test]
    fn caret_admits_patch_but_not_minor_floor_advance() {
        let c = VersionConstraint::parse_npm("^0.1.0");
        let target = semver::Version::parse("0.1.5").unwrap();
        // ^0.1.0 admits 0.1.5 → satisfied unless we force a floor advance.
        assert_eq!(decide(&c, &target, false), BumpDecision::AlreadySatisfied);
        assert_eq!(decide(&c, &target, true), BumpDecision::NeedsBump);
    }

    #[test]
    fn exact_pin_always_bumps_forward_and_conflicts_backward() {
        let c = VersionConstraint::parse_exact("0.1.0");
        assert_eq!(
            decide(&c, &semver::Version::parse("0.1.1").unwrap(), false),
            BumpDecision::NeedsBump
        );
        assert_eq!(
            decide(&c, &semver::Version::parse("0.1.0").unwrap(), false),
            BumpDecision::AlreadySatisfied
        );
        assert!(matches!(
            decide(&c, &semver::Version::parse("0.0.9").unwrap(), false),
            BumpDecision::Conflict(_)
        ));
    }
}
