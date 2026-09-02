//! Orchestrate the `basil build` flow: parse → match event → emit → bridge-solve.

use crate::dsl::parser;
use crate::emitter::{self, ImageConfig};
use crate::event;
use crate::identity::closure::{BaseResolver, compute_closure, hex};
use crate::solver::bridge::{Bridge, BridgeBuildResult};
use crate::solver::config;
use buildkit_client::proto::moby::buildkit::v1::Exporter;
use serde_json::Value as Json;
use std::collections::HashMap;

/// Resolver used by the CLI until the registry client lands (Phase 8): scratch
/// and pinned bases resolve offline; unpinned external bases are an error.
struct CliResolver;
impl BaseResolver for CliResolver {
    fn resolve(&mut self, raw: &str, _tag: Option<&str>, digest: Option<&str>) -> Result<String, String> {
        if raw == "scratch" {
            return Ok("scratch-empty".to_string());
        }
        digest.map(|d| d.to_string()).ok_or_else(|| {
            format!("unpinned base `{raw}`: pin it with @sha256:... (registry resolution not wired yet)")
        })
    }
}

/// Inputs to a build.
pub struct BuildInput<'a> {
    pub file: &'a str,
    pub context: &'a std::path::Path,
    pub event_json: Option<&'a str>,
    pub target_filter: Option<&'a String>,
}

/// Run a build end-to-end and return the exporter digest.
pub async fn run(
    channel: tonic::transport::Channel,
    session_id: &str,
    inp: BuildInput<'_>,
) -> anyhow::Result<Vec<(String, String)>> {
    let src = std::fs::read_to_string(inp.file)
        .map_err(|e| anyhow::anyhow!("reading {}: {}", inp.file, e))?;
    let parsed = parser::parse(&src).map_err(anyhow::Error::msg)?;

    // Event: match reactions.
    let event_payload: Json = match inp.event_json {
        Some(j) => serde_json::from_str(j).map_err(|e| anyhow::anyhow!("invalid event JSON: {e}"))?,
        None => {
            // Manual trigger: select by target filter, or error.
            return Err(anyhow::anyhow!(
                "build requires an event (stdin JSON) or a manual --target"
            ));
        }
    };
    let matched = event::matching_reactions(&parsed, &event_payload);

    // The event ref (branch or tag) — needed for `tag`-triggered promotion.
    let event_ref = event_payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    // Collect deliverable targets to BUILD.
    let mut deliverable_names: Vec<String> = Vec::new();
    for r in &matched {
        for b in &r.build {
            if !deliverable_names.contains(b) {
                deliverable_names.push(b.clone());
            }
        }
    }
    if matched.is_empty() {
        return Err(anyhow::anyhow!(
            "no matching reactions for the delivered event"
        ));
    }
    // A DO-only reaction (no BUILD) runs its tasks without a solve.
    if deliverable_names.is_empty() {
        let _ = run_do_tasks(
            &channel,
            session_id,
            &parsed,
            &matched,
            inp.event_json.unwrap_or(""),
        )
        .await?;
        return Ok(vec![]);
    }

    // Closures (for identity; the tagHash is the content tag we export under).
    let overrides = HashMap::new();
    let closures =
        compute_closure(&parsed.targets, &parsed.default_platforms, inp.context, &overrides, &mut CliResolver)
            .map_err(anyhow::Error::msg)?;

    // Level 1: if every deliverable's content tag already exists in its
    // registry, the exact image is already published — zero solve. (The
    // dependent pull-if-needed happens when a consumer resolves by tagHash.)
    let l1_skip = {
        let mut all_exist = true;
        for name in &deliverable_names {
            let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
            let (host, rest) = name.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("deliverable `{name}` is not a qualified image reference")
            })?;
            let repo = rest.rsplit_once(':').map(|(r, _)| r).unwrap_or(rest);
            let client = crate::solver::registry::RegistryClient::new(
                &crate::solver::registry::ImageRef {
                    host: host.to_string(),
                    repo: repo.to_string(),
                    tag: tag.clone(),
                }
                .base_url(),
            );
            match client.manifest_exists(repo, &tag).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!("Level-1: `{name}:{tag}` does not exist yet (will build)");
                    all_exist = false;
                    break;
                }
                Err(e) => {
                    // Registry check failures are non-fatal; fall through to a
                    // full solve (the image exporter re-checks/pushes).
                    tracing::warn!("Level-1 registry check for `{name}:{tag}` failed: {e}");
                    all_exist = false;
                    break;
                }
            }
        }
        all_exist
    };
    if l1_skip {
        tracing::info!(
            "all deliverable content tags already exist; skipping solve (Level 1)"
        );
        let do_results = run_do_tasks(
            &channel,
            session_id,
            &parsed,
            &matched,
            inp.event_json.unwrap_or(""),
        )
        .await?;
        let _ = do_results;
        // Promotion runs on a tag event even when the solve was skipped (the
        // version tag is a retag of the already-existing content tag).
        promote_on_tag(&matched, &event_ref, &closures).await?;
        return Ok(vec![
            ("level".to_string(), "1-skip".to_string()),
            (
                "digest".to_string(),
                deliverable_names
                    .first()
                    .map(|n| closures.get(n).map(|t| hex(t)).unwrap_or_default())
                    .unwrap_or_default(),
            ),
        ]);
    }

    // Emit the graph for all targets (deps included), and produce per-deliverable
    // config + exporters. buildkit's `image` exporter pushes `<name>:<tagHash>`.
    let mut exporters: Vec<Exporter> = Vec::new();
    let mut result_refs: HashMap<String, String> = HashMap::new();

    let mut em = emitter::emit_file(&parsed.targets, "context", session_id, None).map_err(anyhow::Error::msg)?;
    // The graph is shared; solve once with N exporters (one per deliverable).
    let mut config_by_output = HashMap::new();
    for e in &em.targets {
        if deliverable_names.contains(&e.target) {
            let tag_str = closures.get(&e.target).map(|t| hex(t)).unwrap_or_else(|| "unknown".to_string());
            result_refs.insert(e.target.clone(), tag_str.clone());
            exporters.push(Exporter {
                r#type: "image".to_string(),
                attrs: {
                    let mut a = HashMap::new();
                    let name = format!("{}:{}", e.target, tag_str);
                    a.insert("name".to_string(), name.clone());
                    a.insert("push".to_string(), "true".to_string());
                    // Local/plain registries are HTTP; flag them insecure.
                    let host = e.target.split('/').next().unwrap_or("");
                    if host.starts_with("127.0.0.1")
                        || host == "localhost"
                        || (!host.contains('.') && !host.contains(':'))
                    {
                        a.insert("registry.insecure".to_string(), "true".to_string());
                    }
                    let _ = name;
                    a
                },
            });
            // The emitter returns one config per target output; the deliverable
            // rootfs output must map to its config. For a single-target solve,
            // the last emitted output is the deliverable.
            config_by_output.insert(e.output.vertex, e.config.clone());
        }
    }

    // The bridge Solve takes ONE Definition; we emit the whole file graph. The
    // last emitted output (the last stage) is the graph's root result ref.
    let root_output = em.targets.last().map(|t| t.output).ok_or_else(|| anyhow::anyhow!("no targets to build"))?;
    let graph = std::mem::replace(&mut em.graph, emitter::llb::Graph::new());
    let definition = graph.finish(root_output);

    // Attach config metadata for the root (last) output.
    let last_vertex = em
        .targets
        .last()
        .map(|t| t.output.vertex)
        .unwrap_or(0);
    let root_config = config_by_output
        .get(&last_vertex)
        .cloned()
        .unwrap_or_else(ImageConfig::default);
    let cfg_meta = config::render_bytes(&root_config, "amd64", "linux");

    let mut metadata = HashMap::new();
    metadata.insert("containerimage.config".to_string(), cfg_meta);

    let buildid = format!("basil-{}", uuid_like());
    let mut bridge = Bridge::new(&channel, buildid.clone());

    // Cache refs (Level 2/3): export under the deliverable repo with the exact
    // closure tag and the build-date tags; import the same plus the current
    // period, so a rebuild reattaches cache and accumulates across changes.
    let cache = build_cache_options(&deliverable_names, &closures);

    let result: BridgeBuildResult = bridge
        .build(
            definition,
            metadata,
            session_id,
            &buildid,
            exporters,
            cache,
        )
        .await?;

    // DO/TASK side effects run because the event fired, after BUILD.
    let _ = run_do_tasks(
        &channel,
        session_id,
        &parsed,
        &matched,
        inp.event_json.unwrap_or(""),
    )
    .await?;

    // Promotion: a `tag` event adds `<name>:v<version>` (immutable retag).
    promote_on_tag(&matched, &event_ref, &closures).await?;

    Ok(result
        .digest
        .map(|d| vec![("digest".to_string(), d)])
        .unwrap_or_default())
}

/// Run the `DO` tasks for the matched reactions, after the build solve. Each
/// task target is solved standalone to a ref, then its `TASK` command runs in a
/// one-shot container on that rootfs.
pub async fn run_do_tasks(
    channel: &tonic::transport::Channel,
    session_id: &str,
    parsed: &crate::dsl::ast::File,
    matched: &[&crate::dsl::ast::Reaction],
    event_json: &str,
) -> anyhow::Result<()> {
    let buildid = format!("basil-do-{}", uuid_like());

    // Open a frontend-less outer solve for the task forwarder: it registers a
    // bridge for `buildid` and stays open while we solve task graphs and run
    // their containers. No image exporters (tasks produce no image); we close
    // it with a Return at the end.
    let mut outer = buildkit_client::proto::moby::buildkit::v1::control_client::ControlClient::new(
        channel.clone(),
    );
    let outer_req = buildkit_client::proto::moby::buildkit::v1::SolveRequest {
        r#ref: buildid.clone(),
        definition: None,
        session: session_id.to_string(),
        frontend: String::new(),
        frontend_attrs: HashMap::new(),
        exporters: vec![],
        ..Default::default()
    };
    let outer_handle = tokio::spawn(async move {
        outer.solve(tonic::Request::new(outer_req)).await
    });
    // Let the outer solve register its bridge for `buildid` before we solve tasks.
    tokio::task::yield_now().await;

    let mut bridge = Bridge::new(channel, buildid.clone());

    for r in matched {
        for do_name in &r.do_list {
            let Some(target) = parsed.targets.iter().find(|t| &t.name == do_name) else {
                tracing::warn!("DO target `{do_name}` not found");
                continue;
            };
            let Some(task) = &target.task else {
                tracing::warn!("DO target `{do_name}` has no TASK clause");
                continue;
            };

            // Emit the task target's rootfs (its base image / steps), then append
            // a one-shot exec running the TASK command. We solve it with the task
            // exec ignore-cached so it re-runs each event — the task is a side
            // effect, never a cached layer, and never an exported image.
            let mut graph = emitter::llb::Graph::new();
            let mut stage_outputs = HashMap::new();
            for t in &parsed.targets {
                let e = emitter::emit_target(
                    &mut graph,
                    t,
                    &stage_outputs,
                    "context",
                    session_id,
                    None,
                )
                .map_err(anyhow::Error::msg)?;
                stage_outputs.insert(t.name.clone(), e.output);
                if &t.name == do_name {
                    break;
                }
            }
            let root = stage_outputs
                .get(do_name)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("task target `{do_name}` has no output"))?;

            // Run the task command on the task rootfs as a cache-ignored exec.
            let args = match &task.command {
                crate::dsl::ast::Command::Shell(s) => vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    s.clone(),
                ],
                crate::dsl::ast::Command::Exec(a) => a.clone(),
            };
            let env_arr = vec![format!("BASIL_EVENT={}", event_json)];
            let exec_out = emitter::llb::run(
                &mut graph,
                Some(root),
                args,
                env_arr,
                "/".to_string(),
                String::new(),
                None,
                vec![],
            );
            graph.mark_ignore_cache(exec_out.vertex);
            let def = graph.finish(exec_out);
            // Solve the task exec. A non-zero exit fails the solve -> the task
            // failed; success (ref returned) means exit 0.
            let _ref_id = bridge.solve_ref(def).await.map_err(|e| {
                anyhow::anyhow!("DO task `{do_name}` failed (non-zero exit or solve error): {e}")
            })?;
            tracing::info!("DO task `{do_name}` completed (exit 0)");
        }
    }

    // Close the task solve: Return an empty result so the outer solve completes.
    use std::str::FromStr;
    let mut close = crate::gateway_proto::moby::buildkit::v1::frontend::llb_bridge_client::LlbBridgeClient::new(
        channel.clone(),
    );
    let ret = crate::gateway_proto::moby::buildkit::v1::frontend::ReturnRequest {
        result: None,
        error: None,
    };
    let mut req = tonic::Request::new(ret);
    req.metadata_mut().insert(
        "buildkit-controlapi-buildid",
        tonic::metadata::MetadataValue::from_str(&buildid).expect("ascii buildid"),
    );
    let _ = close.r#return(req).await;
    let _ = outer_handle.await;
    Ok(())
}

/// Build the cache export/import refs for the deliverable repos.
fn build_cache_options(
    deliverable_names: &[String],
    closures: &HashMap<String, [u8; 32]>,
) -> Option<buildkit_client::proto::moby::buildkit::v1::CacheOptions> {
    use buildkit_client::proto::moby::buildkit::v1::{CacheOptions, CacheOptionsEntry};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let date = date_tags(now);

    let mut exports = Vec::new();
    let mut imports = Vec::new();
    for name in deliverable_names {
        let tag = closures.get(name).map(|t| hex(t)).unwrap_or_default();
        let repo_only = name.rsplit_once(':').map(|(r, _)| r).unwrap_or(name);
        // Local/plain registries are HTTP; the image exporter works there but
        // buildkit's registry cache exporter to an explicit HTTP registry needs
        // auth/scope that a dev registry may not grant. Keep cache refs only
        // for HTTPS/qualified registries (the real cross-build reuse path).
        let host = repo_only.split('/').next().unwrap_or("");
        let insecure_local = host.starts_with("127.0.0.1")
            || host == "localhost"
            || (!host.contains('.') && !host.contains(':'));
        if insecure_local {
            continue;
        }
        // Multiple refs per repo: exact closure + date tags (each a separate
        // registry cache exporter entry).
        for t in [&tag, &date.0, &date.1, &date.2] {
            if t.is_empty() {
                continue;
            }
            exports.push(CacheOptionsEntry {
                r#type: "registry".to_string(),
                attrs: {
                    let mut a = HashMap::new();
                    a.insert("ref".to_string(), format!("{repo_only}:cache-{t}"));
                    a.insert("mode".to_string(), "max".to_string());
                    a
                },
            });
        }
        // Import the exact + current day/month/year tags (best-effort).
        for t in [&tag, &date.0, &date.1, &date.2] {
            imports.push(CacheOptionsEntry {
                r#type: "registry".to_string(),
                attrs: {
                    let mut a = HashMap::new();
                    a.insert("ref".to_string(), format!("{repo_only}:cache-{t}"));
                    a
                },
            });
        }
    }
    Some(CacheOptions {
        exports,
        imports,
        ..Default::default()
    })
}

/// Promote deliverables on a `tag` event: after the build, tag each BUILD
/// target as `<name>:v<version>` (registry retag, immutable).
async fn promote_on_tag(
    matched: &[&crate::dsl::ast::Reaction],
    event_ref: &str,
    closures: &HashMap<String, [u8; 32]>,
) -> anyhow::Result<()> {
    let Some((qual_target, version)) = crate::solver::promote::extract_version(event_ref) else {
        return Ok(()); // not a version tag event
    };
    tracing::info!("promotion: tag `{event_ref}` -> version v{version}");

    // Deliverable names to promote: the BUILD list of the matching reactions,
    // restricted to the qualified target when the tag names one.
    let mut promoted: Vec<String> = Vec::new();
    for r in matched.iter().copied() {
        if r.event.token != crate::dsl::ast::EventToken::Tag {
            continue;
        }
        for b in &r.build {
            if let Some(qt) = &qual_target {
                // `target-vX.Y.Z` promotes only target `q`.
                if b.as_str() == qt.as_str() || b.split('/').last() == Some(qt.as_str()) {
                    promoted.push(b.clone());
                }
            } else {
                promoted.push(b.clone());
            }
        }
    }
    promoted.sort();
    promoted.dedup();

    for name in promoted {
        let tag_hash = closures.get(&name).map(|t| crate::identity::closure::hex(t)).unwrap_or_default();
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

/// Compute the `YYYYMMDD`, `YYYYMM`, `YYYY` date tags from a unix timestamp.
fn date_tags(secs: u64) -> (String, String, String) {
    format_date(secs)
}

/// UTC calendar decomposition (days since epoch) without chrono.
fn format_date(secs: u64) -> (String, String, String) {
    // Days since 1970-01-01.
    let days = secs / 86400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        format!("{year}{:02}{:02}", m, d),
        format!("{year}{:02}", m),
        format!("{year}"),
    )
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", n)
}