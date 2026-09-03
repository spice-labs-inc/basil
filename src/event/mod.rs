//! Event input: matching `ON` reactions against a delivered GitHub Actions
//! event payload.
//!
//! The event is the raw GitHub Actions payload (consumed verbatim on stdin);
//! basil defines no event schema of its own yet. Each reaction's `ON` token is
//! resolved against the payload per the design's mapping.

use crate::dsl::ast::{EventToken, File};
use serde_json::Value;

/// Whether a single reaction's `ON` matches the delivered event payload.
pub fn reaction_matches(event: &EventToken, selectors: &[String], payload: &Value) -> bool {
    match event {
        EventToken::PrOpened => pr_action(payload) == Some("opened"),
        EventToken::PrSynchronized => pr_action(payload) == Some("synchronize"),
        EventToken::PrMerged => {
            pr_action(payload) == Some("closed")
                && payload_get(payload, &["pull_request", "merged"])
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
        EventToken::Push => push_matches(payload, selectors),
        EventToken::Tag => push_ref(payload)
            .map(|r| r.starts_with("refs/tags/"))
            .unwrap_or(false),
        EventToken::Release => release_matches(payload, selectors),
        EventToken::Custom(_) => false,
    }
}

/// The reactions in `file` whose event matches `payload`.
pub fn matching_reactions<'a>(file: &'a File, payload: &Value) -> Vec<&'a crate::dsl::ast::Reaction> {
    file.reactions
        .iter()
        .filter(|r| reaction_matches(&r.event.token, &r.event.selectors, payload))
        .collect()
}

fn push_ref(payload: &Value) -> Option<&str> {
    payload.get("ref").and_then(Value::as_str)
}

fn push_matches(payload: &Value, selectors: &[String]) -> bool {
    let Some(r) = push_ref(payload) else {
        return false;
    };
    // A push event with a tag ref is the `tag` token's domain, not `push`.
    if r.starts_with("refs/tags/") {
        return false;
    }
    if r.starts_with("refs/heads/") {
        let branch = &r["refs/heads/".len()..];
        if selectors.is_empty() {
            return true; // bare `ON push` matches any branch push.
        }
        return selectors.iter().any(|s| s == branch);
    }
    false
}

/// Whether a `release` event matches, optional selectors narrow it.
///
/// A `release` payload carries `action` (published/unpublished/created/edited/
/// deleted/prereleased/released) plus a `release` object with `prerelease` and
/// `draft` booleans. Bare `ON release` matches any release; selectors narrow by
/// `action` value or by the `prerelease`/`draft` booleans.
fn release_matches(payload: &Value, selectors: &[String]) -> bool {
    // Must be a release event (has a `release` object) to be in scope.
    if payload.get("release").is_none() {
        return false;
    }
    if selectors.is_empty() {
        return true; // bare `ON release` matches any release.
    }
    let action = payload_get(payload, &["action"]).and_then(Value::as_str).unwrap_or("");
    let prerelease = payload_get(payload, &["release", "prerelease"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let draft = payload_get(payload, &["release", "draft"])
        .and_then(Value::as_bool)
        .unwrap_or(false);
    selectors.iter().any(|s| match s.as_str() {
        "prerelease" => prerelease,
        "draft" => draft,
        other => action == other, // e.g. published, unpublished, created, released, deleted
    })
}

/// The `action` of a `pull_request` event.
fn pr_action(payload: &Value) -> Option<&str> {
    if payload.get("pull_request").is_some() && payload.get("action").is_some() {
        payload.get("action").and_then(Value::as_str)
    } else {
        None
    }
}

/// Get a nested value by a path of keys.
fn payload_get<'a>(payload: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = payload;
    for key in path {
        cur = cur.get(*key)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr(action: &str, merged: bool) -> Value {
        json!({
            "action": action,
            "pull_request": { "merged": merged, "number": 7 },
        })
    }

    fn push(refv: &str) -> Value {
        json!({ "ref": refv })
    }

    fn sel(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn release(action: &str, tag_name: &str, prerelease: bool, draft: bool) -> Value {
        json!({
            "action": action,
            "release": { "tag_name": tag_name, "prerelease": prerelease, "draft": draft },
        })
    }

    #[test]
    fn release_bare_matches_any() {
        assert!(reaction_matches(&EventToken::Release, &sel(&[]), &release("published", "v1.0.0", false, false)));
        assert!(reaction_matches(&EventToken::Release, &sel(&[]), &release("prereleased", "v1.0.0-rc.1", true, false)));
        assert!(reaction_matches(&EventToken::Release, &sel(&[]), &release("created", "v1.0.0", false, true)));
    }

    #[test]
    fn release_selector_narrows_by_action() {
        assert!(reaction_matches(&EventToken::Release, &sel(&["published"]), &release("published", "v1.0.0", false, false)));
        assert!(!reaction_matches(&EventToken::Release, &sel(&["published"]), &release("deleted", "v1.0.0", false, false)));
    }

    #[test]
    fn release_selector_narrows_by_prerelease_or_draft() {
        assert!(reaction_matches(&EventToken::Release, &sel(&["prerelease"]), &release("published", "v1.0.0-rc.1", true, false)));
        assert!(!reaction_matches(&EventToken::Release, &sel(&["prerelease"]), &release("published", "v1.0.0", false, false)));
        assert!(reaction_matches(&EventToken::Release, &sel(&["draft"]), &release("created", "v1.0.0", false, true)));
    }

    #[test]
    fn release_non_event_is_not_matched() {
        assert!(!reaction_matches(&EventToken::Release, &sel(&[]), &push("refs/heads/main")));
    }

    #[test]
    fn pr_opened() {
        assert!(reaction_matches(&EventToken::PrOpened, &sel(&[]), &pr("opened", false)));
        assert!(!reaction_matches(&EventToken::PrOpened, &sel(&[]), &pr("synchronize", false)));
        assert!(!reaction_matches(&EventToken::PrOpened, &sel(&[]), &push("refs/heads/main")));
    }

    #[test]
    fn pr_merged() {
        assert!(reaction_matches(&EventToken::PrMerged, &sel(&[]), &pr("closed", true)));
        assert!(!reaction_matches(&EventToken::PrMerged, &sel(&[]), &pr("closed", false)));
        assert!(!reaction_matches(&EventToken::PrMerged, &sel(&[]), &pr("opened", false)));
    }

    #[test]
    fn push_branch_selector() {
        assert!(reaction_matches(&EventToken::Push, &sel(&["main"]), &push("refs/heads/main")));
        assert!(!reaction_matches(&EventToken::Push, &sel(&["main"]), &push("refs/heads/dev")));
        assert!(reaction_matches(&EventToken::Push, &sel(&["main", "dev"]), &push("refs/heads/dev")));
        // bare `ON push` matches any branch.
        assert!(reaction_matches(&EventToken::Push, &sel(&[]), &push("refs/heads/feature/x")));
        // tag refs are not `push`.
        assert!(!reaction_matches(&EventToken::Push, &sel(&[]), &push("refs/tags/v1")));
    }

    #[test]
    fn tag_ref() {
        assert!(reaction_matches(&EventToken::Tag, &sel(&[]), &push("refs/tags/v1.2.3")));
        assert!(!reaction_matches(&EventToken::Tag, &sel(&[]), &push("refs/heads/main")));
    }

    #[test]
    fn custom_token_never_matches_yet() {
        assert!(!reaction_matches(&EventToken::Custom("image-published".into()), &sel(&[]), &pr("opened", false)));
    }
}