# basil

Basil is a plain-process build tool driven by declarative build files. It
parses a Dockerfile-like DSL, derives each build target's input closure into a
deterministic content identity (its `tagHash`), and drives `buildah` (as a
subprocess over rootless `containers/storage` + runc/crun) to build and push
images. Top-level `ON` reactions map CI events to targets, so the same file
expresses what to build, what to run as side effects, and what to copy back out.

There is no daemon and no orchestration service: basil is one binary you run
wherever you want a build to happen.

## What it does

- **Deterministic identity** — every target's `tagHash` is derived from the
  exact files it reads, its instructions, its base images, and its args, not
  declared by hand. Basil owns the identity; buildah just executes it.
- **Plain-process** — builds via `buildah build`/`push` subprocesses. No
  `buildkitd`, no graph protocol, no tunnel, no daemon lifecycle.
- **Event reactions** — `ON` clauses map events (push, PR, tag) to `BUILD`,
  `DO` (run scheduled tasks), and side effects.
- **Idempotent** — if a target's `<name>:<tagHash>` already exists, basil
  skips the rebuild (and still runs `OUTPUT`/promotion by pulling the image).
- **Reverse `COPY`** — `OUTPUT` extracts files out of the built image onto the
  host.
- **Promotion & GC** — version-tag promotion from `tag` events, and registry
  tag pruning.

## Installation

Basil is a standard Rust crate. Install from source:

```sh
# from the repo
cargo install --path .

# or directly
cargo install --git https://github.com/spice-labs-inc/basil
```

Requirements at runtime:

- **`buildah`** on `PATH` (1.39+), with a working rootless
  `containers/storage` (set up by `buildah`/`podman` on first use).
- **A registry** to push to (Docker Hub, GHCR, a local `registry:2`, …), with
  credentials from the operator's standard Docker/Podman auth config.
- **Rust** (for building from source).

You do **not** need buildkit, podman, or a daemon.

## Quick start

```sh
# build every qualified deliverable in Basilfile (no event needed)
basil build

# build one target explicitly
basil build -t ghcr.io/mycompany/app

# react to a CI event (GitHub Actions payload) from stdin
basil handle < event.json

# inspect a target's derived closure identity
basil closure
```

See **[docs/BASILFILE.md](docs/BASILFILE.md)** for the complete language
reference, and **[docs/DESIGN.md](docs/DESIGN.md)** for the design.