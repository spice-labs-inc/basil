# Codebase Structure

## Directory Layout

```
basil/
├── Basilfile              # Dogfood build: builds the basil binary itself with basil
├── Cargo.toml             # Crate manifest and dependencies
├── Cargo.lock
├── README.md              # Overview, install, quick start
├── TODO                   # Known gaps (e.g. args in AS clauses)
├── docs/
│   ├── DESIGN.md          # Design rationale: identity formula, events, promotion, gc, decisions
│   ├── BASILFILE.md       # Complete Basilfile language reference
│   └── events/            # Real GitHub webhook payload samples, one per ON token
├── src/
│   ├── main.rs            # Binary entry: clap dispatch, per-command tokio runtime
│   ├── lib.rs             # Library crate root: exports dsl, event, fsctx, identity, solver
│   ├── cli.rs             # clap CLI definitions and subcommand arg structs
│   ├── fsctx.rs           # filename/op error context for filesystem failures
│   ├── dsl/               # Lexer + parser + AST for the Basilfile language
│   ├── event/             # ON-reaction matching against GitHub Actions payloads
│   ├── identity/          # Derived input closures + content tagHash
│   └── solver/            # Build orchestration, buildah backend, registry client, pin/promote/gc
├── tests/
│   ├── conformance.rs     # Golden digest fixture (REGENERATE=1 to refresh)
│   ├── event.rs           # Event-matching integration tests
│   ├── buildah_e2e.rs     # Gated e2e against buildah + a live local registry
│   └── conformance/       # Fixture Basilfile (imports a MODULE), ctx/ inputs, mod/, expected.json
├── testdata/              # Basilfile fixtures + a sample push event for manual runs
└── .cortexkit/            # Dream scheduler config (maintain-docs runs daily at 03:00)
```

## Directory Purposes

**`src/dsl/`:**
- Purpose: The Basilfile language front end — reduce source to logical lines, parse into a `File` AST, and reject every deferred/unsupported construct at parse time
- Contains: `lexer.rs`, `ast.rs`, `parser.rs`
- Key files: `src/dsl/parser.rs` (grammar and parse-time rejections), `src/dsl/ast.rs` (all types: `Target`, `Instruction`, `Reaction`, `EventToken`, `Mount`, `Command`)

**`src/identity/`:**
- Purpose: Derive each target's input closure and content `tagHash`
- Contains: `closure.rs` (bottom-up closure resolution + record hashing + `BaseResolver`), `derived_data.rs` (git-tree-object digest of context files with index borrowing), `dockerignore.rs` (Docker ignore rules), `program_hash.rs` (canonical instruction digest with ARG/ENV expansion), `modules.rs` (recursive `MODULE` base discovery + per-module closure tables)
- Key files: `src/identity/closure.rs`, `src/identity/derived_data.rs`

**`src/event/`:**
- Purpose: Match `ON` tokens/selectors against a delivered GitHub Actions JSON payload (consumed verbatim)
- Contains: `mod.rs` with `reaction_matches` and `matching_reactions` (push/tag/release/PR matching, selectors)

**`src/solver/`:**
- Purpose: Solve builds: orchestrate the pipeline, render to buildah, talk to registries, pin/promote/gc
- Contains: `orchestrate.rs` (`BuildInput`, `build_targets`, `handle_event`, `dependency_subset`, module use-or-build, intermediate-stage cache reconciliation, idempotent check), `buildah.rs` (Containerfile renderer + buildah subprocess driver + stage-cache build/push/removal + OUTPUT extraction + task runner), `registry.rs` (registry v2 client, ref parsing, bearer-token auth), `pin.rs`, `promote.rs`, `gc.rs`

**`docs/`:**
- Purpose: Human documentation for the project — design rationale, DSL reference, event payload samples
- Contains: `DESIGN.md`, `BASILFILE.md`, `events/README.md`, and the `events/*.json` webhook samples
- Key files: `docs/DESIGN.md`, `docs/BASILFILE.md` (treat both as the language and design spec)

**`tests/`:**
- Purpose: Integration and conformance tests outside the crate, exercising the public `basil` library API
- Contains: `conformance.rs`, `event.rs`, `buildah_e2e.rs`, and the `conformance/` fixture directory

**`testdata/`:**
- Purpose: Checked-in Basilfile fixtures (one per scenario: `bhapp`, `bhmulti`, `build`, `copyonly`, `execrun`, `offline`, `promo`, `task`, `ver`, `example`) plus `event-push.json` for manual runs
- Key files: `testdata/ver.Basilfile` (VERSION arg + tag promotion), `testdata/task.Basilfile` (DO/TASK path), `testdata/promo.Basilfile` (ON tag)

**`.cortexkit/`:**
- Purpose: Dream scheduler configuration declaring the `maintain-docs` and `retrospective` schedules
- Key files: `.cortexkit/magic-context.jsonc`

## Key File Locations

**Entry Points:** `src/main.rs`: binary entry — parses the clap `Cli`, sets up tracing, builds a tokio runtime per async command, and dispatches `closure`/`build`/`handle`/`pin`/`gc`. `src/lib.rs`: library root exporting `dsl`, `event`, `fsctx`, `identity`, `solver`. `src/cli.rs`: all subcommand arg structs (`ClosureArgs`, `BuildArgs`, `HandleArgs`, `PinArgs`, `GcArgs`).

**Configuration:** `Cargo.toml`: dependencies (tokio `full`, clap derive, serde/serde_json, anyhow + anyhow-tracing, sha2 + sha1, glob, tracing + tracing-subscriber `env-filter`, reqwest `json` + `rustls`). `Basilfile`: the project's own dogfood build — a static-musl `rust:1.88-alpine` builder stage, `OUTPUT` of the release binary, a `scratch` deliverable with `REGISTER 127.0.0.1:5001/basil`, and `ON push(main) BUILD basil`. There is no config file of its own: behavior is driven by argv and environment variables (`RUST_LOG`, `REGISTRY_AUTH_FILE`, `BASIL_INSECURE_REGISTRY`, `BASIL_SECRET_<ID>`, `DOCKER_CONFIG`, `XDG_RUNTIME_DIR`); the `--cache-to` flag on `build`/`handle` and the `BASIL_CACHE_REF` env var (cache-to override) choose where intermediate-stage cache images are published, defaulting to the deliverable's own repo.

**Core Logic:** `src/dsl/parser.rs` (DSL grammar), `src/identity/closure.rs` (identity formula + `tagHash`), `src/solver/orchestrate.rs` (build/event orchestration), `src/solver/buildah.rs` (Containerfile rendering + buildah subprocess), `src/solver/registry.rs` (registry v2 + auth).

**Tests:** `tests/conformance.rs` — golden digest fixture recomputed against `tests/conformance/expected.json` (`REGENERATE=1` refreshes); its `Basilfile` imports a `MODULE` so the module fold is pinned too. `tests/event.rs` — reaction matching, including the requirement that the `docs/events/` payloads match their documented tokens; `tests/buildah_e2e.rs` — gated live end-to-end (skips when buildah or the local registry is unavailable) asserting the pushed image config + content digest, the idempotent-skip / `--force` paths, per-stage cache-image build/push/cleanup, and module use-or-build. Unit tests are co-located in each module as `#[cfg(test)] mod tests`.

## Naming Conventions

**Files:** Rust modules are `snake_case.rs` (`derived_data.rs`, `program_hash.rs`, `orchestrate.rs`); each layer is a folder with a `mod.rs`. Integration tests are `snake_case.rs` at `tests/` root (`buildah_e2e.rs`). Basilfile fixtures end in `.Basilfile` (`bhmulti.Basilfile`) and use kebab-case image names (`ghcr.io/mycompany/app`, `127.0.0.1:5001/bhmulti`).

**Directories:** `src/<layer>/` mirrors the public library modules; fixture directories are lowercase (`testdata/`, `tests/conformance/ctx/`).

**Language/DSL:** DSL keywords are uppercase (`FROM`, `MODULE`, `ARG`, `ENV`, `COPY`, `ADD`, `RUN`, `CMD`, `ENTRYPOINT`, `USER`, `WORKDIR`, `LABEL`, `EXPOSE`, `VOLUME`, `OUTPUT`, `TASK`, `REGISTER`, `GIVEN`, `PLATFORM`, `ON`, `BUILD`, `DO`). Event tokens are kebab-case with selectors in parens (`pr-opened`, `pr-synchronized`, `pr-merged`, `push(main)`, `tag`, `release(published)`). Content tags are `<push-ref>:basil-<tagHash>` where `tagHash` is a 64-hex SHA-256; cache tags are `basil-cache-<stageHash>` (one per non-deliverable stage, reconciled by existence), plus date-encoded `basil-cache-<YYYYMMDD|YYYYMM|YYYY>`; version tags are `v<semver>` (promoted from `tag`/`release` events); anything outside the `basil-` namespace is foreign to `gc`.

## Where to Add New Code

**New DSL instruction:** add an `Instruction` variant in `src/dsl/ast.rs`, parse it in `src/dsl/parser.rs`, render it in `src/identity/program_hash.rs` (identity) and `src/solver/buildah.rs` (Containerfile), and account for any context files it reads in `src/identity/derived_data.rs::context_sources`. Deferred constructs belong in `is_known_or_deferred` with a parse-time error.

**New module behavior:** `MODULE` bases are a separate identity domain — discovery + per-module closure tables live in `src/identity/modules.rs` (`discover_module_index`, `compute_module_digests`), the importer folds the module's deliverable digest in `src/identity/closure.rs` (`base.module.<alias>`), and build-time use-or-build runs in `src/solver/orchestrate.rs::ensure_modules_built` (the module's own intermediate stages cache under its repo via `ensure_stages_cached`).

**New event token:** add an `EventToken` variant in `src/dsl/ast.rs`, classify it in `src/dsl/parser.rs::classify_token`, and match it in `src/event/mod.rs::reaction_matches`. Add a sample payload in `docs/events/` and a row in `docs/events/README.md` — `tests/event.rs::docs_event_fixtures_match_their_tokens` asserts the samples stay correct.

**New command:** add a `Command` variant + args struct in `src/cli.rs` and a match arm in `src/main.rs`; put the logic in the appropriate solver/identity module and call it from there.

**New registry operation:** add a method on `ImageRef`/`RegistryClient` in `src/solver/registry.rs` — auth (`authed`/`token`), redirect policy, and ref normalization are already centralized there.

**New build/event orchestration step:** extend `src/solver/orchestrate.rs` — `build_targets` for the direct-build path, `handle_event` for the event path (both share `resolve_unpinned_bases`, `compute_closure`, `idempotent_published`, module use-or-build, `ensure_stages_cached`/`stage_cache_refs`, and the buildah driver).

**New tests:** co-locate unit tests as `#[cfg(test)] mod tests` inside the module; add integration tests at `tests/<name>.rs` exercising the public `basil` library API; add manual-run Basilfile fixtures to `testdata/`; digest changes to the conformance fixture regenerate via `REGENERATE=1 cargo test --test conformance`.

**Shared utilities / error context:** keep them in `src/fsctx.rs` (filesystem error context) or the module's private helpers; extend the public API surface only through `src/lib.rs`.