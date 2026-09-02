//! Compile the buildkit LLBBridge (gateway) proto into basil using tonic-build
//! 0.12, matching buildkit-client's tonic version so the generated
//! `LlbBridgeClient` accepts buildkit-client's `Channel`. The proto tree is
//! vendored under `proto/` (fetched from the buildkit-client build OUTPUT_DIR;
//! see `proto/README` for provenance). See docs/DESIGN.md — basil attaches
//! image config by calling this service as a frontend-less gateway bridge.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/github.com/moby/buildkit/frontend/gateway/pb/gateway.proto");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    tonic_build::configure()
        .build_server(false) // client only; basil is the frontend caller
        .build_client(true)
        .out_dir(&out_dir)
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::prost_types")
        .compile_protos(
            &["proto/github.com/moby/buildkit/frontend/gateway/pb/gateway.proto"],
            &["proto"],
        )?;
    Ok(())
}