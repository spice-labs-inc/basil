# Basil — Design

Basil is a plain-process build tool. It parses a Dockerfile-like DSL, derives
each target's input closure from its instruction graph, and drives `buildah`
(as a subprocess over rootless containers/storage) to build and push images.
Top-level `ON` reactions map CI events to targets: `BUILD` targets are built
and pushed under a content tag, `DO` targets run their attached `TASK` side
effects, and `OUTPUT` extracts files back to the host. Basil owns the *identity*:
it computes an honest, derived content hash (tagHash) for every target.

## Targets and reactions (encoding intent)

A file holds **targets** (named stages) and **top-level reactions**. A target is
a normal stage — `FROM <base> AS <name>` plus instructions — and may carry
`TASK` clauses: commands run as side effects. A deliverable's `AS` name is its
**fully-qualified image reference** (`ghcr.io/mycompany/app`); intermediates and
task targets use plain internal names. A deliverable may instead keep a short
`AS` name and declare its publish location with `REGISTER <fullname>`: it is
then addressed by its name (`BUILD fennel`, `-t fennel`) but pushed to the
register ref. `REGISTER` does not change the tagHash — content keeps its digest
across registries, so the same image has the same hash anywhere; it only selects
*where* the tag is looked up and pushed. A target may also declare
`GIVEN <context-path>…` to pin its identity to explicit inputs instead of the
`COPY`/`ADD`-derived set (see Identity below). A reaction maps one event to targets with
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

- **`BUILD <target>…`** — build + tag + push. Listing a target here is what makes
  it a **deliverable**; a target never in any `BUILD` is a build-only
  intermediate (always built when a deliverable needs it, never tagged or
  pushed). `BUILD` cannot reference a task target. A deliverable's `AS` name is
  its fully-qualified image reference; basil pushes it as `<name>:<tagHash>`.
- **`DO <target>…`** — run the `TASK` commands attached to that target. `DO`
  references **only task targets** (targets with a `TASK` clause). Side effects:
  not build graph steps, never layers, never part of the image; they run because
  the event matched, whether or not the target built this run.
- **`TASK <cmd>…`** — a target clause: commands to run **on the target's base
  image** (its `FROM` — an external image, or another target's output, including
  a target that is `BUILD`'d), like `RUN` but outside the build. A task target
  may also carry build steps — they prepare the task's runtime image (e.g.
  `COPY` the executable in), built locally, never tagged or pushed.
- **`PLATFORM <os>/<arch>…`** — the platform(s) a target builds for. On a target
  it sets that target's list; before the first `FROM` it sets the **file-wide
  default** (targets without their own inherit it). No `PLATFORM` = the builder's
  native platform. Multiple platforms make the content tag an OCI index (manifest
  list).
- **`FROM --platform=<sel>`** — per-stage base platform selection. `$BUILDPLATFORM`
  (build host's native platform) or `$TARGETPLATFORM` (target platform) are
  build-staged variables buildah expands at build time; a literal `linux/arm64`
  selects directly. A build stage that must run natively (e.g. a compiler) uses
  `FROM --platform=$BUILDPLATFORM …` while the deliverable targets
  `$TARGETPLATFORM`. The selector is folded into the stage's identity.
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
- Targets depend on each other, so the build graph is a DAG across targets
  (edges from
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
  `ON push` matches any branch push (exact match; no glob selectors).
- Within one reaction, `BUILD` targets build first, then the `DO`'d tasks run.
  Both lists are optional (`BUILD`-only, `DO`-only, or both).

## Identity (derived, not declared)

Every target's input closure is **derived from its instruction graph**, not from
a declared allowlist. Per target `T`:

```
C(T) = derivedData(T)
       ∪ C(B)   for every depended stage B          // FROM <stage>, COPY --from=<stage>
       ∪ { resolvedDigest(X) }  for external bases  // FROM <image>
       ∪ { resolvedDigest(X) }  for every IMPORT    // IMPORT <image>, occurrence-ordered
       ∪ platforms(T)                              // declared build platforms
       ∪ programHash(T)                            // ordered, arg-expanded instructions
       ∪ { basil-version }

Each `COMMIT` in a target additionally derives the closure of the instruction
*prefix* it seals — C(prefix) — computed by the same component formula (the
sealed content is exactly that prefix's image). `COMMIT` itself never enters
programHash, so the stage's tagHash is identical with or without it.
```

- **`derivedData`** — the exact context files each instruction reads, resolved
  at parse time: `COPY`/`ADD` sources and `--mount=type=bind,from=context`
  sources, filtered by `.dockerignore` (Docker's own rules, honored as-is),
  globs expanded against the local context. `COPY .` therefore means
  whole-tree identity explicitly — there is no implicit default. The git
  metadata directory (`.git`, at any depth) is always excluded, like Docker's
  context archive: it can never feed identity. The build file
  itself (the recipe that produced the target graph) is excluded: its effect is
  already captured by `programHash`, so hashing it would make identity
  self-referential. A target may
  declare `GIVEN <context-path>…` to **replace** this set: `derivedData` is then
  computed from exactly the given paths (files/dirs/globs, still
  dockerignore-filtered) instead of the build-resolved sources. The build recipe
  still builds the image and still contributes via `programHash` — `GIVEN` only
  overrides which context files feed `derivedData`, decoupling identity from how
  the image is assembled.
- **`programHash`** — digest of the target's ordered instruction program with
  `ARG`/`ENV` references expanded. Includes every `RUN` command string and the
  image-config instructions (`ENV`, `CMD`, `ENTRYPOINT`, `USER`, `WORKDIR`,
  `LABEL`, `EXPOSE`, `VOLUME`), so a command or config change changes identity.
  `ARG` declarations themselves are excluded: an ARG is a textual substitution,
  so only its **substituted form** at a `$VAR` use site matters. An unreferenced
  ARG (global or per-target) has zero identity effect; a referenced one enters
  through the expanded text, so a `--build-arg` override changes the tag only
  for targets that use the variable.
  Excludes comments and whitespace (they don't churn the tag) and the
  reaction/TASK clauses (policy and side effects, not image content).

**Build args (`ARG`)** — declared either at file scope (before the first
`FROM`, Dockerfile global scope) or inside a target. A file-level ARG defaults
its name across the file; a stage only sees it after redeclaring the arg
(bare, or with its own default), mirroring Dockerfile scope rules. The
resolver feeds `programHash`'s expansion only — ARG values never enter the
closure as a separate record.
- **`FROM` digest** — folded resolved, not as the ref string. Because registries
  are content-addressed, a manifest digest is a commitment over every layer:
  the whole base image is covered without fetching a blob.
- **`MODULE`** — a target's base can be another basil build
  (`MODULE <path> [-f <file>] [TARGET <name>] [AS <alias>]`), a separate
  closure domain: the module is parsed, its own bases resolved, its closure
  computed bottom-up, and its deliverable's `tagHash` folded (as
  `base.module.<alias>`), so a module change retags the importer exactly once,
  through the derived artifact. At build time the module is use-or-built: if
  `<module-push-ref>:<tagHash>` exists in its registry the importer's rendered
  `FROM <ref>:<tagHash>` pulls it; otherwise the module is built and pushed
  first (recursive; cycles between modules are an error at discovery). Module
  paths resolve against the importing file's directory (D4); the module's own
  dockerignore/globs/args resolve against its own context. The module's raw
  files never enter the importer's derivedData (only its tagHash does) — a
  module inside the importer context is the operator's responsibility to
  dockerignore if `COPY .` would grab it (D3). `pin` does not recurse into
  module files; module `TASK`/`ON` reactions never fire on importer events;
  `--force` never propagates to module builds (they are reconciled by
  existence, always).
- **`platforms`** — the target's declared build-platform list (its `PLATFORM`
  clause, or the inherited file-wide default). Folded so a single-platform vs
  multi-platform build is a different tag — the content tag always identifies the
  same artifact shape.

`tagHash = H(C(T))` — a deterministic digest over a canonical serialization of
`C(T)`. `derivedData` is digested as a synthetic **git tree object** built with
git's own object model and hash: a subtree the local git index already covers —
fully tracked, unmodified, and entirely contained in the derived set —
contributes git's precomputed tree/blob hashes instead of re-hashing file
contents; partial (dockerignore/GIVEN/glob-filtered) or dirty
(modified/untracked) subtrees are hashed from the working tree. A context
outside any git repository has nothing borrowable and is fully computed — the
same object model, so the digest stays deterministic in both cases. Because the
digest commits to git's modes, the executable bit of a context file now changes
identity (a `chmod +x` retags anything that `COPY`s it). Empty directories
(git cannot represent them) fold in as a synthetic zero-entry tree. Scalar
components (`programHash`, base digests, args, version) are encoded as a sorted,
length-prefixed record.
Determinism is a correctness requirement, pinned by a **conformance fixture**:
checked-in golden inputs whose expected digest the test suite recomputes.

**One digest drives two refs** (per target):

1. `<name>:basil-<tagHash>` — stable immutable content tag, where `<name>` is the
   deliverable's **fully-qualified `AS` name** (`ghcr.io/mycompany/app`). The
   `basil-` prefix is basil's tag namespace: gc only prunes tags in it, so a
   foreign 64-hex tag is never mistaken for basil's.
   **Deliverable targets only.** One platform → a single image; several → an OCI
    index. Checked first; on hit, the idempotent check skips the rebuild.
2. **Intermediate cache** — every non-registered stage in the build graph is
   exported as `<repo>:basil-cache-<stageHash>`, where `<repo>` is the
   deliverable's repo by default (the `--cache-to` flag or `BASIL_CACHE_REF`
   override it). Reconcile-by-existence: a missing cache image for a stage is
   built per-instruction and pushed on any build path *including the
   idempotent skip*; stages are regenerable, so `basil gc` may reap them by
   age. The deliverable's build starts from `FROM <repo>:basil-cache-<stageHash>`
   for internal bases, so the final build pulls its intermediates instead of
   rebuilding them — cross-run and cross-machine reuse. `OUTPUT` extraction
   reads the same cache refs (a pull, never a rebuild).
   **REGISTERed stages are never cached as intermediates**: they are published
   deliverables, so dependents use their `<register>:basil-<hash>` content
   artifact (use-or-build, mirroring module deps). Same rule on the manual
   `basil run <target>` path — build and `run` share the caching; the only
   difference is that deliverables are not run targets.

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

## `IMPORT` — inlining an image mid-build

`IMPORT <image>` inlines the root filesystem content of another image at that
point in the build: the import's files land on top of the working rootfs
(imported files overwrite existing paths — layer-append semantics), and every
instruction after it sees the merged rootfs. It is a **content** operation: the
import image's config (`ENV`/`CMD`/…) does not transfer. Materialization is a
new image layer carrying the import's flattened content — the import's original
layer digests are not re-added individually.

- **Identity**: each import folds its *resolved manifest digest* as
  `import.<n>` (occurrence order) — an unpinned ref (`IMPORT foo:latest`)
  re-hashes when the tag moves. Pinned imports resolve offline; unpinned ones
  need the same registry resolution as `FROM` (the offline `closure` path
  errors on them). `IMPORT`'s position is in `programHash` (order is
  identity). Stage-name refs are rejected: inlining a stage's own layers is
  what `COPY --from=<stage>` already does.
- **One execution model**: every build is imperative — basil drives buildah
  per-instruction over a working container (from/config/copy/run/add/commit)
  with `ARG`/`ENV` substitution identical to `programHash`; no Containerfile is
  ever rendered, and `IMPORT` is just one instruction among the rest.
- **Parity with what a Containerfile would do**: `COPY`/`ADD` copy the exact
  dockerignore-filtered file set identity hashes (per-file, dest placement
  reconstructed), `RUN` gets the same mounts (`--mount` bind/cache, secrets via
  a single-file `--volume` — `buildah run` has no `--secret`), declared build
  args are exposed as run env like `buildah bud`'s build-args, `FROM
  <stage>`/`COPY --from=<stage>` resolve to the stage's published cache image,
  and `--force`/idempotent-skip/`OUTPUT`/promotion behave identically.

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

- The pipeline has two phases after parsing: **build/push/cache**, then **run
  `DO`'d `TASK`s and extract `OUTPUT`s** — always-run side effects keyed by the
  delivered event and the built image, not by build state.

## Build backend (buildah)

- **Execution**: basil drives `buildah` per-instruction over a working
  container (plain subprocesses over rootless containers/storage + runc/crun):
  `buildah from` the resolved base (platform selector and ref as separate
  argv), `buildah config` for image-config instructions, `buildah copy`/`add`
  for `COPY`/`ADD`, `buildah run` for `RUN`, `buildah add --from <img> <ctr> /
  /` for an `IMPORT`, `buildah commit` to the target ref — then `buildah
  push`. **No Containerfile is ever rendered.** No daemon, no graph protocol,
  no tunnel. **buildah is the only execution backend** — basil no longer
  depends on buildkit/buildkitd, an LLB emitter, a gateway bridge, or a
  vendored proto/tonic stack, and not even on `buildah bud`.
- **Identity**: basil still computes each target's `tagHash` from `derivedData`
  + `programHash` + bases + args (a derived content hash it owns), and pushes
  under `<name>:<tagHash>`. Config (`ENV`/`CMD`/`ENTRYPOINT`/…) is written
  natively by buildah.
- **Idempotent check**: before building, if every deliverable's content tag
  already exists in its registry, basil skips the rebuild (and still runs
  `OUTPUT`/promotion by pulling the published image). An image already in
  local storage at the content ref is pushed without replaying the build.
- **Instruction fidelity** (imperative mirrors the Containerfile semantics):
  `ARG`/`ENV` substitution mirrors `programHash` byte-for-byte; context
  `COPY`/`ADD` copy the exact dockerignore-filtered file set identity hashes,
  per-file with Docker's dest placement reconstructed; `RUN`
  `--mount=type=bind|cache` and `--network=none|host|default` pass through,
  secrets mount via a single-file `--volume` (`buildah run` has no `--secret`;
  the file exists only during the run and never lands in the image), and
  `RUN --mount=type=ssh` is a hard error (buildah run has no ssh mount).
  `FROM --platform` selectors pass to `buildah from`; the build-staged
  `$BUILDPLATFORM`/`$TARGETPLATFORM` variables expand to the host platform
  (the value bud would assume without a `--platform` flag).
- **`IMPORT`**: the same execution model — one instruction among the rest. The
  import image is pulled when needed (`localhost`/insecure refs get an
  explicit `--tls-verify=false` pull: `buildah copy/add --from` pull, but
  their internal pull has no http-localhost autodetect) and its rootfs is
  inlined via `buildah add --from <img> <ctr> / /`.

**Deferred** (parse-time error)

**Deferred** (parse-time error)

- Heredocs, `COPY --link`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD`,
  `RUN --security`.

**Correctness surface**

- Instruction fidelity: multi-stage `COPY --from` (resolving to published
  cache images), `ARG`/`ENV` expansion, shell-vs-exec command normalization,
  dockerignore-honest context copies, and that no Containerfile (or temp
  artifact) is ever involved.

**Test strategy**

- buildah e2e conformance against a live registry (config + `tagHash`), the
  closure-digest conformance fixture, and the `COPY --from` stage-fold unit test.

## Event input (`ON`) — reactions

- Event delivered on **stdin** as JSON; read by `basil handle` (with `--event`
  flag for a file). One event per invocation. **For now the JSON is the GitHub
  Actions event payload** (`$GITHUB_EVENT_PATH`), consumed verbatim — basil
  defines no event schema of its own yet.
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
  | `release[(selector…)]` | `release` | any release; selectors narrow by `prerelease`/`draft` or `action` |

  Other events (e.g. `image-published`) are **deferred** — a native carrier
  exists (`registry_package`), but the mapping is not pinned yet.

- Manual trigger = `basil build [CONTEXT] [-f FILE] [-t target]` builds a
  deliverable directly, with no event (default: every qualified deliverable in
  the file; the DSL file defaults to `<context>/Basilfile`, docker-style;
  `--force` rebuilds and re-pushes even when every content tag already
  exists).
- Manual task run = `basil run <target> [-f FILE] [CONTEXT] [--cache-to REPO]`
  runs a target's `TASK` clause in a one-shot container without an event. The
  target need not be a deliverable and is never published. Registered targets
  are excluded: a `REGISTER` clause marks a published deliverable, and `basil
  run` refuses them (the manual `DO` is for non-published targets). Everything
  the task depends on is built the standard way: an internal `FROM <stage>`
  base (and its transitive stage deps) is reconciled as a
  `basil-cache-<stageHash>` cache image in the cache repo (`--cache-to`, else
  `BASIL_CACHE_REF`, else the target's own repo when it is a qualified ref),
  and a module base is built use-or-build. REGISTERed stages in the subset
  follow the build path's caching: they resolve to their published
  `<register>:basil-<hash>` artifact (use-or-build), never a cache image. The
  task then runs on the resolved base image; `BASIL_TARGET=<name>` is set; a
  non-zero task exit fails the invocation.

## Promotion (version tags)

A version event (`v1.5.8`, `target-v1.2.3`) assigns a human version tag to
a target's image. Promotion is not a new verb: it is `ON tag BUILD <target>…`
(or `ON release …`) plus one extra tag on the result. Manual promotion is the
explicit CLI verb `basil promote <VERSION>`, which applies the same immutable
`v<version>` retag to each selected deliverable (`-t` filter, else every
qualified deliverable) without building — the content tag must already exist.

- **Version extraction** — the version comes from the event: a `tag` event
  parses its git tag ref; a `release` event uses the release's `tag_name`. The
  parsing is identical: `target-v1.2.3` names target `target` with version
  `1.2.3`; a bare `v1.5.8` applies to every target in that reaction's `BUILD`
  list.
- **Version as input** — basil reserves `ARG VERSION` and passes
  `--build-arg VERSION=<version>` when a `tag` or `release` event matches. Because identity
  is derived, only a target that actually references `VERSION` consumes it:
  - **Referenced** (`RUN go build -ldflags "-X main.version=$VERSION"`, with the
    arg declared), the override reaches programHash's substitution -> a fresh
    `tagHash` -> a real build -> the built image is tagged `<name>:v1.5.8`.
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
only runs side-effect `TASK`s. (The token is not yet wired to a native carrier,
so it never matches today.)

## `TASK` → semantics

- **`TASK <cmd>…` is the command itself, like `RUN`** — not a file reference.
  It executes in the target's base image (`FROM`), as a one-shot container from
  the resolved base image, outside the build: not part of any image, no cache
  key, never a layer. Anything the command
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
- **`--cache-to` / `BASIL_CACHE_REF`** — where intermediate-stage cache images
  are pushed (default: the deliverable's fully-qualified repo); basil appends
  the `basil-cache-…` tags and uses the same operator credentials, never a
  separate set.
- **Build layer cache**: `COMMIT` provides it — the working container is
  committed into local containers/storage as a `basil-checkpoint-<hash>`
  image (never pushed; docker-style caching at group granularity, coarser
  than per-instruction) and a later build resumes from the deepest existing
  checkpoint, skipping the sealed prefix. Without checkpoints, caching lives
  at the stage level: a stage's `basil-cache-<stageHash>` image and the
  idempotent check (content tag exists) skip the entire rebuild.
- **Intermediate cache**: every non-deliverable stage is exported under
  `basil-cache-<stageHash>` (one tag per stage), reconciled by existence on both
  build and idempotent-skip paths — a missing stage cache is rebuilt and pushed.
- **Multiple deliverables** = one build graph; each deliverable is built and
  pushed under its content tag.

## `basil gc` — tag pruning

`basil gc <age>` deletes basil-owned registry tags older than `<age>`.

- **Age syntax** — an integer with a unit: `h`, `d`, `w`, `y` (`basil gc 30d`).
- **Scope** — the deliverable repos named by the file's qualified `AS`
  names **and their `REGISTER` refs** (each target's `push_ref`), or one repo
  via `--repo <fq>`. The file defaults to `<context>/Basilfile`, docker-style.
  Credentials are the operator's Docker/Podman config, as everywhere else.
- **What it prunes** — only basil's own ephemeral tags (the `basil-`
  namespace), never a foreign tag:
  - `basil-cache-*` refs: the date-encoded ones (`basil-cache-<YYYYMMDD>`,
    `-<YYYYMM>`, `-<YYYY>`) are pruned directly from the tag name;
    `basil-cache-<tagHash>` by the image's `created` time.
  - content tags (`<name>:basil-<tagHash>`), by the image's `created` time.
  - version tags (`<name>:vX.Y.Z`) are **never pruned** — they are
    user-created releases, not basil's to reclaim (no `--include-versions`).
- **Safety** — `--dry-run` lists what would be deleted without deleting.
  Pruning removes the tag reference; blobs are reclaimed by the registry's own
  GC. Deletion is irreversible.

## Dependency strategy

- **Base**: `buildah` (via subprocess), `reqwest` (registry client), `tokio`,
  `clap`. Plain crates, no buildkit/daemon dependency.

## Decided defaults

- **Content tag**: `<fully-qualified-AS>:basil-<tagHash>` — the deliverable's
  `AS` name is its image reference; the closure digest is the tag, prefixed
  with `basil-` so basil's tags are self-identifying for gc and never collide
  with a foreign 64-hex tag. `basil-version` is a closure component, so a basil
  change yields a different tag.
- **Cache refs**: `<fully-qualified-AS>:basil-cache-<tagHash>`,
  `<fully-qualified-AS>:basil-cache-<YYYYMMDD>`, `…:basil-cache-<YYYYMM>`,
  `…:basil-cache-<YYYY>`. The repo defaults to the deliverable's `AS` name,
  overridable via
  `BASIL_CACHE_REF`. Exporter: `type=registry` (my rec).
