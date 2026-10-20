//! Gated end-to-end test for the buildah backend: builds a deliverable via
//! `buildah build`/`push` and verifies the pushed image's config + content
//! digest match basil's closure tagHash. Skipped when `buildah` is unavailable
//! or the local registry is not reachable.

use basil::solver::orchestrate::{build_targets, handle_event, BuildInput};
use serde_json::Value;

/// All buildah-backed tests share one global containers/storage, and the OUTPUT
/// test asserts exact local image/container counts. Serialize them so a
/// concurrent test's own image creation cannot perturb those counts.
static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _guard = E2E_LOCK.lock().await;
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
        cache_to: None,
    };
    let results = handle_event(inp.clone()).await.unwrap();
    let digest = results
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();

    // The pushed image is tagged `basil-<tagHash>`; the `digest` result key is
    // the bare closure hash.
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let ctag = |h: &str| format!("basil-{h}");
    let (_, manifest) = c
        .get_manifest("bah_e2e_probe", &ctag(&digest))
        .await
        .unwrap();
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
    let (_, manifest2) = c
        .get_manifest("bah_e2e_probe", &ctag(&digest2))
        .await
        .unwrap();
    assert!(
        !manifest2.is_empty(),
        "registered image must remain pullable"
    );

    // --force bypasses the idempotent check: the same event rebuilds and
    // re-pushes even though the content tag is registered (no idempotent-skip
    // result is emitted — the build path has no `result` key at all).
    let mut forced = inp.clone();
    forced.force = true;
    let results3 = handle_event(forced).await.unwrap();
    let result3 = results3.iter().find(|(k, _)| k == "result");
    assert!(
        result3.is_none(),
        "--force must take the build path, got {result3:?}"
    );
    let digest3 = results3
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(
        digest3, digest,
        "forced rebuild re-derives the same content tag"
    );
    let (_, manifest3) = c
        .get_manifest("bah_e2e_probe", &ctag(&digest3))
        .await
        .unwrap();
    assert!(!manifest3.is_empty(), "re-pushed image must be pullable");

    // Idempotence invariant: if the image does not exist it must be built (and
    // pushed). Delete the registered content tag and re-run — the next delivery
    // must take the build path, restoring the artifact, not report a skip.
    c.delete_tag("bah_e2e_probe", &ctag(&digest)).await.unwrap();
    let results4 = handle_event(inp.clone()).await.unwrap();
    let result4 = results4.iter().find(|(k, _)| k == "result");
    assert!(
        result4.is_none(),
        "deleted artifact must rebuild, got {result4:?}"
    );
    let (_, manifest4) = c
        .get_manifest("bah_e2e_probe", &ctag(&digest))
        .await
        .unwrap();
    assert!(!manifest4.is_empty(), "deleted artifact must be re-pushed");

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

/// Local image count (`buildah images -q`) and working-container count
/// (`buildah containers -a -q`). Used to prove the OUTPUT path leaves nothing
/// behind in containers/storage.
async fn local_image_count() -> usize {
    let out = tokio::process::Command::new("buildah")
        .args(["images", "-q"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

async fn local_container_count() -> usize {
    let out = tokio::process::Command::new("buildah")
        .args(["containers", "-a", "-q"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Count locally untagged (`<none>`) images. The OUTPUT temp images are
/// untagged, so any leak shows up here as a permanent growth in untagged
/// images regardless of what was already in storage.
async fn local_untagged_image_count() -> usize {
    local_image_names()
        .await
        .into_iter()
        .filter(|n| n.starts_with("<none>") || n.is_empty())
        .count()
}

/// All locally untagged (`<none>`) image IDs — to identify what leaked.
async fn untagged_image_names() -> Vec<String> {
    let out = tokio::process::Command::new("buildah")
        .args(["images", "--format", "{{.ID}} {{.Name}}"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("<none>"))
        .map(|l| l.to_string())
        .collect()
}

/// All local image names (`buildah images --format {{.Name}}`), for asserting
/// no temp tag was minted.
async fn local_image_names() -> Vec<String> {
    let out = tokio::process::Command::new("buildah")
        .args(["images", "--format", "{{.Name}}"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

/// OUTPUT on two *intermediate* (non-deliverable) stages: each needs its own
/// locally-built image to extract from. The stages' `--target` builds must not
/// mint a tag (an image name namespace the registry never sees, where two
/// stages would collide) and must not leave the image or the extraction
/// container behind — both runs (build and idempotent skip) are asserted
/// leak-free.
const OUTPUT_STAGES_BASILFILE: &str = "FROM alpine AS one\n\
RUN echo ONE > /one.txt\n\
OUTPUT /one.txt out/one.txt\n\
\n\
FROM alpine AS two\n\
RUN echo TWO > /two.txt\n\
OUTPUT /two.txt out/two.txt\n\
\n\
FROM scratch AS 127.0.0.1:5001/bah_e2e_out\n\
COPY --from=one /one.txt /a.txt\n\
COPY --from=two /two.txt /b.txt\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_out\n";

#[tokio::test]
async fn buildah_e2e_output_stages_extract_without_leaking() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), OUTPUT_STAGES_BASILFILE).unwrap();
    let file_path = dir.join("Basilfile");
    let file_str = file_path.to_str().unwrap();
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };

    let images_before = local_image_count().await;
    let untagged_before = local_untagged_image_count().await;
    let containers_before = local_container_count().await;
    let names_before = local_image_names().await;

    handle_event(inp.clone()).await.unwrap();
    // Both stages' files land on the host — neither target build clobbered the
    // other.
    assert_eq!(
        std::fs::read_to_string(dir.join("out/one.txt"))
            .unwrap()
            .trim(),
        "ONE"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/two.txt"))
            .unwrap()
            .trim(),
        "TWO"
    );
    // No leak: the per-stage temp images must be removed again (their absence
    // shows as no growth in untagged images) and nothing may be minted under a
    // temp tag. New images = the deliverable + the cached intermediate stages
    // pulled as its `FROM` bases (warm cache, like a pulled base image).
    let images_after = local_image_count().await;
    assert!(
        images_after <= images_before + 4,
        "at most the deliverable + cached stages may be added (was {images_before}, now {images_after})"
    );
    assert_eq!(
        local_untagged_image_count().await,
        untagged_before,
        "per-stage cache builds must be pushed, not left untagged (was {untagged_before}, now {}); untagged IDs: {:?}",
        local_untagged_image_count().await,
        untagged_image_names().await
    );
    assert_eq!(
        local_container_count().await,
        containers_before,
        "extraction containers must be removed"
    );
    let leaked: Vec<String> = local_image_names()
        .await
        .into_iter()
        .filter(|n| n.starts_with("localhost/basil-output") && !names_before.contains(n))
        .collect();
    assert!(
        leaked.is_empty(),
        "OUTPUT must not mint a temp tag (pre-existing entries are ignored): {leaked:?}"
    );
    let minted: Vec<String> = local_image_names()
        .await
        .into_iter()
        .filter(|n| !names_before.contains(n) && !n.starts_with("basil-cache-"))
        .collect();
    assert!(
        minted.len() <= 1,
        "at most one new non-cache image name (the deliverable): {minted:?}"
    );

    // Same guarantees on the idempotent-skip path, which also extracts (from
    // the cached stage images — pulled, never rebuilt).
    std::fs::remove_dir_all(dir.join("out")).unwrap();
    let untagged_before2 = local_untagged_image_count().await;
    let containers_before2 = local_container_count().await;
    let names_before2 = local_image_names().await;
    let results = handle_event(inp.clone()).await.unwrap();
    assert_eq!(
        results
            .iter()
            .find(|(k, _)| k == "result")
            .map(|(_, v)| v.as_str()),
        Some("idempotent-skip")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/one.txt"))
            .unwrap()
            .trim(),
        "ONE"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/two.txt"))
            .unwrap()
            .trim(),
        "TWO"
    );
    assert_eq!(
        local_untagged_image_count().await,
        untagged_before2,
        "noop extraction must not leave a temp image behind"
    );
    assert_eq!(
        local_container_count().await,
        containers_before2,
        "noop extraction must not leave a container behind"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// IMPORT inlines another image's rootfs at a point in the build. The
/// deliverable is built imperatively (buildah from/config/copy/run/add), the
/// import overwrites earlier content (layer-append semantics), a later RUN
/// sees the merged rootfs, and the idempotent skip stays intact.
const IMPORT_BASILFILE: &str = "FROM alpine AS 127.0.0.1:5001/bah_e2e_import\n\
COPY src.txt /out.txt\n\
COPY pre.txt /imported.txt\n\
IMPORT 127.0.0.1:5001/importsrc\n\
RUN sh -c 'cat /out.txt /imported.txt > /merged.txt'\n\
CMD [\"/bin/cat\",\"/merged.txt\"]\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_import\n";

/// Seed the import source image in the local registry: an alpine image
/// carrying `/imported.txt` (the string the e2e assertions look for).
async fn seed_importsrc() -> anyhow::Result<()> {
    let marker = std::env::temp_dir().join(format!("basil-importsrc-{}", std::process::id()));
    std::fs::write(&marker, "from-import\n").unwrap();
    let c = tokio::process::Command::new("buildah")
        .args(["from", "alpine"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("buildah from: {e}"))?;
    let container = String::from_utf8_lossy(&c.stdout).trim().to_string();
    let _ = tokio::process::Command::new("buildah")
        .args([
            "copy",
            &container,
            marker.to_str().unwrap(),
            "/imported.txt",
        ])
        .status()
        .await;
    let _ = tokio::process::Command::new("buildah")
        .args([
            "commit",
            "--format=oci",
            &container,
            "127.0.0.1:5001/importsrc:latest",
        ])
        .status()
        .await;
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &container])
        .status()
        .await;
    let push = tokio::process::Command::new("buildah")
        .args([
            "push",
            "--tls-verify=false",
            "127.0.0.1:5001/importsrc:latest",
            "docker://127.0.0.1:5001/importsrc:latest",
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah push: {e}"))?;
    if !push.status.success() {
        anyhow::bail!(
            "seed push failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        );
    }
    Ok(())
}

#[tokio::test]
async fn buildah_e2e_import_inlines_content() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    seed_importsrc().await.unwrap();
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), IMPORT_BASILFILE).unwrap();
    std::fs::write(dir.join("src.txt"), "hello import\n").unwrap();
    std::fs::write(dir.join("pre.txt"), "pre-import\n").unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: &file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };

    let results = handle_event(inp.clone()).await.unwrap();
    let digest = results
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    let ref_str = format!("127.0.0.1:5001/bah_e2e_import:basil-{digest}");
    // Pull the pushed image and inspect its content: the context COPY, the
    // import (which inlined ON TOP, overwriting the pre-import file), and the
    // RUN that saw the merged rootfs.
    let c = tokio::process::Command::new("buildah")
        .args(["from", "--tls-verify=false", "--pull=true", &ref_str])
        .output()
        .await
        .unwrap();
    assert!(
        c.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&c.stderr)
    );
    let container = String::from_utf8_lossy(&c.stdout).trim().to_string();
    let out = tokio::process::Command::new("buildah")
        .args([
            "run",
            &container,
            "--",
            "/bin/sh",
            "-c",
            "cat /out.txt; echo ---; cat /imported.txt; echo ---; cat /merged.txt",
        ])
        .output()
        .await
        .unwrap();
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &container])
        .status()
        .await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("hello import"),
        "context COPY missing: {text}"
    );
    assert!(
        text.contains("from-import"),
        "IMPORT content missing — the inlined rootfs must be present: {text}"
    );
    assert!(
        !text.contains("pre-import"),
        "pre-import content must be overwritten by the import (layer-append): {text}"
    );
    assert!(
        text.matches("hello import").count() >= 2 || text.contains("hello import\nfrom-import"),
        "RUN must see the merged rootfs: {text}"
    );

    // Idempotent skip: second delivery noops; the content tag stays pullable.
    let results2 = handle_event(inp.clone()).await.unwrap();
    assert_eq!(
        results2
            .iter()
            .find(|(k, _)| k == "result")
            .map(|(_, v)| v.as_str()),
        Some("idempotent-skip")
    );

    // Deleted content tag -> the next delivery must rebuild and re-register
    // (the invariant: if the image does not exist it must be built).
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    c.delete_tag("bah_e2e_import", &format!("basil-{digest}"))
        .await
        .unwrap();
    let results3 = handle_event(inp.clone()).await.unwrap();
    assert!(
        results3.iter().all(|(k, _)| k != "result"),
        "deleted artifact must rebuild (no idempotent-skip), got {results3:?}"
    );
    let (_, manifest) = c
        .get_manifest("bah_e2e_import", &format!("basil-{digest}"))
        .await
        .unwrap();
    assert!(
        !manifest.is_empty(),
        "rebuilt image must be registered again"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An IMPORT in an *intermediate* stage: the stage is built imperatively, its
/// cache image (`basil-cache-<stageHash>`) is pushed to the registry, the
/// deliverable's Containerfile skips the stage and remaps `COPY --from` to
/// the cache ref, and OUTPUT extraction pulls the cache image.
const IMPORT_STAGE_BASILFILE: &str = "FROM alpine AS builder\n\
IMPORT 127.0.0.1:5001/importsrc\n\
RUN sh -c 'echo built > /built.txt'\n\
OUTPUT /imported.txt out/imported.txt\n\
OUTPUT /built.txt out/built.txt\n\
\n\
FROM scratch AS 127.0.0.1:5001/bah_e2e_imp2\n\
COPY --from=builder /imported.txt /a.txt\n\
COPY --from=builder /built.txt /b.txt\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_imp2\n";

#[tokio::test]
async fn buildah_e2e_import_intermediate_stage() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    seed_importsrc().await.unwrap();
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), IMPORT_STAGE_BASILFILE).unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: &file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };

    handle_event(inp.clone()).await.unwrap();
    // OUTPUT from the import stage extracts out of its published cache image
    // (a pull, never a rebuild): the files prove the cache image carries the
    // inlined import content AND the stage's own RUN output.
    assert_eq!(
        std::fs::read_to_string(dir.join("out/imported.txt"))
            .unwrap()
            .trim(),
        "from-import"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/built.txt"))
            .unwrap()
            .trim(),
        "built"
    );

    // The intermediate stage's cache image is published under the basil-cache
    // namespace (at least one: seed_importsrc rebuilds importsrc with a fresh
    // config timestamp every run, so a rerun legitimately mints a new stage
    // hash and a second cache tag).
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let cache_tags: Vec<String> = c
        .list_tags("bah_e2e_imp2")
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.starts_with("basil-cache-"))
        .collect();
    assert!(
        !cache_tags.is_empty(),
        "builder cache image must be published: {cache_tags:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `basil run` runs a target's `TASK` on the resolved base, building every
/// dependent thing the standard way: here an internal `FROM <stage>` base is
/// reconciled as a `basil-cache-<stageHash>` cache image in the cache repo,
/// the task container shows that stage's built content and the BASIL_TARGET
/// env, and only the run target itself is never published.
const RUN_BASILFILE: &str = "FROM alpine AS base\n\
RUN sh -c 'echo base-content > /b.txt'\n\
\n\
FROM base AS runner\n\
TASK sh -c 'grep -q base-content /b.txt && [ \"$BASIL_TARGET\" = runner ] && echo RUN_OK'\n";

#[tokio::test]
async fn buildah_e2e_run_builds_dependencies_and_runs_task() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), RUN_BASILFILE).unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let cache_repo = "127.0.0.1:5001/bah_e2e_run_cache";

    // The internal base stage is built and its cache image published to the
    // cache repo (its /b.txt is visible to the task); the task succeeds.
    let code = basil::solver::orchestrate::run_target(&file_str, &dir, "runner", Some(cache_repo))
        .await
        .unwrap();
    assert_eq!(code, 0, "task must run against the built base stage");
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let cache_tags: Vec<String> = c
        .list_tags("bah_e2e_run_cache")
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.starts_with("basil-cache-"))
        .collect();
    assert!(
        !cache_tags.is_empty(),
        "the dependent stage must be published as a cache image: {cache_tags:?}"
    );
    // The run target itself is never published.
    assert!(
        c.list_tags("runner").await.is_err(),
        "the run target must not be published"
    );

    // A failing task fails the invocation.
    let bad = tempfile();
    std::fs::write(
        bad.join("Basilfile"),
        "FROM alpine AS failing\nTASK sh -c 'exit 7'\n",
    )
    .unwrap();
    let bad_file = bad.join("Basilfile").to_str().unwrap().to_string();
    let err = basil::solver::orchestrate::run_target(&bad_file, &bad, "failing", None)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("exit 7"),
        "task failure must surface: {err:#}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bad);
}

/// A task that declares "TASK --mount=type=bind,source=.,target=/workspace"
/// opts into mounting the build context: the container sees the repository,
/// so the task can read its contents.
#[tokio::test]
async fn buildah_e2e_task_bind_mount_sees_context() {
    if !buildah_present() {
        eprintln!("skipping: buildah unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(dir.join("marker.txt"), "MOUNTMARKER\n").unwrap();
    std::fs::write(
        dir.join("Basilfile"),
        "FROM alpine AS runner\n\
TASK --mount=type=bind,source=.,target=/workspace sh -c 'grep -q MOUNTMARKER /workspace/marker.txt && echo SAW_CONTEXT'\n",
    )
    .unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let code = basil::solver::orchestrate::run_target(&file_str, &dir, "runner", None)
        .await
        .unwrap();
    assert_eq!(code, 0, "the mounted context must be visible to the task");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `COMMIT` checkpoints the build so far into the layer cache: build 1 seals
/// the prefix as a `basil-cache-<prefixHash>` image (same namespace as stage
/// caches); a later build resumes from that checkpoint — the `RUN` before it
/// (which stamps a nanosecond timestamp into the file) is NOT re-executed, so
/// the rebuilt content is byte-identical.
#[tokio::test]
async fn buildah_e2e_commit_resumes_from_checkpoint() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    // The registry outlives test runs; start from a clean repo so the
    // checkpoint-local / stage-cache-remote assertions are exact.
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    if let Ok(tags) = c.list_tags("bah_e2e_commit").await {
        for t in tags {
            let _ = c.delete_tag("bah_e2e_commit", &t).await;
        }
    }
    // Pin the alpine base so the stage closure (the stage cache tag) is
    // computable offline in the test.
    let alpine_digest = basil::solver::registry::parse_ref("alpine", "latest")
        .unwrap()
        .resolve_digest()
        .await
        .unwrap();
    let build_file_txt = format!(
        "FROM alpine@{alpine_digest} AS build\n\
RUN sh -c 'echo step1 > /s1.txt && date +%s%N >> /s1.txt'\n\
COMMIT\n\
RUN sh -c 'cat /s1.txt > /s2.txt'\n\
\n\
FROM scratch AS 127.0.0.1:5001/bah_e2e_commit\n\
COPY --from=build /s2.txt /out.txt\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_commit\n"
    );
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), &build_file_txt).unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: &file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };

    // Build 1: full run; the COMMIT checkpoints the prefix into the LOCAL
    // layer cache (`basil-checkpoint-<prefixHash>` — never pushed). The
    // registry sees only the stage cache tag + the content tag.
    let r1 = handle_event(inp.clone()).await.unwrap();
    let digest = r1
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    let cache_tags: Vec<String> = c
        .list_tags("bah_e2e_commit")
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.starts_with("basil-cache-"))
        .collect();
    assert_eq!(
        cache_tags.len(),
        1,
        "only the stage cache is pushed (checkpoints are local): {cache_tags:?}"
    );
    assert!(
        !cache_tags.iter().any(|t| t.starts_with("basil-checkpoint")),
        "checkpoints must never be pushed: {cache_tags:?}"
    );
    let parsed = basil::dsl::parser::parse(&build_file_txt).unwrap();
    struct Pinned;
    impl basil::identity::closure::BaseResolver for Pinned {
        fn resolve(
            &mut self,
            raw: &str,
            _t: Option<&str>,
            d: Option<&str>,
        ) -> Result<String, String> {
            if raw == "scratch" {
                return Ok("scratch-empty".to_string());
            }
            d.map(|d| d.to_string())
                .ok_or_else(|| "pinned only".to_string())
        }
    }
    let closures = basil::identity::closure::compute_closure(
        &parsed.targets,
        &[],
        &parsed.global_args,
        &dir,
        &dir.join("Basilfile"),
        &std::collections::HashMap::new(),
        &mut Pinned,
        &std::collections::BTreeMap::new(),
    )
    .unwrap();
    let stage_tag = format!(
        "basil-cache-{}",
        basil::identity::closure::hex(&closures["build"])
    );
    assert!(
        cache_tags.contains(&stage_tag),
        "{stage_tag} not in {cache_tags:?}"
    );
    // The checkpoint ref is derived from the prefix closure and lives locally.
    let ck = basil::identity::closure::compute_checkpoint_closures(
        &parsed.targets,
        &closures,
        &[],
        &parsed.global_args,
        &dir,
        &dir.join("Basilfile"),
        &std::collections::HashMap::new(),
        &mut Pinned,
        &std::collections::BTreeMap::new(),
    )
    .unwrap();
    let checkpoint_ref = format!(
        "basil-checkpoint-{}",
        basil::identity::closure::hex(&ck[&("build".to_string(), 1)])
    );
    assert!(
        local_image_named(&checkpoint_ref).await,
        "checkpoint must exist in local storage: {checkpoint_ref}"
    );

    // Save the sealed prefix's content (a nanosecond timestamp). If build 2
    // re-ran the prefix, this would differ.
    let marker = run_grep_marker(&checkpoint_ref, "/s1.txt").await;
    assert!(marker.starts_with("step1"), "{marker}");

    // Force a rebuild: delete the stage cache (so the stage must rebuild) and
    // the content tag (so the idempotent check does not skip); keep the
    // checkpoint tag so build 2 resumes from it.
    c.delete_tag("bah_e2e_commit", &stage_tag).await.unwrap();
    c.delete_tag("bah_e2e_commit", &format!("basil-{digest}"))
        .await
        .unwrap();

    let r2 = handle_event(inp.clone()).await.unwrap();
    let digest2 = r2
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(
        digest2, digest,
        "resumed rebuild must derive the same content tag"
    );
    let (_, manifest) = c
        .get_manifest("bah_e2e_commit", &format!("basil-{digest2}"))
        .await
        .unwrap();
    assert!(
        !manifest.is_empty(),
        "rebuilt deliverable must be registered"
    );

    // The rebuilt stage cache carries the SAME prefix content — proof the
    // checkpoint was resumed, not re-executed.
    let marker2 = run_grep_marker(
        &format!("127.0.0.1:5001/bah_e2e_commit:{stage_tag}"),
        "/s1.txt",
    )
    .await;
    assert_eq!(
        marker2, marker,
        "prefix must be resumed from the checkpoint, not re-run (timestamps differ)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `buildah from <ref>` (local-first; pulls with the insecure allow for
/// localhost registries) + `cat <path>`, returning trimmed content.
async fn run_grep_marker(ref_str: &str, path: &str) -> String {
    let c = tokio::process::Command::new("buildah")
        .args(["from", "--tls-verify=false", ref_str])
        .output()
        .await
        .unwrap();
    assert!(
        c.status.success(),
        "from {ref_str}: {}",
        String::from_utf8_lossy(&c.stderr)
    );
    let container = String::from_utf8_lossy(&c.stdout).trim().to_string();
    let out = tokio::process::Command::new("buildah")
        .args(["run", &container, "--", "/bin/cat", path])
        .output()
        .await
        .unwrap();
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &container])
        .status()
        .await;
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Whether a ref names a locally-stored image (buildah's layer cache). Bare
/// names normalize to `localhost/<name>` in listings.
async fn local_image_named(name: &str) -> bool {
    let out = tokio::process::Command::new("buildah")
        .args(["images", "--format", "{{.Name}}"])
        .output()
        .await
        .unwrap();
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    names.iter().any(|n| n == name) || names.iter().any(|n| n == &format!("localhost/{name}"))
}

/// A REGISTERed stage used as a dependency (COPY --from) is a published
/// deliverable, never an intermediate cache image: `build -t app` builds it
/// as the deliverable (published at `<register>:basil-<hash>`), then the
/// dependent pulls that artifact. `basil run` uses the same caching; the only
/// difference is that deliverables are not run targets.
const REGDEP_BASILFILE: &str = "FROM alpine AS app_bin\n\
RUN sh -c 'echo registered-content > /r.txt'\n\
\n\
FROM alpine AS helper\n\
RUN sh -c 'echo helper-content > /h.txt'\n\
\n\
FROM app_bin AS reg\n\
COPY --from=helper /h.txt /h.txt\n\
REGISTER 127.0.0.1:5001/bah_e2e_regdep\n\
\n\
FROM app_bin AS app\n\
COPY --from=reg /r.txt /r.txt\n\
COPY --from=reg /h.txt /h.txt\n\
REGISTER 127.0.0.1:5001/bah_e2e_regdep_app\n";

#[tokio::test]
async fn buildah_e2e_registered_stage_dependency_uses_published_ref() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(dir.join("Basilfile"), REGDEP_BASILFILE).unwrap();
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let filter = "app".to_string();
    let inp = BuildInput {
        file: &file_str,
        context: &dir,
        event_json: None,
        target_filter: Some(&filter),
        force: false,
        cache_to: Some("127.0.0.1:5001/bah_e2e_regdep_cache"),
    };
    let results = build_targets(inp).await.unwrap();
    let app_tag = results
        .iter()
        .find(|(k, _)| k == "app")
        .map(|(_, v)| v.clone())
        .expect("build_targets reports the app content tag");
    // The build path returns the bare digest; the idempotent-skip path the
    // namespaced `basil-<hash>`. Accept both.
    let app_tag = app_tag
        .strip_prefix("basil-")
        .unwrap_or(&app_tag)
        .to_string();
    assert!(
        app_tag.len() == 64 && app_tag.chars().all(|c| c.is_ascii_hexdigit()),
        "app content tag malformed: {app_tag}"
    );

    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let is_content_tag = |t: &str| {
        t.strip_prefix("basil-")
            .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
    };
    // The registered stage is published at its content tag under its REGISTER
    // repo...
    let reg_tags = c.list_tags("bah_e2e_regdep").await.unwrap();
    assert!(
        reg_tags.iter().any(|t| is_content_tag(t)),
        "registered stage must be published, got {reg_tags:?}"
    );
    // ...and never as an intermediate cache image.
    assert!(
        !reg_tags.iter().any(|t| t.starts_with("basil-cache-")),
        "registered stage must not be cache-reconciled: {reg_tags:?}"
    );
    // The app image folded the registered stage's content: both files must be
    // present in the published artifact.
    let marker = format!("127.0.0.1:5001/bah_e2e_regdep_app:basil-{app_tag}");
    let r = run_grep_marker(&marker, "/r.txt").await;
    assert_eq!(
        r, "registered-content",
        "app must contain /r.txt from the registered stage: {r}"
    );
    let h = run_grep_marker(&marker, "/h.txt").await;
    assert_eq!(
        h, "helper-content",
        "app must contain /h.txt from the registered stage: {h}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `ADD . .` must behave like a directory copy (docker-style): every
/// dockerignore-filtered context file lands, subtree-preserving, per-file
/// dest-reconstructed. A regression: the ADD executor arm once copied only
/// the first file, so `cargo metadata` inside a real build failed on a
/// missing Cargo.toml.
#[tokio::test]
async fn buildah_e2e_add_dot_copies_whole_context() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(
        dir.join("Basilfile"),
        "FROM alpine AS 127.0.0.1:5001/bah_e2e_add\n\
WORKDIR /w\n\
ADD . .\n\
RUN find /w -type f | sort\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_add\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/c.txt"), "c\n").unwrap();
    // A dockerignored file must NOT land in the image.
    std::fs::write(dir.join(".dockerignore"), "skip.tmp\n").unwrap();
    std::fs::write(dir.join("skip.tmp"), "x\n").unwrap();

    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let file_str = dir.join("Basilfile").to_str().unwrap().to_string();
    let inp = BuildInput {
        file: &file_str,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: true, // always rebuild: the assertion inspects the RUN output
        cache_to: None,
    };
    // Capture the stage RUN output: the deliverable is scratch, so the files
    // land via ADD in an intermediate-ish stage; assert through the pushed
    // content instead — the ADD stage is the deliverable here (alpine).
    let results = handle_event(inp).await.unwrap();
    let digest = results
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    let out = tokio::process::Command::new("buildah")
        .args([
            "from",
            "--tls-verify=false",
            &format!("127.0.0.1:5001/bah_e2e_add:basil-{digest}"),
        ])
        .output()
        .await
        .unwrap();
    let container = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let find = tokio::process::Command::new("buildah")
        .args(["run", &container, "--", "find", "/w", "-type", "f"])
        .output()
        .await
        .unwrap();
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &container])
        .status()
        .await;
    let files: Vec<String> = String::from_utf8_lossy(&find.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("/w/").map(|s| s.to_string()))
        .collect();
    assert!(
        files.iter().any(|f| f == "a.txt")
            && files.iter().any(|f| f == "b.txt")
            && files.iter().any(|f| f == "sub/c.txt"),
        "ADD . . must copy the whole context subtree, got {files:?}"
    );
    assert!(
        !files.iter().any(|f| f == "skip.tmp"),
        "dockerignored file must not be ADDed, got {files:?}"
    );
    assert!(
        !files.iter().any(|f| f == "Basilfile"),
        "the build file must stay excluded, got {files:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `basil is-changed` reflects registry state: nothing published
/// -> changed (exit-0 condition); after a build publishes the content tag ->
/// unchanged; deleting the content tag flips it back to changed.
#[tokio::test]
async fn buildah_e2e_is_changed_flips_after_build() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let dir = tempfile();
    std::fs::write(
        dir.join("Basilfile"),
        "FROM alpine AS 127.0.0.1:5001/bah_e2e_chg\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_chg\n",
    )
    .unwrap();
    let file = dir.join("Basilfile").to_str().unwrap().to_string();
    let tgt = "127.0.0.1:5001/bah_e2e_chg".to_string();
    let before = basil::solver::orchestrate::is_changed(&file, &dir, &[tgt.clone()])
        .await
        .unwrap();
    assert!(before, "nothing published yet must be reported changed");

    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: &file,
        context: &dir,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };
    let results = handle_event(inp).await.unwrap();
    assert!(
        results.iter().any(|(k, _)| k == "digest"),
        "must publish, got {results:?}"
    );
    let after = basil::solver::orchestrate::is_changed(&file, &dir, &[tgt.clone()])
        .await
        .unwrap();
    assert!(!after, "published image must be reported unchanged");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A sibling module (its own context + Basilfile): the importer FROMs its
/// image, so the module is built+published once, then the importer pulls it.
/// The second delivery of the same event must not rebuild the module (its
/// content tag exists) — use-or-build verified end to end.
const MODULE_BASILFILE: &str = "FROM alpine AS 127.0.0.1:5001/bah_e2e_mod\n\
COPY mod.txt /mod.txt\n";

const IMPORTER_BASILFILE: &str =
    "MODULE ../mod -f Basilfile TARGET 127.0.0.1:5001/bah_e2e_mod AS mod\n\
FROM scratch AS 127.0.0.1:5001/bah_e2e_imp\n\
COPY --from=mod /mod.txt /out.txt\n\
CMD [\"/bin/cat\",\"/out.txt\"]\n\
ON push(main) BUILD 127.0.0.1:5001/bah_e2e_imp\n";

#[tokio::test]
async fn buildah_e2e_module_dependency() {
    if !buildah_present() || !registry_up().await {
        eprintln!("skipping: buildah or registry unavailable");
        return;
    }
    let _guard = E2E_LOCK.lock().await;
    let root = tempfile();
    // Importer context `app/` with the module as a SIBLING (`./mod` resolved
    // file-relative against app/Basilfile -> <root>/mod).
    let app = root.join("app");
    let mod_dir = root.join("mod");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::write(app.join("Basilfile"), IMPORTER_BASILFILE).unwrap();
    std::fs::write(mod_dir.join("Basilfile"), MODULE_BASILFILE).unwrap();
    std::fs::write(mod_dir.join("mod.txt"), "from module\n").unwrap();

    let file_path = app.join("Basilfile");
    let file_str = file_path.to_str().unwrap();
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;
    let inp = BuildInput {
        file: file_str,
        context: &app,
        event_json: Some(event),
        target_filter: None,
        force: false,
        cache_to: None,
    };
    // First delivery: builds the module (not yet registered), then the importer.
    let results = handle_event(inp.clone()).await.unwrap();
    let digest = results
        .iter()
        .find(|(k, _)| k == "digest")
        .map(|(_, v)| v.clone())
        .unwrap();
    // The importer image folds the module content: the pushed config must
    // contain the module's file (copied from the module image).
    let c = basil::solver::registry::RegistryClient::new("http://127.0.0.1:5001");
    let ctag = |h: &str| format!("basil-{h}");
    let (_, manifest) = c.get_manifest("bah_e2e_imp", &ctag(&digest)).await.unwrap();
    let m: Value = serde_json::from_slice(&manifest).unwrap();
    let cfg_digest = m["config"]["digest"].as_str().unwrap().to_string();
    let cfg_url = format!("http://127.0.0.1:5001/v2/bah_e2e_imp/blobs/{cfg_digest}");
    let resp = reqwest::Client::new().get(&cfg_url).send().await.unwrap();
    let cfg: Value = resp.json().await.unwrap();
    assert_eq!(
        cfg["config"]["Cmd"],
        serde_json::json!(["/bin/cat", "/out.txt"])
    );
    // The module is published at `basil-<tagHash>`: its repo lists a namespaced
    // content tag alongside any mutable tags.
    let is_content_tag = |t: &str| {
        t.strip_prefix("basil-")
            .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
    };
    let mod_tags = c.list_tags("bah_e2e_mod").await.unwrap();
    assert!(
        mod_tags.iter().any(|t| is_content_tag(t)),
        "module must be published at its content tag, got {mod_tags:?}"
    );

    // Second delivery: module already registered -> importer noops; the module
    // must NOT be rebuilt (its content tag exists) — the noop path only runs
    // OUTPUT extraction, which this fixture has none of.
    let results2 = handle_event(inp.clone()).await.unwrap();
    let (_, result2) = results2
        .iter()
        .find(|(k, _)| k == "result")
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    assert_eq!(result2, "idempotent-skip", "second delivery must noop");

    // Delete the module's content tag from the registry. The importer has
    // already nooped, but the module is part of the declared artifact set: the
    // next run must re-register it (same tagHash — the importer image stays
    // valid) even though the importer itself still skips.
    let mod_tags_before = c.list_tags("bah_e2e_mod").await.unwrap();
    let content_tag = mod_tags_before
        .iter()
        .find(|t| is_content_tag(t))
        .expect("module content tag present")
        .clone();
    c.delete_tag("bah_e2e_mod", &content_tag).await.unwrap();
    let results3 = handle_event(inp.clone()).await.unwrap();
    let (_, result3) = results3
        .iter()
        .find(|(k, _)| k == "result")
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    assert_eq!(
        result3, "idempotent-skip",
        "importer still skips after module deletion"
    );
    let mod_tags_after = c.list_tags("bah_e2e_mod").await.unwrap();
    assert!(
        mod_tags_after.iter().any(|t| t == &content_tag),
        "deleted module must be re-registered at its content tag, got {mod_tags_after:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
