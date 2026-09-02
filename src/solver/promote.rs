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
    None
}

/// Loose semver check: `N(.N)*` with optional pre-release/build.
fn is_version(s: &str) -> bool {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 1 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
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
}