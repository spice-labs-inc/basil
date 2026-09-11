//! Closure `C(T)` and `tagHash`.
//!
//! Per the design:
//!
//! ```text
//! C(T) = derivedData(T)
//!        ∪ C(B)      for each depended stage B      // FROM <stage>, COPY --from=<stage>
//!        ∪ { resolvedDigest(X) }  for external bases // FROM <image>
//!        ∪ args(T)                                   // resolved ARG / --build-arg
//!        ∪ platforms(T)                              // declared build platforms
//!        ∪ programHash(T)                            // ordered, arg-expanded instructions
//!        ∪ { basil-version }
//! ```
//!
//! `tagHash = H(C(T))`, a SHA-256 over a canonical, deterministically sorted,
//! length-prefixed record of scalar components. `derivedData` is folded in as
//! its git-tree digest; an internal base `C(B)` is folded in as B's
//! own `tagHash` (so the record stays bounded even across deep DAGs). The
//! closure is resolved bottom-up (dependencies before dependents).

use super::derived_data;
use super::program_hash;
use crate::dsl::ast::{Platform, Target};
use sha2::{Digest, Sha256};
use std::path::Path;

#[cfg(test)]
use std::collections::BTreeMap;

/// The basil build-time version folded into every closure.
pub const BASIL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolve a third-party base image reference to its manifest digest.
///
/// Pinned bases (`alpine@sha256:...`) resolve offline from the DSL text. An
/// unpinned base (`alpine:3.23`) requires a registry metadata read; the `build`
/// command supplies a resolver backed by the registry client. The offline
/// path (used by `closure` and tests) rejects unpinned bases so a closure
/// without network is never falsely computed.
pub trait BaseResolver {
    fn resolve(&mut self, raw: &str, tag: Option<&str>, digest: Option<&str>) -> Result<String, String>;
}

/// Offline resolver: only pinned bases can be resolved without a registry.
pub struct OfflineResolver;

impl BaseResolver for OfflineResolver {
    fn resolve(&mut self, raw: &str, _tag: Option<&str>, digest: Option<&str>) -> Result<String, String> {
        // `scratch` is the empty image — a fixed sentinel, never a registry pull.
        if raw == "scratch" {
            return Ok("scratch-empty".to_string());
        }
        digest.map(|d| d.to_string()).ok_or_else(|| {
            "unpinned base image requires registry resolution (not available offline)".to_string()
        })
    }
}

/// Per-target build-arg resolver: the `ARG`s a target declares, with their
/// resolved value (`--build-arg` override, else the clause default, else the
/// global-scope default of the same name). Bare redeclarations inherit the
/// global default, mirroring Dockerfile scope rules.
///
/// This map is the *resolver* for `$VAR` substitution only — it never folds
/// itself into the identity record. An ARG only reaches identity through the
/// substituted form at a `${VAR}` use site in an identity-affecting
/// instruction (RUN/COPY/ENV/...). A global arg the target never redeclares is
/// invisible here, exactly like buildah's stage scope.
#[derive(Debug, Clone, Default)]
pub struct Args {
    map: std::collections::BTreeMap<String, Option<String>>,
}

impl Args {
    pub fn from_target(
        target: &Target,
        global_args: &[crate::dsl::ast::Arg],
        overrides: &std::collections::HashMap<String, String>,
    ) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for instr in &target.instructions {
            if let crate::dsl::ast::Instruction::Arg(a) = instr {
                let global_default = global_args
                    .iter()
                    .find(|g| g.name == a.name)
                    .and_then(|g| g.default.clone());
                let value = overrides
                    .get(&a.name)
                    .cloned()
                    .or_else(|| a.default.clone())
                    .or(global_default);
                map.insert(a.name.clone(), value);
            }
        }
        Args { map }
    }
}

/// Per-target state for the reader used during `programHash` (ARG/ENV values).
struct VarCtx {
    args: std::collections::BTreeMap<String, Option<String>>,
    env: std::collections::BTreeMap<String, String>,
}

impl VarCtx {
    fn resolve(&self, name: &str) -> Option<String> {
        if let Some(v) = self.env.get(name) {
            return Some(v.clone());
        }
        if let Some(Some(v)) = self.args.get(name) {
            return Some(v.clone());
        }
        None
    }
}

/// A resolved base for a target.
#[derive(Debug, Clone)]
enum BaseRef {
    /// External image manifest digest (pinned, or resolved from registry).
    External(String),
    /// Another target's closure hash.
    Stage(String),
    /// A module deliverable's closure hash + its import key.
    Module([u8; 32], String),
}

/// Compute every target's `tagHash` bottom-up.
///
/// `context` is the build context root (where `COPY`/`ADD` resolve and
/// `.dockerignore` lives). `overrides` are `--build-arg` values. `resolver`
/// is used only for external base digests; internal stages are always offline.
pub fn compute_closure(
    targets: &[Target],
    file_default_platforms: &[Platform],
    global_args: &[crate::dsl::ast::Arg],
    context: &Path,
    build_file: &Path,
    overrides: &std::collections::HashMap<String, String>,
    resolver: &mut dyn BaseResolver,
    modules: &std::collections::BTreeMap<String, [u8; 32]>,
) -> Result<std::collections::HashMap<String, [u8; 32]>, String> {
    let mut result: std::collections::HashMap<String, [u8; 32]> =
        std::collections::HashMap::new();
    // Map stage name -> target.
    let by_name: std::collections::HashMap<&str, &Target> =
        targets.iter().map(|t| (t.name.as_str(), t)).collect();

    // Snapshot the git state under the context once: clean tracked subtrees
    // borrow git's precomputed hashes, so every target's derived-data digest
    // shares one probe round.
    let repo = derived_data::discover_git(context);

    // Topological order: repeatedly resolve targets whose bases are done.
    let mut done = std::collections::HashSet::new();
    let mut remaining: Vec<(&str, &Target)> = targets.iter().map(|t| (t.name.as_str(), t)).collect();

    while !remaining.is_empty() {
        let mut progressed = false;
        let mut next = Vec::new();
        for (name, target) in remaining {
            // Associate arg values for this target. Args are the resolver for
            // `$VAR` substitution; they never fold into the identity record
            // themselves (identity reflects the substituted form only).
            let args = Args::from_target(target, global_args, overrides);

            // Stage dependencies: the FROM base (if a local stage) plus every
            // COPY --from=<stage> source. Both must be resolved before this
            // target; their closure hashes are folded into the tagHash (the
            // DOC-string invariant `∪ C(B) for each depended stage B`).
            let mut stage_deps: Vec<String> = Vec::new();
            if let crate::dsl::ast::Base::Image(crate::dsl::ast::BaseImage { raw, .. }) =
                &target.base
            {
                if let Some(bs) = by_name.get(raw.as_str()) {
                    if bs.name != name {
                        stage_deps.push(bs.name.to_string());
                    }
                }
            }
            for instr in &target.instructions {
                if let crate::dsl::ast::Instruction::Copy(c) = instr {
                    if let Some(from) = &c.from {
                        // `COPY --from=<stage>`: fold that stage's closure.
                        // Image refs (contain : or /) are not local stages.
                        if by_name.contains_key(from.as_str()) {
                            stage_deps.push(from.clone());
                        }
                    }
                }
            }
            stage_deps.sort();
            stage_deps.dedup();

            // If any stage dep isn't resolved yet, defer this target. (stage_deps
            // only contains real stages, so an unresolved dep is an upstream
            // target still in the queue; genuine cycles surface via the
            // `!progressed` check below.)
            if stage_deps.iter().any(|d| !done.contains(d.as_str())) {
                next.push((name, target));
                continue;
            }

            // Resolve the base reference.
            let base_platform = match &target.base {
                crate::dsl::ast::Base::Image(b) => b.platform.clone(),
                crate::dsl::ast::Base::Module(_) => None,
            };
            let _base_platform = base_platform;
            let base_ref = match &target.base {
                crate::dsl::ast::Base::Image(crate::dsl::ast::BaseImage { raw, tag, digest, .. }) => {
                    if let Some(bs_name) = by_name.get(raw.as_str()).map(|t| t.name.as_str()) {
                        if bs_name != name {
                            Some(BaseRef::Stage(hex(&result[bs_name])))
                        } else {
                            Some(BaseRef::External(
                                resolver
                                    .resolve(raw, tag.as_deref(), digest.as_deref())
                                    .map_err(|e| format!("`{raw}`: {e}"))?,
                            ))
                        }
                    } else {
                        Some(BaseRef::External(
                            resolver
                                .resolve(raw, tag.as_deref(), digest.as_deref())
                                .map_err(|e| format!("`{raw}`: {e}"))?,
                        ))
                    }
                }
                crate::dsl::ast::Base::Module(m) => {
                    let key = crate::identity::modules::module_key(
                        build_file.parent().unwrap_or_else(|| Path::new(".")),
                        m,
                    )
                    .map_err(|e| format!("module base: {e}"))?;
                    let hash = modules.get(&key).ok_or_else(|| {
                        format!(
                            "module `{}` (line {}): no computed digest — the module was not \
                             resolved before the importer",
                            m.path, m.line
                        )
                    })?;
                    Some(BaseRef::Module(*hash, key.clone()))
                }
            };


            // Derive data (context files this target reads). The build file that
            // produced `targets` is excluded: its effect is already captured by
            // programHash, so including it would make identity self-referential.
            let (derived, bytes) = derived_data::derived_data_digest_and_bytes(target, context, build_file, &repo)
                .map_err(|e| format!("deriving data for `{name}`: {e}"))?;
            // Surface large data the operator may want dockerignored: the byte
            // count covers only what basil actually reads while hashing — a
            // clean tracked blob covered by a borrowed git tree hash is never
            // read and never warns.
            const LARGE_CONTEXT_BYTES: u64 = 100 * 1024 * 1024; // 100 MiB
            if bytes > LARGE_CONTEXT_BYTES {
                tracing::warn!(
                    "identity `{name}`: derived-data hashes {:.0} MiB of context not covered by a git tree (100 MiB+) — check .dockerignore for large blobs",
                    bytes as f64 / (1024.0 * 1024.0)
                );
            }

            // Program hash with ARG/ENV expansion.
            let vctx = VarCtx {
                args: args.map.clone(),
                env: target_env(target),
            };
            let prog = program_hash::program_hash(&target.instructions, &|k| vctx.resolve(k));

            // Assemble canonical components and compute tagHash. ARG is NOT
            // folded here: identity reflects the substituted form at `${VAR}`
            // use sites inside identity-affecting instructions, never the ARG
            // declaration itself.
            let mut record = Vec::new();
            push(
                &mut record,
                "derivedData",
                &hex(&derived),
            );
            let platforms = target_platform_list(target, file_default_platforms);
            for p in platforms {
                push(&mut record, "platform", &p.to_string());
            }
            push(&mut record, "program", &hex(&prog));
            push(&mut record, "basil", BASIL_VERSION);
            if let Some(BaseRef::External(digest)) = &base_ref {
                push(&mut record, "base.external", digest);
            }
            if let Some(BaseRef::Stage(hash)) = &base_ref {
                push(&mut record, &format!("base.stage.{}", stage_clean(name)), hash);
            }
            if let Some(BaseRef::Module(hash, _key)) = &base_ref {
                push(&mut record, &format!("base.module.{}", stage_clean(name)), &hex(hash));
            }
            if let Some(p) = &_base_platform {
                push(&mut record, "base.platform", p);
            }
            // Fold every stage dependency's closure hash (incl. COPY --from
            // sources): changes to a depended stage must change the deliverable.
            for dep in &stage_deps {
                if let Some(h) = result.get(dep) {
                    push(&mut record, &format!("dep.stage.{}", stage_clean(dep)), &hex(h));
                }
            }

            let tag = sha256(record_sorted(record));
            result.insert(name.to_string(), tag);
            done.insert(name.to_string());
            progressed = true;
        }
        remaining = next;
        if !progressed && !remaining.is_empty() {
            // A cycle among stage bases or an unresolvable forward reference.
            return Err("stage base cycle or forward reference (stages must resolve bottom-up)".to_string());
        }
    }
    Ok(result)
}

/// `ENV` pairs accumulate for later RUN/env resolution in `programHash`.
fn target_env(target: &Target) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    for instr in &target.instructions {
        if let crate::dsl::ast::Instruction::Env(e) = instr {
            for (k, v) in &e.pairs {
                env.insert(k.clone(), v.clone());
            }
        }
    }
    env
}

fn target_platform_list(target: &Target, file_default: &[Platform]) -> Vec<Platform> {
    // Per-target platforms override the file-wide default (inherited). A target
    // with no declaration and no file default contributes none (the builder's
    // native platform is a Phase 5 concern).
    target
        .platforms
        .clone()
        .unwrap_or_else(|| file_default.to_vec())
}

fn stage_clean(name: &str) -> String {
    // Sanitize a stage name for the record key (deliverable names contain '/')
    // while keeping it unambiguous.
    name.replace('/', "_")
}

/// A sorted, length-prefixed record with no collisions.
fn record_sorted(mut entries: Vec<(String, String)>) -> Vec<u8> {
    entries.sort();
    let mut out = Vec::new();
    for (k, v) in entries {
        out.extend_from_slice(&(k.len() as u32).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

fn push(out: &mut Vec<(String, String)>, key: &str, value: &str) {
    out.push((key.to_string(), value.to_string()));
}

fn sha256(bytes: Vec<u8>) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(&bytes);
    h.finalize().into()
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse;
    use std::collections::HashMap;

    fn offline() -> OfflineResolver {
        OfflineResolver
    }

    /// Resolver that fakes a manifest digest for any unpinned external base, so
    /// closure tests stay offline while exercising the external path.
    struct FakeResolver;
    impl BaseResolver for FakeResolver {
        fn resolve(&mut self, raw: &str, tag: Option<&str>, digest: Option<&str>) -> Result<String, String> {
            Ok(digest
                .map(|d| d.to_string())
                .unwrap_or_else(|| format!("fake:{raw}:{}", tag.unwrap_or(""))))
        }
    }

    /// A small isolated context dir (so `COPY .` is bounded and deterministic).
    fn tiny_ctx(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("basil-ctx-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn deterministic_tag_for_same_target() {
        let dsl = "FROM alpine
COPY marker.txt /x
RUN go build\n";
        let f = parse(dsl).unwrap();
        let dir = tiny_ctx("det");
        let r1 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        let r2 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(hex(&r1["alpine"]).len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_in_instruction_changes_tag() {
        let dir = tiny_ctx("chg");
        let a = parse("FROM alpine\nRUN echo one\n").unwrap();
        let b = parse("FROM alpine\nRUN echo two\n").unwrap();
        let ra = compute_closure(&a.targets, &[], &a.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        let rb = compute_closure(&b.targets, &[], &b.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_ne!(ra["alpine"], rb["alpine"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_external_base_resolves_offline() {
        let dir = tiny_ctx("pin");
        let dsl = "FROM alpine:3.23@sha256:abcdef1234567890\nRUN echo hi\n";
        let f = parse(dsl).unwrap();
        let r = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut offline(), &BTreeMap::new()).unwrap();
        assert_eq!(hex(&r["alpine"]).len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpinned_external_requires_resolver() {
        let dir = tiny_ctx("unp");
        let dsl = "FROM alpine:3.23\nRUN echo hi\n";
        let f = parse(dsl).unwrap();
        let err = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut offline(), &BTreeMap::new()).unwrap_err();
        assert!(err.contains("pinned"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_dependency_folds_base_hash() {
        let dir = tiny_ctx("stage");
        let dsl = "\
FROM golang AS build
RUN echo build

FROM scratch AS ghcr.io/mycompany/app
COPY --from=build /a /a
";
        let f = parse(dsl).unwrap();
        let r = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert!(r.contains_key("build"));
        assert!(r.contains_key("ghcr.io/mycompany/app"));
        assert_ne!(r["build"], r["ghcr.io/mycompany/app"]);

        // A change to the build stage (its COPY source content) must change the
        // deliverable that depends on it via COPY --from — the identity must
        // fold the depended stage's closure, not just the FROM base.
        std::fs::write(dir.join("dep"), "v2").unwrap();
        let dsl2 = "\
FROM golang AS build
COPY dep /dep

FROM scratch AS ghcr.io/mycompany/app
COPY --from=build /a /a
";
        let f2 = parse(dsl2).unwrap();
        let r2 = compute_closure(&f2.targets, &[], &f2.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_ne!(
            r["ghcr.io/mycompany/app"],
            r2["ghcr.io/mycompany/app"],
            "COPY --from stage input change must change the deliverable's tagHash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arg_override_changes_tag_only_if_used() {
        let dir = tiny_ctx("arg");
        let dsl = "FROM alpine\nARG VERSION\nRUN echo $VERSION\n";
        let f = parse(dsl).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("VERSION".to_string(), "1.0".to_string());
        let r1 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        let r2 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &overrides, &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_ne!(r1["alpine"], r2["alpine"]);
    }

    #[test]
    fn unreferenced_global_arg_does_not_change_tag() {
        // The directive: ARG itself never enters identity — only the substituted
        // form at a `${VAR}` use site does. A file-level `ARG VERSION=""` that
        // no instruction references must leave the tagHash identical to the
        // same file without it.
        let dir = tiny_ctx("glob-unused");
        let without = parse("FROM alpine\nRUN echo hi\n").unwrap();
        let with = parse("ARG VERSION=\"\"\nFROM alpine\nRUN echo hi\n").unwrap();
        let r_without = compute_closure(&without.targets, &[], &without.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        let r_with = compute_closure(&with.targets, &[], &with.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_eq!(r_without["alpine"], r_with["alpine"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_arg_value_enters_tag_only_when_referenced() {
        // A stage that redeclares the global arg (bare `ARG VERSION`, Dockerfile
        // global-scope rules) and references `$VERSION` must see the substituted
        // value; the default and the override both change the tag.
        let dir = tiny_ctx("glob-used");
        let dsl = "ARG VERSION=\"0.0.0\"\nFROM alpine\nARG VERSION\nRUN echo $VERSION\n";
        let f = parse(dsl).unwrap();

        // Default (no override) vs override: different substituted forms.
        let mut overrides = HashMap::new();
        overrides.insert("VERSION".to_string(), "1.5.8".to_string());
        let r_default = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        let r_override = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &overrides, &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_ne!(r_default["alpine"], r_override["alpine"]);

        // The global default flows through the bare stage redeclaration: the
        // substituted form contains `0.0.0` (the default), not empty.
        let dsl2 = "ARG VERSION=\"9.9.9\"\nFROM alpine\nARG VERSION\nRUN echo $VERSION\n";
        let f2 = parse(dsl2).unwrap();
        let r2 = compute_closure(&f2.targets, &[], &f2.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_ne!(r_default["alpine"], r2["alpine"], "global default must reach the substituted form");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_arg_invisible_in_stage_without_redeclaration() {
        // Dockerfile scope (option A): a global ARG not redeclared in the stage
        // is invisible there — `$FOO` expands empty, exactly like buildah. The
        // resolver must mirror this so identity equals the built image.
        let dir = tiny_ctx("glob-invis");
        let dsl = "ARG FOO=bar\nFROM alpine\nRUN echo [$FOO]\n";
        let f = parse(dsl).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("FOO".to_string(), "qux".to_string());
        let r1 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &overrides, &mut FakeResolver, &BTreeMap::new()).unwrap();
        // An override for an arg the stage never redeclares changes nothing.
        let r2 = compute_closure(&f.targets, &[], &f.global_args, &dir, std::path::Path::new("__buildfile__"), &HashMap::new(), &mut FakeResolver, &BTreeMap::new()).unwrap();
        assert_eq!(r1["alpine"], r2["alpine"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}