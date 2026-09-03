//! Orchestrate the `basil build` flow: parse → match event → identity → buildah build.

use crate::dsl::parser;
use crate::event;
use crate::identity::closure::{BaseResolver, compute_closure, hex};
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
        let host = ir.host.clone();
        let repo = ir.repo.clone();
        let dedup_key = format!("{host}/{repo}:{tag}");
        if !seen.insert(dedup_key) {
            continue;
        }
        let client =
            crate::solver::registry::RegistryClient::new(&crate::solver::registry::ImageRef {
                host: host.clone(),
                repo: repo.clone(),
                tag: tag.clone(),
            }
            .base_url());
        let raw = &b.raw;
        match client.resolve_digest(&repo, &tag).await {
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
                    "could not resolve unpinned base `{raw}` ({host}/{repo}:{tag}): {e}"
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
        let tag_hash = closures.get(name).map(|t| crate::identity::closure::hex(t)).unwrap_or_default();
        let Some((host, rest)) = name.split_once('/') else {
            tracing::warn!("promote: `{name}` not a qualified reference");
            continue;
        };
        let repo = rest.rsplit_once(':').map(|(r, _)| r).unwrap_or(rest);
        let base = crate::solver::registry::ImageRef {
            host: host.to_string(),
            repo: repo.to_string(),
            tag: String::new(),
        }
        .base_url();
        let client = crate::solver::registry::RegistryClient::new(&base);
        let src = format!("{repo}:{tag_hash}");
        match client.retag(repo, &tag_hash, &format!("v{version}")).await {
            Ok(d) => tracing::info!("promote `{name}:{src}` -> v{version} (digest {})", &d[..d.len().min(24)]),
            Err(e) => {
                anyhow::bail!("promotion immutability/retag failure for `{name}:v{version}`: {e}")
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
    let src = std::fs::read_to_string(inp.file)
        .map_err(|e| anyhow::anyhow!("reading {}: {}", inp.file, e))?;
    println!("basil: parsed {}", inp.file);
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;

    // Qualified deliverable targets are those named as image refs (host/repo).
    let qualified: Vec<&str> = parsed
        .targets
        .iter()
        .filter(|t| t.name.split('/').count() >= 2)
        .map(|t| t.name.as_str())
        .collect();
    let names: Vec<String> = match inp.target_filter {
        Some(t) => vec![t.clone()],
        None => qualified.into_iter().map(|s| s.to_string()).collect(),
    };
    if names.is_empty() {
        return Err(anyhow::anyhow!(
            "no deliverable target to build: pass -t <target> or name a deliverable as a qualified image reference"
        ));
    }

    // Resolve unpinned external bases (network: auth token + manifest GETs).
    println!("basil: resolving {} base(s) from registries...", parsed.targets.len());
    let t0 = std::time::Instant::now();
    let resolved = resolve_unpinned_bases(&parsed.targets)
        .await
        .map_err(anyhow::Error::msg)?;
    println!("basil: resolved {} base(s) in {:.1}s", resolved.len(), t0.elapsed().as_secs_f64());
    let t0 = std::time::Instant::now();
    println!("basil: computing identity ({} targets)...", parsed.targets.len());
    let mut resolver = RegistryResolver { resolved };
    let closures = compute_closure(
        &parsed.targets,
        &parsed.default_platforms,
        inp.context,
        &HashMap::new(),
        &mut resolver,
    )
    .map_err(anyhow::Error::msg)?;
    println!("basil: identity computed in {:.1}s", t0.elapsed().as_secs_f64());
    for name in &names {
        println!(
            "basil: identity {name} -> {}",
            closures.get(name).map(|t| hex(t)).unwrap_or_default()
        );
    }

    let mut out: Vec<(String, String)> = Vec::new();

    // If the content tag already exists in the registry, the exact image is
    // already published — don't rebuild (parity with handle_event's idempotent
    // check).
    let idempotent_skip = idempotent_all_exist(&names, &closures).await?;
    if idempotent_skip {
        println!("basil: content tags already published -- no build (idempotent check)");
        for name in &names {
            // OUTPUT still runs: pull the published image and extract.
            let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
            crate::solver::buildah::extract_outputs_from_image(
                &format!("{name}:{tag}"),
                inp.context,
                &parsed.targets,
                name,
            )
            .await?;
            out.push((name.clone(), tag.clone()));
        }
        println!("basil: done ({})", names.join(", "));
        return Ok(out);
    }

    for name in &names {
        let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
        let insecure = is_local_registry(name);
        crate::solver::buildah::build_deliverable(
            &parsed.targets,
            name,
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

/// Build using the buildah backend (plain subprocess, no daemon), driven by
/// a delivered event: match reactions, build/promote the deliverables.
pub async fn handle_event(inp: BuildInput<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let src = std::fs::read_to_string(inp.file)
        .map_err(|e| anyhow::anyhow!("reading {}: {}", inp.file, e))?;
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

    let mut deliverable_names: Vec<String> = Vec::new();
    for r in &matched {
        for b in &r.build {
            if !deliverable_names.contains(b) {
                deliverable_names.push(b.clone());
            }
        }
    }
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
    let mut resolver = RegistryResolver { resolved };
    let closures = compute_closure(
        &parsed.targets,
        &parsed.default_platforms,
        inp.context,
        &build_args,
        &mut resolver,
    )
    .map_err(anyhow::Error::msg)?;
    println!("basil: identity computed in {:.1}s", t1.elapsed().as_secs_f64());

    // Idempotent check: skip when every deliverable's content tag exists.
    let idempotent_skip = idempotent_all_exist(&deliverable_names, &closures).await?;
    if idempotent_skip {
        println!("basil: content tags already published -- no build (idempotent check)");
        // OUTPUT still runs on a skip: pull the published image and extract.
        for name in &deliverable_names {
            let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
            crate::solver::buildah::extract_outputs_from_image(
                &format!("{name}:{tag}"),
                inp.context,
                &parsed.targets,
                name,
            )
            .await?;
        }
        promote_on_tag(&matched, &event_payload, &event_ref, &closures).await?;
        println!("basil: done ({})", deliverable_names.join(", "));
        return Ok(vec![
            ("result".to_string(), "idempotent-skip".to_string()),
            (
                "digest".to_string(),
                deliverable_names
                    .first()
                    .map(|n| closures.get(n).map(|t| hex(t)).unwrap_or_default())
                    .unwrap_or_default(),
            ),
        ]);
    }

    // Build each deliverable with buildah.
    for name in &deliverable_names {
        let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
        let insecure = is_local_registry(name);
        crate::solver::buildah::build_deliverable(
            &parsed.targets,
            name,
            &tag,
            inp.context,
            &build_args,
            insecure,
        )
        .await?;
    }

    promote_on_tag(&matched, &event_payload, &event_ref, &closures).await?;
    println!("basil: done ({})", deliverable_names.join(", "));
    Ok(vec![(
        "digest".to_string(),
        closures.get(&deliverable_names[0]).map(|t| hex(t)).unwrap_or_default(),
    )])
}

/// Idempotent check: every deliverable's content tag already exists in its
/// registry.
async fn idempotent_all_exist(
    names: &[String],
    closures: &std::collections::HashMap<String, [u8; 32]>,
) -> anyhow::Result<bool> {
    let mut all = true;
    for name in names {
        let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
        let Some((host, rest)) = name.split_once('/') else {
            anyhow::bail!("deliverable `{name}` is not a qualified image reference");
        };
        let repo = rest.rsplit_once(':').map(|(r, _)| r).unwrap_or(rest);
        let client = crate::solver::registry::RegistryClient::new(&crate::solver::registry::ImageRef {
            host: host.to_string(),
            repo: repo.to_string(),
            tag: tag.clone(),
        }
        .base_url());
        match client.manifest_exists(repo, &tag).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!("idempotent: `{name}:{tag}` does not exist yet (will build)");
                all = false;
                break;
            }
            Err(e) => {
                tracing::warn!("idempotent registry check for `{name}:{tag}` failed: {e}");
                all = false;
                break;
            }
        }
    }
    Ok(all)
}

fn is_local_registry(name: &str) -> bool {
    let host = name.split('/').next().unwrap_or("");
    host.starts_with("127.0.0.1")
        || host == "localhost"
        || (!host.contains('.') && !host.contains(':'))
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