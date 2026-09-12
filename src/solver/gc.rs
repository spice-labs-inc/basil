//! `basil gc <age>` — prune basil-owned registry tags older than `age`.
//!
//! Only basil's own ephemeral tags are pruned: the date-encoded `cache-*` tags
//! (by the date embedded in the name) and `cache-<tagHash>` / content `<tagHash>`
//! tags (by the image's `created` time). Version tags (`vX.Y.Z`) are never
//! pruned.

use crate::solver::registry::RegistryClient;

/// Parse an age like `30d`, `1w`, `8h`, `1y` into seconds.
pub fn parse_age(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.len() < 2 {
        return Err(format!("invalid age `{s}` (expected e.g. 30d, 1w, 8h, 2y)"));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().map_err(|_| format!("invalid age `{s}`"))?;
    let factor = match unit {
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        "y" => 365 * 86400,
        _ => return Err(format!("unknown age unit `{unit}` (h/d/w/y)")),
    };
    Ok(n * factor)
}

enum Kind {
    /// Date-encoded `cache-<YYYYMMDD|YYYYMM|YYYY>` — aged by the embedded date.
    CacheDate(u64, usize),
    /// `cache-<tagHash>` or content `<tagHash>` — aged by image `created`.
    CacheHash,
    /// A version tag `vX.Y.Z` — never pruned.
    Version,
    /// Not basil-owned — never pruned.
    Foreign,
}

fn classify(tag: &str) -> Kind {
    // Basil-owned tags live in the `basil-` namespace: cache namespaces
    // (`basil-cache-*`) sort above content tags (`basil-<tagHash>`). Tags
    // outside the namespace — including any bare 64-hex or `cache-*` tag — are
    // never pruned.
    if let Some(rest) = tag.strip_prefix(crate::solver::registry::BASIL_CACHE_PREFIX) {
        if rest.len() >= 4 && rest.len() <= 8 && rest.chars().all(|c| c.is_ascii_digit()) {
            let n: u64 = rest.parse().unwrap_or(0);
            return Kind::CacheDate(n, rest.len());
        }
        return Kind::CacheHash;
    }
    if let Some(rest) = tag.strip_prefix(crate::solver::registry::BASIL_TAG_PREFIX) {
        if rest.len() == 64 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Kind::CacheHash;
        }
        return Kind::Foreign;
    }
    if let Some(rest) = tag.strip_prefix('v') {
        if !rest.is_empty() && rest.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
            return Kind::Version;
        }
    }
    Kind::Foreign
}

/// Convert `YYYYMMDD` (8), `YYYYMM` (6), or `YYYY` (4) to a unix start-of-period.
fn date_to_unix(n: u64, width: usize) -> Option<u64> {
    let (year, month, day) = match width {
        8 => (n / 10000, (n / 100) % 100, n % 100),
        6 => (n / 100, n % 100, 1),
        _ => (n, 1, 1),
    };
    if !(1970..=3000).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Civil-from-days inverse (Howard Hinnant): days since epoch to unix 00:00.
    let y = year as i64;
    let m = month as i64;
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days as u64) * 86400)
}

/// Run a gc pass over `repo`'s tags. Returns the tags deleted (or, with
/// `dry_run`, that would be deleted).
pub async fn gc_repo(
    client: &RegistryClient,
    repo: &str,
    age_secs: u64,
    now: u64,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let tags = client.list_tags(repo).await?;
    let cutoff = now.saturating_sub(age_secs);
    let mut deleted = Vec::new();

    for tag in tags {
        let prune = match classify(&tag) {
            Kind::Version | Kind::Foreign => false,
            Kind::CacheDate(ymd, width) => {
                match date_to_unix(ymd, width) {
                    Some(start) => {
                        let period_len = match width {
                            8 => 86400,
                            6 => 31 * 86400,
                            _ => 365 * 86400,
                        };
                        start + period_len <= cutoff
                    }
                    None => {
                        tracing::warn!("unparseable cache date in `{tag}`");
                        false
                    }
                }
            }
            Kind::CacheHash => match client.get_config_created(repo, &tag).await {
                Ok(Some(created)) => created < cutoff,
                Ok(None) => {
                    tracing::warn!("no created time for {repo}:{tag}; skipping");
                    false
                }
                Err(e) => {
                    tracing::warn!("created-time lookup for {repo}:{tag} failed: {e}; skipping");
                    false
                }
            },
        };

        if prune {
            if dry_run {
                tracing::info!("gc (dry-run) would delete {repo}:{tag}");
            } else {
                client.delete_tag(repo, &tag).await?;
                tracing::info!("gc deleted {repo}:{tag}");
            }
            deleted.push(tag);
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ages() {
        assert_eq!(parse_age("30d").unwrap(), 30 * 86400);
        assert_eq!(parse_age("1w").unwrap(), 7 * 86400);
        assert_eq!(parse_age("8h").unwrap(), 8 * 3600);
        assert_eq!(parse_age("2y").unwrap(), 2 * 365 * 86400);
        assert!(parse_age("x").is_err());
        assert!(parse_age("5m").is_err());
    }

    #[test]
    fn date_conversion() {
        // 2024-01-15 00:00 UTC.
        let ts = date_to_unix(20240115, 8).unwrap();
        assert_eq!(ts, 1705276800);
    }

    #[test]
    fn prunes_only_basil_namespaced_tags() {
        assert!(matches!(
            classify("basil-cache-20240901"),
            Kind::CacheDate(20240901, 8)
        ));
        assert!(matches!(
            classify("basil-cache-202409"),
            Kind::CacheDate(202409, 6)
        ));
        assert!(matches!(
            classify("basil-cache-0cc970bb04c43e8bd05099f7d58df63c3de606d6e0234dba9997d7f02387d609"),
            Kind::CacheHash
        ));
        assert!(matches!(
            classify("basil-0cc970bb04c43e8bd05099f7d58df63c3de606d6e0234dba9997d7f02387d609"),
            Kind::CacheHash
        ));
        assert!(matches!(classify("v1.5.8"), Kind::Version));
        assert!(matches!(classify("latest"), Kind::Foreign));
        // Pre-namespace shapes are foreign now (no transition needed): a bare
        // 64-hex tag could be anyone's, and a bare `cache-*` is not basil's.
        assert!(matches!(
            classify("0cc970bb04c43e8bd05099f7d58df63c3de606d6e0234dba9997d7f02387d609"),
            Kind::Foreign
        ));
        assert!(matches!(classify("cache-20240901"), Kind::Foreign));
    }
}