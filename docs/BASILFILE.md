# Basilfile reference

A Basilfile is a **Dockerfile-like DSL** with a superset of instructions plus
basil-only clauses. It holds **targets** (named build stages) and **top-level
reactions**. Because basil drives `buildah`, the build instructions mirror the
Dockerfile semantics buildah understands; basil-only directives (`ON`, `TASK`,
`PLATFORM`, `OUTPUT`, `REGISTER`) are handled by basil and stripped from the
Containerfile it hands to buildah.

A file is a sequence of `FROM`…`ON` sections. Everything between a `FROM` and
the next `FROM`/`ON` is that target's build steps.

## Targets

A target is a named stage:

```
FROM <base> [AS <name>]
```

```dockerfile
FROM golang:1.23 AS build
COPY . /src
RUN go build -o /src/app

FROM scratch AS ghcr.io/mycompany/app        # deliverable
COPY --from=build /src/app /app
```

A deliverable can also keep a short stage name and declare where it is pushed
with `REGISTER`:

```dockerfile
FROM golang:1.23 AS build
COPY . /src
RUN go build -o /src/app

FROM scratch AS fennel
COPY --from=build /src/app /app
REGISTER ghcr.io/mycompany/fennel        # pushed here; addressed as `fennel`
```

- The **base** is an image reference (`alpine`, `alpine:3.20`,
  `alpine@sha256:…`), `scratch`, or another target's name (a stage alias — a
  base matching a target's `AS` name resolves to that stage, not an image pull).
- The **stage name**: an `AS <name>` alias, or the base's name when absent.
- A **deliverable** is a target whose `AS` name is a fully-qualified image
  reference (contains a `/`), e.g. `ghcr.io/mycompany/app`, OR a target with a
  `REGISTER` ref (short name + where it is published). Deliverables are pushed
  under `<push-ref>:basil-<tagHash>`.
- **`REGISTER <fullname>`** — gives a short-named target its fully-qualified
  push location. The target is *addressed* by its `AS` name (`-t fennel`,
  `BUILD fennel`, `COPY --from=fennel`); the image is *published* to the
  register ref (`-t 127.0.0.1:5001/fennel` also resolves it). `REGISTER` does
  NOT change the tagHash — content keeps its digest across registries; it only
  selects where the hash is looked up and pushed. `REGISTER` must appear inside
  the target (after its `FROM`).
- Intermediates (plain names) are built when needed by a deliverable but never
  tagged or pushed.

## GIVEN clause

```
GIVEN <context-path>…
```

Declares a target's closure inputs explicitly. When present, the target's
*derived-data* component (`derivedData(T)` in the identity formula) is computed
from exactly these context paths — files, directories, or globs — instead of
from its `COPY`/`ADD`/bind-mount sources:

```dockerfile
FROM scratch AS app
COPY a.txt /a               # still builds the image...
GIVEN b.txt                 # ...but identity is driven by b.txt alone
```

- **Identity only.** `GIVEN` only chooses which context files feed `tagHash`. The
  `RUN`/`COPY`/`ADD` instructions still build the image, and the full recipe still
  contributes through `programHash`. A changed recipe changes the hash; a changed
  non-given context file does not.
- **Relative paths only.** No leading `/`, no `..` escape past the context (the
  same rule as `OUTPUT` host destinations). Both files and dirs are allowed;
  dirs dive recursively, globs expand, and `.dockerignore` still applies. Multiple
  `GIVEN` lines accumulate into one closure set (e.g. `GIVEN Cargo.*` plus
  `GIVEN */Cargo.toml`).
- An explicit `GIVEN` path excluded by `.dockerignore` is an error (identity must
  not hash an input the build would reject).
- A target may declare `GIVEN` with no paths to pin *no* context inputs (an
  all-arg/external target).
- Not rendered to the builder — like `ON`, `TASK`, `PLATFORM`, `OUTPUT`,
  `REGISTER`, it is a basil-side directive stripped from the Containerfile.

Regardless of `GIVEN`, the Basilfile itself is never a `derivedData` input: its
job is encoded in `programHash`, so hashing it would make identity
self-referential (editing the recipe would change the content hash as well as
the program hash).

## Reactions

```
ON <event>[(<selector>…)]  [BUILD <target>…]  [DO <target>…]
```

```dockerfile
ON push(main)          BUILD ghcr.io/mycompany/app
ON pr-merged           BUILD ghcr.io/mycompany/app   DO migrate
ON image-published     DO notify smoke
```

- **`BUILD`** — build + tag + push the deliverable targets. A target listed
  here is a deliverable.
- **`DO`** — run the `TASK` clause attached to each target (side effects,
  outside the image build).
- Both lists are optional (`BUILD`-only, `DO`-only, or both). Within one
  reaction, `BUILD` runs first, then `DO`.

### Event tokens

| token | matches (GitHub Actions payload) |
|---|---|
| `pr-opened` | `pull_request` with `action == "opened"` |
| `pr-synchronized` | `pull_request` with `action == "synchronize"` |
| `pr-merged` | `pull_request` with `action == "closed"` and `merged == true` |
| `push[(branch…)]` | push ref; bare match any branch, `push(main, develop)` match those |
| `tag` | push ref starting with `refs/tags/` |
| `release[(selector…)]` | a `release` event; bare matches **any** release, selectors narrow it (e.g. `release(published)`, `release(prerelease)`, `release(draft)`). Selectors match the `prerelease`/`draft` booleans or any `action` value |

Other tokens (e.g. `image-published`) parse but are deferred (no native carrier
yet).

## Instructions

### `FROM`
```
FROM [--platform=<sel>] <base> [AS <name>]
```
The target's base image and optional stage name. `--platform=<sel>` picks the
base's platform: `$BUILDPLATFORM` expands to the build host's native platform;
`$TARGETPLATFORM` expands to the target's declared `PLATFORM` (per-target
`PLATFORM`, else the file-wide default); when the target declares none (or
several) it falls back to the build host's platform. A literal `linux/arm64`
selects that platform directly. A build stage that must run natively uses
FROM --platform=`$BUILDPLATFORM` ...
See [Targets](#targets).

### `MODULE`
```
MODULE <path> [-f <file>] [TARGET <name>] [AS <name>]
```
A target's base is **another basil build** instead of an image: `<path>` is the
module's context directory (its own Basilfile + its own build inputs),
resolved relative to the importing file's directory (absolute paths allowed).
`-f` names the module's Basilfile (default `<path>/Basilfile`); `TARGET`
selects its deliverable (when omitted the module file must declare exactly one
qualified deliverable — a `REGISTER` or qualified name); `AS` gives the
importer's stage alias for `COPY --from=<alias>`.

The module is a separate identity domain: its deliverable's `tagHash` is
folded into the importer (`base.module.<alias>`), and its build is
use-or-build — if `<module-push-ref>:basil-<tagHash>` already exists in the module's
registry the importer's `FROM <ref>:basil-<tagHash>` just pulls it, otherwise the
module is built and pushed first (recursively; module cycles are an error). A
module inside the importer's context is *not* excluded from the importer's
`derivedData` — dockerignore it there if `COPY .` would grab it (sibling
modules avoid the question entirely).
```
FROM scratch AS app
MODULE ../shared -f Basilfile.shared TARGET build AS shared
COPY --from=shared /out/artifact /artifact
```

### `ARG`
```
ARG <name>[=<default>]
```
A build arg. Resolved from `--build-arg` (a `tag` event also threads
`VERSION`). Only a target that actually references the arg in a `RUN`/`ENV`
value consumes it and changes its identity.

### `ENV`
```
ENV <K>=<V> [<K>=<V>…]
```
Sets image environment variables (and config). Accumulates for later `RUN`
steps and the image's `Env`.

### `IMPORT`
```
IMPORT <image>[:tag][@digest]
```
Inlines the root filesystem content of another image into the build **at that
point**: the import image's files are copied over the working rootfs (imported
files overwrite existing paths — layer-append semantics), and everything after
sees the merged rootfs. Only the import's **content** is inlined; its config
(`ENV`/`CMD`/…) is not transferred. Subsequent instructions continue on the
merged rootfs.

`IMPORT` cannot be expressed in a Containerfile, so a stage containing one is
built imperatively (`buildah from`/`config`/`copy`/`run`/`add` + `commit`)
instead of rendered to `buildah bud`. The image may be pinned (`@sha256`) for
a hermetic build; unpinned refs are resolved per build (like `FROM`) and the
resolved digest folds into the `tagHash` — `basil pin` pins them too.

- Only **external** images (a ref naming a local stage is an error).
- An import lands as a new image layer carrying the import's flattened content.
- `RUN --mount=type=secret` works via a file bind mount; `type=ssh` is
  **not** supported in an IMPORT stage (parse-time build error).

Known gaps: `IMPORT` of a local stage (use `COPY --from=<stage>` instead),
`FROM --platform=$BUILDPLATFORM` inside an IMPORT stage.

### `COPY`
```
COPY [--from=<stage>] [--chown=<u:g>] [--chmod=<mode>] <src>… <dest>
```
Copies into the image. Without `--from`, sources are context paths (globs
allowed, `.dockerignore` applied). With `--from=<stage>`, sources are taken
from another stage's filesystem.

### `ADD`
```
ADD [--chown=<u:g>] [--chmod=<mode>] <src>… <dest>
```
Like `COPY`; a URL source marks the target non-hermetic.

### `RUN`
```
RUN [--mount=…] [--network=<none|host|default>] <command>
```
Executes a command during the build. Two forms:
- shell form: `RUN echo hi`
- exec form: `RUN ["echo", "hi"]`

`--mount` and `--network` are rendered to buildah (which supports them):
- `--mount=type=bind,source=…,target=…[,readonly]`
- `--mount=type=cache[,id=…],target=…`
- `--mount=type=secret,id=…[,target=…]` — the secret value is supplied via
  `BASIL_SECRET_<ID>` (uppercased ID) pointing at the file; missing → error.
- `--mount=type=ssh[,id=…]` — forwarded as `--ssh` (the agent).
- `--network=none|host|default`.

Known limitation: `--mount=type=bind` renders correctly but buildah 1.39
rootless may fail it at runtime ("mount not really mounted"); cache/secret/ssh
and `--network` work.

### `OUTPUT`
```
OUTPUT <container-src> <host-dest>
```
Reverse `COPY`: after the image is built/pushed, copy a file out of the image's
root filesystem to the host at `host-dest` (relative to the build context;
**relative paths only** — no leading `/`, no `..`). Runs on every build
**including an idempotent-skip** (basil pulls the published image and extracts).
Never fed to the builder; folded into the target's identity.

### `COMMIT`
```
COMMIT
```
Checkpoints the build so far into the **local buildah layer cache**: the
working container is committed into rootless containers/storage as a
`basil-checkpoint-<hash>` image (`<hash>` = the closure of the instruction
prefix it seals) — **never pushed to any registry**, exactly like docker's
per-instruction layer cache but at a coarser granularity (a whole group of
instructions). On a later build, the stage **resumes from the deepest
checkpoint present in local storage** — the sealed instruction prefix is not
re-executed — and any `COMMIT` whose image is missing seals it on the way.
A pure cache-policy directive: it never changes image content (the final
image is identical with or without it, so `tagHash` is unaffected).

- The checkpoint ref is derived from the exact instruction prefix it seals:
  any change to the prefix changes the hash and makes the old checkpoint
  inert.
- `COMMIT` can appear multiple times per stage; resume always uses the deepest
  existing one (e.g. after a new `RUN` is added below the last `COMMIT`, a
  later run resumes from that last checkpoint and only the tail re-runs).
- Checkpoints are machine-local cache: `basil gc` (registry-only) never sees
  them; clean them with `buildah rmi` like any other cached image.

### Image-config instructions
```
CMD <command>            # shell form: CMD echo hi
CMD ["prog", "arg"]      # exec form
ENTRYPOINT <command>     # or ENTRYPOINT ["prog", "arg"]
USER <user>              # USER root / USER 1000:1000
WORKDIR <path>
LABEL <K>=<V> [<K>=<V>…]
EXPOSE <port>…
VOLUME <path>…
```

### Deferred (parse-time error)
Heredocs (`<<EOF`), `COPY --link`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`,
`ONBUILD`, `RUN --security`, exec-form `RUN` with unrecognized flags.

## `TASK` clause

A target may carry a `TASK` — a side-effect command run on its base image as a
one-shot container, **not part of any image**:

```
TASK <command>          # shell form
TASK ["prog", "arg"]    # exec form
```

Tasks run when a matching reaction's `DO` names the target — or manually with
`basil run <target>`. `basil run` excludes registered targets: a target with a
`REGISTER` clause is a published deliverable, so it is refused as a run target
(the manual `DO` is for non-published targets). `basil run` builds everything
the task depends on the **standard way** first: an internal `FROM <stage>`
base (and its transitive stage deps) is reconciled as a
`basil-cache-<stageHash>` cache image in the cache repo (`--cache-to`,
`BASIL_CACHE_REF`, else the target's own repo when it is a qualified ref), and
a module base is built use-or-build; only the run target itself is never
published. REGISTERed stages in the dependency subset follow the same caching
as the build path: they resolve to their published `<register>:basil-<hash>`
artifact (use-or-build), never an intermediate cache image. The task then runs
on the resolved base image. Exit 0 = success; a non-zero exit fails the
invocation. The side-effect environment includes
`BASIL_EVENT` (the raw event) on the event path, and `BASIL_TARGET` (the
target name) on the manual `run` path.

## `PLATFORM` clause
```
PLATFORM <os>/<arch>[/<variant>]…
```
Sets the platform(s) a target builds for. Inside a target it sets that target's
list; before the first `FROM` it sets the file-wide default. Multiple platforms
make the content tag an OCI index.

## Identity (`tagHash`)
`COMMIT` is transparent to identity: it never enters `programHash` (like
`OUTPUT`), and the stage's `tagHash` is unchanged by adding or removing one
(the final image is the same). Each checkpoint point gets its own derived
closure — `C(prefix)` — computed from the exact instructions before it, which
is what names its local `basil-checkpoint-<hash>` image.

Every target's `tagHash` is **derived**, not declared:

```
C(T) = derivedData(T)                          # context files the target reads
     ∪ C(B)  for every depended stage B         # FROM <stage>, COPY --from=<stage>
     ∪ {resolved-digest}  for external bases    # FROM <image>
     ∪ C(M)  for every module base             # MODULE <path> — the module's deliverable tagHash
     ∪ {resolved-digest}  for every IMPORT      # IMPORT <image> — occurrence-ordered
     ∪ args(T)                                  # resolved ARG / --build-arg
     ∪ platforms(T)
     ∪ programHash(T)                          # ordered, arg-expanded instructions
     ∪ {basil-version}
```

The content tag is `<deliverable-AS-name>:basil-<tagHash>` — stable and immutable.
Whether an image is rebuilt is decided by whether that tag already exists (the
**idempotent check**).

## Command reference

```
basil closure [CONTEXT] [TARGET] [-f FILE]   # report each target's tagHash
basil build [CONTEXT] [-f FILE] [-t TARGET] [--force] [--cache-to REPO]  # build a deliverable directly (no event); --force rebuilds even when the content tag already exists; --cache-to overrides where intermediate stages are cached
basil handle [CONTEXT] [-f FILE] [--event F] [--cache-to REPO] # read an event (stdin or --event) and react
basil promote <VERSION> [CONTEXT] [-f FILE] [-t TARGET] # set the immutable v<VERSION> tags on the deliverables' published images (the content tags must already exist — promotion never builds)
basil pin [FILE]                            # resolve unpinned bases to @sha256 in place
basil gc <AGE> [CONTEXT] [-f FILE] [--repo FQ] [--dry-run] [--cache-only]   # prune basil-owned tags older than <AGE> (--cache-only prunes only the basil-cache-* namespace) (repos from qualified AS names AND REGISTER refs)
```

The context is a bare positional arg, docker-style: the DSL file defaults to
`<context>/Basilfile`; `-f FILE` overrides it (a relative `-f` resolves against
the current directory, not the context). `pin` does not recurse into `MODULE`
files — pin each module file directly; `gc` stays per-file too.
- **`build`** never reads stdin; with no `-t` it builds every qualified
  deliverable.
- **`handle`** reads the event JSON from stdin (or `--event`), matches `ON`
  reactions, builds/promotes, runs `DO` tasks and `OUTPUT`s.
- **`pin`** resolves every unpinned **external** image base to `@sha256` and
  rewrites the file in place. Internal stage refs (`FROM <stage>`), `scratch`,
  and already-pinned bases are left untouched; each distinct base is resolved
  once. Idempotent — re-running it reports "no unpinned bases".
- **`gc`** prunes only basil's own ephemeral tags (`basil-cache-*`, `basil-<tagHash>` content tags);
  version tags (`vX.Y.Z`) are never pruned. `--dry-run` lists without deleting.
- `AGE` uses an integer plus unit: `h`, `d`, `w`, `y` (`30d`, `4w`, `1y`).

## Notable environment variables

- `BUILDKIT_HOST` — **not used** (buildah backend; there is no daemon).
- `BASIL_INSECURE_REGISTRY` — set to `1`/`true`/`yes` to accept invalid TLS
  certs for the registry client (e.g. a self-signed test registry).
- Registry auth comes from the operator's standard Docker/Podman config.