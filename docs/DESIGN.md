# Basil — Design

Basil is a plain-process, buildkit-graph-owning build tool. It parses a
Dockerfile-like DSL, derives each target's input closure from its instruction
graph, emits its own Low-Level Build graph (LLB), and drives a `buildkitd`
daemon it owns. Top-level `ON` reactions map CI events to targets: `BUILD`
targets are solved and pushed, `DO` targets run their attached `TASK` side
effects. Basil owns the graph so it can express these reactions and compute an
honest, derived content identity.

## Targets and reactions (encoding intent)

A file holds **targets** (named stages) and **top-level reactions**. A target is
a normal stage — `FROM <base> AS <name>` plus instructions — and may carry
`TASK` clauses: commands run as side effects. A deliverable's `AS` name is its
**fully-qualified image reference** (`ghcr.io/mycompany/app`); intermediates and
task targets use plain internal names. A reaction maps one event to targets with
two verbs:

```
ON <event>(<selector>)  BUILD <target>…  DO <target>…
```

```
FROM golang AS build
COPY . /src
RUN go build -o /src/app

FROM scratch AS ghcr.io/mycompany/app         # deliverable (BUILD'd)
COPY --from=build /src/app /app

FROM ghcr.io/mycompany/app AS smoke           # task target: base is a BUILD'd target
TASK [ "/app", "--smoke" ]

FROM alpine AS notify                         # task target (internal name)
COPY notify-my-thing.sh /usr/local/bin/
TASK notify-my-thing.sh

FROM alpine AS migrate
COPY migrate.sh /usr/local/bin/
TASK migrate.sh

ON pr-merged        BUILD ghcr.io/mycompany/app   DO migrate
ON image-published  DO notify smoke
```

- **`BUILD <target>…`** — solve + tag + push. Listing a target here is what makes
  it a **deliverable**; a target never in any `BUILD` is a build-only
  intermediate (always built when a deliverable needs it, never tagged or
  pushed). `BUILD` cannot reference a task target. A deliverable's `AS` name is
  its fully-qualified image reference; basil pushes it as `<name>:<tagHash>`.
- **`DO <target>…`** — run the `TASK` commands attached to that target. `DO`
  references **only task targets** (targets with a `TASK` clause). Side effects:
  not graph `ExecOp`s, never layers, never part of the image; they run because
  the event matched, whether or not the target built this run.
- **`TASK <cmd>…`** — a target clause: commands to run **on the target's base
  image** (its `FROM` — an external image, or another target's output, including
  a target that is `BUILD`'d), like `RUN` but outside the solve. A task target
  may also carry build steps — they prepare the task's runtime image (e.g.
  `COPY` the executable in), built locally, never tagged or pushed.
- **`PLATFORM <os>/<arch>…`** — the platform(s) a target builds for. On a target
  it sets that target's list; before the first `FROM` it sets the **file-wide
  default** (targets without their own inherit it). No `PLATFORM` = the builder's
  native platform. Multiple platforms make the content tag an OCI index (manifest
  list).
- **`OUTPUT <container-src> <host-dest>`** — a **reverse `COPY`**: after the
  deliverable is built/pushed, copy a file out of the built image's root
  filesystem onto the host. `host-dest` is **relative to the build context only**
  (no leading `/`, no `..` escaping the context) — validated at parse time, so it
  can never write outside the context. It executes on every build of the
  deliverable **including an idempotent-skip**: when the image is already
  published and no rebuild happens, basil pulls `<name>:<tagHash>` and extracts
  the OUTPUT from it. `OUTPUT` is not a container build step (never fed to the
  builder) but is folded into the target's `programHash`, so changing it changes
  the content tag.
- Targets depend on each other, so the LLB is a DAG across targets (edges from
  `COPY --from=<target>` and `FROM <target>`). `FROM <x>` resolves like
  Dockerfile: a local `AS` alias shadows a registry reference, so a base that
  matches a target name is the stage, not an image pull.
- **Dependency resolution is by closure hash only** — nothing is ever fetched by
  a mutable `latest`/most-recent reference. A depended-upon deliverable is
  resolved at `<name>:<tagHash>`: pull that exact image if it exists, otherwise
  build it and push it under its closure tag (so later builds just pull),
  regardless of whether its own event fired — so a dependent always builds on
  exactly the base today's source defines. An intermediate is always built
  locally (it has no registry name to pull).
- **Selector** — the event token may take an optional value that refines the
  match: `ON push(main)` matches a push to branch `main`
  (`ref` == `refs/heads/main`); `ON push(main, develop)` matches either; a bare
  `ON push` matches any branch push. Exact match today; glob patterns are a
  later extension.
- Within one reaction, `BUILD` targets solve first, then the `DO`'d tasks run.
  Both lists are optional (`BUILD`-only, `DO`-only, or both).

## Identity (derived, not declared)

Every target's input closure is **derived from its instruction graph**, not from
a declared allowlist. Per target `T`:

```
C(T) = derivedData(T)
       ∪ C(B)   for every depended stage B          // FROM <stage>, COPY --from=<stage>
       ∪ { resolvedDigest(X) }  for external bases  // FROM <image>
       ∪ args(T)                                   // resolved ARG / --build-arg
       ∪ platforms(T)                              // declared build platforms
       ∪ programHash(T)                            // ordered, arg-expanded instructions
       ∪ { basil-version }
```

- **`derivedData`** — the exact context files each instruction reads, resolved
  at parse time: `COPY`/`ADD` sources and `--mount=type=bind,from=context`
  sources, filtered by `.dockerignore` (Docker's own rules, honored as-is),
  globs expanded against the local context. `COPY .` therefore means
  whole-tree identity explicitly — there is no implicit default.
- **`programHash`** — digest of the target's ordered instruction program with
  `ARG`/`ENV` references expanded. Includes every `RUN` command string and the
  image-config instructions (`ENV`, `CMD`, `ENTRYPOINT`, `USER`, `WORKDIR`,
  `LABEL`, `EXPOSE`, `VOLUME`), so a command or config change changes identity.
  Excludes comments and whitespace (they don't churn the tag) and the
  reaction/TASK clauses (policy and side effects, not image content).
- **`FROM` digest** — folded resolved, not as the ref string. Because registries
  are content-addressed, a manifest digest is a commitment over every layer:
  the whole base image is covered without fetching a blob.
- **`platforms`** — the target's declared build-platform list (its `PLATFORM`
  clause, or the inherited file-wide default). Folded so a single-platform vs
  multi-platform build is a different tag — the content tag always identifies the
  same artifact shape.

`tagHash = H(C(T))` — a deterministic digest over a canonical serialization of
`C(T)`. `derivedData` is digested as an OCI-style synthetic snapshot
(deterministic tar: sorted entries, `uid`/`gid` 0, normalized headers; gzip at
fixed level, no mtime/name in the header). Scalar components (`programHash`,
base digests, args, version) are encoded as a sorted, length-prefixed record.
Determinism is a correctness requirement, pinned by a **conformance fixture**:
checked-in golden inputs whose expected digest the test suite recomputes.

**One digest drives two refs** (per target):

1. `<name>:<tagHash>` — stable immutable content tag, where `<name>` is the
   deliverable's **fully-qualified `AS` name** (`ghcr.io/mycompany/app`).
   **Deliverable targets only.** One platform → a single image; several → an OCI
   index. Checked first; on hit, return the ref with zero daemon work (Level 1
   skip).
2. **Cache refs** — exported under the deliverable's repo with four tags:
   `cache-<tagHash>` (exact closure), `cache-<YYYYMMDD>`, `cache-<YYYYMM>`,
   `cache-<YYYY>` (build date). The exact tag reattaches the identical closure;
   the date tags are shared by every build in that period, so cache accumulates
   across content changes, and each period ages out on a retention schedule.
   Imports read `cache-<tagHash>` plus the current day/month/year tags.

Only the exact-closure cache tag (`cache-<tagHash>`) is keyed on `tagHash`; the
date tags are shared, best-effort cache and do not affect identity.

**`FROM` — three cases.**

- **Internal target** (`FROM <target>`): resolved by that target's own closure
  hash — idempotent, offline, no warning. The dependent always builds on
  exactly the base today's source defines (pull the hash, else build it).
- **Pinned 3rd-party image** (`FROM alpine:3.23@sha256:hash`): the digest is
  read from the DSL text — offline, stable, hermetic, idempotent.
- **Unpinned 3rd-party image** (`FROM alpine:3.23`): non-hermetic but accepted —
  the tag is a semantic-version promise, so drift is expected to stay backward
  compatible. Basil resolves the tag to its manifest digest and folds that, so
  content stays honest (a republish changes the tag), and **warns** (gentle):
  the same file later yields a different tag. Idempotency is the caller's
  responsibility — pin 3rd-party images for hermetic builds.

Consequence: the closure hash is **fully offline** when every external `FROM` is
pinned (and other bases are internal targets) — no manifest fetch, no blob
fetch. An unpinned base needs its manifest resolved (a metadata read, never a
layer download) before `tagHash` exists, so Level 1 is not offline for those
targets. Multi-arch tags fold the index digest (platform-agnostic, still a full
commitment).

## `RUN` and non-hermeticity (tools, not a gate)

Code is in the closure; data is not.

- **`RUN`'s command is in `programHash`** — editing the command changes identity.
- **`RUN`'s runtime reads are out of the closure** — network fetches,
  `--mount=type=secret|ssh`, anything the command reaches at runtime. The
  identity covers what the target does and what it was given, not what it
  fetched.
- **`ADD <url>`** contributes only its URL string (already in `programHash`);
  the remote content is never fetched to compute identity. A URL change changes
  identity; remote drift under a stable URL does not, and the target is marked
  non-hermetic.

Basil gives tools to be hermetic and to *analyze* hermeticity; it does not
require it:

- Non-hermetic constructs (`ADD <url>`, `--mount=type=secret|ssh`, detected
  network access in `RUN`) mark the target non-hermetic and **warn gently**
  (parse-time, non-blocking, once per target). Heuristic, not escape analysis.
- The derived closure is the audit surface: basil can report a target's exact
  input set, so hermeticity is something an operator can verify rather than
  something basil enforces.

## Architecture

```
┌──────────────── basil (one binary) ─────────────────┐
│  parse DSL → derive closure → event check            │
│  → buildah build/push (<name>:<tagHash>)              │
│  → idempotent check (skip rebuild if already pushed)  │
│  → run DO'd TASKs / extract OUTPUTs (outside build)   │
│  → promote version tags, gc                           │
└───────────────────────────────────────────────────────┘
            │ buildah subprocess (plain-process, no daemon)
            ▼
      containers/storage  ──(runc/crun exec steps)──
```

- The pipeline has three phases: **emit+solve** (build/push/cache), then
  **run `DO`'d `TASK`s** — always-run side effects outside the graph, keyed by
  the delivered event, not by build state.

- **Client shell**: `buildkit-client` (arcbox/AprilNEA) — session, filesync,
  auth, cache, tunnel, progress, CLI. MIT/Apache, maintained (fresh pushes,
  releases, CI). The modern, operative base.
- **LLB emitter**: `buildkit-client` has **no client-side graph builder** — it
  hands everything to buildkit's built-in `dockerfile.v0` (`solve.rs`:
  `frontend: "dockerfile.v0"`, `definition: None`). Basil needs to **own the
  graph** so it can express reactions and `TASK`s and derive its own identity.
  So basil **recreates** an LLB emitter on top of the maintained proto types
  (`buildkit-rs` `ops.proto`), rather than resurrecting an abandoned crate.
  Contribute the emitter back once stable.

## Emitter instruction scope (v1)

The recreated emitter translates a fixed instruction subset to LLB. Anything
outside it is a parse-time error — surfaced, never silently mishandled.

**In scope**

- Build graph: `FROM` (stage alias or external image, pinned/unpinned), `COPY`
  (context after `.dockerignore` + globs; `--from=<stage|image>`;
  `--chown`/`--chmod`), `ADD` (local, incl. tar auto-extract; URL = non-hermetic),
  `RUN` (shell + exec form; `--mount=type=bind|secret|ssh|cache`).
- Image config: `ARG`, `ENV`, `CMD`, `ENTRYPOINT`, `USER`, `WORKDIR`, `LABEL`,
  `EXPOSE`, `VOLUME`.

**Deferred** (parse-time error)

- Heredocs (`COPY`/`RUN` `<<EOF`), `COPY --link`, `RUN --network`/`--security`,
  `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD`, per-instruction `--platform`
  (the target `PLATFORM` clause covers multi-arch).

**Correctness surface** (where emitter effort lives)

- `RUN` cache key — attach env + mounts so buildkit's key is sound and stable.
- `COPY`/`ADD` source resolution — globs, dir/file semantics, `--from` stage FS
  vs context, symlinks.
- `ARG`/`ENV` scoping + `${VAR}` expansion; `ENV` persisted to config and to
  subsequent `RUN` env.
- Image config — `CMD`/`ENTRYPOINT` shell-vs-exec normalization; `USER`/
  `WORKDIR`/`LABEL`/`EXPOSE`/`VOLUME` change tracking.

**Test strategy**

- Differential conformance: for each in-scope instruction, build identical
  inputs via buildkit's `dockerfile.v0` frontend and via basil's emitter; assert
  equal image config + layer digests. This pins real behavior, not an
  interpretation of it. Plus the existing closure-digest conformance fixture.

## Daemon mode

- **Ephemeral (default)** — each `basil` invocation spawns its own rootless
  `buildkitd` in a unique per-invocation state dir, solves, and stops it at exit.
  Concurrent invocations never collide (unique state dir); no discovery, no
  locking. Cross-build reuse comes from the registry `cache-*` refs, not local
  daemon state.
- **External (opt-in)** — when `BUILDKIT_HOST` is set (a unix socket path or
  `tcp://` address), basil connects to that daemon and never manages its
  lifecycle. This is the warm-local-cache path for iterative development: run a
  persistent `buildkitd` yourself (e.g. a systemd user unit) and point basil at
  it.
- **Not provided** — a basil-managed persistent socket with lock/lease and
  stale-socket recovery. Registry cache makes it unnecessary for CI, and external
  mode covers development; revisit only if ephemeral startup cost proves real.

## Event input (`ON`) — reactions

- Event delivered on **stdin** as JSON; read by `basil build` (with `--event`
  flag). One event per invocation. **For now the JSON is the GitHub Actions
  event payload** (`$GITHUB_EVENT_PATH`), consumed verbatim — basil defines no
  event schema of its own yet.
- Each reaction's `ON` is compared against the delivered event at parse time;
  non-matching reactions are ignored. A matching `BUILD` target whose image
  already exists **pulls it** rather than rebuilding; matching `DO` targets
  still run their tasks — the event fired.
- `ON` tokens resolve against that payload:

  | token | GitHub event | match |
  |---|---|---|
  | `pr-opened` | `pull_request` | `action == "opened"` |
  | `pr-synchronized` | `pull_request` | `action == "synchronize"` |
  | `pr-merged` | `pull_request` | `action == "closed"` && `pull_request.merged == true` |
  | `push` | `push` | `ref` == `refs/heads/<selector>` (bare: any branch) |
  | `tag` | `push` | `ref` starts with `refs/tags/` |

  Other events (e.g. `image-published`) are **deferred** — a native carrier
  exists (`registry_package`), but the mapping is not pinned yet.

- Manual trigger = an event supplied from stdin without a CI workflow.

## Promotion (version tags)

A version-tag event (`v1.5.8`, `target-v1.2.3`) assigns a human version tag to
a target's image. Promotion is not a new verb: it is `ON tag BUILD <target>…`
plus one extra tag on the result.

- **Version extraction** — the event's git tag is parsed: `target-v1.2.3` names
  target `target` with version `1.2.3`; a bare `v1.5.8` applies to every target
  in that reaction's `BUILD` list.
- **Version as input** — basil reserves `ARG VERSION` and passes
  `--build-arg VERSION=<version>` when a `tag` event matches. Because identity
  is derived, only a target that actually references `VERSION` consumes it:
  - **Referenced** (`RUN go build -ldflags "-X main.version=$VERSION"`) — the
    version joins `args(T)` -> a fresh `tagHash` -> a real build -> the built
    image is tagged `<name>:v1.5.8`.
  - **Unreferenced** — the closure is unchanged -> `<name>:<tagHash>` already
    exists -> the same manifest is **retagged** as `<name>:v1.5.8` (a registry
    PUT, no build).
- **Immutability** — version tags do not move. If `<name>:v1.5.8` already
  exists and resolves to a different manifest, basil refuses (error) rather
  than overwrite it.
- **Write-only aliases** — version tags are convenience aliases for external
  consumers. Basil never fetches by them; internal dependency resolution
  remains closure-hash only (no moving tags enter the graph).

## `ON` event filtering (reaction loops)

`image-published` is self-referential in principle (basil both builds *and*
pushes images). But **`TASK`s produce no image, so they cannot re-trigger an
`image-published` event — no loop is possible from a `TASK`.** An
`image-published` reaction therefore never causes further image production; it
only runs side-effect `TASK`s. If the DSL later supports a build reaction that
yields/republishes an image, the guard that was previously required would
reapply: abort if the event's image identity equals the target image being
produced. Until that form exists, no guard is needed.

## `TASK` → semantics

- **`TASK <cmd>…` is the command itself, like `RUN`** — not a file reference.
  It executes in the target's base image (`FROM`), as a one-shot container from
  the resolved base image, outside the LLB solve: not a buildkit `ExecOp`, no
  cache key, never a layer, never part of any image. Anything the command
  invokes must already be present in that image — `COPY`ed in by the target's
  build steps; basil injects nothing.
- **Trigger** — a task runs because its reaction's event fired, unconditionally
  and un-deduplicated. If the event matched, the task runs whether the target
  was built, pulled, or not built at all.
- **Execution form** — shell form (`TASK echo built`) runs via the image's shell
  (`/bin/sh -c`); exec form (`TASK ["prog", "arg"]`) executes directly. Working
  directory and user default to the image's `WORKDIR`/`USER`. Network is allowed
  (a task is an explicitly non-hermetic side effect).
- **Environment** — `BASIL_EVENT` (raw event JSON from stdin), `BASIL_IMAGE`
  (resolved digest the task runs in), `BASIL_REF_<TARGET>` (content-tag
  reference of each `BUILD` target in the same reaction; `<TARGET>` is the
  fully-qualified `AS` name uppercased with `/`, `.`, `-` → `_`). Registry
  credentials come from the operator's Docker/Podman config, as everywhere else.
- **Exit semantics** — exit 0 = success; non-zero fails the invocation (logged,
  surfaced). A failed task does not un-push an image already published by the
  same reaction. Re-running the event re-executes the task.
- **Ordering** — within a reaction, `BUILD` targets solve before `DO` tasks run.
  A task's base may be another target (`FROM <target>`), including a target that
  is `BUILD`'d, so the task runs in that target's resolved image once it exists.
- **No cache interaction** — tasks aren't in the solved graph; nothing to
  dedupe, cache-skip, or export.

## Registry & cache export

- **Credentials**: basil maintains none of its own. Auth for every registry
  operation (pushing content tags, exporting/importing cache) is the operator's
  existing Docker/Podman credentials (`$DOCKER_CONFIG` `auths`/`credHelpers`,
  Podman `auth.json`).
- **`BASIL_CACHE_REF`** — overrides the cache repo (default: the deliverable's
  fully-qualified name); basil appends the `cache-…` tags and uses the same
  operator credentials, never a separate set.
- Exporters (buildkit-native, via `exportCache`): `type=registry` (separate
  cache blobs, keeps the content tag clean — **recommended**) or `type=inline`
  (cache inside the image, simpler/self-contained).
- **Export**: one cache export per tag — `cache-<tagHash>`, `cache-<YYYYMMDD>`,
  `cache-<YYYYMM>`, `cache-<YYYY>`. buildkit's `CacheExports` is a repeated
  field, so this needs no extra solve.
- **Import**: `cache-<tagHash>` plus the current day/month/year tags, each
  `[ type=registry, ref=... ]`, for Level 2 reattach.
- **Multiple deliverables** = **one solve with N exporters**, one per
  deliverable target. The stock `buildkit-client` only exposes a single-solve
  exporter (comma-joined tags for one image); basil **must extend/recreate it
  to emit N exporters** in one `SolveRequest`. That support is a required
  deliverable of the recreated emitter.

## `basil gc` — tag pruning

`basil gc <age>` deletes basil-owned registry tags older than `<age>`.

- **Age syntax** — an integer with a unit: `h`, `d`, `w`, `y` (`basil gc 30d`).
- **Scope** — the deliverable repos named by the file's fully-qualified `AS`
  names, or one repo via `--repo <fq>`. Credentials are the operator's
  Docker/Podman config, as everywhere else.
- **What it prunes** — only basil's own ephemeral tags:
  - `cache-*` refs: the date-encoded ones (`cache-<YYYYMMDD>`, `-<YYYYMM>`,
    `-<YYYY>`) are pruned directly from the tag name; `cache-<tagHash>` by the
    image's `created` time.
  - content tags (`<name>:<tagHash>`), by the image's `created` time.
  - version tags (`<name>:vX.Y.Z`) are **never pruned** — they are
    user-created releases, not basil's to reclaim (no `--include-versions`).
- **Safety** — `--dry-run` lists what would be deleted without deleting.
  Pruning removes the tag reference; blobs are reclaimed by the registry's own
  GC. Deletion is irreversible.

## Dependency & contribution strategy

- **Base**: `buildkit-client` (maintained). No vendoring needed for the shell.
- **LLB emitter**: recreated by basil on top of the maintained proto types
  (`buildkit-rs` `ops.proto`). Avoids resurrecting the abandoned `buildkit-llb`.
- **Contribute back**: once stable, contribute the recreated LLB emitter
  upstream; keep basil-specific DSL→LLB separate.

## Decided defaults

- **Content tag**: `<fully-qualified-AS>:<tagHash>` — the deliverable's `AS`
  name is its image reference; the closure digest is the tag (no `basil`
  marker). `basil-version` is a closure component, so a basil change yields a
  different tag.
- **Cache refs**: `<fully-qualified-AS>:cache-<tagHash>`,
  `<fully-qualified-AS>:cache-<YYYYMMDD>`, `…:cache-<YYYYMM>`, `…:cache-<YYYY>`.
  The repo defaults to the deliverable's `AS` name, overridable via
  `BASIL_CACHE_REF`. Exporter: `type=registry` (my rec).
