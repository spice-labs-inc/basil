# Architecture

## Pattern Overview

**Overall:** Closure-driven build orchestrator with derived content identity and a plain-process `buildah` backend. Basil parses a Dockerfile-like DSL, derives every build target's input closure into a deterministic content digest (`tagHash`), and drives `buildah` (as a subprocess over rootless `containers/storage`) to build and push images. Top-level `ON` reactions map CI events to `BUILD` (deliverables), `DO` (one-shot `TASK` side effects), and `OUTPUT` (reverse `COPY`).

**Key Characteristics:**
- Single binary, no daemon, no graph protocol — *one* process, `buildah` as a subprocess
- Identity is **derived, not declared** — every `tagHash` is computed from the exact files a target reads, its instruction program, its bases, and its args; basil owns the identity, buildah just executes it
- Event-triggered reactions — a delivered GitHub Actions payload (stdin or `--event`) selects which targets build, which tasks run, and which version tags are promoted
- Idempotent — a build is skipped when every deliverable's `<push-ref>:<tagHash>` content tag already exists in its registry (`--force` bypasses); `OUTPUT` and promotion still run on the skip by pulling the published image
- Offline-capable — the closure is fully offline when every external `FROM` is pinned (`@sha256`); unpinned bases need one registry metadata read (never a layer fetch)
- Determinism is a correctness requirement — pinned by a checked-in conformance fixture (`tests/conformance/expected.json`) that the test suite recomputes

## Layers

**CLI:**
- Purpose: Parse argv, resolve the DSL file docker-style (`<context>/Basilfile`, or `-f` relative to the current directory), and dispatch each subcommand
- Location: `src/main.rs`, `src/cli.rs`
- Contains: `Cli`/`Command` clap definitions and per-command arg structs; per-command handoff to dsl/identity/solver; the offline `CliResolver` (base resolver) used by `closure`
- Depends on: `src/dsl/`, `src/identity/`, `src/solver/`, `src/fsctx.rs`
- Used by: the end user (binary entry point)

**DSL:**
- Purpose: Lex, parse, and model the Basilfile language — a Dockerfile superset plus `ON`, `TASK`, `PLATFORM`, `OUTPUT`, `REGISTER`, `GIVEN`
- Location: `src/dsl/` (`lexer.rs`, `ast.rs`, `parser.rs`)
- Contains: `LogicalLine` lexing (line continuations, comment skipping), the `File`/`Target`/`Instruction` AST, and parse-time rejection of every deferred construct (`heredocs`, `COPY --link`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD`, `RUN --security`)
- Depends on: std only
- Used by: identity, event, solver, CLI

**Identity:**
- Purpose: Derive each target's input closure and content `tagHash` (SHA-256 over a canonical record)
- Location: `src/identity/` (`closure.rs`, `derived_data.rs`, `dockerignore.rs`, `program_hash.rs`)
- Contains: the `BaseResolver` trait + `OfflineResolver`, the `Args` build-arg resolver, git-tree-object digest of context files with index borrowing, Docker `.dockerignore` matching, and the canonical instruction renderer
- Depends on: `src/dsl/ast`, `src/fsctx.rs`
- Used by: `src/solver/orchestrate.rs`, `src/main.rs` (`closure` command), tests

**Event:**
- Purpose: Match `ON` reaction tokens/selectors against a delivered GitHub Actions payload (consumed verbatim; basil defines no event schema of its own)
- Location: `src/event/mod.rs`
- Contains: `reaction_matches`, `matching_reactions`; push/tag/release/PR matching including selectors
- Depends on: `src/dsl/ast`
- Used by: `src/solver/orchestrate.rs`

**Solver:**
- Purpose: Orchestrate builds and registry operations: parse → match event → identity → buildah build/push → OUTPUT/tasks → promotion/gc
- Location: `src/solver/` (`orchestrate.rs`, `buildah.rs`, `registry.rs`, `pin.rs`, `promote.rs`, `gc.rs`)
- Contains: `BuildInput`, `build_targets`, `handle_event`, dependency subsetting, the Containerfile renderer (`Graph`), the buildah subprocess driver, the registry v2 HTTP client (`RegistryClient`/`ImageRef`), and the pin/promote/gc passes
- Depends on: `src/dsl/`, `src/identity/`, `src/event/`, `src/fsctx.rs`, tokio, reqwest
- Used by: `src/main.rs`

**fsctx:**
- Purpose: Attach `filename=<path>` and `op=<op>` key-value fields to every file/filesystem failure so operators always know which file an error relates to
- Location: `src/fsctx.rs`
- Contains: `fs_err` (wraps `io::Result`), `with_filename` (wraps non-io errors)
- Depends on: `anyhow-tracing`
- Used by: identity, solver, CLI

## Data Flow

**Parse pipeline** (every command):
1. `resolve_build_file` resolves the DSL path (`-f`, else `<context>/Basilfile`) — `src/main.rs`
2. `fsctx::fs_err` reads the file with filename/op context — `src/fsctx.rs`
3. `dsl::lexer::lex` reduces source to logical lines (joins `\` continuations, skips full-line comments) — `src/dsl/lexer.rs`
4. `dsl::parser::parse` builds the `File` AST and rejects deferred directives — `src/dsl/parser.rs`

**Identity / closure** (used by `closure`, `build`, `handle`, conformance tests):
1. `compute_closure` resolves targets bottom-up (dependencies before dependents), detecting stage cycles — `src/identity/closure.rs`
2. `derived_data::derived_data_digest_and_bytes` digests the target's exact context files (`COPY`/`ADD`/bind-mount sources or `GIVEN` paths, dockerignore-filtered) as a synthetic git tree object — `src/identity/derived_data.rs`
3. `program_hash::program_hash` digests the canonical, ARG/ENV-expanded instruction program — `src/identity/program_hash.rs`
4. Components are assembled into a sorted, length-prefixed, collision-free record and SHA-256'd → `tagHash` — `src/identity/closure.rs`. The formula:
   `C(T) = derivedData(T) ∪ C(B)` per depended stage (`FROM <stage>`, `COPY --from=<stage>`) `∪ resolvedDigest(X)` per external base `∪ platforms(T) ∪ programHash(T) ∪ {basil-version}`. ARG declarations are never folded as their own record — only the substituted form at a `$VAR` use site inside an identity-affecting instruction enters identity, so an unreferenced ARG has zero effect.

**Direct build** (`basil build`):
1. `build_targets` parses, selects deliverables (qualified `name` or a `REGISTER` ref), and computes the transitive dependency subset when `-t` is given — `src/solver/orchestrate.rs`
2. `resolve_unpinned_bases` resolves each unpinned external base to its manifest digest via an async registry pre-pass (fail-hard on unresolvable) — `src/solver/orchestrate.rs` + `src/solver/registry.rs`
3. `compute_closure` computes `tagHash` per in-scope target — `src/identity/closure.rs`
4. `idempotent_published` HEAD-checks whether every content tag already exists in its registry; if so the build is skipped (`--force` bypasses the probe) — `src/solver/orchestrate.rs` + `src/solver/registry.rs`
5. `build_deliverable` renders the stage graph to a multi-stage Containerfile fed on buildah's stdin (`-f -`, no temp file), runs `buildah build --layers -t <push-ref>:<tag>`, then `buildah push` with the operator's auth — `src/solver/buildah.rs`
6. `extract_outputs` runs any `OUTPUT` reverse-copies from the built image (or the pulled image on the idempotent skip) — `src/solver/buildah.rs`

**Event flow** (`basil handle`):
1. Event JSON is read from `--event` or stdin — `src/main.rs`
2. `event::matching_reactions` filters `ON` reactions against the payload — `src/event/mod.rs`
3. `handle_event` threads a `VERSION` build-arg for matching `tag`/`release` reactions, resolves bases, and computes closures — `src/solver/orchestrate.rs`
4. The idempotent check can skip the rebuild; `OUTPUT` extraction and promotion still run on the skip — `src/solver/orchestrate.rs`
5. Matched deliverables build via `build_deliverable`; `DO`'d `TASK` clauses run as one-shot containers via `run_task` (`buildah run`, `BASIL_EVENT` set) — `src/solver/buildah.rs`
6. `promote_on_tag` sets immutable `v<version>` tags on the promoted push-ref for `tag`/`release` matches — `src/solver/orchestrate.rs` + `src/solver/promote.rs` + `src/solver/registry.rs`

**Pin** (`basil pin`):
1. `pin_file` parses, then resolves each distinct unpinned *external* base to its manifest digest (internal stages, `scratch`, and already-pinned bases are left alone) — `src/solver/pin.rs`
2. `rewrite_from_base` swaps the base token in each `FROM` line, preserving `AS`, `--platform=`, indentation, and trailing comments — `src/solver/pin.rs`

**GC** (`basil gc`):
1. `parse_age` converts `30d`/`1w`/`8h`/`1y` to seconds — `src/solver/gc.rs`
2. `gc_repo` lists each target repo's tags (from the file's qualified `AS` targets or `--repo`) and prunes only basil-owned ephemeral tags: `cache-<YYYYMMDD|MM|Y>` by embedded date, `cache-<tagHash>` and content `<tagHash>` tags by the OCI config `created` time; `v*` version tags and foreign tags are never pruned (`--dry-run` lists without deleting) — `src/solver/gc.rs` + `src/solver/registry.rs`

## Key Abstractions

**`File` / `Target` / `Instruction` (AST):**
- Purpose: Model a parsed Basilfile: targets (stages), top-level reactions, file-default platforms, and global-scope args; per-target base image, instructions, `TASK`, `REGISTER`, `GIVEN`, `PLATFORM`
- Location: `src/dsl/ast.rs`
- Pattern: Enums for instruction kinds, commands (shell/exec), mount types, and event tokens; `Target::push_ref()` resolves the publish location (`REGISTER` or name)

**`BaseResolver`:**
- Purpose: Resolve a third-party base image to a manifest digest during closure computation, keeping identity offline when every external base is pinned
- Location: `src/identity/closure.rs`
- Implementations: `OfflineResolver` (closure/unit tests), `CliResolver` (`src/main.rs`), `RegistryResolver` (`src/solver/orchestrate.rs`, backed by an async registry pre-pass), `PinnedResolver` (`tests/conformance.rs`)
- Pattern: Trait injected into `compute_closure`; `scratch` resolves to a fixed sentinel, never a pull

**`RepoState` / git-tree digest:**
- Purpose: Digest `derivedData` as a synthetic git tree object built with git's own object model and hash, borrowing git's precomputed tree/blob hashes for fully tracked, unmodified, entirely-covered subtrees (no re-hash); git's modes make the exec bit part of identity
- Location: `src/identity/derived_data.rs`
- Pattern: One metadata-only probe of the enclosing repo per closure run (`discover_git`); dirty/partial/dockerignore-filtered/outside-git subtrees are computed from the working tree with the same object model, so the digest is deterministic either way; `.git` metadata at any depth is always excluded

**`Dockerignore`:**
- Purpose: Docker `.dockerignore` rule matching honored as-is for the context file set
- Location: `src/identity/dockerignore.rs`
- Pattern: Compiled rules (`*`/`?`/`[...]` per component, `**` across separators, `/` anchoring, trailing `/` directories, `!` negation); the last matching rule wins

**`Graph` (Containerfile renderer):**
- Purpose: Render the in-scope stage graph (targets + file-level global args) to a multi-stage Containerfile, stripping basil-only directives (`ON`, `TASK`, `PLATFORM`, `OUTPUT`, `REGISTER`, `GIVEN`) that live in basil, not the image
- Location: `src/solver/buildah.rs`
- Pattern: Deterministic renderer feeding `buildah build -f -` on stdin; the deliverable is forced last and tagged; `--target` builds of intermediate stages use the alias-preserving `render_containerfile_all`

**`ImageRef` / `RegistryClient`:**
- Purpose: Parse image refs (`[host[:port]/]repo[:tag]`) and speak registry HTTP v2: manifest existence HEAD, tag list/delete, digest resolution, OCI config `created` time, and immutable retag (refuses to overwrite a version tag pointing at a different manifest)
- Location: `src/solver/registry.rs`
- Pattern: Per-host client with a cached bearer token; `parse_ref` normalization (bare `alpine` → `index.docker.io/library/alpine`; Docker Hub hosts map to the `registry-1.docker.io` API host; localhost/127.0.0.1 are detected as insecure http); bearer-token exchange against the operator's Docker/Podman credentials

**`BuildInput`:**
- Purpose: Carry everything a build entry point needs about a run
- Location: `src/solver/orchestrate.rs`
- Pattern: Plain struct threaded through `build_targets`/`handle_event`

## Entry Points

**`basil closure [CONTEXT] [-f FILE] [TARGET]`:**
- Location: `src/main.rs`
- Triggers: manual invocation
- Responsibilities: parse, compute every target's closure with the offline `CliResolver` (unpinned external bases error — a closure without network is never falsely computed), and report each target's `tagHash` and instruction count

**`basil build [CONTEXT] [-f FILE] [-t TARGET] [--force]`:**
- Location: `src/main.rs` → `src/solver/orchestrate.rs::build_targets`
- Triggers: manual invocation (never reads stdin; `-t` matches the stage `AS` name *or* the `REGISTER` ref)
- Responsibilities: build every qualified deliverable in the file (or the `-t` target's transitive dependency subset), resolve bases, compute closures, run the idempotent check, buildah build + push, extract `OUTPUT`s; never executes `TASK` clauses (tasks are event-triggered)

**`basil handle [CONTEXT] [-f FILE] [--event F] [--force]`:**
- Location: `src/main.rs` → `src/solver/orchestrate.rs::handle_event`
- Triggers: a delivered event (stdin or `--event`)
- Responsibilities: match `ON` reactions, thread `VERSION` for `tag`/`release`, resolve bases, compute closures, run the idempotent check, build/push matched deliverables, run `DO` tasks, promote version tags immutably, extract `OUTPUT`s

**`basil pin [FILE]`:**
- Location: `src/main.rs` → `src/solver/pin.rs::pin_file`
- Triggers: manual invocation
- Responsibilities: resolve every unpinned external base to `@sha256` and rewrite the file in place (idempotent; internal stages, `scratch`, and pinned bases untouched; each distinct ref resolved once)

**`basil gc <AGE> [FILE] [--repo FQ] [--dry-run]`:**
- Location: `src/main.rs` → `src/solver/gc.rs::gc_repo`
- Triggers: manual invocation
- Responsibilities: prune basil-owned registry tags (`cache-*`, content tags) older than `<age>` from the file's fully-qualified `AS` targets or an explicit `--repo`; never prune version tags

## Error Handling

**Strategy:** Fail closed. The parser rejects every unsupported or deferred DSL construct at parse time with a line number — never silently. An unpinned external base that cannot be resolved is a hard error at every layer (identity must never proceed on an unresolved input); the offline `closure` path errors on any unpinned base. An explicit `COPY`/`ADD`/`GIVEN` source excluded by `.dockerignore` is a hard error (identity must not hash an input the build will reject), as is a source inside `.git`. Parser-relative path violations fail too: `OUTPUT` host destinations and `GIVEN` paths must be relative (no leading `/`, no `..` escape). Registry probe failures on the idempotent path degrade to building so any real registry problem resurface at push time; version-tag immutability violations refuse rather than overwrite; `RUN --mount=type=secret` without the `BASIL_SECRET_<ID>` env is a hard error. Non-hermetic constructs (`ADD <url>`, secret/ssh mounts) are noted, not blocked. `OUTPUT`/`TASK` extraction failures fail the invocation. No `panic!`/`unwrap` path is reachable from user input.

## Cross-Cutting Concerns

**Logging:** `tracing` + `tracing-subscriber` fmt with `EnvFilter` from `RUST_LOG` (default `info`), initialized once in `src/main.rs`; `basil: ...` status lines on stdout mark each build phase (parse, resolve, identity, build, push, output); `with_filename`/`fs_err` attach `filename=`/`op=` fields to every file error.

**Caching:** `buildah build --layers` reuses the operator's local containers/storage layer cache. The registry content tag (`<push-ref>:<tagHash>`) is the idempotency key that skips rebuilds; `cache-<tagHash>`, `cache-<YYYYMMDD>`, `cache-<YYYYMM>`, `cache-<YYYY>` tags namespace regenerable cache refs so cache accumulates and ages out on a retention schedule (`basil gc`).

**Storage:** containers/storage owned by rootless buildah for layer/build state; the registry v2 HTTP API via `reqwest` (rustls) for manifest HEADs, tag listing, deletes, config-blob reads, and immutable manifest PUTs. Redirects are followed only with a bounded policy; wrong-host redirects surface the final URL and content-type instead of being silently believed.

**Auth:** Basil maintains no credentials of its own. Pushes use `REGISTRY_AUTH_FILE`, else Podman `auth.json`, else Docker's `config.json`; the registry client loads basic-auth from `$DOCKER_CONFIG`/Podman `auth.json` for the bearer-token exchange. `BASIL_INSECURE_REGISTRY=1|true|yes` opts into accepting invalid TLS certs for the registry client (never the default); localhost/`127.0.0.1` registries are treated as insecure (http) automatically. Task env includes `BASIL_EVENT` (the raw event ref); secret mounts read `BASIL_SECRET_<ID>` (uppercased ID) for the file path.