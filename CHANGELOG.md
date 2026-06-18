# Changelog

All notable changes to wave. The format is loosely
[Keep a Changelog](https://keepachangelog.com/) — version headers mirror the
published bazel-registry entries.

## 0.0.1 — initial

- `wave-core`: the cascade engine — version-constraint policy, a graph-provider
  chain (Bazel `MODULE.bazel` + npm `package.json`), cross-repo DAG assembly with
  longest-path tiers + cycle detection, a resumable reconcile state machine
  (open → CI → merge → publish-detect → next tier) over the `forge` contract, and
  tracing (durable event log + projection to a tier waterfall).
- `wave`: the CLI — `propose` / `start` / `status` / `reconcile` / `trace`.
  Tokens from the environment; repos enumerated via the forge REST API or `--repos`.
