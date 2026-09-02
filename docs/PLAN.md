# Basil — Implementation Plan

Derived from `docs/DESIGN.md`. Milestones are sequenced so each phase's
dependencies exist before it starts; every phase carries its own test strategy
(feature work is not complete without tests).

## Grounding (verified against crates.io and source)

- `buildkit-client = 0.1.4` (AprilNEA/arcbox-labs) — MIT/Apache, maintained.
- Its `proto.rs` ships `pub mod pb` (`tonic::include_proto!("pb")`) — the full
  LLB `ops.proto` (`Op`, `Definition`, `ExecOp`, `SourceOp`, `FileOp`, `Mount`,
  …) is bundled, pulled transitively by compiling `control.proto` (which imports
  `ops.proto`). → **No separate `buildkit-rs-proto` dependency**, and no
  proto-version skew between our emitted `Definition` and `SolveRequest.definition`.
- Its `build()` hardcodes `frontend: "dockerfile.v0"`, `definition: None`, and a
  single comma-joined exporter. The seam basil needs is public:
  `BuildKitClient::control()` (raw `ControlClient`) plus `Session` / `FileSync` /
  `AuthServer` / `SecretsServer`. → basil assembles `SolveRequest` itself with
  `definition: Some(graph)` + N `Exporter`s, reusing only session/auth/filesync.
- `dockerfile-parser` is Docker-only (no `ON`/`TASK`/`PLATFORM`, no reactions).
  A bespoke parser is required either way.
- Operational notes:
  - `buildkit-client`'s `build.rs` downloads `.proto` files from GitHub at
    compile time (defaults to buildkit `master` → non-hermetic dependency build;
    pin via `BUILDKIT_REF`).
  - buildkitd's wire format must match the buildkit version `buildkit-client` was
    generated against — since basil owns the daemon, pin both.

## Phase 0 — Scaffolding + ground truth

- Add dependencies (`buildkit-client`, `clap`, `tokio`, `serde`/`serde_json`,
  `sha2`, glob, `flate2`, `tar`) and the module skeleton.
- Smoke test: compile `buildkit-client`, emit a trivial `pb::Definition`, and
  solve it against an ephemeral `buildkitd` — proves the emit→solve seam
  end-to-end before any DSL work.
- Git discipline: one branch at a time; commit locally only; no pushes unless
  requested.

## Phase 1 — DSL lexer + parser (pure, no daemon)

- Hand-rolled lexer/parser for the fixed instruction subset + basil extensions
  (`ON`, `TASK`, `PLATFORM`).
- AST: `File`, `Target`, `Reaction`, `Instruction`, `Task`.
- Anything outside the subset is a parse-time error (heredocs, `--link`,
  `--network`/`--security`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD`,
  per-instruction `--platform`) — surfaced, never mishandled.
- **Tests**: unit tests per construct; error-surfacing tests for every deferred
  feature.

## Phase 2 — Identity / closure (`tagHash`)

- `derivedData` (context-file resolution, `.dockerignore` honoring, glob
  expansion, OCI-style deterministic tar snapshot).
- `programHash` (arg-expanded program digest).
- `FROM`-digest folding (internal target / pinned external / unpinned external).
- `args`, `platforms`, `basil-version`; canonical serialization.
- `basil closure <target>` report subcommand (the audit surface, and the means
  to test closure correctness without a daemon).
- **Tests**: golden conformance fixture — checked-in inputs with expected
  digests recomputed by the suite. Digests are real computed values, never
  fabricated.

## Phase 3 — Event matching (`ON`)

- GitHub Actions payload (stdin, verbatim) → token resolution (`pr-opened`,
  `pr-synchronized`, `pr-merged`, `push`, `tag`) + selector matching; pick
  matching reactions.
- **Tests**: table-driven over real `$GITHUB_EVENT_PATH` payload fixtures.

## Phase 4 — LLB emitter

- Target instruction graph → `pb::Definition`: `FROM` (alias/external,
  pinned/unpinned), `COPY`/`ADD` (dockerignore + globs, `--from`,
  `--chown`/`--chmod`, tar auto-extract), `RUN` (shell/exec,
  `--mount=bind|secret|ssh|cache`), image-config ops, `ARG`/`ENV` expansion.
- N exporters in one `SolveRequest` (the required extension of the stock
  single-solve path).
- **Tests**: differential conformance — identical inputs through buildkit's
  `dockerfile.v0` frontend vs basil's emitter; assert equal image config + layer
  digests.

## Phase 5 — Daemon lifecycle + solve levels

- Ephemeral `buildkitd` (rootless, unique per-invocation state dir, stop at
  exit); external via `BUILDKIT_HOST`.
- Level 1 (content tag exists → zero solve), Level 2 (cache import),
  Level 3 (full solve + push content tag + `cache-*` exports).
- **Tests**: integration against a throwaway local registry (`registry:2`) +
  ephemeral daemon — "exists → skip/pull" and "absent → build/push" paths.

## Phase 6 — `TASK` execution

- Run `DO`'d tasks outside the solve: shell/exec form, env (`BASIL_EVENT`,
  `BASIL_IMAGE`, `BASIL_REF_<TARGET>`), base-image resolution, ordering
  (`BUILD` before `DO`), exit semantics.

## Phase 7 — Promotion (version tags)

- `tag` event → version extraction, `ARG VERSION`, retag-vs-rebuild,
  immutability check.

## Phase 8 — `basil gc`

- Age parsing, scope (`--repo`), `--dry-run`, prune only basil-owned tags (never
  version tags), Docker/Podman credential loading.

## Phase 9 — Hardening + docs

- Credential handling (Docker/Podman config), warnings (unpinned base,
  non-hermetic constructs), conformance run in CI.

## AST / module layout (proposed)

```
src/
  main.rs          (entry)
  cli.rs           (clap: build, closure, gc)
  dsl/
    ast.rs         (File, Target, Reaction, Instruction, Task)
    lexer.rs
    parser.rs
  identity/
    closure.rs     (derivedData resolution)
    program_hash.rs
    serialization.rs (canonical length-prefixed record)
    digest.rs      (tagHash, deterministic tar)
  event/
    mod.rs         (GitHub payload → matched reactions)
  emitter/
    mod.rs
    llb/           (Op building: exec, source, file, image-config)
    mounts.rs
    arg_expand.rs
  solver/
    mod.rs         (SolveRequest assembly, N exporters, cache import/export)
    daemon.rs      (ephemeral/external buildkitd lifecycle)
    levels.rs      (Level 1/2/3)
  task/
    mod.rs         (TASK execution)
  registry/
    secrets.rs     (Docker/Podman auth loading)
    client.rs      (tag check, push refs, catalog for gc)
  gc/
    mod.rs
  promote/
    mod.rs         (version tags)
tests/
  conformance/     (golden fixtures)
  differential/    (dockerfile.v0 vs emitter)
```

## Sequencing

| Milestone | Blocks | Unblocked now |
|---|---|---|
| 0 → 1 → 2 | — | yes |
| 3 | independent of 1/2 | yes, parallel |
| 4 | 1 (AST), 2 (identity for `FROM` folding) | after 1–2 |
| 5 | 4 | after 4 |
| 6 | 3 + 5 | after 3/5 |
| 7, 8 | 5 | after 5 |
| 9 | all | last |

Recommended start: **Phase 0 + Phase 1** (scaffold + smoke test + DSL parser) —
no daemon/registry dependency, establishes compile-time ground truth.
