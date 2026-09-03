# Basilfile reference

A Basilfile is a **Dockerfile-like DSL** with a superset of instructions plus
basil-only clauses. It holds **targets** (named build stages) and **top-level
reactions**. Because basil drives `buildah`, the build instructions mirror the
Dockerfile semantics buildah understands; basil-only directives (`ON`, `TASK`,
`PLATFORM`, `OUTPUT`) are handled by basil and stripped from the Containerfile
it hands to buildah.

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

- The **base** is an image reference (`alpine`, `alpine:3.20`,
  `alpine@sha256:…`), `scratch`, or another target's name (a stage alias — a
  base matching a target's `AS` name resolves to that stage, not an image pull).
- The **stage name**: an `AS <name>` alias, or the base's name when absent.
- A **deliverable** is a target whose `AS` name is a fully-qualified image
  reference (contains a `/`), e.g. `ghcr.io/mycompany/app`. Deliverables are
  pushed under `<name>:<tagHash>`.
- Intermediates (plain names) are built when needed by a deliverable but never
  tagged or pushed.

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
base's platform: `$BUILDPLATFORM` (build host's native platform) or
`$TARGETPLATFORM` (the target platform), both build-staged vars buildah expands
at build time; a literal `linux/arm64` selects that platform directly. A
build stage that must run natively uses `FROM --platform=$BUILDPLATFORM …`.
See [Targets](#targets).

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
`ONBUILD`, exec-form `RUN` with unrecognized flags.

## `TASK` clause

A target may carry a `TASK` — a side-effect command run on its base image as a
one-shot container, **not part of any image**:

```
TASK <command>          # shell form
TASK ["prog", "arg"]    # exec form
```

Tasks run only when a matching reaction's `DO` names the target. Exit 0 =
success; a non-zero exit fails the invocation. The side-effect environment
includes `BASIL_EVENT` (the raw event).

## `PLATFORM` clause
```
PLATFORM <os>/<arch>[/<variant>]…
```
Sets the platform(s) a target builds for. Inside a target it sets that target's
list; before the first `FROM` it sets the file-wide default. Multiple platforms
make the content tag an OCI index.

## Identity (`tagHash`)

Every target's `tagHash` is **derived**, not declared:

```
C(T) = derivedData(T)                          # context files the target reads
     ∪ C(B)  for every depended stage B         # FROM <stage>, COPY --from=<stage>
     ∪ {resolved-digest}  for external bases    # FROM <image>
     ∪ args(T)                                  # resolved ARG / --build-arg
     ∪ platforms(T)
     ∪ programHash(T)                          # ordered, arg-expanded instructions
     ∪ {basil-version}
```

The content tag is `<deliverable-AS-name>:<tagHash>` — stable and immutable.
Whether an image is rebuilt is decided by whether that tag already exists (the
**idempotent check**).

## Command reference

```
basil closure [FILE] [TARGET] [--context DIR]   # report each target's tagHash
basil build [FILE] [--context DIR] [-t TARGET]  # build a deliverable directly (no event)
basil handle [FILE] [--context DIR] [--event F] # read an event (stdin or -F) and react
basil gc <AGE> [FILE] [--repo FQ] [--dry-run]   # prune basil-owned tags older than <AGE>
```

- **`build`** never reads stdin; with no `-t` it builds every qualified
  deliverable.
- **`handle`** reads the event JSON from stdin (or `--event`), matches `ON`
  reactions, builds/promotes, runs `DO` tasks and `OUTPUT`s.
- **`gc`** prunes only basil's own ephemeral tags (`cache-*`, content tags);
  version tags (`vX.Y.Z`) are never pruned. `--dry-run` lists without deleting.
- `AGE` uses an integer plus unit: `h`, `d`, `w`, `y` (`30d`, `4w`, `1y`).

## Notable environment variables

- `BUILDKIT_HOST` — **not used** (buildah backend; there is no daemon).
- `BASIL_INSECURE_REGISTRY` — set to `1`/`true`/`yes` to accept invalid TLS
  certs for the registry client (e.g. a self-signed test registry).
- Registry auth comes from the operator's standard Docker/Podman config.