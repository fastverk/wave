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

    /// The default chain: Bazel, then npm, then Cargo.
    #[must_use]
    pub fn default_chain() -> Self {
        Self::new(vec![
            Box::new(BazelDepProvider::new()),
            Box::new(NpmProvider::new()),
            Box::new(CargoProvider::new()),
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
}
