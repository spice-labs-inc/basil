//! Orchestrate the `basil build` flow: parse → match event → identity → buildah build.

use crate::dsl::parser;
use crate::event;
use crate::identity::closure::{compute_closure, BaseResolver};
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
    fn resolve(
        &mut self,
        raw: &str,
        tag: Option<&str>,
        digest: Option<&str>,
    ) -> Result<String, String> {
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
    // IMPORT instructions inline an external image mid-build: resolve their
    // refs to manifest digests exactly like unpinned bases, so closure can
    // fold the digest into identity. Pinned imports resolve textually; a ref
    // naming a local stage is left alone (closure rejects it with a clear
    // message). Dedup is shared with base pulls — same ref, one resolution.
    for t in targets {
        for instr in &t.instructions {
            let crate::dsl::ast::Instruction::Import(im) = instr else {
                continue;
            };
            if im.digest.is_some() {
                continue;
            }
            if by_name.contains(im.raw.as_str()) && im.raw != t.name {
                continue; // local stage ref — closure errors with context
            }
            let tag = im.tag.clone().unwrap_or_else(|| "latest".to_string());
            let ir = match crate::solver::registry::parse_ref(&im.raw, &tag) {
                Ok(ir) => ir,
                Err(e) => return Err(format!("IMPORT `{raw}`: {e}", raw = im.raw)),
            };
            if !seen.insert(ir.dedup_key(&tag)) {
                continue;
            }
            let raw = &im.raw;
            match ir.resolve_digest().await {
                Ok(digest) => {
                    tracing::info!("resolved IMPORT `{raw}` -> {digest}");
                    resolved.insert(raw.clone(), digest.clone());
                    resolved.insert(format!("{raw}:{tag}"), digest);
                }
                Err(e) => {
                    return Err(format!(
                        "could not resolve IMPORT `{raw}` ({}): {e}",
                        ir.dedup_key(&tag)
                    ));
                }
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
        let src = format!(
            "{}:{}",
            ir.repo,
            crate::solver::registry::content_tag(&tag_hash)
        );
        match client
            .retag(
                &ir.repo,
                &crate::solver::registry::content_tag(&tag_hash),
                &format!("v{version}"),
            )
            .await
        {
            Ok(d) => tracing::info!(
                "promote `{push_ref}:{src}` -> v{version} (digest {})",
                &d[..d.len().min(24)]
            ),
            Err(e) => {
                anyhow::bail!(
                    "promotion immutability/retag failure for `{push_ref}:v{version}`: {e}"
                )
            }
        }
    }
    Ok(())
}

/// Whether any of the named targets would change on a build: its current
/// content tagHash is not yet published at its push ref. A change-detection
/// gate for CI - exit 0 if changed for any target, 1 if none. Each name
/// must resolve to a deliverable (qualified AS name or REGISTER ref); a
/// registry probe failure is an error, never a silent "unchanged".
pub async fn is_changed(
    file: &str,
    context: &std::path::Path,
    targets: &[String],
) -> anyhow::Result<bool> {
    if targets.is_empty() {
        anyhow::bail!("is-changed needs at least one target");
    }
    let ctx = solve(file, context, None).await?;
    // Validate every requested name before probing anything (fail closed:
    // a typo'd target must not silently report "unchanged").
    let resolved: Vec<(String, String, String)> = targets
        .iter()
        .map(|name| {
            let t = ctx
                .parsed
                .targets
                .iter()
                .find(|x| x.name.as_str() == name || x.register.as_deref() == Some(name))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "target {name} not found in {file} (known: {})",
                        ctx.parsed
                            .targets
                            .iter()
                            .map(|x| format!("{}({})", x.name, x.push_ref()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            if !is_deliverable(t) {
                anyhow::bail!(
                    "target {name} is not a deliverable: give it a qualified name or a REGISTER ref"
                );
            }
            let hash = digest_of(&t.name, &ctx.closures)?;
            Ok((t.name.clone(), t.push_ref().to_string(), hash))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut changed = false;
    for (name, push_ref, hash) in resolved {
        let ir = crate::solver::registry::parse_ref(&push_ref, "")
            .map_err(|e| anyhow::anyhow!("is-changed {push_ref}: {e}"))?;
        let client = ir.client();
        let tag = crate::solver::registry::content_tag(&hash);
        let exists = client
            .manifest_exists(&ir.repo, &tag)
            .await
            .map_err(|e| anyhow::anyhow!("probing {push_ref}:{tag}: {e}"))?;
        if exists {
            println!("basil: {name} unchanged ({push_ref}:{tag})");
        } else {
            println!("basil: {name} changed ({push_ref}:{tag} not published)");
            changed = true;
        }
    }
    Ok(changed)
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
    /// (stage, commit seq) -> checkpoint closure for every `COMMIT` in the
    /// in-scope targets — the identity of each sealed instruction prefix.
    pub checkpoint_closures: std::collections::BTreeMap<(String, usize), [u8; 32]>,
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
    println!(
        "basil: {} target(s) in scope (of {} total)",
        subset.len(),
        parsed.targets.len()
    );

    // The build file's own path/context; module paths resolve against its
    // directory (D4: file-relative).
    let build_file = std::path::absolute(file).unwrap_or_else(|_| file.into());
    let build_file_owned = build_file.clone();

    // Discover module dependencies (recursive, cycle-checked, pure file walk).
    let index = crate::identity::modules::discover_module_index(
        &subset,
        build_file_owned
            .parent()
            .unwrap_or(std::path::Path::new(".")),
    )
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
            .map_err(|msg| crate::fsctx::with_module(anyhow_tracing::anyhow!("{msg}"), &key))?;
        resolved.extend(module_resolved);
    }
    println!(
        "basil: resolved {} base(s) in {:.1}s",
        resolved.len(),
        t0.elapsed().as_secs_f64()
    );
    let t0 = std::time::Instant::now();
    println!("basil: computing identity ({} target(s))...", subset.len());
    let mut resolver = RegistryResolver { resolved };
    // Module deliverable digests first (their own bases resolved above), so
    // the importer closure folds `base.module.*` from them.
    let module_digests = crate::identity::modules::compute_module_digests(&index, &mut resolver)
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
            format!(
                "{}:{}",
                e.push_ref,
                crate::solver::registry::content_tag(&tag)
            ),
        );
    }
    // Checkpoint identities (`COMMIT`): the closure of each sealed prefix,
    // derived with the same resolver (external bases/imports) and the full
    // stage closures (stage deps).
    let checkpoint_closures = crate::identity::closure::compute_checkpoint_closures(
        &subset,
        &closures,
        &parsed.default_platforms,
        &parsed.global_args,
        context,
        &build_file_owned,
        &HashMap::new(),
        &mut resolver,
        &module_digests.deliverable,
    )
    .map_err(anyhow::Error::msg)?;
    println!(
        "basil: identity computed in {:.1}s",
        t0.elapsed().as_secs_f64()
    );
    for name in &names {
        println!("basil: identity {name} -> {}", digest_of(name, &closures)?);
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
        checkpoint_closures,
    })
}

/// Run a non-published target's `TASK` clause in a one-shot container via
/// buildah, without an event: the manual form of the event path's `DO`.
/// Registered (REGISTER-clause) targets are excluded as run targets — they
/// are published deliverables. The run target is never published. Everything
/// the task runs on is built the standard way first — an internal `FROM
/// <stage>` base (and its transitive stage deps) is reconciled as a
/// `basil-cache-<stageHash>` cache image in the cache repo (`--cache-to`,
/// `BASIL_CACHE_REF`, the target's own repo when it is a qualified ref), and a
/// module base is built use-or-build. REGISTERed stages in the subset follow
/// the same rule as the build path: they resolve to their published
/// content-tagged artifact (`<register>:basil-<hash>`, use-or-build) instead
/// of being cache-reconciled — the only difference between build and `run` is
/// that deliverables are not run targets. The task then runs on the resolved
/// base image. A failing task fails the invocation. Returns the task's exit
/// code.
pub async fn run_target(
    file: &str,
    context: &std::path::Path,
    target_filter: &str,
    cache_to: Option<&str>,
) -> anyhow::Result<i32> {
    let src = crate::fsctx::fs_err(std::fs::read_to_string(file), file, "read")?;
    println!("basil: parsed {file}");
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;
    let (name, target, task) = resolve_task_target(&parsed, target_filter)
        .map_err(|e| anyhow::anyhow!("`{target_filter}`: {e}"))?;

    // The required dependency set: for an internal `FROM <stage>` base, the
    // base and its transitive stage deps; for a module base, the module (and
    // any stage deps of the target); for an external base, nothing.
    let subset = match &target.base {
        crate::dsl::ast::Base::Module(_) => dependency_subset(&parsed.targets, &[name.to_string()]),
        crate::dsl::ast::Base::Image(_) => match base_stage_name(&parsed, target) {
            Some(bs) => dependency_subset(&parsed.targets, &[bs]),
            None => Vec::new(),
        },
    };
    let build_file = std::path::absolute(file).unwrap_or_else(|_| file.into());
    let file_dir = build_file.parent().unwrap_or(std::path::Path::new("."));

    let task_image = if subset.is_empty() {
        // External base: nothing to build; the task runs on the image as
        // written (buildah from pulls it).
        resolved_image(target.base_image().expect("base is an image"))
    } else {
        // Identity preamble, exactly like the build path.
        let mut resolved = resolve_unpinned_bases(&subset)
            .await
            .map_err(anyhow::Error::msg)?;
        let index = crate::identity::modules::discover_module_index(&subset, file_dir)
            .map_err(anyhow::Error::msg)?;
        for key in &index.order {
            let e = &index.entries[key];
            let module_resolved = resolve_unpinned_bases(&e.parsed.targets)
                .await
                .map_err(|msg| crate::fsctx::with_module(anyhow_tracing::anyhow!("{msg}"), &key))?;
            resolved.extend(module_resolved);
        }
        let mut resolver = RegistryResolver { resolved };
        let module_digests =
            crate::identity::modules::compute_module_digests(&index, &mut resolver)
                .map_err(anyhow::Error::msg)?;
        let closures = compute_closure(
            &subset,
            &parsed.default_platforms,
            &parsed.global_args,
            context,
            &build_file,
            &HashMap::new(),
            &mut resolver,
            &module_digests.deliverable,
        )
        .map_err(anyhow::Error::msg)?;
        let mut module_refs: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for key in &index.order {
            let e = &index.entries[key];
            let tag = crate::identity::closure::hex(&module_digests.deliverable[key]);
            module_refs.insert(
                key.clone(),
                format!(
                    "{}:{}",
                    e.push_ref,
                    crate::solver::registry::content_tag(&tag)
                ),
            );
        }
        // Checkpoint identities (`COMMIT`) — the closure of each sealed prefix.
        let checkpoint_closures = crate::identity::closure::compute_checkpoint_closures(
            &subset,
            &closures,
            &parsed.default_platforms,
            &parsed.global_args,
            context,
            &build_file,
            &HashMap::new(),
            &mut resolver,
            &module_digests.deliverable,
        )
        .map_err(anyhow::Error::msg)?;
        // Module dependencies: use-or-build (they are published artifacts).
        ensure_modules_built(&index, &module_refs, &module_digests, cache_to).await?;
        // Stage dependencies: the standard cache-image reconciliation. The
        // run target itself is never published — it is only excluded from
        // cache building when a module base puts it in the subset.
        let cache_to = effective_run_cache_to(cache_to, target);
        let run_deliverable = [(name.to_string(), target.push_ref().to_string())];
        let stage_cache = stage_cache_refs(&subset, &run_deliverable, &closures, &cache_to);
        let checkpoint_refs = checkpoint_refs(&checkpoint_closures);
        let g = crate::solver::buildah::Graph::new(
            &subset,
            &parsed.global_args,
            &parsed.default_platforms,
            file_dir,
            &module_refs,
            &stage_cache,
            &checkpoint_refs,
        );
        println!(
            "basil: reconciling {} dependency stage(s) as cache images in {cache_to}...",
            subset.len()
        );
        ensure_stages_cached(g, context, &build_file, &run_deliverable).await?;
        // The image the task runs on: the resolved base.
        match &target.base {
            crate::dsl::ast::Base::Module(m) => {
                let key = crate::identity::modules::module_key(file_dir, m)
                    .map_err(anyhow::Error::msg)?;
                module_refs.get(&key).cloned().ok_or_else(|| {
                    anyhow::anyhow!("module `{}`: no computed image for the task", m.path)
                })?
            }
            crate::dsl::ast::Base::Image(b) => {
                let bs = base_stage_name(&parsed, target).expect("subset implies internal base");
                stage_cache
                    .get(&bs)
                    .cloned()
                    .unwrap_or_else(|| resolved_image(b))
            }
        }
    };

    // No delivered event; the target itself is the only task context.
    let env = vec![format!("BASIL_TARGET={name}")];
    println!("basil: run {name} from {task_image}");
    let code = crate::solver::buildah::run_task(&task_image, &task.command, &env).await?;
    if code != 0 {
        anyhow::bail!("task `{name}` failed with exit {code}");
    }
    println!("basil: task {name} completed (exit 0)");
    Ok(code)
}

/// The cache repo for a `run`'s stage images: `--cache-to`, else
/// `BASIL_CACHE_REF`, else the run target's own repo when it is a qualified
/// ref; a bare non-qualified target has no natural repo, so the operator must
/// say where its dependent stage cache images go.
fn effective_run_cache_to(flag: Option<&str>, target: &crate::dsl::ast::Target) -> String {
    if let Some(f) = flag {
        return f.to_string();
    }
    if let Ok(env) = std::env::var("BASIL_CACHE_REF") {
        if !env.trim().is_empty() {
            return env;
        }
    }
    // A qualified target names its own repo (mirrors the deliverable-repo
    // default in the build path).
    let push_ref = target.push_ref();
    if push_ref.contains('/') {
        return push_ref.to_string();
    }
    "127.0.0.1:5001/basil-cache".to_string()
}

/// Select a `run` target by AS name or REGISTER ref and require its `TASK`
/// clause. Module-base targets are runnable too (the module is built
/// use-or-build and the task runs on its image). Pure — unit-tested without
/// buildah.
fn resolve_task_target<'a>(
    parsed: &'a crate::dsl::ast::File,
    filter: &str,
) -> Result<
    (
        &'a str,
        &'a crate::dsl::ast::Target,
        &'a crate::dsl::ast::TaskCmd,
    ),
    String,
> {
    let target = parsed
        .targets
        .iter()
        .find(|t| t.name == filter || t.register.as_deref() == Some(filter))
        .ok_or_else(|| {
            format!(
                "target `{filter}` not found in file (known: {})",
                parsed
                    .targets
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    // A REGISTERed target is a published deliverable; `run` is the manual DO
    // for non-published targets, so it is excluded regardless of a TASK.
    if target.register.is_some() {
        return Err(format!(
            "target `{}` is REGISTERed (`{}`) — a published deliverable; \
             `basil run` excludes registered targets (run a non-registered \
             stage with a TASK clause)",
            target.name,
            target.register.as_deref().unwrap_or(&target.name),
        ));
    }
    let task = target
        .task
        .as_ref()
        .ok_or_else(|| format!("target has no TASK clause to run"))?;
    Ok((target.name.as_str(), target, task))
}

/// The stage a target's `FROM` base names (an internal-stage base, applying
/// the self-name rule: a base naming its own target stays external). `None`
/// for external images and module bases.
fn base_stage_name<'a>(
    parsed: &'a crate::dsl::ast::File,
    target: &crate::dsl::ast::Target,
) -> Option<String> {
    let crate::dsl::ast::Base::Image(b) = &target.base else {
        return None;
    };
    parsed
        .targets
        .iter()
        .find(|t| t.name == b.raw && t.name != target.name)
        .map(|t| t.name.clone())
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
        checkpoint_closures,
        file_dir,
        build_file,
        ..
    } = solved;

    let mut out: Vec<(String, String)> = Vec::new();

    // Every non-deliverable stage in scope gets a cache image
    // (`<repo>:basil-cache-<stageHash>`). The cache repo is `--cache-to`, else
    // `BASIL_CACHE_REF`, else the first deliverable's repo (D1).
    let cache_to = effective_cache_to(inp.cache_to, &deliverables);
    let stage_cache = stage_cache_refs(&subset, &deliverables, &closures, &cache_to);
    // `COMMIT` checkpoints seal the same namespace: each prefix gets a
    // `basil-cache-<prefixHash>` ref derived from its checkpoint closure.
    let checkpoint_refs = checkpoint_refs(&checkpoint_closures);

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
            let cache_g = crate::solver::buildah::Graph::new(
                &subset,
                &parsed.global_args,
                &parsed.default_platforms,
                &file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            );
            ensure_stages_cached(cache_g, inp.context, &build_file, &deliverables).await?;
            let g = crate::solver::buildah::Graph::new(
                &subset,
                &parsed.global_args,
                &parsed.default_platforms,
                &file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            );
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
    let cache_g = crate::solver::buildah::Graph::new(
        &subset,
        &parsed.global_args,
        &parsed.default_platforms,
        &file_dir,
        &module_refs,
        &stage_cache,
        &checkpoint_refs,
    );
    ensure_stages_cached(cache_g, inp.context, &build_file, &deliverables).await?;

    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            crate::solver::buildah::Graph::new(
                &subset,
                &parsed.global_args,
                &parsed.default_platforms,
                &file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            ),
            name,
            push_ref,
            &tag,
            inp.context,
            &build_file,
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
    let SolveContext {
        deliverables,
        closures,
        ..
    } = solved;
    let version = version.trim_start_matches('v').to_string();
    if version
        .split('.')
        .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
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
                tracing::warn!(
                    "promote probe for `{push_ref}:{content}` failed ({e}); will attempt retag"
                );
            }
        }
        let dest = format!("v{version}");
        client.retag(&ir.repo, &content, &dest).await.map_err(|e| {
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
pub fn dependency_subset(
    all: &[crate::dsl::ast::Target],
    roots: &[String],
) -> Vec<crate::dsl::ast::Target> {
    let by_name: std::collections::HashMap<&str, &crate::dsl::ast::Target> =
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

    #[test]
    fn resolve_task_target_by_name() {
        let src = "FROM alpine AS plain\nTASK echo hi\n";
        let f = parse(src).unwrap();
        let (name, _, task) = resolve_task_target(&f, "plain").unwrap();
        assert_eq!(name, "plain");
        assert!(matches!(&task.command, crate::dsl::ast::Command::Shell(s) if s == "echo hi"));
    }

    #[test]
    fn resolve_task_target_refuses_registered() {
        let src = "FROM alpine AS notify\nTASK echo hi\nREGISTER 127.0.0.1:5001/notify\n";
        let f = parse(src).unwrap();
        // By the AS name.
        let e = resolve_task_target(&f, "notify").unwrap_err();
        assert!(
            e.contains("REGISTER") && e.contains("excludes registered"),
            "{e}"
        );
        assert!(e.contains("127.0.0.1:5001/notify"), "{e}");
        // By the REGISTER ref.
        let e = resolve_task_target(&f, "127.0.0.1:5001/notify").unwrap_err();
        assert!(e.contains("excludes registered"), "{e}");
    }

    #[test]
    fn resolve_task_target_errors() {
        let src = "FROM alpine AS plain\nRUN echo x\nFROM alpine AS notify\nTASK echo hi\n";
        let f = parse(src).unwrap();
        let e = resolve_task_target(&f, "nope").unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let e = resolve_task_target(&f, "plain").unwrap_err();
        assert!(e.contains("no TASK"), "{e}");
        // Module-base targets resolve too (the module is built use-or-build
        // and the task runs on its image).
        let m = parse("MODULE vendor/x AS m\nTASK echo hi\n").unwrap();
        let (name, _, _) = resolve_task_target(&m, "m").unwrap();
        assert_eq!(name, "m");
    }

    #[test]
    fn stage_cache_refs_registered_uses_published_ref() {
        let src = "\
FROM alpine AS base
RUN echo b

FROM base AS reg
RUN echo r
REGISTER 127.0.0.1:5001/reg

FROM reg AS app
COPY --from=reg /x /x
";
        let f = parse(src).unwrap();
        // Every target gets a fake but deterministic closure hash.
        let closures: std::collections::HashMap<String, [u8; 32]> = f
            .targets
            .iter()
            .map(|t| (t.name.clone(), [0x11u8; 32]))
            .collect();
        let refs = stage_cache_refs(
            &f.targets,
            &[("app".to_string(), "127.0.0.1:5001/app".to_string())],
            &closures,
            "127.0.0.1:5001/cache",
        );
        // A REGISTERed stage resolves to its published content-tagged artifact,
        // not an intermediate cache ref.
        let reg = refs.get("reg").expect("reg maps to its published ref");
        assert!(reg.starts_with("127.0.0.1:5001/reg:basil-"), "{reg}");
        assert!(
            !reg.contains("basil-cache-"),
            "registered stage must not be cache-reconciled: {reg}"
        );
        // Non-registered intermediates keep the cache ref.
        let base = refs.get("base").expect("base maps to a cache ref");
        assert!(
            base.starts_with("127.0.0.1:5001/cache:basil-cache-"),
            "{base}"
        );
        // The deliverable itself is excluded.
        assert!(
            !refs.contains_key("app"),
            "deliverable must not appear in stage refs"
        );
    }

    #[test]
    fn base_stage_name_internal_external_self() {
        let src = "FROM alpine AS base\nRUN echo b\n\nFROM base AS runner\nTASK echo r\n\nFROM alpine AS alpine\nTASK echo s\n";
        let f = parse(src).unwrap();
        let runner = f.targets.iter().find(|t| t.name == "runner").unwrap();
        assert_eq!(base_stage_name(&f, runner).as_deref(), Some("base"));
        // A base naming its own target stays external (self-name rule).
        let alpine = f.targets.iter().find(|t| t.name == "alpine").unwrap();
        assert_eq!(base_stage_name(&f, alpine), None);
        // External image base -> no stage.
        let plain = parse("FROM node:20 AS n\nTASK echo n\n").unwrap();
        let n = &plain.targets[0];
        assert_eq!(base_stage_name(&plain, n), None);
    }

    #[test]
    fn do_targets_mixed_build_and_do_reactions() {
        // A reaction pair BUILD app DO deploy must yield both - the deploy
        // runs after the build, not only when no deliverable is built.
        let src = "FROM alpine AS app\nRUN echo a\n\nFROM alpine AS deploy\nTASK echo deploy\nON push(main) BUILD app DO deploy\n";
        let f = parse(src).unwrap();
        let refs: Vec<&crate::dsl::ast::Reaction> = vec![&f.reactions[0]];
        let runs = do_targets_for(&refs, &f.targets);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].0, "deploy");
    }

    #[test]
    fn do_targets_skips_unknown_and_taskless() {
        // Unknown names and task-less targets are no-ops (the original
        // DO-only path's semantics, preserved).
        let src =
            "FROM alpine AS a\nTASK echo a\nFROM alpine AS b\nON push(main) DO a DO b DO nope\n";
        let f = parse(src).unwrap();
        let refs: Vec<&crate::dsl::ast::Reaction> = vec![&f.reactions[0]];
        let runs = do_targets_for(&refs, &f.targets);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].0, "a");
    }

    #[test]
    fn do_targets_preserve_reaction_order() {
        let src = "FROM alpine AS one\nTASK echo 1\nFROM alpine AS two\nTASK echo 2\nON push(main) DO two\nON release DO one\n";
        let f = parse(src).unwrap();
        let refs: Vec<&crate::dsl::ast::Reaction> = vec![&f.reactions[0], &f.reactions[1]];
        let runs = do_targets_for(&refs, &f.targets);
        let names: Vec<&str> = runs.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["two", "one"]);
    }

    #[test]
    fn unmatched_event_is_a_noop() {
        // An event matching no reaction returns Ok(vec![]) - never a registry
        // touch, never a hard error: basil handle is fed every webhook event.
        let dir = std::env::temp_dir().join(format!("basil-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Basilfile");
        std::fs::write(
            &file,
            "FROM alpine AS ghcr.io/x/app\nON push(main) BUILD ghcr.io/x/app\n",
        )
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(handle_event(BuildInput {
            file: file.to_str().unwrap(),
            context: std::path::Path::new(&dir),
            event_json: Some(r#"{"ref": "refs/heads/other"}"#),
            target_filter: None,
            force: false,
            cache_to: None,
        }));
        match res {
            Ok(pairs) => assert!(pairs.is_empty(), "{pairs:?}"),
            Err(e) => panic!("unmatched event must be a no-op, got: {e:#}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The ordered DO executions a set of matched reactions implies: every
/// DO <name> in reaction order, mapped to its target. A DO naming an
/// unknown target, or a target without a TASK clause, is a no-op (skipped),
/// matching the original DO-only path's semantics. The target's TASK runs on
/// its resolved base image; see run_do_tasks.
fn do_targets_for<'a>(
    matched: &[&'a crate::dsl::ast::Reaction],
    targets: &'a [crate::dsl::ast::Target],
) -> Vec<(&'a str, &'a crate::dsl::ast::Target)> {
    let mut out = Vec::new();
    for r in matched {
        for do_name in &r.do_list {
            if let Some(t) = targets.iter().find(|t| &t.name == do_name) {
                if t.task.is_some() {
                    out.push((do_name.as_str(), t));
                }
            }
        }
    }
    out
}

/// Run every DO task a set of matched reactions implies, one-shot, in
/// reaction order, each on its target's resolved base image with
/// BASIL_EVENT set. A non-zero task exit fails the invocation. Called on
/// the DO-only path and after the deliverable build / idempotent-skip paths,
/// so a reaction pair like BUILD app DO deploy runs the deploy after the
/// artifact is published either way.
async fn run_do_tasks(
    matched: &[&crate::dsl::ast::Reaction],
    targets: &[crate::dsl::ast::Target],
    event_ref: &str,
) -> anyhow::Result<()> {
    for (do_name, t) in do_targets_for(matched, targets) {
        let Some(task) = &t.task else {
            continue; // do_targets_for filters to task-bearing targets; no panic path
        };
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
    Ok(())
}

/// Build using the buildah backend (plain subprocess, no daemon), driven by
/// a delivered event: match reactions, build/promote the deliverables.
pub async fn handle_event(inp: BuildInput<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let src = crate::fsctx::fs_err(std::fs::read_to_string(inp.file), inp.file, "read")?;
    println!("basil: parsed {}", inp.file);
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;
    parser::validate_reactions(&parsed).map_err(anyhow::Error::msg)?;

    let event_payload: Json = match inp.event_json {
        Some(j) => {
            serde_json::from_str(j).map_err(|e| anyhow::anyhow!("invalid event JSON: {e}"))?
        }
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
            let entry = t
                .map(|t| (t.name.clone(), t.push_ref().to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "ON reaction BUILD \"{b}\" matches no target - reactions are validated at parse; refusing to proceed"
                    )
                })?;
            if !deliverables.contains(&entry) {
                deliverables.push(entry);
            }
        }
    }
    let deliverable_names: Vec<String> = deliverables.iter().map(|(n, _)| n.clone()).collect();
    // An event that matches no reaction is a no-op: basil handle is fed
    // every event from a webhook/CI forwarder, so an unrelated push must not
    // fail the job - there is simply nothing for it to do.
    if matched.is_empty() {
        println!("basil: event matched no reactions -- nothing to do");
        return Ok(vec![]);
    }
    // DO-only reactions: run the tasks one-shot, in reaction order. When the
    // event also matches BUILD reactions, the DOs run *after* the build (see
    // below) so a task like deploy sees what was just built.
    if deliverable_names.is_empty() {
        run_do_tasks(&matched, &parsed.targets, &event_ref).await?;
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
    println!(
        "basil: resolving {} base(s) from registries...",
        parsed.targets.len()
    );
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
            .map_err(|msg| crate::fsctx::with_module(anyhow_tracing::anyhow!("{msg}"), &key))?;
        resolved.extend(module_resolved);
    }
    println!(
        "basil: resolved {} base(s) in {:.1}s",
        resolved.len(),
        t0.elapsed().as_secs_f64()
    );
    let t1 = std::time::Instant::now();
    println!(
        "basil: computing identity ({} targets)...",
        parsed.targets.len()
    );
    let mut resolver = RegistryResolver { resolved };
    // Module deliverable digests first (their own bases resolved above), so
    // the importer closure folds `base.module.*` from them.
    let module_digests = crate::identity::modules::compute_module_digests(&index, &mut resolver)
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
            format!(
                "{}:{}",
                e.push_ref,
                crate::solver::registry::content_tag(&tag)
            ),
        );
    }
    // Checkpoint identities (`COMMIT`) — the closure of each sealed prefix.
    let checkpoint_closures = crate::identity::closure::compute_checkpoint_closures(
        &parsed.targets,
        &closures,
        &parsed.default_platforms,
        &parsed.global_args,
        inp.context,
        &build_file,
        &build_args,
        &mut resolver,
        &module_digests.deliverable,
    )
    .map_err(anyhow::Error::msg)?;
    println!(
        "basil: identity computed in {:.1}s",
        t1.elapsed().as_secs_f64()
    );

    // Every non-deliverable stage in scope gets a cache image
    // (`<repo>:basil-cache-<stageHash>`). The cache repo is `--cache-to`, else
    // `BASIL_CACHE_REF`, else the first deliverable's repo (D1).
    let cache_to = effective_cache_to(inp.cache_to, &deliverables);
    let stage_cache = stage_cache_refs(&parsed.targets, &deliverables, &closures, &cache_to);
    // `COMMIT` checkpoints seal the same namespace: each prefix gets a
    // `basil-cache-<prefixHash>` ref derived from its checkpoint closure.
    let checkpoint_refs = checkpoint_refs(&checkpoint_closures);

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
            let cache_g = crate::solver::buildah::Graph::new(
                &parsed.targets,
                &parsed.global_args,
                &parsed.default_platforms,
                file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            );
            ensure_stages_cached(cache_g, inp.context, &build_file, &deliverables).await?;
            let g = crate::solver::buildah::Graph::new(
                &parsed.targets,
                &parsed.global_args,
                &parsed.default_platforms,
                file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            );
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
            promote_on_tag(
                &matched,
                &event_payload,
                &event_ref,
                &closures,
                &parsed.targets,
            )
            .await?;
            run_do_tasks(&matched, &parsed.targets, &event_ref).await?;
            println!("basil: done ({})", deliverable_names.join(", "));
            return Ok(vec![
                ("result".to_string(), "idempotent-skip".to_string()),
                (
                    "digest".to_string(),
                    digest_of(&deliverable_names[0], &closures)?,
                ),
            ]);
        }
    }

    // Build path: every module dependency must exist as its content-tagged
    // image before the importer renders FROM it — probe; build + push the
    // module when missing (recursive). `--force` never propagates: modules are
    // still reconciled by existence, only the importer is force-built.
    ensure_modules_built(&index, &module_refs, &module_digests, inp.cache_to).await?;
    // Every intermediate stage must exist as its cache image first (D2).
    let cache_g = crate::solver::buildah::Graph::new(
        &parsed.targets,
        &parsed.global_args,
        &parsed.default_platforms,
        file_dir,
        &module_refs,
        &stage_cache,
        &checkpoint_refs,
    );
    ensure_stages_cached(cache_g, inp.context, &build_file, &deliverables).await?;

    // Build each deliverable with buildah.
    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            crate::solver::buildah::Graph::new(
                &parsed.targets,
                &parsed.global_args,
                &parsed.default_platforms,
                file_dir,
                &module_refs,
                &stage_cache,
                &checkpoint_refs,
            ),
            name,
            push_ref,
            &tag,
            inp.context,
            &build_file,
            &build_args,
            insecure,
            inp.force,
        )
        .await?;
    }

    promote_on_tag(
        &matched,
        &event_payload,
        &event_ref,
        &closures,
        &parsed.targets,
    )
    .await?;
    run_do_tasks(&matched, &parsed.targets, &event_ref).await?;
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
        let full_tag = from_ref
            .strip_prefix(&format!("{}:", e.push_ref))
            .unwrap_or(from_ref);
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
        // `COMMIT` checkpoints inside the module: their refs use the module's
        // cache repo like its stage caches.
        let module_checkpoint_refs = checkpoint_refs(&module_closures.checkpoints[key]);
        let g_for_cache = crate::solver::buildah::Graph::new(
            &e.parsed.targets,
            &e.parsed.global_args,
            &e.parsed.default_platforms,
            &e.context,
            module_refs,
            &stage_cache,
            &module_checkpoint_refs,
        );
        ensure_stages_cached(g_for_cache, &e.context, &e.file, &deliv).await?;
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
            &e.parsed.default_platforms,
            &e.context,
            module_refs,
            &stage_cache,
            &module_checkpoint_refs,
        );
        crate::solver::buildah::build_deliverable(
            g,
            &e.deliverable,
            &e.push_ref,
            hash,
            &e.context,
            &e.file,
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
fn effective_cache_to(flag: Option<&str>, deliverables: &[(String, String)]) -> String {
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

/// (stage, commit seq) -> local checkpoint ref for every `COMMIT` point, from
/// the checkpoint closures derived for the in-scope set. `basil-checkpoint-*`
/// is a local-only namespace (the buildah layer cache): these images are
/// never pushed to any registry — unlike stage caches — so no repo prefix is
/// attached and `basil gc` (registry-only) never sees them.
fn checkpoint_refs(
    checkpoints: &std::collections::BTreeMap<(String, usize), [u8; 32]>,
) -> std::collections::BTreeMap<(String, usize), String> {
    let mut out = std::collections::BTreeMap::new();
    for ((stage, seq), h) in checkpoints {
        out.insert(
            (stage.clone(), *seq),
            format!("basil-checkpoint-{}", crate::identity::closure::hex(h)),
        );
    }
    out
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
        let Some(hash) = closures.get(&t.name) else {
            continue;
        };
        let hex = crate::identity::closure::hex(hash);
        let ref_str = if t.register.is_some() {
            // A REGISTERed stage is a published deliverable: dependents use
            // its content-tagged artifact (`<register>:basil-<hash>`), never
            // an intermediate cache image. Same rule for build and `run` — the
            // only difference between the two is that deliverables are not run
            // targets.
            format!(
                "{}:{}",
                t.push_ref(),
                crate::solver::registry::content_tag(&hex)
            )
        } else {
            format!("{cache_to}:{}", crate::solver::registry::cache_tag(&hex))
        };
        out.insert(t.name.clone(), ref_str);
    }
    out
}

/// Ensure every intermediate stage exists as its cache image in the registry:
/// probe `<cache-to>:basil-cache-<stageHash>`; on miss, build `--target
/// <stage>`, push it, and remove the local image. REGISTERed stages in scope
/// are never cache-reconciled: they resolve to their published
/// `<register>:basil-<hash>` artifact and are use-or-build'd as deliverables
/// (mirroring module deps). Iterates in subset order
/// (dependencies first). `--force` never propagates here — stages are
/// reconciled by existence, always (D2). A stage containing `IMPORT` is built
/// imperatively (committed straight to its cache ref) instead of via
/// `--target`.
async fn ensure_stages_cached(
    g: crate::solver::buildah::Graph<'_>,
    context: &std::path::Path,
    build_file: &std::path::Path,
    deliverables: &[(String, String)],
) -> anyhow::Result<()> {
    let deliv: std::collections::BTreeSet<&str> =
        deliverables.iter().map(|(name, _)| name.as_str()).collect();
    for t in g.targets {
        if deliv.contains(t.name.as_str()) {
            continue;
        }
        let Some(ref_str) = g.stage_cache.get(&t.name) else {
            continue;
        };
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
        // A REGISTERed stage is a published deliverable, never an intermediate:
        // build it as the deliverable (use-or-build, mirroring module deps) so
        // dependents pull the published content-tagged artifact instead of a
        // basil-cache image.
        if t.register.is_some() {
            let hash = tag
                .strip_prefix(crate::solver::registry::BASIL_TAG_PREFIX)
                .unwrap_or(&tag);
            let insecure = ir.is_local();
            println!("basil: stage {} -> building deliverable {ref_str}", t.name);
            crate::solver::buildah::build_deliverable(
                g,
                &t.name,
                t.push_ref(),
                hash,
                context,
                build_file,
                &std::collections::HashMap::new(),
                insecure,
                false,
            )
            .await?;
            continue;
        }
        // Every stage is built per-instruction (imperative), committed
        // straight to its cache ref, then pushed + dropped locally — the
        // reconcile-by-existence contract.
        println!("basil: stage {} -> building cache {ref_str}", t.name);
        crate::solver::imperative::build_stage_imperative(
            g,
            &t.name,
            context,
            build_file,
            ref_str,
            &HashMap::new(),
        )
        .await?;
        let insecure = ir.is_local();
        crate::solver::buildah::push_tagged(&t.name, &repo, &tag, &ref_str, insecure).await?;
        crate::solver::buildah::remove_local_image(ref_str).await?;
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

#[cfg(test)]
mod is_changed_tests {
    use super::*;

    fn tmp_ctx(tag: &str, content: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("basil-ischg-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Basilfile");
        std::fs::write(&file, content).unwrap();
        (dir, file.to_str().unwrap().to_string())
    }

    #[test]
    fn is_changed_errors_on_unknown_target() {
        let (dir, file) = tmp_ctx(
            "unknown",
            "FROM scratch AS app
REGISTER 127.0.0.1:5001/x
",
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(is_changed(&file, &dir, &["nope".to_string()]));
        let e = res.unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
        assert!(e.to_string().contains("nope"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_changed_errors_on_non_deliverable() {
        let (dir, file) = tmp_ctx(
            "nd",
            "FROM scratch AS app\nFROM scratch AS 127.0.0.1:5001/ok\n",
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(is_changed(&file, &dir, &["app".to_string()]));
        let e = res.unwrap_err();
        assert!(e.to_string().contains("not a deliverable"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
