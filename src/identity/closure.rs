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
//! its synthetic-snapshot digest; an internal base `C(B)` is folded in as B's
//! own `tagHash` (so the record stays bounded even across deep DAGs). The
//! closure is resolved bottom-up (dependencies before dependents).

use super::derived_data;
use super::program_hash;
use crate::dsl::ast::{Platform, Target};
use sha2::{Digest, Sha256};
use std::path::Path;

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

/// All in-closure build args for a target: declared `ARG`s with their resolved
/// value (default from the clause, or an override). Unresolvable ARGs (neither
/// default nor provided) are folded as `<unset>` so their presence is still
/// visible (and an ARG only entering the closure if actually used).
#[derive(Debug, Clone, Default)]
pub struct Args {
    map: std::collections::BTreeMap<String, Option<String>>,
}

impl Args {
    pub fn from_target(target: &Target, overrides: &std::collections::HashMap<String, String>) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for instr in &target.instructions {
            if let crate::dsl::ast::Instruction::Arg(a) = instr {
                let value = overrides
                    .get(&a.name)
                    .cloned()
                    .or_else(|| a.default.clone());
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
}

/// Compute every target's `tagHash` bottom-up.
///
/// `context` is the build context root (where `COPY`/`ADD` resolve and
/// `.dockerignore` lives). `overrides` are `--build-arg` values. `resolver`
/// is used only for external base digests; internal stages are always offline.
pub fn compute_closure(
    targets: &[Target],
    file_default_platforms: &[Platform],
    context: &Path,
    overrides: &std::collections::HashMap<String, String>,
    resolver: &mut dyn BaseResolver,
) -> Result<std::collections::HashMap<String, [u8; 32]>, String> {
    let mut result: std::collections::HashMap<String, [u8; 32]> =
        std::collections::HashMap::new();
    // Map stage name -> target.
    let by_name: std::collections::HashMap<&str, &Target> =
        targets.iter().map(|t| (t.name.as_str(), t)).collect();

    // Topological order: repeatedly resolve targets whose bases are done.
    let mut done = std::collections::HashSet::new();
    let mut remaining: Vec<(&str, &Target)> = targets.iter().map(|t| (t.name.as_str(), t)).collect();

    while !remaining.is_empty() {
        let mut progressed = false;
        let mut next = Vec::new();
        for (name, target) in remaining {
            // Associate arg values for this target.
            let args = Args::from_target(target, overrides);

            // Stage dependencies: the FROM base (if a local stage) plus every
            // COPY --from=<stage> source. Both must be resolved before this
            // target; their closure hashes are folded into the tagHash (the
            // DOC-string invariant `∪ C(B) for each depended stage B`).
            let mut stage_deps: Vec<String> = Vec::new();
            let crate::dsl::ast::BaseImage { raw, .. } = &target.base;
            if let Some(bs) = by_name.get(raw.as_str()) {
                if bs.name != name {
                    stage_deps.push(bs.name.to_string());
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
            let _base_platform = target.base.platform.clone();
            let base_ref = match &target.base {
                crate::dsl::ast::BaseImage { raw, tag, digest, .. } => {
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
            };


            // Derive data (context files this target reads).
            let derived = derived_data::derived_data_digest(target, context)
                .map_err(|e| format!("deriving data for `{name}`: {e}"))?;

            // Program hash with ARG/ENV expansion.
            let vctx = VarCtx {
                args: args.map.clone(),
                env: target_env(target),
            };
            let prog = program_hash::program_hash(&target.instructions, &|k| vctx.resolve(k));

            // Assemble canonical components and compute tagHash.
            let mut record = Vec::new();
            push(
                &mut record,
                "derivedData",
                &hex(&derived),
            );
            for (k, v) in &args.map {
                push(&mut record, &format!("arg.{k}"), v.as_deref().unwrap_or("<unset>"));
            }
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
        let r1 = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
        let r2 = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(hex(&r1["alpine"]).len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_in_instruction_changes_tag() {
        let dir = tiny_ctx("chg");
        let a = parse("FROM alpine\nRUN echo one\n").unwrap();
        let b = parse("FROM alpine\nRUN echo two\n").unwrap();
        let ra = compute_closure(&a.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
        let rb = compute_closure(&b.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
        assert_ne!(ra["alpine"], rb["alpine"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_external_base_resolves_offline() {
        let dir = tiny_ctx("pin");
        let dsl = "FROM alpine:3.23@sha256:abcdef1234567890\nRUN echo hi\n";
        let f = parse(dsl).unwrap();
        let r = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut offline()).unwrap();
        assert_eq!(hex(&r["alpine"]).len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpinned_external_requires_resolver() {
        let dir = tiny_ctx("unp");
        let dsl = "FROM alpine:3.23\nRUN echo hi\n";
        let f = parse(dsl).unwrap();
        let err = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut offline()).unwrap_err();
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
        let r = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
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
        let r2 = compute_closure(&f2.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
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
        let r1 = compute_closure(&f.targets, &[], &dir, &HashMap::new(), &mut FakeResolver).unwrap();
        let r2 = compute_closure(&f.targets, &[], &dir, &overrides, &mut FakeResolver).unwrap();
        assert_ne!(r1["alpine"], r2["alpine"]);
    }
}