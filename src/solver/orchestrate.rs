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
        let b = &t.base;
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
pub struct BuildInput<'a> {
    pub file: &'a str,
    pub context: &'a std::path::Path,
    pub event_json: Option<&'a str>,
    pub target_filter: Option<&'a String>,
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
        let src = format!("{}:{tag_hash}", ir.repo);
        match client.retag(&ir.repo, &tag_hash, &format!("v{version}")).await {
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
/// Build one or more deliverable targets directly (no event). This path must
/// never execute TASK clauses: tasks are event-triggered (DO reactions in
/// `handle_event`), so a plain build of an image has no side-effects. Here we
/// only compute the closure and run `build_deliverable`.
pub async fn build_targets(inp: BuildInput<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let src = crate::fsctx::fs_err(std::fs::read_to_string(inp.file), inp.file, "read")?;
    println!("basil: parsed {}", inp.file);
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
    // `-t fennel` and `-t 127.0.0.1:5001/fennel` build the same deliverable.
    let deliverables: Vec<(String, String)> = match &inp.target_filter {
        Some(key) => {
            let t = parsed
                .targets
                .iter()
                .find(|x| x.name.as_str() == key.as_str() || x.register.as_deref() == Some(key.as_str()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "target `{key}` not found in {} (known: {})",
                        inp.file,
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

    // Resolve unpinned external bases (network: auth token + manifest GETs).
    println!("basil: resolving base(s) from registries...");
    let t0 = std::time::Instant::now();
    let resolved = resolve_unpinned_bases(&subset)
        .await
        .map_err(anyhow::Error::msg)?;
    println!("basil: resolved {} base(s) in {:.1}s", resolved.len(), t0.elapsed().as_secs_f64());
    let t0 = std::time::Instant::now();
    println!("basil: computing identity ({} target(s))...", subset.len());
    let build_file = std::path::absolute(inp.file).unwrap_or_else(|_| inp.file.into());
    let mut resolver = RegistryResolver { resolved };
    let closures = compute_closure(
        &subset,
        &parsed.default_platforms,
        inp.context,
        &build_file,
        &HashMap::new(),
        &mut resolver,
    )
    .map_err(anyhow::Error::msg)?;
    println!("basil: identity computed in {:.1}s", t0.elapsed().as_secs_f64());
    for name in &names {
        println!(
            "basil: identity {name} -> {}",
            digest_of(name, &closures)?
        );
    }

    let mut out: Vec<(String, String)> = Vec::new();

    // If the content tag already exists in the registry, the exact image is
    // already published — don't rebuild (parity with handle_event's idempotent
    // check).
    let idempotent_skip = idempotent_all_exist(&deliverables, &closures).await?;
    if idempotent_skip {
        println!("basil: content tags already published -- no build (idempotent check)");
        for (name, push_ref) in &deliverables {
            // OUTPUT still runs: pull the published image and extract.
            let tag = digest_of(name, &closures)?;
            crate::solver::buildah::extract_outputs_from_image(
                &format!("{push_ref}:{tag}"),
                inp.context,
                &subset,
                name,
            )
            .await?;
            out.push((name.clone(), tag.clone()));
        }
        println!("basil: done ({})", names.join(", "));
        return Ok(out);
    }

    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            &subset,
            name,
            push_ref,
            &tag,
            inp.context,
            &HashMap::new(),
            insecure,
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
        let crate::dsl::ast::BaseImage { raw, .. } = &t.base;
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
                        let image = resolved_image(&t.base);
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
    let resolved = resolve_unpinned_bases(&parsed.targets)
        .await
        .map_err(anyhow::Error::msg)?;
    println!("basil: resolved {} base(s) in {:.1}s", resolved.len(), t0.elapsed().as_secs_f64());
    let t1 = std::time::Instant::now();
    println!("basil: computing identity ({} targets)...", parsed.targets.len());
    let build_file = std::path::absolute(inp.file).unwrap_or_else(|_| inp.file.into());
    let mut resolver = RegistryResolver { resolved };
    let closures = compute_closure(
        &parsed.targets,
        &parsed.default_platforms,
        inp.context,
        &build_file,
        &build_args,
        &mut resolver,
    )
    .map_err(anyhow::Error::msg)?;
    println!("basil: identity computed in {:.1}s", t1.elapsed().as_secs_f64());

    // Idempotent check: skip when every deliverable's content tag exists.
    let idempotent_skip = idempotent_all_exist(&deliverables, &closures).await?;
    if idempotent_skip {
        println!("basil: content tags already published -- no build (idempotent check)");
        // OUTPUT still runs on a skip: pull the published image and extract.
        for (name, push_ref) in &deliverables {
            let tag = digest_of(name, &closures)?;
            crate::solver::buildah::extract_outputs_from_image(
                &format!("{push_ref}:{tag}"),
                inp.context,
                &parsed.targets,
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

    // Build each deliverable with buildah.
    for (name, push_ref) in &deliverables {
        let tag = digest_of(name, &closures)?;
        let insecure = crate::solver::registry::parse_ref(push_ref, &tag)
            .map(|ir| ir.is_local())
            .unwrap_or(false);
        crate::solver::buildah::build_deliverable(
            &parsed.targets,
            name,
            push_ref,
            &tag,
            inp.context,
            &build_args,
            insecure,
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

/// Idempotent check: every deliverable's content tag already exists in its
/// registry. `deliverables` are `(stage name, push ref)`; the check looks up
/// the push ref (the registry where content is published).
async fn idempotent_all_exist(
    deliverables: &[(String, String)],
    closures: &std::collections::HashMap<String, [u8; 32]>,
) -> anyhow::Result<bool> {
    let mut all = true;
    for (name, push_ref) in deliverables {
        let tag = digest_of(name, closures)?;
        let ir = crate::solver::registry::parse_ref(push_ref, &tag)
            .map_err(|e| anyhow::anyhow!("deliverable `{push_ref}`: {e}"))?;
        let client = ir.client();
        match client.manifest_exists(&ir.repo, &tag).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!("idempotent: `{push_ref}:{tag}` does not exist yet (will build)");
                all = false;
                break;
            }
            Err(e) => {
                tracing::warn!("idempotent registry check for `{push_ref}:{tag}` failed: {e}");
                all = false;
                break;
            }
        }
    }
    Ok(all)
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