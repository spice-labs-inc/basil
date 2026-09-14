//! Orchestrate the `basil build` flow: parse → match event → identity → buildah build.

use crate::dsl::parser;
use crate::event;
use crate::identity::closure::{BaseResolver, compute_closure};
use serde_json::Value as Json;
use std::collections::HashMap;

/// Resolver backed by a pre-resolved map of external base -> manifest digest.
/// Unpinned external bases are resolved against their registry in an async
/// pre-pass (see [`resolve_unpinned_bases`]), then looked up here. Pinned and
/// `scratch` bases resolve textually.
struct RegistryResolver {
    resolved: HashMap<String, String>,
}
impl BaseResolver for RegistryResolver {
    fn resolve(&mut self, raw: &str, tag: Option<&str>, digest: Option<&str>) -> Result<String, String> {
        if raw == "scratch" {
            return Ok("scratch-empty".to_string());
        }
        if let Some(d) = digest {
            return Ok(d.to_string());
        }
        let key = match tag {
            Some(t) => format!("{raw}:{t}"),
            None => raw.to_string(),
        };
        self.resolved
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("unpinned base `{raw}`: could not resolve via registry"))
    }
}

/// Build a resolver by resolving every unpinned external base found in
/// `targets` against its registry. This turns `<host>/<repo>:<tag>` into its
/// pinned manifest digest so identity is computed over a real, current digest.
/// `scratch` and already-pinned bases are left untouched. A base that cannot
/// be resolved is a hard error — identity must not proceed on an unresolved
/// base (it would hash the wrong input).
async fn resolve_unpinned_bases(
    targets: &[crate::dsl::ast::Target],
) -> Result<HashMap<String, String>, String> {
    let mut resolved = HashMap::new();
    // Resolve each distinct base ref once per run; many stages reuse the same
    // base (e.g. ghcr.io/.../build-base), so dedupe by host/repo:tag.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // A base naming a *different* local target is an internal stage, not an
    // image — resolve it as the stage's closure, never as an external pull. The
    // self-name rule (`FROM alpine` naming a stage `alpine`) stays external.
    let by_name: std::collections::HashSet<&str> =
        targets.iter().map(|t| t.name.as_str()).collect();
    for t in targets {
        let crate::dsl::ast::Base::Image(b) = &t.base else {
            continue; // module bases resolve to their own digests, never a pull
        };
        if b.raw == "scratch" || b.digest.is_some() {
            continue;
        }
        if by_name.contains(b.raw.as_str()) && b.raw != t.name {
            // Internal stage dependency — its identity is folded as C(stage),
            // not resolved from a registry.
            continue;
        }
        // Normalize the base ref via parse_ref: bare names (`rust`, `alpine`)
        // map to Docker Hub `library/<name>`; qualified names keep host/repo.
        // Default tag is latest.
        let tag = b.tag.clone().unwrap_or_else(|| "latest".to_string());
        let ir = match crate::solver::registry::parse_ref(&b.raw, &tag) {
            Ok(ir) => ir,
            Err(e) => return Err(format!("unpinned base `{raw}`: {e}", raw = b.raw)),
        };
        // Dedupe distinct refs so a shared base (e.g. build-base) resolves once.
        if !seen.insert(ir.dedup_key(&tag)) {
            continue;
        }
        let raw = &b.raw;
        match ir.resolve_digest().await {
            Ok(digest) => {
                tracing::info!("resolved base `{raw}` -> {digest}");
                // compute_closure passes tag=None when the DSL base has no
                // explicit tag, so it looks up by `raw` alone; store that key
                // (and the raw:tag form for the explicit-tag path).
                resolved.insert(raw.clone(), digest.clone());
                resolved.insert(format!("{raw}:{tag}"), digest);
            }
            Err(e) => {
                // Fail hard: an unpinned base we couldn't resolve means we can't
                // compute an honest identity for it.
                return Err(format!(
                    "could not resolve unpinned base `{raw}` ({}): {e}",
                    ir.dedup_key(&tag)
                ));
            }
        }
    }
    Ok(resolved)
}

/// Inputs to a build.
/// Everything a build entry point needs about the run.
#[derive(Clone)]
pub struct BuildInput<'a> {
    pub file: &'a str,
    pub context: &'a std::path::Path,
    pub event_json: Option<&'a str>,
    pub target_filter: Option<&'a String>,
    /// Bypass the idempotent check: always rebuild and re-push.
    pub force: bool,
    /// Where intermediate-stage cache images are pushed (`basil-cache-*` refs);
    /// `None` defaults to each deliverable's own repo.
    pub cache_to: Option<&'a str>,
}
async fn promote_on_tag(
    matched: &[&crate::dsl::ast::Reaction],
    payload: &Json,
    event_ref: &str,
    closures: &HashMap<String, [u8; 32]>,
    targets: &[crate::dsl::ast::Target],
) -> anyhow::Result<()> {
    use crate::dsl::ast::EventToken;

    // Promote each Tag/Release reaction that carries a version. A bare push/PR
    // reaction has no version -> nothing to promote.
    let mut promoted: Vec<(String, String)> = Vec::new(); // (name, version)
    for r in matched.iter().copied() {
        let token = &r.event.token;
        if !matches!(token, EventToken::Tag | EventToken::Release) {
            continue;
        }
        let Some((qual_target, version)) =
            crate::solver::promote::event_version(token, payload, event_ref)
        else {
            continue;
        };
        tracing::info!("promotion: {token:?} -> version v{version}");
        for b in &r.build {
            if let Some(qt) = &qual_target {
                // `target-vX.Y.Z` promotes only target `q`.
                if b.as_str() == qt.as_str() || b.split('/').last() == Some(qt.as_str()) {
                    promoted.push((b.clone(), version.clone()));
                }
            } else {
                promoted.push((b.clone(), version.clone()));
            }
        }
    }
    promoted.sort();
    promoted.dedup();

    for (name, version) in &promoted {
        // Resolve the build entry to its push ref (register, else name) so the
        // version tag is set where the content is published.
        let push_ref = targets
            .iter()
            .find(|t| &t.name == name || t.register.as_ref() == Some(name))
            .map(|t| t.push_ref().to_string())
            .unwrap_or_else(|| name.clone());
        let tag_hash = digest_of(name, closures)?;
        let ir = crate::solver::registry::parse_ref(&push_ref, "")
            .map_err(|e| anyhow::anyhow!("promote `{push_ref}`: {e}"))?;
        let client = ir.client();
        // Promote FROM the namespaced content tag (`basil-<hash>`) to the
        // immutable `v<version>` tag.
        let src = format!("{}:{}", ir.repo, crate::solver::registry::content_tag(&tag_hash));
        match client.retag(&ir.repo, &crate::solver::registry::content_tag(&tag_hash), &format!("v{version}")).await {
            Ok(d) => tracing::info!("promote `{push_ref}:{src}` -> v{version} (digest {})", &d[..d.len().min(24)]),
            Err(e) => {
                anyhow::bail!("promotion immutability/retag failure for `{push_ref}:v{version}`: {e}")
            }
        }
    }
    Ok(())
}

/// Build one or more deliverable targets directly (no event): the `-t`
/// target, or every qualified deliverable target when none is given. Computes
/// Everything a build/promote entry point needs after parsing + identity:
/// the file AST, the selected deliverables, the in-scope target subset, module
/// discovery/closure tables, and the importer's closures.
pub struct SolveContext {
    pub parsed: crate::dsl::ast::File,
    pub deliverables: Vec<(String, String)>,
    pub names: Vec<String>,
    pub subset: Vec<crate::dsl::ast::Target>,
    pub build_file: std::path::PathBuf,
    pub file_dir: std::path::PathBuf,
    pub index: crate::identity::modules::ModuleIndex,
    pub module_digests: crate::identity::modules::ModuleClosures,
    pub module_refs: std::collections::BTreeMap<String, String>,
    pub closures: std::collections::HashMap<String, [u8; 32]>,
}

/// Parse the file, select deliverables (`-t` filter, else every qualified
/// deliverable), resolve module deps and unpinned bases, and compute closures
/// — the shared preamble of `build_targets` and `promote_version`.
async fn solve(
    file: &str,
    context: &std::path::Path,
    target_filter: Option<&str>,
) -> anyhow::Result<SolveContext> {
    let src = crate::fsctx::fs_err(std::fs::read_to_string(file), file, "read")?;
    println!("basil: parsed {file}");
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;

    // A target is a deliverable if it names a registry ref directly (qualified
    // name) or declares a `REGISTER` full ref. Each is tracked as
    // (stage name, push ref) — the stage is addressed by `name`; the image is
    // pushed/looked up by `push_ref` (register, else name).
    let deliverable_entries: Vec<(String, String)> = parsed
        .targets
        .iter()
        .filter(|t| is_deliverable(t))
        .map(|t| (t.name.clone(), t.push_ref().to_string()))
        .collect();
    // With -t <key>, find the target by its AS name OR its REGISTER ref, so both
    // `-t fennel` and `-t 127.0.0.1:5001/fennel` address the same deliverable.
    let deliverables: Vec<(String, String)> = match target_filter {
        Some(key) => {
            let t = parsed
                .targets
                .iter()
                .find(|x| x.name.as_str() == key || x.register.as_deref() == Some(key))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "target `{key}` not found in {file} (known: {})",
                        parsed
                            .targets
                            .iter()
                            .map(|x| format!("{}({})", x.name, x.push_ref()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            if !is_deliverable(t) {
                return Err(anyhow::anyhow!(
                    "target `{key}` is not a deliverable: give it a qualified name or a `REGISTER` ref"
                ));
            }
            vec![(t.name.clone(), t.push_ref().to_string())]
        }
        None => deliverable_entries,
    };
    if deliverables.is_empty() {
        return Err(anyhow::anyhow!(
            "no deliverable target to build: pass -t <target> or name a deliverable as a qualified image reference"
        ));
    }
    let names: Vec<String> = deliverables.iter().map(|(n, _)| n.clone()).collect();

    // Only compute identity for the stages the build actually needs. With
    // -t <target>, that's the target + its transitive FROM / COPY --from stage
    // deps — unrelated deliverables aren't built or hashed.
    let subset = dependency_subset(&parsed.targets, &names);
    println!("basil: {} target(s) in scope (of {} total)", subset.len(), parsed.targets.len());

    // The build file's own path/context; module paths resolve against its
    // directory (D4: file-relative).
    let build_file = std::path::absolute(file).unwrap_or_else(|_| file.into());
    let build_file_owned = build_file.clone();

    // Discover module dependencies (recursive, cycle-checked, pure file walk).
    let index = crate::identity::modules::discover_module_index(&subset, build_file_owned.parent().unwrap_or(std::path::Path::new(".")))
        .map_err(anyhow::Error::msg)?;
    if !index.order.is_empty() {
        println!("basil: {} module(s) discovered", index.order.len());
    }

    // Resolve unpinned external bases (network: auth token + manifest GETs) —
    // for the importer's in-scope set AND every module file's targets.
    println!("basil: resolving base(s) from registries...");
    let t0 = std::time::Instant::now();
    let mut resolved = resolve_unpinned_bases(&subset)
        .await
        .map_err(anyhow::Error::msg)?;
    for key in &index.order {
        let e = &index.entries[key];
        let module_resolved = resolve_unpinned_bases(&e.parsed.targets)
            .await
            .map_err(anyhow::Error::msg)?;
        resolved.extend(module_resolved);
    }
    println!("basil: resolved {} base(s) in {:.1}s", resolved.len(), t0.elapsed().as_secs_f64());
    let t0 = std::time::Instant::now();
    println!("basil: computing identity ({} target(s))...", subset.len());
    let mut resolver = RegistryResolver { resolved };
    // Module deliverable digests first (their own bases resolved above), so
    // the importer closure folds `base.module.*` from them.
    let module_digests =
        crate::identity::modules::compute_module_digests(&index, &mut resolver)
            .map_err(anyhow::Error::msg)?;
    let closures = compute_closure(
        &subset,
        &parsed.default_platforms,
        &parsed.global_args,
        context,
        &build_file_owned,
        &HashMap::new(),
        &mut resolver,
        &module_digests.deliverable,
    )
    .map_err(anyhow::Error::msg)?;
    // Module key -> `FROM <push-ref>:<tagHash>` for the importer's Containerfile.
    let mut module_refs: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for key in &index.order {
        let e = &index.entries[key];
        let tag = crate::identity::closure::hex(&module_digests.deliverable[key]);
        module_refs.insert(
            key.clone(),
            format!("{}:{}", e.push_ref, crate::solver::registry::content_tag(&tag)),
        );
    }
    println!("basil: identity computed in {:.1}s", t0.elapsed().as_secs_f64());
    for name in &names {
        println!(
            "basil: identity {name} -> {}",
            digest_of(name, &closures)?
        );
    }

    Ok(SolveContext {
        parsed,
        deliverables,
        names,
        subset,
        build_file: build_file_owned.clone(),
        file_dir: build_file_owned
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf(),
        index,
        module_digests,
        module_refs,
        closures,
    })
}

/// Build one or more deliverable targets directly (no event). This path must
/// never execute TASK clauses: tasks are event-triggered (DO reactions in
/// `handle_event`), so a plain build of an image has no side-effects. Here we
/// only compute the closure and run `build_deliverable`.
pub async fn build_targets(inp: BuildInput<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let solved = solve(inp.file, inp.context, inp.target_filter.map(String::as_str)).await?;
    let SolveContext {
        parsed,
        deliverables,
        names,
        subset,
        index,
        module_digests,
        module_refs,
        closures,
        file_dir,
        ..
    } = solved;

    let mut out: Vec<(String, String)> = Vec::new();

    // Every non-deliverable stage in scope gets a cache image
    // (`<repo>:basil-cache-<stageHash>`). The cache repo is `--cache-to`, else
    // `BASIL_CACHE_REF`, else the first deliverable's repo (D1).
    let cache_to = effective_cache_to(inp.cache_to, &deliverables);
    let stage_cache = stage_cache_refs(&subset, &deliverables, &closures, &cache_to);

    // If every content tag already exists in its registry, the exact images
    // are already published — don't rebuild (parity with handle_event's
    // idempotent check). The probes confirmed registration; the noop path
    // still states each registered image and its content tag. `--force`
    // bypasses the check entirely: the probe is skipped and every deliverable
    // rebuilds and re-pushes.
    if !inp.force {
        let published = idempotent_published(&deliverables, &closures).await?;
        if !published.is_empty() {
            println!("basil: content tags already published -- no build (idempotent check)");
            // The modules are part of the declared artifact set too: reconcile
            // them by existence even on the noop path, so a deleted module is
            // re-registered (same tagHash — the importer image stays valid).
            ensure_modules_built(&index, &module_refs, &module_digests, inp.cache_to).await?;
            // Stages are reconciled by existence too: a missing cache image is
            // rebuilt and pushed even on the noop path (D2).
            let cache_g = crate::solver::buildah::Graph::new(&subset, &parsed.global_args, &file_dir, &module_refs, &stage_cache);
            ensure_stages_cached(cache_g, inp.context, &deliverables).await?;
            let g = crate::solver::buildah::Graph::new(&subset, &parsed.global_args, &file_dir, &module_refs, &stage_cache);
            for (name, push_ref, tag) in &published {
                println!("basil: image {push_ref}:{tag} (registered)");
                // OUTPUT still runs: pull the published image and extract.
                crate::solver::buildah::extract_outputs_from_image(
                    &format!("{push_ref}:{tag}"),
                    inp.context,
                    g,
                    name,
                )
                .await?;
                out.push((name.clone(), tag.clone()));
            }
            println!("basil: done ({})", names.join(", "));
            return Ok(out);
        }
    }

    // Build path: every module dependency must exist as its content-tagged
    // image before the importer renders FROM it — probe; build + push the
    // module when missing (recursive; the module's own modules were already
    // ensured when its digest was computed? No — ensure here, bottom-up).
    ensure_modules_built(&index, &module_refs, &module_digests, inp.cache_to).await?;
    // Every intermediate stage must exist as its cache image before the
    // deliverable's Containerfile references it (reconcile by existence).
    let cache_g = crate::solver::buildah::Graph::new(&subset, &parsed.global_args, &file_dir, &module_refs, &stage_cache);
    ensure_stages_cached(cache_g, inp.context, &deliverables).await?;

    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            crate::solver::buildah::Graph::new(&subset, &parsed.global_args, &file_dir, &module_refs, &stage_cache),
            name,
            push_ref,
            &tag,
            inp.context,
            &HashMap::new(),
            insecure,
            inp.force,
        )
        .await?;
        out.push((name.clone(), tag));
    }
    println!(
        "basil: done ({} deliverable{})",
        out.len(),
        if out.len() == 1 { "" } else { "s" }
    );
    Ok(out)
}

/// Manual promotion: set the immutable `v<version>` tag on each selected
/// deliverable, pointing at its current content tag (`basil-<tagHash>`).
/// Mirrors `promote_on_tag`'s semantics (immutable retag — refuses to
/// overwrite a version tag that points at a different manifest, no-op when it
/// already points at this one) but is driven by an explicit CLI version, not an
/// event. No image is built or pushed: the content tag must already exist.
pub async fn promote_version(
    file: &str,
    context: &std::path::Path,
    target_filter: Option<&str>,
    version: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let solved = solve(file, context, target_filter).await?;
    let SolveContext { deliverables, closures, .. } = solved;
    let version = version.trim_start_matches('v').to_string();
    if version.split('.').any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        anyhow::bail!("invalid version `{version}` (expected e.g. 1.2.3)");
    }

    let mut promoted: Vec<(String, String)> = Vec::new();
    for (name, push_ref) in &deliverables {
        let ir = crate::solver::registry::parse_ref(push_ref, "")
            .map_err(|e| anyhow::anyhow!("promote `{push_ref}`: {e}"))?;
        let client = ir.client();
        let tag_hash = digest_of(name, &closures)?;
        let content = crate::solver::registry::content_tag(&tag_hash);
        // The promoted tag must already be published (the content tag must
        // exist) — promotion never builds.
        match client.manifest_exists(&ir.repo, &content).await {
            Ok(true) => {}
            Ok(false) => {
                anyhow::bail!(
                    "cannot promote `{push_ref}:v{version}`: content tag `{push_ref}:{content}` is not published — build it first"
                );
            }
            Err(e) => {
                tracing::warn!("promote probe for `{push_ref}:{content}` failed ({e}); will attempt retag");
            }
        }
        let dest = format!("v{version}");
        client
            .retag(&ir.repo, &content, &dest)
            .await
            .map_err(|e| {
                anyhow::anyhow!("promotion immutability/retag failure for `{push_ref}:v{version}`: {e}")
            })?;
        println!("basil: promoted {push_ref}:{dest} -> {content}");
        promoted.push((push_ref.clone(), dest));
    }
    Ok(promoted)
}

/// The subset of targets reachable from `roots`: each root plus every stage it
/// depends on via `FROM <stage>` or `COPY --from=<stage>`, transitively. This
/// lets a targeted build avoid computing identity/render for unrelated stages.
pub fn dependency_subset(all: &[crate::dsl::ast::Target], roots: &[String]) -> Vec<crate::dsl::ast::Target> {    let by_name: std::collections::HashMap<&str, &crate::dsl::ast::Target> =
        all.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut keep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(name) = stack.pop() {
        if !keep.insert(name.clone()) {
            continue;
        }
        let Some(t) = by_name.get(name.as_str()) else {
            continue; // external ref, not a local stage
        };
        // A module base reaches no local stage — its artifact is a separate
        // build, so the subset ends at the importing target.
        let crate::dsl::ast::Base::Image(crate::dsl::ast::BaseImage { raw, .. }) = &t.base else {
            continue;
        };
        if let Some(b) = by_name.get(raw.as_str()) {
            if b.name != name {
                stack.push(b.name.clone());
            }
        }
        for instr in &t.instructions {
            if let crate::dsl::ast::Instruction::Copy(c) = instr {
                if let Some(from) = &c.from {
                    if by_name.contains_key(from.as_str()) {
                        stack.push(from.clone());
                    }
                }
            }
        }
    }
    // Preserve source order for determinism.
    all.iter()
        .filter(|t| keep.contains(t.name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse;

    #[test]
    fn dependency_subset_reaches_transitive_stages() {
        let src = "\
FROM golang AS build
COPY . /src
RUN go build -o /src/app

FROM scratch AS a/platform/app
COPY --from=build /src/app /app

FROM alpine AS unrelated
RUN echo other
";
        let f = parse(src).unwrap();
        let sub = dependency_subset(&f.targets, &["a/platform/app".to_string()]);
        let names: Vec<&str> = sub.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"a/platform/app"), "{names:?}");
        assert!(names.contains(&"build"), "{names:?}");
        // The FROM build is a stage dep; the unrelated alpine target is excluded.
        assert!(!names.contains(&"unrelated"), "{names:?}");
    }

    #[test]
    fn dependency_subset_includes_scratch_root() {
        let src = "FROM scratch AS app\nENTRYPOINT [\"/app\"]\n";
        let f = parse(src).unwrap();
        let sub = dependency_subset(&f.targets, &["app".to_string()]);
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "app");
    }
}

/// Build using the buildah backend (plain subprocess, no daemon), driven by
/// a delivered event: match reactions, build/promote the deliverables.
pub async fn handle_event(inp: BuildInput<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let src = crate::fsctx::fs_err(std::fs::read_to_string(inp.file), inp.file, "read")?;
    println!("basil: parsed {}", inp.file);
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;

    let event_payload: Json = match inp.event_json {
        Some(j) => serde_json::from_str(j).map_err(|e| anyhow::anyhow!("invalid event JSON: {e}"))?,
        None => {
            return Err(anyhow::anyhow!(
                "build requires an event (stdin JSON) or a manual --target"
            ));
        }
    };
    let matched = event::matching_reactions(&parsed, &event_payload);
    let event_ref = event_payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    // Resolve each `BUILD` entry to the target's (stage name, push ref). An
    // entry may be the stage name (`fennel`) or its REGISTER ref; either way the
    // image is pushed/looked up by the push ref.
    let mut deliverables: Vec<(String, String)> = Vec::new();
    for r in &matched {
        for b in &r.build {
            let t = parsed
                .targets
                .iter()
                .find(|t| &t.name == b || t.register.as_ref() == Some(b));
            let entry = t.map(|t| (t.name.clone(), t.push_ref().to_string()))
                .unwrap_or_else(|| (b.clone(), b.clone()));
            if !deliverables.contains(&entry) {
                deliverables.push(entry);
            }
        }
    }
    let deliverable_names: Vec<String> = deliverables.iter().map(|(n, _)| n.clone()).collect();
    if matched.is_empty() {
        return Err(anyhow::anyhow!("no matching reactions for the delivered event"));
    }
    // DO-only: run tasks via buildah run.
    if deliverable_names.is_empty() {
        for r in &matched {
            for do_name in &r.do_list {
                if let Some(t) = parsed.targets.iter().find(|t| &t.name == do_name) {
                    if let Some(task) = &t.task {
                        let crate::dsl::ast::Base::Image(base_image) = &t.base else {
                            anyhow::bail!("DO task `{do_name}`: module bases are not runnable as tasks");
                        };
                        let image = resolved_image(base_image);
                        let env = vec![format!("BASIL_EVENT={}", event_ref)];
                        let code = crate::solver::buildah::run_task(&image, &task.command, &env).await?;
                        if code != 0 {
                            anyhow::bail!("DO task `{do_name}` failed with exit {code}");
                        }
                        tracing::info!("DO task `{do_name}` completed (exit 0)");
                    }
                }
            }
        }
        return Ok(vec![]);
    }

    let mut build_args = HashMap::new();
    // Thread a VERSION build-arg when a Tag/Release reaction carries one (from
    // the git tag ref or the release tag_name). Only targets that reference it
    // consume it in their identity.
    use crate::dsl::ast::EventToken;
    let version_arg = matched.iter().find_map(|r| {
        if matches!(r.event.token, EventToken::Tag | EventToken::Release) {
            crate::solver::promote::event_version(&r.event.token, &event_payload, &event_ref)
                .map(|(_, v)| v)
        } else {
            None
        }
    });
    if let Some(version) = version_arg {
        build_args.insert("VERSION".to_string(), version);
    }
    println!("basil: resolving {} base(s) from registries...", parsed.targets.len());
    let t0 = std::time::Instant::now();
    let mut resolved = resolve_unpinned_bases(&parsed.targets)
        .await
        .map_err(anyhow::Error::msg)?;
    // The build file's own path/context; module paths resolve against its
    // directory (D4: file-relative).
    let build_file = std::path::absolute(inp.file).unwrap_or_else(|_| inp.file.into());
    let file_dir = build_file.parent().unwrap_or(std::path::Path::new("."));
    // Discover module dependencies (recursive, cycle-checked, pure file walk).
    let index = crate::identity::modules::discover_module_index(&parsed.targets, file_dir)
        .map_err(anyhow::Error::msg)?;
    if !index.order.is_empty() {
        println!("basil: {} module(s) discovered", index.order.len());
    }
    for key in &index.order {
        let e = &index.entries[key];
        let module_resolved = resolve_unpinned_bases(&e.parsed.targets)
            .await
            .map_err(anyhow::Error::msg)?;
        resolved.extend(module_resolved);
    }
    println!("basil: resolved {} base(s) in {:.1}s", resolved.len(), t0.elapsed().as_secs_f64());
    let t1 = std::time::Instant::now();
    println!("basil: computing identity ({} targets)...", parsed.targets.len());
    let mut resolver = RegistryResolver { resolved };
    // Module deliverable digests first (their own bases resolved above), so
    // the importer closure folds `base.module.*` from them.
    let module_digests =
        crate::identity::modules::compute_module_digests(&index, &mut resolver)
            .map_err(anyhow::Error::msg)?;
    let closures = compute_closure(
        &parsed.targets,
        &parsed.default_platforms,
        &parsed.global_args,
        inp.context,
        &build_file,
        &build_args,
        &mut resolver,
        &module_digests.deliverable,
    )
    .map_err(anyhow::Error::msg)?;
    // Module key -> `FROM <push-ref>:<tagHash>` for the importer's Containerfile.
    let mut module_refs: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for key in &index.order {
        let e = &index.entries[key];
        let tag = crate::identity::closure::hex(&module_digests.deliverable[key]);
        module_refs.insert(
            key.clone(),
            format!("{}:{}", e.push_ref, crate::solver::registry::content_tag(&tag)),
        );
    }
    println!("basil: identity computed in {:.1}s", t1.elapsed().as_secs_f64());

    // Every non-deliverable stage in scope gets a cache image
    // (`<repo>:basil-cache-<stageHash>`). The cache repo is `--cache-to`, else
    // `BASIL_CACHE_REF`, else the first deliverable's repo (D1).
    let cache_to = effective_cache_to(inp.cache_to, &deliverables);
    let stage_cache = stage_cache_refs(&parsed.targets, &deliverables, &closures, &cache_to);

    // Idempotent check: skip when every deliverable's content tag exists, and
    // state each registered image on the noop path. `--force` bypasses the
    // check entirely — no probe, always rebuild and re-push.
    if !inp.force {
        let published = idempotent_published(&deliverables, &closures).await?;
        if !published.is_empty() {
            println!("basil: content tags already published -- no build (idempotent check)");
            // Modules are part of the declared artifact set too: reconcile them
            // by existence even on the noop path, so a deleted module is
            // re-registered (same tagHash — the importer image stays valid).
            ensure_modules_built(&index, &module_refs, &module_digests, inp.cache_to).await?;
            // Stages are reconciled by existence too (D2).
            let cache_g = crate::solver::buildah::Graph::new(&parsed.targets, &parsed.global_args, file_dir, &module_refs, &stage_cache);
            ensure_stages_cached(cache_g, inp.context, &deliverables).await?;
            let g = crate::solver::buildah::Graph::new(&parsed.targets, &parsed.global_args, file_dir, &module_refs, &stage_cache);
            for (_, push_ref, tag) in &published {
                println!("basil: image {push_ref}:{tag} (registered)");
            }
            // OUTPUT still runs on a skip: pull the published image and extract.
            for (name, push_ref, tag) in &published {
                crate::solver::buildah::extract_outputs_from_image(
                    &format!("{push_ref}:{tag}"),
                    inp.context,
                    g,
                    name,
                )
                .await?;
            }
            promote_on_tag(&matched, &event_payload, &event_ref, &closures, &parsed.targets).await?;
            println!("basil: done ({})", deliverable_names.join(", "));
            return Ok(vec![
                ("result".to_string(), "idempotent-skip".to_string()),
                ("digest".to_string(), digest_of(&deliverable_names[0], &closures)?),
            ]);
        }
    }

    // Build path: every module dependency must exist as its content-tagged
    // image before the importer renders FROM it — probe; build + push the
    // module when missing (recursive). `--force` never propagates: modules are
    // still reconciled by existence, only the importer is force-built.
    ensure_modules_built(&index, &module_refs, &module_digests, inp.cache_to).await?;
    // Every intermediate stage must exist as its cache image first (D2).
    let cache_g = crate::solver::buildah::Graph::new(&parsed.targets, &parsed.global_args, file_dir, &module_refs, &stage_cache);
    ensure_stages_cached(cache_g, inp.context, &deliverables).await?;

    // Build each deliverable with buildah.
    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            crate::solver::buildah::Graph::new(&parsed.targets, &parsed.global_args, file_dir, &module_refs, &stage_cache),
            name,
            push_ref,
            &tag,
            inp.context,
            &build_args,
            insecure,
            inp.force,
        )
        .await?;
    }

    promote_on_tag(&matched, &event_payload, &event_ref, &closures, &parsed.targets).await?;
    println!("basil: done ({})", deliverable_names.join(", "));
    Ok(vec![(
        "digest".to_string(),
        digest_of(&deliverable_names[0], &closures)?,
    )])
}

/// Ensure each module dependency exists as its content-tagged image in its
/// registry: probe `<push-ref>:<tagHash>`; build + push the module when
/// missing. Iterates dependency-first so a module's own module deps are in
/// place before it builds. `--force` does NOT propagate here — modules are
/// reconciled by existence, always (use-or-build).
async fn ensure_modules_built(
    index: &crate::identity::modules::ModuleIndex,
    module_refs: &std::collections::BTreeMap<String, String>,
    module_closures: &crate::identity::modules::ModuleClosures,
    cache_to: Option<&str>,
) -> anyhow::Result<()> {
    for key in &index.order {
        let e = &index.entries[key];
        let from_ref = &module_refs[key];
        // `from_ref` is `<push-ref>:basil-<hash>`; keep the full namespaced tag
        // for the existence probe (that is the tag as stored in the registry)
        // but hand build_deliverable the bare hash — it re-prefixes internally.
        let full_tag = from_ref.strip_prefix(&format!("{}:", e.push_ref)).unwrap_or(from_ref);
        let hash = full_tag
            .strip_prefix(crate::solver::registry::BASIL_TAG_PREFIX)
            .unwrap_or(full_tag);
        let ir = crate::solver::registry::parse_ref(&e.push_ref, full_tag)
            .map_err(|err| anyhow::anyhow!("module `{}` ref `{}`: {err}", key, e.push_ref))?;
        let exists = match ir.client().manifest_exists(&ir.repo, full_tag).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!("module `{}` registry probe failed ({err}); will build", key);
                false
            }
        };
        // The module's own intermediate stages are cached under its repo too
        // (cache-to override, else the module's push_ref), reconciled by
        // existence exactly like the importer's stages.
        let module_repo = cache_to.unwrap_or(&e.push_ref);
        let deliv = [(e.deliverable.clone(), e.push_ref.clone())];
        let stage_cache = stage_cache_refs(
            &e.parsed.targets,
            &deliv,
            &module_closures.tables[key],
            module_repo,
        );
        let g_for_cache = crate::solver::buildah::Graph::new(
            &e.parsed.targets,
            &e.parsed.global_args,
            &e.context,
            module_refs,
            &stage_cache,
        );
        ensure_stages_cached(g_for_cache, &e.context, &deliv).await?;
        if exists {
            println!("basil: module {key} -> {from_ref} (using existing image)");
            continue;
        }
        println!("basil: module {key} -> building {from_ref}");
        let insecure = ir.is_local();
        // The module's own graph: its file's targets + global args, its own
        // file dir for ITS module deps, the full refs map (nested modules
        // included), and its own stage-cache refs for rendering.
        let g = crate::solver::buildah::Graph::new(
            &e.parsed.targets,
            &e.parsed.global_args,
            &e.context,
            module_refs,
            &stage_cache,
        );
        crate::solver::buildah::build_deliverable(
            g,
            &e.deliverable,
            &e.push_ref,
            hash,
            &e.context,
            &std::collections::HashMap::new(),
            insecure,
            // Modules are reconciled by existence, always: never force-built.
            false,
        )
        .await?;
    }
    Ok(())
}

/// The cache repo for intermediate-stage images: `--cache-to`, else
/// `BASIL_CACHE_REF`, else the first deliverable's repo (the D1 default).
fn effective_cache_to(
    flag: Option<&str>,
    deliverables: &[(String, String)],
) -> String {
    if let Some(f) = flag {
        return f.to_string();
    }
    if let Ok(env) = std::env::var("BASIL_CACHE_REF") {
        if !env.trim().is_empty() {
            return env;
        }
    }
    deliverables
        .first()
        .map(|(_, push_ref)| push_ref.clone())
        .unwrap_or_else(|| "127.0.0.1:5001/basil-cache".to_string())
}

/// Stage name -> `<cache-to>:basil-cache-<stageHash>` for every in-scope
/// target that is not a deliverable (the deliverable is published under its
/// content tag instead).
fn stage_cache_refs(
    targets: &[crate::dsl::ast::Target],
    deliverables: &[(String, String)],
    closures: &std::collections::HashMap<String, [u8; 32]>,
    cache_to: &str,
) -> std::collections::BTreeMap<String, String> {
    let deliv: std::collections::BTreeSet<&str> =
        deliverables.iter().map(|(name, _)| name.as_str()).collect();
    let mut out = std::collections::BTreeMap::new();
    for t in targets {
        if deliv.contains(t.name.as_str()) {
            continue;
        }
        let Some(hash) = closures.get(&t.name) else { continue };
        let tag = crate::solver::registry::cache_tag(&crate::identity::closure::hex(hash));
        out.insert(t.name.clone(), format!("{cache_to}:{tag}"));
    }
    out
}

/// Ensure every intermediate stage exists as its cache image in the registry:
/// probe `<cache-to>:basil-cache-<stageHash>`; on miss, build `--target
/// <stage>`, push it, and remove the local image. Iterates in subset order
/// (dependencies first). `--force` never propagates here — stages are
/// reconciled by existence, always (D2).
async fn ensure_stages_cached(
    g: crate::solver::buildah::Graph<'_>,
    context: &std::path::Path,
    deliverables: &[(String, String)],
) -> anyhow::Result<()> {
    let deliv: std::collections::BTreeSet<&str> =
        deliverables.iter().map(|(name, _)| name.as_str()).collect();
    for t in g.targets {
        if deliv.contains(t.name.as_str()) {
            continue;
        }
        let Some(ref_str) = g.stage_cache.get(&t.name) else { continue };
        // `<cache-to>:basil-cache-<hash>` -> repo + tag.
        let (repo, tag) = ref_str
            .rsplit_once(':')
            .map(|(r, t)| (r.to_string(), t.to_string()))
            .unwrap_or_else(|| (ref_str.clone(), String::new()));
        let ir = crate::solver::registry::parse_ref(&repo, &tag)
            .map_err(|e| anyhow::anyhow!("stage `{}` cache ref `{ref_str}`: {e}", t.name))?;
        let exists = match ir.client().manifest_exists(&ir.repo, &tag).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!("stage `{}` cache probe failed ({err}); will build", t.name);
                false
            }
        };
        if exists {
            println!("basil: stage {} -> {ref_str} (cached)", t.name);
            continue;
        }
        println!("basil: stage {} -> building cache {ref_str}", t.name);
        let image_id = crate::solver::buildah::build_stage_image(g, &t.name, context).await?;
        let insecure = ir.is_local();
        // buildah push needs the full `<host>/<repo>` ref, not the repo path.
        crate::solver::buildah::push_tagged(&t.name, &repo, &tag, &image_id, insecure).await?;
        crate::solver::buildah::remove_local_image(&image_id).await?;
        println!("basil: stage {} cached at {ref_str}", t.name);
    }
    Ok(())
}

/// Idempotent check: every deliverable's content tag must already exist in its
/// registry. Returns the registered (stage name, push ref, content tag) triples
/// when the build can be skipped outright — the noop path prints each published
/// image from this list, so a noop rebuild still makes the registry
/// registration explicit. A missing tag stops the check (the build will
/// publish it); a probe error falls back to building, which re-surfaces any
/// real registry problem at push time.
async fn idempotent_published(
    deliverables: &[(String, String)],
    closures: &std::collections::HashMap<String, [u8; 32]>,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut published = Vec::new();
    for (name, push_ref) in deliverables {
        let tag_hash = digest_of(name, closures)?;
        let tag = crate::solver::registry::content_tag(&tag_hash);
        let ir = crate::solver::registry::parse_ref(&push_ref, &tag)
            .map_err(|e| anyhow::anyhow!("deliverable `{push_ref}`: {e}"))?;
        let client = ir.client();
        match client.manifest_exists(&ir.repo, &tag).await {
            Ok(true) => published.push((name.clone(), push_ref.clone(), tag)),
            Ok(false) => {
                tracing::info!("idempotent: `{push_ref}:{tag}` does not exist yet (will build)");
                return Ok(Vec::new());
            }
            Err(e) => {
                tracing::warn!("idempotent registry check for `{push_ref}:{tag}` failed: {e}");
                return Ok(Vec::new());
            }
        }
    }
    Ok(published)
}

/// Whether a target is a deliverable: it names a registry ref directly
/// (qualified `name`), or declares a `REGISTER` full ref.
fn is_deliverable(t: &crate::dsl::ast::Target) -> bool {
    t.register.is_some() || t.name.split('/').count() >= 2
}

/// The `tagHash` for a built deliverable, or an error if its closure wasn't
/// computed (a target that was never in scope — e.g. a `-t` typo — must not
/// silently produce an empty digest and skip the build).
fn digest_of(
    name: &str,
    closures: &std::collections::HashMap<String, [u8; 32]>,
) -> anyhow::Result<String> {
    closures
        .get(name)
        .map(|t| crate::identity::closure::hex(t))
        .ok_or_else(|| anyhow::anyhow!("no identity computed for `{name}`"))
}



fn resolved_image(base: &crate::dsl::ast::BaseImage) -> String {
    let mut s = base.raw.clone();
    if let Some(t) = &base.tag {
        s.push(':');
        s.push_str(t);
    }
    if let Some(d) = &base.digest {
        s.push('@');
        s.push_str(d);
    }
    s
}