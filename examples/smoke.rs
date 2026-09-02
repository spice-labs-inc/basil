//! Phase 0 smoke test: prove the emit→solve seam.
//!
//! Emits a raw LLB `pb::Definition` (a `SourceOp` for a base image feeding an
//! `ExecOp` that writes a file into its `/out` output), then solves it against a
//! live buildkitd using our own `SolveRequest` with `definition: Some(graph)`
//! and a `local` exporter. This validates that basil can own the graph and
//! drive a daemon it does not itself run (external mode).
//!
//! Run with a reachable buildkitd, e.g.:
//!
//! ```text
//! docker run -d --name basil-smoke-buildkitd --privileged \
//!   -p 127.0.0.1:8818:8818 moby/buildkit:buildx-stable-1 \
//!   --addr tcp://0.0.0.0:8818
//! cargo run --example smoke -- --addr tcp://127.0.0.1:8818
//! ```
//! The exporter writes to `<daemon-workdir>/out` on the buildkitd host; for the
//! docker case that is inside the container, inspect with `docker exec`.

use buildkit_client::client::BuildKitClient;
use buildkit_client::proto::moby::buildkit::v1::{
    CacheOptions, Exporter, SolveRequest,
};
use buildkit_client::proto::pb;
use clap::Parser;
use prost::Message;
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(about = "basil Phase 0 smoke test")]
struct Args {
    /// buildkitd gRPC address, e.g. tcp://127.0.0.1:8818
    #[arg(long, default_value = "tcp://127.0.0.1:8818")]
    addr: String,
    /// Path (on the buildkitd host) for the `local` exporter output.
    #[arg(long, default_value = "/out")]
    output: String,
}

/// Digest of a marshaled Op — the LLB vertex digest used as an input edge and
/// as the `Definition.metadata` key.
fn op_digest(op: &pb::Op) -> String {
    let bytes = op.encode_to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Build the LLB graph for `FROM alpine:3` + `RUN echo 'basil smoke ok' >
/// /result.txt`: a `docker-image` SourceOp feeding an ExecOp with the rootfs
/// mounted as the exec output. Exported with the `cacheonly` exporter, which
/// solves the graph without needing image-config finalization, so this proves
/// the emit→solve seam directly.
fn build_smoke_definition() -> pb::Definition {
    let source = pb::Op {
        inputs: vec![],
        platform: None,
        constraints: None,
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: "docker-image://docker.io/library/alpine:3".to_string(),
            attrs: Default::default(),
        })),
    };
    let source_digest = op_digest(&source);

    let root_mount = pb::Mount {
        input: 0,
        output: 0,
        dest: "/".to_string(),
        readonly: false,
        ..Default::default()
    };

    let exec = pb::Op {
        inputs: vec![pb::Input {
            digest: source_digest.clone(),
            index: 0,
        }],
        platform: None,
        constraints: None,
        op: Some(pb::op::Op::Exec(pb::ExecOp {
            meta: Some(pb::Meta {
                args: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo 'basil smoke ok' > /result.txt".to_string(),
                ],
                cwd: "/".to_string(),
                ..Default::default()
            }),
            mounts: vec![root_mount],
            ..Default::default()
        })),
    };
    let exec_digest = op_digest(&exec);

    let mut def = pb::Definition::default();
    def.def.push(source.encode_to_vec());
    def.def.push(exec.encode_to_vec());
    def.metadata.insert(source_digest, pb::OpMetadata::default());
    def.metadata.insert(exec_digest, pb::OpMetadata::default());
    def
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();
    let args = Args::parse();

    let graph = build_smoke_definition();
    tracing::info!(
        "emitted smoke graph: {} ops, {} metadata entries",
        graph.def.len(),
        graph.metadata.len()
    );

    let mut client = BuildKitClient::connect(&args.addr).await?;
    client.health_check().await?;
    tracing::info!("connected + health check passed at {}", args.addr);

    // An active session is required for the solve to reference. This graph has
    // no context/filesync inputs, but buildkit still needs the session stream
    // (the `local` exporter and generic solve path attach to it).
    let mut session = buildkit_client::session::Session::new();
    session
        .start(client.control().clone())
        .await
        .map_err(|e| anyhow::anyhow!("failed to start session: {}", e))?;
    tracing::info!("session started: {}", session.get_id());

    // Assemble our own SolveRequest with a raw LLB definition and a `tar`
    // exporter writing daemon-side — proves the solve runs end-to-end
    // (the stock build() hardcodes dockerfile.v0 and a single push exporter).
    let mut export_attrs = std::collections::HashMap::new();
    export_attrs.insert("output".to_string(), args.output.clone());
    let request = SolveRequest {
        r#ref: "smoke-1".to_string(),
        definition: Some(graph),
        session: session.get_id(),
        frontend: String::new(),
        frontend_attrs: Default::default(),
        cache: Some(CacheOptions::default()),
        entitlements: vec![],
        frontend_inputs: Default::default(),
        internal: false,
        source_policy: None,
        exporters: vec![Exporter {
            r#type: "tar".to_string(),
            attrs: export_attrs,
        }],
        enable_session_exporter: false,
        ..Default::default()
    };

    // Attach session identity headers so buildkit associates the solve with the
    // active session (same as the stock build()).
    let mut grpc_request = tonic::Request::new(request);
    {
        let metadata = grpc_request.metadata_mut();
        for (key, values) in session.metadata() {
            if let Ok(k) = key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>() {
                for value in values {
                    if let Ok(v) =
                        value.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
                    {
                        metadata.append(k.clone(), v);
                    }
                }
            }
        }
    }

    let response = client.control().solve(grpc_request).await?;
    let response = response.into_inner();
    tracing::info!(
        "solve completed; exporter response: {:?}",
        response.exporter_response
    );

    Ok(())
}