//! Event matching against realistic GitHub Actions payload shapes.
//!
//! These mirror the real `$GITHUB_EVENT_PATH` JSON consumed verbatim on stdin.

use basil::dsl::parser::parse;
use basil::event;

fn parse_file() -> basil::dsl::ast::File {
    parse(
        "ON pr-merged        BUILD ghcr.io/mycompany/app DO migrate\n\
         ON push(main)       BUILD ghcr.io/mycompany/app\n\
         ON push             DO notify\n\
         ON tag              BUILD ghcr.io/mycompany/app\n",
    )
    .unwrap()
}

const PR_MERGED: &str = r#"{
  "action": "closed",
  "number": 42,
  "pull_request": {
    "merged": true,
    "head": { "ref": "feature/foo" },
    "base": { "ref": "main" }
  },
  "repository": { "full_name": "mycompany/app" }
}"#;

const PUSH_MAIN: &str = r#"{
  "ref": "refs/heads/main",
  "before": "0",
  "after": "1",
  "repository": { "full_name": "mycompany/app" }
}"#;

const PUSH_TAG: &str = r#"{
  "ref": "refs/tags/v1.5.8",
  "before": "0",
  "after": "1"
}"#;

#[test]
fn pr_merged_selects_build_and_do() {
    let file = parse_file();
    let payload: serde_json::Value = serde_json::from_str(PR_MERGED).unwrap();
    let matched = event::matching_reactions(&file, &payload);
    assert_eq!(matched.len(), 1);
    let r = matched[0];
    assert_eq!(r.build, vec!["ghcr.io/mycompany/app"]);
    assert_eq!(r.do_list, vec!["migrate"]);
}

#[test]
fn push_main_matches_both_selectors_and_bare() {
    let file = parse_file();
    let payload: serde_json::Value = serde_json::from_str(PUSH_MAIN).unwrap();
    let matched = event::matching_reactions(&file, &payload);
    // `push(main)` and bare `push` both match; the bare tag never does.
    assert_eq!(matched.len(), 2);
}

#[test]
fn tag_matches_only_tag_reaction() {
    let file = parse_file();
    let payload: serde_json::Value = serde_json::from_str(PUSH_TAG).unwrap();
    let matched = event::matching_reactions(&file, &payload);
    assert_eq!(matched.len(), 1);
    assert!(matches!(matched[0].event.token, basil::dsl::ast::EventToken::Tag));
}

/// The checked-in docs/events/ payloads must match the token they're meant to
/// demonstrate, so the samples stay correct against the real matcher.
#[test]
fn docs_event_fixtures_match_their_tokens() {
    use basil::dsl::ast::EventToken;
    use basil::event::reaction_matches;
    let cases: &[(&str, EventToken, &[&str], &str)] = &[
        ("push", EventToken::Push, &["master"], "docs/events/push.json"),
        ("push-bare", EventToken::Push, &[], "docs/events/push.json"),
        ("tag", EventToken::Tag, &[], "docs/events/tag-push.json"),
        ("pr-opened", EventToken::PrOpened, &[], "docs/events/pull_request-opened.json"),
        ("pr-synchronized", EventToken::PrSynchronized, &[], "docs/events/pull_request-synchronize.json"),
        ("pr-merged", EventToken::PrMerged, &[], "docs/events/pull_request-merged.json"),
    ];
    for (name, tok, selectors, path) in cases {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let selectors: Vec<String> = selectors.iter().map(|s| s.to_string()).collect();
        assert!(
            reaction_matches(tok, &selectors, &payload),
            "docs/events fixture `{name}` ({path}) did not match its token"
        );
    }
}