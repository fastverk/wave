//! Graph providers: parse a repo's manifest into the name it publishes + its
//! dependency edges, and rewrite one edge's version. An ordered [`ProviderChain`]
//! is tried per repo until one applies (Bazel `MODULE.bazel`, then npm
//! `package.json`, …). All methods are pure (text in, edges/text out) so the
//! graph engine is testable without any I/O.
//!
//! Each provider compiles its regexes once, into struct fields, when it is
//! constructed (`new`). The bump uses one fixed regex that matches every
//! dependency line and a closure that rewrites only the one whose name matches —
//! so there are no per-module dynamic patterns to recompile.

use regex::{Captures, Regex};

use crate::edge::{DepEdge, EdgeKind, VersionConstraint};

/// Parse + rewrite one manifest kind.
pub trait GraphProvider: Send + Sync {
    /// The manifest filename this provider reads (e.g. `"MODULE.bazel"`).
    fn manifest_name(&self) -> &'static str;
    /// The edge kind this provider produces.
    fn kind(&self) -> EdgeKind;
    /// The name this repo publishes, if the manifest declares one.
    fn published_name(&self, text: &str) -> Option<String>;
    /// The version this repo currently publishes, if the manifest declares one.
    /// Used as the publish-detection baseline + to resolve a downstream's target
    /// once an upstream releases.
    fn published_version(&self, text: &str) -> Option<String>;
    /// Parse all dependency edges. `None` = the manifest isn't this kind
    /// (so the chain falls through to the next provider).
    fn parse_edges(&self, text: &str) -> Option<Vec<DepEdge>>;
    /// Rewrite one dependency's version → `target`. Returns `(new_text, changed)`.
    fn bump(&self, text: &str, module: &str, target: &str) -> (String, bool);
}

/// An ordered list of providers tried per repo until one applies.
pub struct ProviderChain {
    providers: Vec<Box<dyn GraphProvider>>,
}

impl ProviderChain {
    #[must_use]
    pub fn new(providers: Vec<Box<dyn GraphProvider>>) -> Self {
        Self { providers }
    }

    /// The default chain: Bazel, then npm, then Cargo, then the pnpm catalog.
    /// Callers union the edges from every manifest a repo carries (see
    /// `node_for`), so a pnpm catalog workspace contributes both its
    /// `package.json` pins and its `pnpm-workspace.yaml` catalog entries.
    #[must_use]
    pub fn default_chain() -> Self {
        Self::new(vec![
            Box::new(BazelDepProvider::new()),
            Box::new(NpmProvider::new()),
            Box::new(CargoProvider::new()),
            Box::new(PnpmCatalogProvider::new()),
        ])
    }

    /// The providers in order.
    #[must_use]
    pub fn providers(&self) -> &[Box<dyn GraphProvider>] {
        &self.providers
    }

    /// Find the provider that owns `manifest_name`.
    #[must_use]
    pub fn for_manifest(&self, manifest_name: &str) -> Option<&dyn GraphProvider> {
        self.providers
            .iter()
            .find(|p| p.manifest_name() == manifest_name)
            .map(AsRef::as_ref)
    }
}

// ─── Bazel ──────────────────────────────────────────────────────────────

/// Reads `bazel_dep(name = "X", version = "Y")` edges + the `module(name=…)`
/// this repo publishes. Holds its compiled regexes.
pub struct BazelDepProvider {
    /// One `bazel_dep(…)` declaration — `head` runs through the version's
    /// opening quote, `name`/`ver` are the captured fields, `tail` is the
    /// closing quote. Tolerates multi-line declarations (`\s*` spans newlines).
    dep_re: Regex,
    /// `module(name = "X")` — the published name.
    module_name_re: Regex,
    /// `module(… version = "Y")` — the published version.
    module_version_re: Regex,
}

impl Default for BazelDepProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BazelDepProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dep_re: Regex::new(
                r#"(?P<head>bazel_dep\(\s*name\s*=\s*"(?P<name>[^"]+)"\s*,\s*version\s*=\s*")(?P<ver>[^"]+)(?P<tail>")"#,
            )
            .expect("valid bazel_dep regex"),
            module_name_re: Regex::new(r#"module\(\s*name\s*=\s*"(?P<name>[^"]+)""#)
                .expect("valid module-name regex"),
            module_version_re: Regex::new(r#"module\([^)]*?version\s*=\s*"(?P<ver>[^"]+)""#)
                .expect("valid module-version regex"),
        }
    }
}

impl GraphProvider for BazelDepProvider {
    fn manifest_name(&self) -> &'static str {
        "MODULE.bazel"
    }
    fn kind(&self) -> EdgeKind {
        EdgeKind::BazelDep
    }

    fn published_name(&self, text: &str) -> Option<String> {
        self.module_name_re
            .captures(text)
            .map(|c| c["name"].to_string())
    }

    fn published_version(&self, text: &str) -> Option<String> {
        self.module_version_re
            .captures(text)
            .map(|c| c["ver"].to_string())
    }

    fn parse_edges(&self, text: &str) -> Option<Vec<DepEdge>> {
        // Only treat this as a MODULE.bazel if it actually has bazel module
        // syntax; otherwise return None so the chain falls through.
        if !text.contains("bazel_dep(") && !text.contains("module(") {
            return None;
        }
        let edges = self
            .dep_re
            .captures_iter(text)
            .map(|c| DepEdge {
                module: c["name"].to_string(),
                current: VersionConstraint::parse_exact(&c["ver"]),
                manifest_path: self.manifest_name().to_string(),
                kind: EdgeKind::BazelDep,
            })
            .collect();
        Some(edges)
    }

    fn bump(&self, text: &str, module: &str, target: &str) -> (String, bool) {
        let mut changed = false;
        let out = self.dep_re.replace_all(text, |c: &Captures| {
            if &c["name"] == module {
                changed = true;
                format!("{}{target}{}", &c["head"], &c["tail"])
            } else {
                c[0].to_string()
            }
        });
        (out.into_owned(), changed)
    }
}

// ─── npm ────────────────────────────────────────────────────────────────

/// Reads `dependencies` / `devDependencies` / `peerDependencies` edges + the
/// `"name"` this package.json publishes. Holds its compiled bump regex.
pub struct NpmProvider {
    /// One `"<name>": "<op><version>"` dependency entry — `head` runs through
    /// the value's opening quote, `op` is any caret/tilde, `ver` is the version
    /// (anchored on a leading digit so non-semver specs are left alone), `tail`
    /// is the closing quote.
    dep_re: Regex,
}

impl NpmProvider {
    const SECTIONS: [&'static str; 3] = ["dependencies", "devDependencies", "peerDependencies"];

    #[must_use]
    pub fn new() -> Self {
        Self {
            dep_re: Regex::new(
                r#"(?P<head>"(?P<name>[^"]+)"\s*:\s*")(?P<op>[\^~]?)(?P<ver>[0-9][^"]*)(?P<tail>")"#,
            )
            .expect("valid npm dependency regex"),
        }
    }
}

impl Default for NpmProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphProvider for NpmProvider {
    fn manifest_name(&self) -> &'static str {
        "package.json"
    }
    fn kind(&self) -> EdgeKind {
        EdgeKind::Npm
    }

    fn published_name(&self, text: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        v.get("name")?.as_str().map(str::to_string)
    }

    fn published_version(&self, text: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        v.get("version")?.as_str().map(str::to_string)
    }

    fn parse_edges(&self, text: &str) -> Option<Vec<DepEdge>> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if !v.is_object() {
            return None;
        }
        let mut edges = Vec::new();
        for section in Self::SECTIONS {
            let Some(obj) = v.get(section).and_then(|s| s.as_object()) else {
                continue;
            };
            for (name, spec) in obj {
                let Some(spec) = spec.as_str() else { continue };
                edges.push(DepEdge {
                    module: name.clone(),
                    current: VersionConstraint::parse_npm(spec),
                    manifest_path: self.manifest_name().to_string(),
                    kind: EdgeKind::Npm,
                });
            }
        }
        Some(edges)
    }

    fn bump(&self, text: &str, module: &str, target: &str) -> (String, bool) {
        // Rewrite only the entry whose key matches `module`, keeping any
        // caret/tilde operator. Non-semver specs never match (the `ver` group is
        // anchored on a leading digit).
        let mut changed = false;
        let out = self.dep_re.replace_all(text, |c: &Captures| {
            if &c["name"] == module {
                changed = true;
                format!("{}{}{target}{}", &c["head"], &c["op"], &c["tail"])
            } else {
                c[0].to_string()
            }
        });
        (out.into_owned(), changed)
    }
}

// ─── Cargo ──────────────────────────────────────────────────────────────

/// Reads `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` +
/// `[workspace.dependencies]` from `Cargo.toml`, and the `[package]` name +
/// version this crate publishes. Path / git / `workspace = true` deps are
/// skipped (no registry version to track).
pub struct CargoProvider {
    /// `name = "ver"` — the string-form dependency (`ver` anchored on a leading
    /// digit so non-version keys and the inline-table form are left alone).
    dep_re: Regex,
}

impl CargoProvider {
    const SECTIONS: [&'static str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

    #[must_use]
    pub fn new() -> Self {
        Self {
            dep_re: Regex::new(
                r#"(?m)^(?P<head>\s*(?P<name>[A-Za-z0-9_-]+)\s*=\s*")(?P<ver>[0-9][^"]*)(?P<tail>")"#,
            )
            .expect("valid cargo dependency regex"),
        }
    }

    /// The version constraint from a dependency value (string or inline table),
    /// or `None` for path / git / workspace deps.
    fn constraint(spec: &toml::Value) -> Option<VersionConstraint> {
        match spec {
            toml::Value::String(s) => Some(VersionConstraint::parse_cargo(s)),
            toml::Value::Table(t) => {
                let is_workspace = t.get("workspace").and_then(|w| w.as_bool()) == Some(true);
                if is_workspace || t.contains_key("path") || t.contains_key("git") {
                    return None;
                }
                t.get("version")
                    .and_then(|v| v.as_str())
                    .map(VersionConstraint::parse_cargo)
            }
            _ => None,
        }
    }
}

impl Default for CargoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphProvider for CargoProvider {
    fn manifest_name(&self) -> &'static str {
        "Cargo.toml"
    }
    fn kind(&self) -> EdgeKind {
        EdgeKind::Cargo
    }

    fn published_name(&self, text: &str) -> Option<String> {
        let v: toml::Value = toml::from_str(text).ok()?;
        v.get("package")?.get("name")?.as_str().map(str::to_string)
    }

    fn published_version(&self, text: &str) -> Option<String> {
        // A workspace member may carry `version.workspace = true`; only a
        // concrete string is reported (the workspace floor lives elsewhere).
        let v: toml::Value = toml::from_str(text).ok()?;
        v.get("package")?.get("version")?.as_str().map(str::to_string)
    }

    fn parse_edges(&self, text: &str) -> Option<Vec<DepEdge>> {
        let v: toml::Value = toml::from_str(text).ok()?;
        if !v.is_table() {
            return None;
        }
        let mut tables: Vec<&toml::Table> = Vec::new();
        for section in Self::SECTIONS {
            if let Some(t) = v.get(section).and_then(|s| s.as_table()) {
                tables.push(t);
            }
        }
        if let Some(t) = v
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            tables.push(t);
        }
        let mut edges = Vec::new();
        for tbl in tables {
            for (name, spec) in tbl {
                if let Some(current) = Self::constraint(spec) {
                    edges.push(DepEdge {
                        module: name.clone(),
                        current,
                        manifest_path: self.manifest_name().to_string(),
                        kind: EdgeKind::Cargo,
                    });
                }
            }
        }
        Some(edges)
    }

    fn bump(&self, text: &str, module: &str, target: &str) -> (String, bool) {
        // String form only (`module = "ver"`). Inline-table `{ version = "…" }`
        // rewriting is a Phase-2 follow-up — discovery is report-only today.
        let mut changed = false;
        let out = self.dep_re.replace_all(text, |c: &Captures| {
            if &c["name"] == module {
                changed = true;
                format!("{}{target}{}", &c["head"], &c["tail"])
            } else {
                c[0].to_string()
            }
        });
        (out.into_owned(), changed)
    }
}

// ─── pnpm catalog ───────────────────────────────────────────────────────

/// Reads pnpm's `catalog:` block in `pnpm-workspace.yaml` — the repo-wide
/// version map that `"dep": "catalog:"` entries in each `package.json` resolve
/// through. In a catalog workspace this file, not `package.json`, is where the
/// version actually lives, so it is the file a bump must rewrite. (`NpmProvider`
/// correctly ignores those entries: `catalog:` parses to
/// [`VersionConstraint::Other`], which discovery skips.)
///
/// The workspace root publishes nothing, so `published_name`/`published_version`
/// are always `None` — this provider contributes edges only.
///
/// Only the default `catalog:` block is read; pnpm's named `catalogs:` (plural)
/// blocks are deliberately not matched (`catalogs:` is not the `catalog:` header).
pub struct PnpmCatalogProvider {
    /// One `  '<name>': <op><ver>` catalog entry. `head` runs from the line's
    /// indent through any quote + caret/tilde and `rest` carries any trailing
    /// spacing/comment, so a rewrite that keeps `head`/`tail`/`rest` preserves
    /// the file's quoting style, operator, and end-of-line comment verbatim.
    /// `ver` is anchored on a leading digit, so `catalog:`-style or non-version
    /// values never match.
    entry_re: Regex,
}

impl PnpmCatalogProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entry_re: Regex::new(
                r#"(?m)^(?P<head>[ \t]+(?P<name>'[^']+'|"[^"]+"|[A-Za-z0-9@._/-]+)[ \t]*:[ \t]*(?P<vq>['"]?)(?P<op>[\^~]?))(?P<ver>[0-9][^'"\s#]*)(?P<tail>['"]?)(?P<rest>[ \t]*(?:#[^\n]*)?)$"#,
            )
            .expect("valid pnpm catalog entry regex"),
        }
    }

    /// Byte range of the default `catalog:` block's body. The block runs from the
    /// line after the header to the next line at column 0 that is neither blank
    /// nor a comment (`packages:`, `catalogs:`, …) — so entries are only ever
    /// read or rewritten inside it, never in `packages:` or anywhere else.
    fn catalog_span(text: &str) -> Option<(usize, usize)> {
        let mut offset = 0usize;
        let mut start: Option<usize> = None;
        for line in text.split_inclusive('\n') {
            let len = line.len();
            let l = line.trim_end_matches(['\n', '\r']);
            match start {
                None => {
                    if Self::is_catalog_header(l) {
                        start = Some(offset + len);
                    }
                }
                Some(s) => {
                    let ends_block = !l.trim().is_empty()
                        && !l.starts_with([' ', '\t'])
                        && !l.trim_start().starts_with('#');
                    if ends_block {
                        return Some((s, offset));
                    }
                }
            }
            offset += len;
        }
        start.map(|s| (s, text.len()))
    }

    /// A bare `catalog:` header at column 0 (trailing comment allowed). Does not
    /// match `catalogs:` — pnpm's named-catalog block, a different shape.
    fn is_catalog_header(line: &str) -> bool {
        line.strip_prefix("catalog:")
            .is_some_and(|rest| rest.trim().is_empty() || rest.trim_start().starts_with('#'))
    }

    /// A YAML scalar with its surrounding quotes removed, if any.
    fn unquote(s: &str) -> &str {
        let b = s.as_bytes();
        let quoted = b.len() >= 2
            && ((b[0] == b'\'' && b[b.len() - 1] == b'\'')
                || (b[0] == b'"' && b[b.len() - 1] == b'"'));
        if quoted {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }
}

impl Default for PnpmCatalogProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphProvider for PnpmCatalogProvider {
    fn manifest_name(&self) -> &'static str {
        "pnpm-workspace.yaml"
    }
    fn kind(&self) -> EdgeKind {
        EdgeKind::Npm
    }

    /// A workspace root publishes nothing.
    fn published_name(&self, _text: &str) -> Option<String> {
        None
    }
    /// A workspace root publishes nothing.
    fn published_version(&self, _text: &str) -> Option<String> {
        None
    }

    fn parse_edges(&self, text: &str) -> Option<Vec<DepEdge>> {
        // No `catalog:` block ⇒ not a catalog workspace; fall through the chain
        // (a pnpm-workspace.yaml carrying only `packages:` yields no edges).
        let (start, end) = Self::catalog_span(text)?;
        let edges = self
            .entry_re
            .captures_iter(&text[start..end])
            .map(|c| DepEdge {
                module: Self::unquote(&c["name"]).to_string(),
                current: VersionConstraint::parse_npm(&format!("{}{}", &c["op"], &c["ver"])),
                manifest_path: self.manifest_name().to_string(),
                kind: EdgeKind::Npm,
            })
            .collect();
        Some(edges)
    }

    fn bump(&self, text: &str, module: &str, target: &str) -> (String, bool) {
        let Some((start, end)) = Self::catalog_span(text) else {
            return (text.to_string(), false);
        };
        let mut changed = false;
        let body = self.entry_re.replace_all(&text[start..end], |c: &Captures| {
            if Self::unquote(&c["name"]) == module {
                changed = true;
                // `rest` is re-emitted, not dropped: the match consumes any
                // end-of-line comment, so replacing without it silently eats
                // whatever the entry documented.
                format!("{}{target}{}{}", &c["head"], &c["tail"], &c["rest"])
            } else {
                c[0].to_string()
            }
        });
        if !changed {
            return (text.to_string(), false);
        }
        (
            format!("{}{}{}", &text[..start], body, &text[end..]),
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE_BAZEL: &str = r#"
module(name = "rules_lang", version = "0.0.13")
bazel_dep(name = "rules_cc", version = "0.1.0")
bazel_dep(
    name = "rules_proto",
    version = "6.0.0",
)
"#;

    #[test]
    fn bazel_parse_and_publish() {
        let p = BazelDepProvider::new();
        assert_eq!(p.published_name(MODULE_BAZEL).as_deref(), Some("rules_lang"));
        assert_eq!(p.published_version(MODULE_BAZEL).as_deref(), Some("0.0.13"));
        let edges = p.parse_edges(MODULE_BAZEL).unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].module, "rules_cc");
        assert_eq!(edges[1].module, "rules_proto");
    }

    #[test]
    fn bazel_bump_preserves_others() {
        let p = BazelDepProvider::new();
        let (out, changed) = p.bump(MODULE_BAZEL, "rules_cc", "0.2.0");
        assert!(changed);
        assert!(out.contains(r#"name = "rules_cc", version = "0.2.0""#));
        assert!(out.contains(r#"name = "rules_proto""#));
        assert!(out.contains(r#"version = "6.0.0""#));
    }

    #[test]
    fn bazel_bump_multiline() {
        let p = BazelDepProvider::new();
        let (out, changed) = p.bump(MODULE_BAZEL, "rules_proto", "6.1.0");
        assert!(changed);
        assert!(out.contains(r#"version = "6.1.0""#));
        // the single-line neighbor is untouched
        assert!(out.contains(r#"version = "0.1.0""#));
    }

    const PKG_JSON: &str = r#"{
  "name": "@savvi-studio/api-router",
  "version": "0.1.0",
  "dependencies": {
    "@savvi-studio/modules": "^0.1.0",
    "@aion/kernel": "^0.1.7",
    "zod": "4.4.3"
  }
}"#;

    #[test]
    fn npm_parse_and_publish() {
        let p = NpmProvider::new();
        assert_eq!(
            p.published_name(PKG_JSON).as_deref(),
            Some("@savvi-studio/api-router")
        );
        assert_eq!(p.published_version(PKG_JSON).as_deref(), Some("0.1.0"));
        let edges = p.parse_edges(PKG_JSON).unwrap();
        assert_eq!(edges.len(), 3);
        let modules: Vec<_> = edges.iter().map(|e| e.module.as_str()).collect();
        assert!(modules.contains(&"@savvi-studio/modules"));
    }

    #[test]
    fn npm_bump_keeps_caret() {
        let p = NpmProvider::new();
        let (out, changed) = p.bump(PKG_JSON, "@savvi-studio/modules", "0.1.1");
        assert!(changed);
        assert!(out.contains(r#""@savvi-studio/modules": "^0.1.1""#));
        // untouched neighbors + the package's own version
        assert!(out.contains(r#""zod": "4.4.3""#));
        assert!(out.contains(r#""version": "0.1.0""#));
    }

    #[test]
    fn chain_falls_through() {
        let chain = ProviderChain::default_chain();
        assert!(chain.for_manifest("MODULE.bazel").is_some());
        assert!(chain.for_manifest("package.json").is_some());
        assert!(chain.for_manifest("Cargo.toml").is_some());
        assert!(chain.for_manifest("go.mod").is_none());
    }

    const CARGO_TOML: &str = r#"
[package]
name = "wave-core"
version = "0.1.0"

[dependencies]
anyhow = "1.0.86"
serde = { version = "1.0", features = ["derive"] }
fastverk-forge = { path = "../../forge" }
local-thing = { git = "https://example.com/x" }
shared = { workspace = true }

[build-dependencies]
prost-build = "0.13"

[workspace.dependencies]
tokio = { version = "1.40", features = ["full"] }
"#;

    #[test]
    fn cargo_parse_and_publish() {
        let p = CargoProvider::new();
        assert_eq!(p.published_name(CARGO_TOML).as_deref(), Some("wave-core"));
        assert_eq!(p.published_version(CARGO_TOML).as_deref(), Some("0.1.0"));
        let edges = p.parse_edges(CARGO_TOML).unwrap();
        let modules: Vec<_> = edges.iter().map(|e| e.module.as_str()).collect();
        // registry deps kept; path / git / workspace=true skipped.
        assert!(modules.contains(&"anyhow"));
        assert!(modules.contains(&"serde"));
        assert!(modules.contains(&"prost-build"));
        assert!(modules.contains(&"tokio")); // from [workspace.dependencies]
        assert!(!modules.contains(&"fastverk-forge"));
        assert!(!modules.contains(&"local-thing"));
        assert!(!modules.contains(&"shared"));
        // bare "1.0" is caret in Cargo.
        let anyhow = edges.iter().find(|e| e.module == "anyhow").unwrap();
        assert!(matches!(anyhow.current, VersionConstraint::Caret(_)));
    }

    #[test]
    fn cargo_bump_string_form() {
        let p = CargoProvider::new();
        let (out, changed) = p.bump(CARGO_TOML, "anyhow", "1.1.0");
        assert!(changed);
        assert!(out.contains(r#"anyhow = "1.1.0""#));
        assert!(out.contains(r#"prost-build = "0.13""#)); // neighbor untouched
    }

    // ── pnpm catalog ────────────────────────────────────────────────────
    //
    // Shaped after a real catalog workspace: a `packages:` block that must never
    // be touched, a load-bearing comment inside `catalog:`, mixed first-party and
    // third-party entries, and a trailing block after the catalog.

    const PNPM_WORKSPACE: &str = r#"# Collapse peer-dependency variants.
dedupePeerDependents: true

packages:
  - packages/kernel
  - packages/logger

catalog:
  # @aion/* framework packages — single source of truth for the published
  # framework version (all bumped together on release).
  '@aion/kernel': ^0.2.0
  '@aion/http-utils': ^0.2.0
  '@aws-sdk/client-s3': ^3.1030.0
  zod: ^3.23.8

onlyBuiltDependencies:
  - esbuild
"#;

    #[test]
    fn pnpm_catalog_parses_entries_and_publishes_nothing() {
        let p = PnpmCatalogProvider::new();
        assert_eq!(p.published_name(PNPM_WORKSPACE), None);
        assert_eq!(p.published_version(PNPM_WORKSPACE), None);

        let edges = p.parse_edges(PNPM_WORKSPACE).unwrap();
        let names: Vec<_> = edges.iter().map(|e| e.module.as_str()).collect();
        // Quotes stripped; the `packages:` list + `onlyBuiltDependencies:` block
        // are outside the catalog span and contribute nothing.
        assert_eq!(
            names,
            ["@aion/kernel", "@aion/http-utils", "@aws-sdk/client-s3", "zod"]
        );
        assert!(edges.iter().all(|e| e.kind == EdgeKind::Npm));
        assert!(edges
            .iter()
            .all(|e| e.manifest_path == "pnpm-workspace.yaml"));
        assert_eq!(
            edges[0].current,
            VersionConstraint::Caret(semver::Version::new(0, 2, 0))
        );
    }

    #[test]
    fn pnpm_catalog_bump_is_surgical() {
        let p = PnpmCatalogProvider::new();
        let (out, changed) = p.bump(PNPM_WORKSPACE, "@aion/http-utils", "0.2.3");
        assert!(changed);
        // Rewrote the target, preserving quote style + the caret operator.
        assert!(out.contains("'@aion/http-utils': ^0.2.3"));
        // Neighbors untouched.
        assert!(out.contains("'@aion/kernel': ^0.2.0"));
        assert!(out.contains("zod: ^3.23.8"));
        // The comment documenting the framework-version invariant SURVIVES — a
        // YAML round-trip would eat it. This is the whole point of regex-surgery.
        assert!(out.contains("# @aion/* framework packages — single source of truth"));
        // Structure outside the catalog is byte-identical.
        assert!(out.contains("  - packages/kernel"));
        assert!(out.contains("dedupePeerDependents: true"));
        assert!(out.contains("onlyBuiltDependencies:"));
        // Nothing else moved.
        assert_eq!(out.lines().count(), PNPM_WORKSPACE.lines().count());
    }

    #[test]
    fn pnpm_catalog_bump_unknown_module_is_a_noop() {
        let p = PnpmCatalogProvider::new();
        let (out, changed) = p.bump(PNPM_WORKSPACE, "@aion/not-here", "9.9.9");
        assert!(!changed);
        assert_eq!(out, PNPM_WORKSPACE);
    }

    #[test]
    fn pnpm_catalog_never_rewrites_outside_the_catalog_block() {
        // A `packages:` entry that looks superficially like an entry with a
        // version must not be reachable — the span guard, not the regex, is what
        // protects it.
        const TRICKY: &str = r#"packages:
  foo: ^1.0.0

catalog:
  foo: ^1.0.0
"#;
        let p = PnpmCatalogProvider::new();
        let edges = p.parse_edges(TRICKY).unwrap();
        assert_eq!(edges.len(), 1, "only the catalog's `foo` is an edge");
        let (out, changed) = p.bump(TRICKY, "foo", "2.0.0");
        assert!(changed);
        assert_eq!(
            out,
            "packages:\n  foo: ^1.0.0\n\ncatalog:\n  foo: ^2.0.0\n",
            "the packages: entry is untouched"
        );
    }

    #[test]
    fn pnpm_catalog_absent_falls_through_the_chain() {
        // A workspace with no catalog: yields None so the chain moves on, rather
        // than claiming the repo with an empty edge set.
        const NO_CATALOG: &str = "packages:\n  - packages/a\n";
        assert!(PnpmCatalogProvider::new().parse_edges(NO_CATALOG).is_none());
        // `catalogs:` (pnpm's NAMED catalogs) is a different block and must not
        // be mistaken for the default one.
        const NAMED_ONLY: &str = "catalogs:\n  react17:\n    react: ^17.0.0\n";
        assert!(PnpmCatalogProvider::new().parse_edges(NAMED_ONLY).is_none());
    }

    #[test]
    fn pnpm_catalog_quoting_styles_and_trailing_comments() {
        const STYLES: &str = r#"catalog:
  "@scope/dq": ~1.2.3
  '@scope/sq': ^1.0.0
  bare: 2.0.0
  quoted-val: '^3.0.0'
  with-comment: ^4.0.0 # pinned deliberately
"#;
        let p = PnpmCatalogProvider::new();
        let edges = p.parse_edges(STYLES).unwrap();
        assert_eq!(
            edges.iter().map(|e| e.module.as_str()).collect::<Vec<_>>(),
            ["@scope/dq", "@scope/sq", "bare", "quoted-val", "with-comment"]
        );
        // Each rewrite preserves that entry's own quoting + operator.
        assert!(p.bump(STYLES, "@scope/dq", "1.2.4").0.contains(r#""@scope/dq": ~1.2.4"#));
        assert!(p.bump(STYLES, "bare", "2.1.0").0.contains("bare: 2.1.0"));
        assert!(p.bump(STYLES, "quoted-val", "3.1.0").0.contains("quoted-val: '^3.1.0'"));
        let (out, _) = p.bump(STYLES, "with-comment", "4.1.0");
        assert!(
            out.contains("with-comment: ^4.1.0 # pinned deliberately"),
            "trailing comment survives: {out}"
        );
    }

    #[test]
    fn pnpm_catalog_is_in_the_default_chain() {
        let chain = ProviderChain::default_chain();
        let p = chain
            .for_manifest("pnpm-workspace.yaml")
            .expect("pnpm catalog provider is routable by manifest name");
        assert_eq!(p.kind(), EdgeKind::Npm);
    }
}
