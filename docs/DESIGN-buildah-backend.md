# Design: buildah backend (execute on buildah instead of buildkit)

Status: **S1–S3 implemented, S4 in progress**. Commits `a791ba7`, `0d17df8`,
`3fef88d`. buildah is the **default** backend (`--backend buildah`):

- single + multi-stage builds via `buildah build`/`push` (no daemon/tunnel),
  correct OCI config attached natively, closure tagHash identity;
- promotion + Level-1 verified on buildah-built tags;
- DO/TASK via `buildah run` (env order fixed; exit codes correct).

Remaining: **S4** (delete the buildkit execution layer + deps — currently kept
behind `--backend buildkit`, both green) and **S5** (differential conformance
against `buildah bud`).

_Original proposal retained below for review._
Status: **proposal for review** — no code written. Follows the rule to finish the
design and get approval before implementing.

## 1. Why this is worth it (and what we lose)

basil's differentiator is **closure identity** (tagHash), event reactions,
promotion, tasks, and gc — all in `identity/`, `event/`, `solver/{promote,gc,
orchestrate}` and independent of how the DSL is executed.

Today the *execution* layer is buildkit-specific and is the recurring source of
operational pain:

- the `buildkitd` daemon (spawn/hunt, the shared-worker hang from the dogfood build),
- the session gRPC tunnel + its h2 limits (had to patch the vendored fork),
- the gateway bridge + LLB config-attach protocol (a whole sub-project),
- a vendored ~140K `buildkit-client` fork + tonic + a `proto/` tree.

A **buildah backend** replaces all of that with `buildah build` + `buildah push`
(plain subprocesses, no daemon, no tunnel) using the operator's local
containers storage and runc.

What we'd lose: LLB-level control of the exact layer graph, and the
differential-conformance capability of mirroring dockerfile.v0 layer-for-layer.
basil's *identity* determinism is retained (the tagHash is computed by us, not
produced by buildah).

## 2. The core seam (proven)

On this host (buildah 1.39.3, runc+crun, rootless storage present):

```
buildah build --layers -t <final:alg> -f <containerfile> <ctx>
  # → tags <final:alg> into containers/storage (no daemon)

buildah push --tls-verify=false <final:alg> docker://<reg>/<deliverable>:<tagHash>
  # → pushes to the registry
```

`buildah bud` already parses the `FROM/COPY/RUN/ENV/CMD/ARG` instructions basil
emits. basil-only directives (`ON`, `TASK`, `PLATFORM`, `AS <deliverable>`) must
be **stripped to a Containerfile**; the deliverable's `AS` name becomes the
final `-t` tag (buildah's `AS` is a stage alias, not a registry target).

`ARG VERSION`/`--build-arg` is honored by buildah (`--build-arg VERSION=…`),
including "referenced ⇒ cache miss ⇒ real build" semantics, which is exactly
basil's promotion requirement.

## 3. What stays (untouched)

- `identity/` — closure, dockerignore, derived-data, program-hash (tagHash).
- `event/`, `dsl/` parser/lexer/ast.
- `solver/promote.rs`, `solver/registry.rs` (Level-1 skip, retag, gc).
- `solver/gc.rs`, `solver/orchestrate.rs`'s reaction/event/closure/promotion flow.
- the CLI surface (`build`, `closure`, `gc`).

## 4. What gets deleted

```
src/emitter/llb.rs            (LLB emitter: source/exec/file/merge graph)
src/emitter/mod.rs            (the LLB graph driver)
src/solver/bridge.rs          (frontend-less LLBBridge gateway)
src/solver/daemon.rs          (buildkitd spawn — replaced by subprocess)
src/gateway_proto.rs          (vendored proto module)
?third_party/                  (buildkit-client fork, if dropped)
build.rs / proto/              (the tonic codegen)
```

Cargo: drop `buildkit-client`, `tonic` + its transitive stack, `reqwest` can
stay (registry client already uses it).

## 5. The new abstraction

Introduce an **`execution` backend trait** so how the DSL runs is a choice:

```rust
trait Builder {
    /// Build the deliverable and leave it pushable/tagged.
    fn build(&self, target: &Target, args: &BuildArgs, ctx: &Ctx) -> Result<BuildRecipe>;
    /// Run a DO/TASK command in the target's base image.
    fn run_task(&self, image: &str, cmd: &Command, env: &[String]) -> Result<i32>;
}
```

- `buildah::Builder` — concrete impl (subprocess `buildah build`/`push`).
- `buildkit::Builder` — keep the existing impl behind the trait, or drop it.

The orchestrator computes the tagHash/closure (unchanged), then asks the
backend to materialize `repo:tagHash`, promotes, gc's.

## 6. The concrete mapping (design)

| basil concept (today, buildkit)               | buildah backend                        |
|----------------------------------------------|----------------------------------------|
| `emit_file` → Definition → gateway Solve      | strip DSL → `buildah build --layers`   |
| image attrib via gateway config              | buildah writes OCI config natively     |
| image export/exporter                        | tag into storage, `buildah push`       |
| Level 1 (tag exists → skip)                | unchanged (registry client)            |
| cache refs (`cache-<tagHash>`, dates)        | buildah `--layers` + `--cache-from`    |
| `DO`/`TASK` via throwaway solve              | `buildah run <ctr> [env] cmd`          |
| `-daemon--ephemeral`                         | none (subprocess)                      |
| `.dockerignore`                              | buildah honors `.containerignore`/`.dockerignore` |

## 7. Risks / honesty

1. **Layer granularity / Level-3 conformance**: buildah's `--layers` boundaries
   and `--from` reuse differ from buildkit's DAG. The differential-conformance
   test would need a buildah reference (not dockerfile.v0). basil's identity
   tagHash stays ours, so correctness of *identity* is unaffected.
2. **Multi-arch**: buildah `--platform` per build; tagHash is platform-scoped
   (match the design's PLATFORM handling).
3. **`ON`/`TASK`/`AS` stripping**: must be lossless and robust (the parser
   already isolates these).
4. **Rootless storage assumptions**: buildah needs containers/storage; on CI
   without it, the backend must surface a clear error (or keep `--ephemeral`
   fallback semantics for buildkit until dropped).
5. **Task `--from` another target**: buildah `run` on a built image; this is
   actually simpler/correct than the current throwaway-solve.

## 8. Migration path (staged, each independently shippable)

- **S1**: add `execution` trait + `buildah::Builder`; run `buildah build` on a
  stripped Containerfile; keep buildkit path working side-by-side (flag `--backend`).
- **S2**: wire `--layers`/`--cache-from` = current cache refs; `buildah push`
  for export; confirm promotion + Level-1 + gc against buildah-built tags.
- **S3**: `DO`/`TASK` via `buildah run`; drop the throwaway-solve task path.
- **S4**: flip default to buildah; delete `llb.rs`, `bridge.rs`, `gateway_proto`,
  `proto/`, `third_party` buildkit dependency (keep the trait for legacy).
- **S5**: restore differential conformance against `buildah bud` as the oracle.

## 9. Ask

Approve and I'll implement **S1 + S2** (buildah backend behind `--backend`, both
paths green, then promote it to default). Or note corrections to the scope.