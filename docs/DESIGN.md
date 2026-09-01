# Basil — Design

Basil is a plain-process, buildkit-graph-owning build tool. It parses a
Dockerfile-like DSL with three extra clauses, emits its own Low-Level Build
graph (LLB), and drives a `buildkitd` daemon it owns. It is **not** a thin
wrapper around buildkit's built-in `dockerfile.v0` frontend: basil owns the
graph so it can express `GIVEN`/`ON`/`TASK`.

## Clause semantics (encoding intent)

A file holds **multiple targets** (named stages, like a multi-stage
Dockerfile); the three clauses are **per-target**. An *explicit* `GIVEN`
marks a stage as a deliverable product; no `GIVEN` defaults to `GIVEN .`
(build-only intermediate).

```
FROM golang AS build            # intermediate: no GIVEN → not a product
COPY . /src
RUN go build

FROM scratch AS app             # deliverable
COPY --from=build /src/app /app
GIVEN [ app.cue ]                       # identity + hermetic input set
ON [ image-published, pr-merged ]       # per-target policy

FROM nginx AS edge              # second deliverable, different gate
GIVEN [ edge.cue ]
ON [ pr-opened ]
TASK [ notify.sh ]                      # side effect, NOT a layer
```

A stage with an explicit `GIVEN` is a deliverable (tagged/pushed). **No
`GIVEN` defaults to `GIVEN .`**, so every stage carries a defined input set;
stages relying on the default are build-only intermediates.

- **`GIVEN`** — the stage's hermetic input set and identity. The **base hash is
  the OCI-style layer digest** of a synthetic snapshot of the **full input
  closure** (own `GIVEN` ∪ depended stages' `GIVEN` ∪ build file). **A stage may
  only `COPY` the subset of the context listed in its `GIVEN`** (parse-time
  reject otherwise); `GIVEN .` (default for intermediates) allows copying
  anything. The **effective input closure of a deliverable includes its own
  `GIVEN` plus the `GIVEN` of every stage it depends on** (`FROM` / `COPY
  --from`, transitively), so a deliverable's graph-hash honestly covers what
  its intermediates read. The **tag hash** folds the same full closure together
  with `basil-version` — so a basil change, changed dependence, or changed
  intermediate input yields a different tag even when the deliverable's own
  listed files are unchanged.
- **`ON`** — per-target policy. Which CI events this target reacts to; compared
  against the delivered event at parse time. A non-matching target yields a
  `skipped` (exit 0) without blocking sibling targets.
- **`TASK`** — non-hermetic side effect attached to a target. Runs because its
  event fired, unconditionally and un-deduplicated; not a graph `ExecOp`,
  never a layer and never part of the image.
- Targets depend on each other, so the LLB is a DAG across targets. Edges come
  from `COPY --from=<earlier>`, `FROM <target>` bases, and a `TASK` following a
  `FROM <target>` (task ordered after that base target). **Skip applies only to
  deliverable targets**: an intermediate (implicit `GIVEN .`) is always built.
  A skipped deliverable that others depend on (via `COPY --from` or `FROM`) is
  **pulled and used as the base/mount** rather than skipped, so dependents have
  something to build on (see Identity/Idempotency).

## Architecture

```
┌─────────────── basil (one binary) ──────────────────┐
│  parse DSL → hash(GIVEN) → event check → emit LLB   │
│  → solve via buildkit-client → push/cache           │
│  → run gated TASKs (outside the solve)              │
│  → daemon lifecycle (owned buildkitd)               │
└──────────────────────────────────────────────────────┘
            │ gRPC session + filesync + tunnel
            ▼
        buildkitd  ──(we own start/stop)──
```

- The pipeline has three phases: **emit+solve** (build/push/cache), then
  **run gated `TASK`s** — always-run side effects outside the graph, keyed by
  the delivered event, not by build state.

- **Client shell**: `buildkit-client` (arcbox/AprilNEA) — session, filesync,
  auth, cache, tunnel, progress, CLI. MIT/Apache, maintained (fresh pushes,
  releases, CI). The modern, operative base.
- **LLB emitter**: `buildkit-client` has **no client-side graph builder** — it
  hands everything to buildkit's built-in `dockerfile.v0` (`solve.rs`:
  `frontend: "dockerfile.v0"`, `definition: None`). Basil needs to **own the
  graph** so it can express `GIVEN`/`ON`/`TASK`. So basil **recreates** an LLB
  emitter on top of the maintained proto types (`buildkit-rs` `ops.proto`),
  rather than resurrecting an abandoned crate. Contribute the emitter back
  once stable.

## Daemon mode (F)

- **Default**: basil manages `buildkitd` as a **spawned sidecar** on demand;
  reuse it across builds when warm (persistent socket).
- **Nice-to-have**: connect to an externally already-running `buildkitd`
  (a well-known socket/endpoint), for operator-managed setups.

## `GIVEN` → identity & idempotency

**Definition of the base hash**: the `GIVEN` base hash is the **OCI-style
layer digest** of a **synthetic snapshot** containing exactly the **full input
closure** of the target — its own GIVEN files, the GIVEN files of every stage
it depends on (`FROM`/`COPY --from`, transitively), and the build file — at
their build-context-relative paths — **not** the repo's whole tree. It is an
**input-set identity digest**: it identifies the exact source content the
build consumes, not a blob a registry stores.

Basil produces the digest deterministically:

- Assemble a synthetic directory of exactly the closure files at their
  relative paths, preserving mode, exec bit, and symlinks.
- Serialize to a **deterministic tar**: entries sorted, `uid`/`gid` fixed to 0,
  normalized header fields.
- **Compress deterministically** (fixed level, no mtime/name in the gzip
  header) — required, or identical content yields different digests.
- `digest = sha256(tar.gz)`.

**On byte provenance**: bytes are bytes — the digest is deterministic over
bytes. Altered inputs produce an altered hash (correct by construction); a
different filter is a different file set and thus a different hash (also
correct). The only rule is **hash the exact files**: the digest describes
exactly the set it was computed over. If the pre-flight Level‑1 skip must
match the build, pass the build the same files — do not reconcile client vs
server after the fact.

Tar is content-addressed by digest; there is no "matches `git write-tree`"
oracle as there was for the earlier git-tree scheme. Determinism (ordering,
mtimes, ownership, compression) is therefore a **correctness requirement**,
not an optimization. Determinism is pinned by a **conformance fixture**: a
checked-in set of golden inputs whose expected digest is recorded; the test
suite recomputes it and fails on any change.

**Two keys** (the sound split, **per target**, `name` = target name from `AS`):

1. `registry/ns:<name>-<tagHash>` — stable immutable content tag. Checked
   first; on hit, return the ref with **zero daemon work** (Level 1 skip).
   No `basil` marker: the format is simply `<name>-<tagHash>`. `tagHash` is the
   digest of the **full input closure** (own GIVEN ∪ every depended stage's
   GIVEN ∪ build file) folded with `basil-version`, so the tag stays correct
   across basil changes, changed dependences, and changed intermediate inputs.
2. `registry/ns:<name>-cache-<full-closure-digest>` — **cache namespace**, keyed
   on the same full closure (GIVEN ∪ FROM.digest ∪ args ∪ basil-version).
   Imported/exported around a rebuild (Level 2 reattach). The cache key and the
   tag hash use the **same** full closure, so the sound fast path and the
   reattach path cannot diverge.

**Soundness**: Level 1 is exact *only if* the closure is pinned. Basil **warns**
on an unpinned `FROM` (no `@sha256`), because then the stable tag can't claim
exact-GIVEN identity.

### Event input (`ON`) — per target

- Event delivered on **stdin** as JSON; read by `basil build` (with `--event`
  flag). One event per invocation.
- Each target's `ON` is compared against the delivered event at parse time;
  non-matching targets are `skipped` (exit 0) without blocking matching
  siblings. A matching target whose image already exists **pulls it and runs
  its `TASK`s** rather than rebuilding.
- Parser maps event types → workflow `on:` triggers. Manual trigger = an event
  supplied from stdin without a CI workflow.

## `ON` event filtering (reaction loops)

`image-published` is self-referential in principle (basil both builds *and*
pushes images). But **`TASK`s produce no image, so they cannot re-trigger an
`image-published` event — no loop is possible from a `TASK`.** An
`image-published` reaction therefore never causes further image production; it
only runs side-effect `TASK`s. If the DSL later supports a build reaction that
yields/republishes an image, the guard that was previously required would
reapply: abort if the event's image identity equals the target image being
produced. Until that form exists, no guard is needed.

## `TASK` → semantics (per target)

- **A `TASK` runs because its event fired — unconditionally and
  un-deduplicated.** The event is the only trigger. If the event fired, the
  task runs, whether the required build ran, the output image was pulled, or
  nothing built at all.
- **`TASK` is not a graph `ExecOp`.** It is basil-orchestrated side effects
  outside the LLB/solve/cache, run after the artifact exists (or regardless).
  It is never a layer and never part of the image.
- **No cache interaction.** `IgnoreCache`/build-keying is irrelevant: the task
  isn't in the solved graph, so there is nothing to dedupe or cache-skip.
- **Ordering**: a `TASK` may follow a `FROM <base-target>`, creating a DAG
  edge — the task runs after that base target's artifact is built or pulled. It
  still never yields an image and never participates in the cache graph.
- Cache export simply has nothing to export from it — side effects don't leak
  into the cache graph.

## Registry & cache export

- Registry is the only back-end surface basil owns: `BASIL_REGISTRY` (host +
  creds), `BASIL_CACHE_REF` (optional override for the cache namespace).
- Exporters (buildkit-native, via `exportCache`): `type=registry` (separate
  cache blobs, keeps the `GIVEN` tag clean — **recommended**) or `type=inline`
  (cache inside the image, simpler/self-contained).
- Import: `[ type=registry, ref=... ]` for Level 2.
- **Multiple deliverables** = **one solve with N exporters**, one per
  deliverable target. The stock `buildkit-client` only exposes a single-solve
  exporter (comma-joined tags for one image); basil **must extend/recreate it
  to emit N exporters** in one `SolveRequest`. That support is a required
  deliverable of the recreated emitter.

## Dependency & contribution strategy

- **Base**: `buildkit-client` (maintained). No vendoring needed for the shell.
- **LLB emitter**: recreated by basil on top of the maintained proto types
  (`buildkit-rs` `ops.proto`). Avoids resurrecting the abandoned `buildkit-llb`.
- **Contribute back**: once stable, contribute the recreated LLB emitter
  upstream; keep basil-specific DSL→LLB separate.

## Decided defaults

- **GIVEN tag format**: `<name>-<tagHash>` (OCI-layer digest folded with
  basil-version, no `basil` marker).
- **Cache default**: `type=registry` (my rec).