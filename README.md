> [!IMPORTANT]
> **This repository is retired.** `wave` is developed in the
> [`fastverk/platform`](https://github.com/fastverk/platform) ship vehicle, at
> [`wave/`](https://github.com/fastverk/platform/tree/main/wave). Open issues and
> pull requests there.
>
> The published module is unchanged — `bazel_dep(name = "wave", version = "0.1.0")`
> resolves exactly as before. This remote keeps its full history and every tag, so
> existing registry entries and `git_override` pins stay valid. The `wave.v1`
> protos are published from
> [`fastverk/contracts`](https://github.com/fastverk/contracts).
>
> Retired at [`c689d65`](https://github.com/fastverk/wave/commit/c689d650fe33e34507c42d3dcb65c56954454a07),
> the commit the vehicle imported — nothing here is unimported. Background:
> [Consolidation](https://docs.fastverk.com/consolidation.html).

# wave

Cross-repo dependency-cascade engine — *not* Renovate.

When an upstream module publishes a new version, `wave` opens version-bump
changes in every downstream repo, auto-merges them on green, and **cascades** the
bump through the dependency DAG in tier order: a tier-2 repo's bump opens only
once the tier-1 repo it consumes has actually published. Built on
[`forge`](https://github.com/fastverk/forge), so it drives GitHub and self-hosted
GitLab through one contract.

- **`wave-core`** — the engine: a version-constraint policy, a graph-provider
  chain (Bazel `MODULE.bazel` + npm `package.json`, extensible), cross-repo DAG
  assembly with longest-path tiers + cycle detection, a resumable reconcile state
  machine (`PENDING → MR_OPEN → CI_RUNNING → CI_GREEN → MERGING → MERGED →
  PUBLISHED`), and tracing (a durable event log projected to a tier waterfall).
- **`wave`** — the CLI: `propose`, `start`, `status`, `reconcile`, `trace`.

## Use

```sh
export GITLAB_TOKEN=…              # or GITHUB_TOKEN / FORGE_TOKEN
wave propose @acme/core 1.4.0 --forge gitlab --host gitlab.example.com --group acme
wave start   @acme/core 1.4.0 --forge gitlab --host gitlab.example.com --group acme
wave reconcile                     # the daemon tick — run on a timer / after CI
wave trace <wave-id>               # the cascade waterfall
```

Repos come from `--group`/`--org` (enumerated via the forge REST API) or an
explicit `--repos a,b,c`.

## Install

`.bazelrc`:

```
common --registry=https://registry.fastverk.com/
common --registry=https://bcr.bazel.build/
```

`MODULE.bazel`:

```python
bazel_dep(name = "wave", version = "0.0.1")
```

Depend on `@wave//wave-core:wave_core` (the engine) or run the `@wave//wave`
binary. The crates also build with cargo against a checked-out sibling
[`forge`](https://github.com/fastverk/forge).
