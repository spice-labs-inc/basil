//! The frontend-less gateway bridge: how basil owns the graph AND delivers
//! image config.
//!
//! buildkit only reads `containerimage.config` from a solve-result's metadata,
//! which a frontend attaches. Basil therefore drives a frontend-less solve and
//! speaks the `LLBBridge` service as the bridge client:
//!
//! 1. Submit a control `Solve` with `frontend: ""`, `definition: None`, and
//!    the build `ref` (buildid). buildkitd registers a forwarder for that id
//!    and serves `LLBBridge`.
//! 2. Call `LLBBridge.Solve(Definition=<basil's graph>)` → a `Ref`.
//! 3. Call `LLBBridge.Return(Result{ Ref, metadata: {"containerimage.config":
//!    <config json>} })`. The exporter reads that config.
//!
//! See docs/DESIGN.md; protocol cross-checked against moby/buildkit
//! (solver/llbsolver/solver.go, control/gateway/gateway.go, client/build.go).

//! #![allow(missing_docs)]
use crate::gateway_proto::moby::buildkit::v1::frontend::llb_bridge_client::LlbBridgeClient;
use crate::gateway_proto::moby::buildkit::v1::frontend as gw;
use buildkit_client::proto::moby::buildkit::v1::{
    control_client::ControlClient, Exporter, SolveRequest,
};
use std::collections::HashMap;
use std::str::FromStr;

/// The gRPC metadata key basil sets on every LLBBridge call to identify the
/// build (buildkit `client/buildid`).
const BUILDID_KEY: &str = "buildkit-controlapi-buildid";

/// Result of a bridge build: the exporter digest + response.
pub struct BridgeBuildResult {
    pub digest: Option<String>,
}

/// A `tonic::Channel`-based bridge client for one build.
pub struct Bridge<'a> {
    channel: &'a tonic::transport::Channel,
    buildid: String,
}

impl<'a> Bridge<'a> {
    pub fn new(channel: &'a tonic::transport::Channel, buildid: String) -> Self {
        Bridge { channel, buildid }
    }

    /// Solve a definition and return the resulting ref id. Used to get a
    /// task target's rootfs ref for `DO`/`TASK` execution.
    pub async fn solve_ref(
        &mut self,
        definition: buildkit_client::proto::pb::Definition,
    ) -> anyhow::Result<String> {
        let mut bridge = LlbBridgeClient::new(self.channel.clone());
        let solve_req = gw::SolveRequest {
            definition: Some(convert_definition(definition)),
            frontend: String::new(),
            frontend_opt: HashMap::new(),
            allow_result_return: true,
            allow_result_array_ref: true,
            r#final: false,
            exporter_attr: Vec::new(),
            cache_imports: vec![],
            frontend_inputs: HashMap::new(),
            // Force materialization so a non-zero task exec surfaces as a solve
            // error (lazy results would otherwise mask the exit).
            evaluate: true,
            source_policies: vec![],
        };
        let resp = bridge
            .solve(with_buildid(tonic::Request::new(solve_req), &self.buildid))
            .await
            .map_err(|e| anyhow::anyhow!("LLBBridge.Solve (task) failed: {e}"))?;
        let sr = resp.into_inner();
        let result = sr
            .result
            .ok_or_else(|| anyhow::anyhow!("task solve returned no result"))?;
        match result.result {
            Some(gw::result::Result::Ref(rf)) => Ok(rf.id),
            Some(gw::result::Result::Refs(rf)) => {
                Ok(rf.refs.into_values().next().map(|r| r.id).unwrap_or_default())
            }
            _ => Err(anyhow::anyhow!("task solve result has no Ref")),
        }
    }

    /// Drive the outer frontend-less solve: register the bridge on buildkitd,
    /// solve basil's graph, attach the config, and return the outer result.
    pub async fn build(
        &mut self,
        definition: buildkit_client::proto::pb::Definition,
        config_metadata: HashMap<String, Vec<u8>>,
        session_id: &str,
        build_ref: &str,
        exporters: Vec<Exporter>,
        cache: Option<buildkit_client::proto::moby::buildkit::v1::CacheOptions>,
    ) -> anyhow::Result<BridgeBuildResult> {
        // Outer solve: frontend-less, no definition. Blocks until Return.
        let outer_channel = self.channel.clone();
        let mut outer = ControlClient::new(outer_channel);
        let outer_req = SolveRequest {
            r#ref: build_ref.to_string(),
            definition: None,
            session: session_id.to_string(),
            frontend: String::new(),
            frontend_attrs: HashMap::new(),
            exporters,
            cache,
            ..Default::default()
        };
        let outer_handle = tokio::spawn(async move {
            outer.solve(tonic::Request::new(outer_req)).await
        });

        // Bridge client on the same channel, with the buildid header.
        let mut bridge = LlbBridgeClient::new(self.channel.clone());

        // The bridge's SolveRequest uses the gateway proto's own Definition
        // (a distinct prost type, wire-identical to buildkit-client's). Convert.
        let definition = convert_definition(definition);

        // Solve the actual graph via the bridge.
        let solve_req = gw::SolveRequest {
            definition: Some(definition),
            frontend: String::new(),
            frontend_opt: HashMap::new(),
            allow_result_return: true,
            allow_result_array_ref: true,
            r#final: false,
            exporter_attr: Vec::new(),
            cache_imports: vec![],
            frontend_inputs: HashMap::new(),
            evaluate: false,
            source_policies: vec![],
        };
        let resp = bridge
            .solve(with_buildid(tonic::Request::new(solve_req), &self.buildid))
            .await
            .map_err(|e| anyhow::anyhow!("LLBBridge.Solve failed: {e}"))?;
        let solve_resp = resp.into_inner();

        // The bridge solve returns a `Result` (AllowResultReturn) with a Ref.
        let solved = solve_resp
            .result
            .ok_or_else(|| anyhow::anyhow!("bridge solve returned no result"))?;
        let ref_id = solved
            .result
            .as_ref()
            .and_then(|r| match r {
                gw::result::Result::Ref(rf) => Some(rf.id.clone()),
                gw::result::Result::Refs(rf) => rf.refs.values().next().map(|r| r.id.clone()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("bridge result has no Ref"))?;

        // Rebuild the Result with the config metadata and return it to buildkit.
        let final_result = gw::Result {
            metadata: config_metadata,
            attestations: HashMap::new(),
            result: Some(gw::result::Result::Ref(gw::Ref {
                id: ref_id,
                def: None,
            })),
        };
        let ret = gw::ReturnRequest {
            result: Some(final_result),
            error: None,
        };
        bridge
            .r#return(with_buildid(tonic::Request::new(ret), &self.buildid))
            .await
            .map_err(|e| anyhow::anyhow!("LLBBridge.Return failed: {e}"))?;

        // Wait for the outer solve to complete and surface its result.
        let outer_resp = outer_handle
            .await
            .map_err(|e| anyhow::anyhow!("outer solve task failed: {e}"))??;
        let inner = outer_resp.into_inner();
        let digest = inner
            .exporter_response
            .get("containerimage.digest")
            .cloned();
        Ok(BridgeBuildResult { digest })
    }
}

fn with_buildid<T>(mut req: tonic::Request<T>, id: &str) -> tonic::Request<T> {
    req.metadata_mut()
        .insert(BUILDID_KEY, tonic::metadata::MetadataValue::from_str(id).expect("ascii buildid"));
    req
}

/// Convert a buildkit-client Definition (the emitter's type) to the gateway
/// proto's own Definition (a distinct prost type, wire-identical).
fn convert_definition(
    definition: buildkit_client::proto::pb::Definition,
) -> crate::gateway_proto::pb::Definition {
    crate::gateway_proto::pb::Definition {
        def: definition.def,
        metadata: definition
            .metadata
            .into_iter()
            .map(|(k, v)| {
                let md = crate::gateway_proto::pb::OpMetadata {
                    ignore_cache: v.ignore_cache,
                    description: v.description,
                    ..Default::default()
                };
                (k, md)
            })
            .collect(),
        source: None,
    }
}