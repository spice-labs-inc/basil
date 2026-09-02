//! Differential conformance: build the same inputs through buildkit's
//! `dockerfile.v0` and through basil's emitter, then assert the resulting image
//! config is equal (the design's "equal image config + layer digests"
//! correctness surface).
//!
//! Gated on a reachable buildkitd + registry + a remote buildx builder. The
//! fixtures are `tests/differential/Dockerfile` (basil DSL) and
//! `tests/differential/reference/Dockerfile` (plain Dockerfile). Layer digests:
//! the base image layer is asserted byte-identical; multi-stage copy layer
//! granularity is a KNOWN limitation (see note).

use serde_json::Value;

fn buildkitd_ok() -> bool {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:8818".parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

/// Fetch a manifest (resolving index → linux/amd64 child) from the registry.
fn fetch_manifest(repo: &str, tag: &str) -> Value {
    let mt = "application/vnd.oci.image.manifest.v1+json";
    let idx = reqwest_get(&format!("http://127.0.0.1:5001/v2/{repo}/manifests/{tag}"),
        "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json");
    // If it's an index, take the linux/amd64 child.
    if let Some(manifests) = idx.get("manifests").and_then(Value::as_array) {
        let child = manifests
            .iter()
            .find(|m| {
                m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64"
            })
            .expect("no linux/amd64 child in index");
        let d = child["digest"].as_str().unwrap();
        return reqwest_get(&format!("http://127.0.0.1:5001/v2/{repo}/manifests/{d}"), mt);
    }
    idx
}

fn reqwest_get(url: &str, accept: &str) -> Value {
    let body = std::process::Command::new("curl")
        .args(["-s", url, "-H", &format!("Accept: {accept}")])
        .output()
        .expect("curl")
        .stdout;
    serde_json::from_slice(&body).expect("json")
}

fn fetch_config(repo: &str, config_digest: &str) -> Value {
    reqwest_get(
        &format!("http://127.0.0.1:5001/v2/{repo}/blobs/{config_digest}"),
        "application/vnd.oci.image.config.v1+json, application/octet-stream",
    )
}

#[tokio::test]
async fn config_matches_dockerfile_v0() {
    if !buildkitd_ok() {
        eprintln!("skipping: buildkitd not reachable");
        return;
    }

    // 1. Build basil's deliverable.
    let channel = tonic::transport::Endpoint::from_shared("tcp://127.0.0.1:8818")
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = buildkit_client::client::BuildKitClient::connect("tcp://127.0.0.1:8818")
        .await
        .unwrap();
    let mut session = buildkit_client::session::Session::new();
    session.add_file_sync(std::path::PathBuf::from("tests/differential")).await;
    session.start(client.control().clone()).await.unwrap();
    let sid = session.get_id();
    let inp = basil::solver::orchestrate::BuildInput {
        file: "tests/differential/Dockerfile",
        context: std::path::Path::new("tests/differential"),
        event_json: Some(r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#),
        target_filter: None,
    };
    basil::solver::orchestrate::run(channel, &sid, inp).await.unwrap();

    // 2. Build the dockerfile.v0 reference via buildx (if available).
    let status = std::process::Command::new("docker")
        .args([
            "buildx", "build", "--builder", "basil-bx",
            "-f", "tests/differential/reference/Dockerfile",
            "-t", "127.0.0.1:5001/diffref:latest", "--push",
            "tests/differential",
        ])
        .status()
        .expect("docker buildx");
    assert!(status.success(), "dockerfile.v0 reference build failed");

    // 3. Compare configs.
    let b_tag = reqwest_get(
        "http://127.0.0.1:5001/v2/diff/tags/list",
        "application/json",
    )["tags"][0]
        .as_str()
        .unwrap()
        .to_string();
    let b_man = fetch_manifest("diff", &b_tag);
    let r_man = fetch_manifest("diffref", "latest");
    let b_cfg = fetch_config("diff", b_man["config"]["digest"].as_str().unwrap());
    let r_cfg = fetch_config("diffref", r_man["config"]["digest"].as_str().unwrap());

    let b = &b_cfg["config"];
    let r = &r_cfg["config"];

    // The shared base layer must be byte-identical (first layer = alpine).
    assert_eq!(
        b_man["layers"][0]["digest"].as_str().unwrap(),
        r_man["layers"][0]["digest"].as_str().unwrap(),
        "base image layer digest should match dockerfile.v0"
    );

    // Config fields must match (dockerfile.v0 sets ArgsEscaped; basil does not
    // — exclude it).
    for field in ["Cmd", "Entrypoint", "User", "WorkingDir", "ExposedPorts", "Volumes"] {
        assert_eq!(b[field], r[field], "config field `{field}` differs");
    }
    // Env: basil adds PATH default; compare sorted.
    let mut be: Vec<&str> = b["Env"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    let mut re: Vec<&str> = r["Env"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    be.sort();
    re.sort();
    assert_eq!(be, re, "Env differs");
}