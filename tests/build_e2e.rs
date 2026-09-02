//! End-to-end build test: emit → gateway bridge → registry export.
//!
//! Gated: requires a reachable buildkitd (default tcp://127.0.0.1:8818) and a
//! reachable registry. Uses a scratch-based, real-pinned-base free fixture so it
//! builds fast and offline-of-registry (only pushes to the local registry).
//! Skips cleanly when the daemon is unreachable.

use basil::dsl::parser::parse;
use basil::solver::orchestrate::{self, BuildInput};
use std::collections::HashMap;

fn buildkitd_ok() -> bool {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:8818".parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

#[tokio::test]
async fn builds_scratch_deliverable_with_config() {
    if !buildkitd_ok() {
        eprintln!("skipping: buildkitd not reachable at 127.0.0.1:8818");
        return;
    }

    // A minimal scratch-based deliverable with a context COPY + config, pushed
    // to the throwaway local registry.
    let file = "tests/e2e/Basilfile";
    let ctx = "tests/e2e";
    let event = r#"{"ref":"refs/heads/main","before":"0","after":"1"}"#;

    let channel = tonic::transport::Endpoint::from_shared("tcp://127.0.0.1:8818")
        .unwrap()
        .connect()
        .await
        .unwrap();

    // A real session is required: the context COPY transfers files via the
    // session's filesync.
    let mut client = buildkit_client::client::BuildKitClient::connect("tcp://127.0.0.1:8818")
        .await
        .unwrap();
    let mut session = buildkit_client::session::Session::new();
    session.add_file_sync(std::path::PathBuf::from(ctx)).await;
    session.start(client.control().clone()).await.unwrap();
    let session_id = session.get_id();

    let inp = BuildInput {
        file,
        context: std::path::Path::new(ctx),
        event_json: Some(event),
        target_filter: None,
    };
    let results = orchestrate::run(channel, &session_id, inp).await.unwrap();
    // The orchestrator returns exporter digest; assert we got a digest.
    assert!(!results.is_empty());
    assert!(results.iter().any(|(k, _)| k == "digest"));
}

/// Sanity: the fixture fixture parses and has the expected shape.
#[test]
fn e2e_fixture_parses_and_has_deliverable() {
    let src = std::fs::read_to_string("tests/e2e/Basilfile").unwrap();
    let f = parse(&src).unwrap();
    let delivers: Vec<&str> = f
        .targets
        .iter()
        .map(|t| t.name.as_str())
        .filter(|n| n.contains("5001"))
        .collect();
    assert!(!delivers.is_empty(), "fixture should have a registry deliverable");
    let _ = HashMap::<String, String>::new();
}

/// DO/TASK execution via a cache-ignored exec in a throwaway solve: a passing
/// command completes, a non-zero exit fails.
#[tokio::test]
async fn do_task_runs_and_fails_on_nonzero() {
    if !buildkitd_ok() {
        eprintln!("skipping: buildkitd not reachable at 127.0.0.1:8818");
        return;
    }
    let channel = tonic::transport::Endpoint::from_shared("tcp://127.0.0.1:8818")
        .unwrap()
        .connect()
        .await
        .unwrap();

    let src = "FROM alpine:3.20@sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e AS ok\n\
TASK /bin/true\n\
FROM alpine:3.20@sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e AS bad\n\
TASK exit 7\n\
ON push(main) DO ok, bad\n";
    let parsed = basil::dsl::parser::parse(src).unwrap();
    let matched = basil::event::matching_reactions(&parsed, &serde_json::json!({"ref":"refs/heads/main"}));
    assert_eq!(matched.len(), 1);
    let ev = r#"{"ref":"refs/heads/main"}"#;
    // `ok` runs first and succeeds; `bad` exits 7 and must fail the overall run.
    let res = basil::solver::orchestrate::run_do_tasks(&channel, "test-session", &parsed, &matched, ev).await;
    assert!(res.is_err(), "a non-zero task exit must fail the run");
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("bad") || msg.contains("exit"), "error should mention the task/exit: {msg}");
}