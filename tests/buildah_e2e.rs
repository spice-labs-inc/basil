//! Gated end-to-end test for the buildah backend: builds a deliverable via
//! `buildah build`/`push` and verifies the pushed image's config + content
//! digest match basil's closure tagHash. Skipped when `buildah` is unavailable
//! or the local registry is not reachable.

use basil::solver::orchestrate::{BuildInput, handle_event};
use serde_json::Value;

fn buildah_present() -> bool {
    std::process::Command::new("buildah")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

async fn registry_up() -> bool {
    // The registry answers /v2/ on any healthy deployment (200, or 401 when
    // auth is on) — the probe must not require an existing repo, which a fresh
    // throwaway registry has none of.
    reqwest::Client::new()
        .head("http://127.0.0.1:5001/v2/")
        .send()
        .await
        .is_ok_and(|r| r.status().is_success() || r.status().as_u16() == 401)
}

/// The fixture: a scratch deliverable carrying a context file + config.
const BASILFILE: &str = "FROM scratch AS 127.0.0.1:5001/bah_e2e_probe\n\
COPY src.txt /out.txt\n\
ENV HARNESS=e2e\n\
CMD [\"/bin/cat\",\"/out.txt\"]\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_probe\n";

#[tokio::test]
async fn buildah_e2e_build_and_config() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let dir = tempfile();
    std::fs::write(dir.join("basil.Basilfile"), BASILFILE).unwrap();
    std::fs::write(dir.join("src.txt"), "hello buildah\n").unwrap();

    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let file_path = dir.join("basil.Basilfile");
    let file_str = file_path.to_str().unwrap();
    let inp = BuildInput {
        file: file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
    };
    let results = handle_event(inp.clone()).await.unwrap();
    let digest = results
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();

    // The pushed image has the closure tagHash as its tag.
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let (_, manifest) = c.get_manifest("bah_e2e_probe", &digest).await.unwrap();
    let m: Value = serde_json::from_slice(&manifest).unwrap();
    let cfg_digest = m["config"]["digest"].as_str().unwrap().to_string();
    // Fetch the config blob directly (async).
    let cfg_url = format!("http://127.0.0.1:5001/v2/bah_e2e_probe/blobs/{cfg_digest}");
    let resp = reqwest::Client::new().get(&cfg_url).send().await.unwrap();
    let cfg: Value = resp.json().await.unwrap();
    let cfg = &cfg["config"];
    assert_eq!(cfg["Cmd"], serde_json::json!(["/bin/cat", "/out.txt"]));
    assert!(
        cfg["Env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap() == "HARNESS=e2e"),
        "HARNESS env missing: {:?}",
        cfg["Env"]
    );
    assert!(digest.len() == 64, "bad digest {digest}");

    // A second delivery of the same event noops: the content tag exists, so
    // basil re-confirms the image is registered and reports an idempotent skip
    // instead of rebuilding. The registered image must still be pullable.
    let results2 = handle_event(inp.clone()).await.unwrap();
    let (_, result) = results2
        .iter()
        .find(|(k, _)| k == "result")
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    assert_eq!(result, "idempotent-skip", "second delivery must noop");
    let digest2 = results2
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(digest2, digest, "noop re-derives the same content tag");
    let (_, manifest2) = c.get_manifest("bah_e2e_probe", &digest2).await.unwrap();
    assert!(!manifest2.is_empty(), "registered image must remain pullable");

    // --force bypasses the idempotent check: the same event rebuilds and
    // re-pushes even though the content tag is registered (no idempotent-skip
    // result is emitted — the build path has no `result` key at all).
    let mut forced = inp;
    forced.force = true;
    let results3 = handle_event(forced).await.unwrap();
    let result3 = results3.iter().find(|(k, _)| k == "result");
    assert!(result3.is_none(), "--force must take the build path, got {result3:?}");
    let digest3 = results3
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(digest3, digest, "forced rebuild re-derives the same content tag");
    let (_, manifest3) = c.get_manifest("bah_e2e_probe", &digest3).await.unwrap();
    assert!(!manifest3.is_empty(), "re-pushed image must be pullable");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Minimal temp dir helper.
fn tempfile() -> std::path::PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("bah_e2e_{n}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}