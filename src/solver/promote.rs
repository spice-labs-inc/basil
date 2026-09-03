//! Promotion: assign a human version tag on a `tag` event.
//!
//! Promotion is `ON tag BUILD <name>…` plus an extra `<name>:v<version>` tag on
//! the result. Only the registry tag moves — the closure/tagHash identity never
//! does, so consumers still resolve by content. Version tags are immutable:
//! basil refuses to overwrite an existing `<name>:v<ver>` that resolves to a
//! different manifest.

/// Parse a git tag ref into (optional target, version).
///
/// - `refs/tags/v1.5.8`        -> (None, "1.5.8")
/// - `refs/tags/target-v1.2.3` -> (Some("target"), "1.2.3")
pub fn extract_version(tag_ref: &str) -> Option<(Option<String>, String)> {
    let r = tag_ref.trim_start_matches("refs/tags/");
    let r = r.trim_start_matches('/');
    // `target-vX.Y.Z` — the part before the last `-v`.
    if let Some(idx) = r.rfind("-v") {
        let (prefix, ver) = r.split_at(idx + 1);
        // prefix is `target-`, ver is `v1.2.3`
        let ver = ver.trim_start_matches('v');
        if is_version(ver) {
            let target = prefix.trim_end_matches('-').to_string();
            return Some((Some(target), ver.to_string()));
        }
    }
    // Bare `vX.Y.Z`.
    if let Some(ver) = r.strip_prefix('v') {
        if is_version(ver) {
            return Some((None, ver.to_string()));
        }
    }
    // Bare `X.Y.Z` (no leading `v`), e.g. a release tagged `0.0.1`.
    if is_version(r) {
        return Some((None, r.to_string()));
    }
    None
}

/// Loose semver check: `N(.N)*` with optional pre-release/build.
fn is_version(s: &str) -> bool {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 1 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The version (and optional qualified target) an event promotes, per token.
///
/// - `Tag`: from the git tag ref (`refs/tags/v1.5.8`, `target-v1.2.3`).
/// - `Release`: from the release's `tag_name` (e.g. `v1.5.8`, `target-v1.2.3`);
///   the same `target-v…` prefix convention applies.
/// - Anything else: `None` (no promotion).
pub fn event_version(
    token: &crate::dsl::ast::EventToken,
    payload: &serde_json::Value,
    event_ref: &str,
) -> Option<(Option<String>, String)> {
    match token {
        crate::dsl::ast::EventToken::Tag => extract_version(event_ref),
        crate::dsl::ast::EventToken::Release => {
            let tag_name = payload
                .get("release")
                .and_then(|r| r.get("tag_name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if tag_name.is_empty() {
                None
            } else {
                extract_version(&format!("refs/tags/{tag_name}"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_version() {
        assert_eq!(
            extract_version("refs/tags/v1.5.8"),
            Some((None, "1.5.8".to_string()))
        );
    }

    #[test]
    fn qualified_target() {
        assert_eq!(
            extract_version("refs/tags/target-v1.2.3"),
            Some((Some("target".to_string()), "1.2.3".to_string()))
        );
    }

    #[test]
    fn non_version_is_none() {
        assert_eq!(extract_version("refs/tags/release"), None);
        assert_eq!(extract_version("refs/heads/main"), None);
    }

    #[test]
    fn bare_numeric_no_v_prefix() {
        assert_eq!(extract_version("refs/tags/0.0.1"), Some((None, "0.0.1".to_string())));
    }

    #[test]
    fn release_version_comes_from_tag_name() {
        use crate::dsl::ast::EventToken as T;
        let payload = serde_json::json!({
            "action": "published",
            "release": { "tag_name": "v2.0.0", "prerelease": false, "draft": false },
        });
        assert_eq!(
            event_version(&T::Release, &payload, "refs/heads/main"),
            Some((None, "2.0.0".to_string()))
        );
    }

    #[test]
    fn release_qualified_target_from_tag_name() {
        use crate::dsl::ast::EventToken as T;
        let payload = serde_json::json!({
            "action": "published",
            "release": { "tag_name": "app-v1.2.3", "prerelease": false, "draft": false },
        });
        assert_eq!(
            event_version(&T::Release, &payload, ""),
            Some((Some("app".to_string()), "1.2.3".to_string()))
        );
    }
}