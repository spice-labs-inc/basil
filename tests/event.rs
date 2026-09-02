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