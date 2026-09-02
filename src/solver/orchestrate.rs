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

    // Collect deliverable targets to BUILD.
    let mut deliverable_names: Vec<String> = Vec::new();
    for r in &matched {
        for b in &r.build {
            if !deliverable_names.contains(b) {
                deliverable_names.push(b.clone());
            }
        }
    }
    if deliverable_names.is_empty() {
        return Err(anyhow::anyhow!(
            "no matching BUILD targets for the delivered event"
        ));
    }

    // Closures (for identity; the tagHash is the content tag we export under).
    let overrides = HashMap::new();
    let closures =
        compute_closure(&parsed.targets, &parsed.default_platforms, inp.context, &overrides, &mut CliResolver)
            .map_err(anyhow::Error::msg)?;

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
                    a.insert("name".to_string(), format!("{}:{}", e.target, tag_str));
                    a.insert("push".to_string(), "true".to_string());
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
    let result: BridgeBuildResult = bridge
        .build(definition, metadata, session_id, &buildid, exporters)
        .await?;

    Ok(result
        .digest
        .map(|d| vec![("digest".to_string(), d)])
        .unwrap_or_default())
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", n)
}