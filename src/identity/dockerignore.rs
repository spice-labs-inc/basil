//! Docker `.dockerignore` rule matching, honored as-is.
//!
//! Basil derives a target's input closure from the files its instructions
//! actually read, filtered by `.dockerignore` exactly as Docker would. This
//! module implements Docker's pattern semantics (the subset Docker actually
//! supports): relative-to-root patterns, `*`/`?`/`[...]` per path component,
//! `**` across separators, a leading `/` anchoring to the root, a trailing `/`
//! matching directories, and `!` negation where the last matching rule wins.

use std::path::Path;

/// Outcome of matching a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// No rule matched; the default (include) applies.
    None,
    /// An earlier `!` pattern re-included this path.
    Include,
    /// An ignore pattern matched this path.
    Ignore,
}

const RULE_IGNORE: u8 = 0;
const RULE_INCLUDE: u8 = 1;

#[derive(Debug, Clone)]
struct Rule {
    /// `RULE_IGNORE` or `RULE_INCLUDE`.
    action: u8,
    /// True if the pattern contained a `/` (root-anchored). A bare pattern
    /// (no `/`) matches a path component at any depth.
    anchored: bool,
    segments: Vec<String>,
}

/// A compiled set of dockerignore patterns.
#[derive(Debug, Clone)]
pub struct Dockerignore {
    patterns: Vec<Rule>,
}

/// Split a pattern by unescaped `/`, honoring `[ ]` groups and `**`.
fn split_glob(path: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut chars = path.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '/' if !in_class => {
                if !cur.is_empty() {
                    segs.push(std::mem::take(&mut cur));
                }
            }
            '[' if !in_class => {
                in_class = true;
                cur.push(c);
            }
            ']' if in_class => {
                in_class = false;
                cur.push(c);
            }
            '\\' => {
                // escape next char
                if let Some(n) = chars.next() {
                    cur.push(n);
                } else {
                    cur.push('\\');
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

impl Dockerignore {
    /// Parse a `.dockerignore` body (lines of patterns).
    pub fn parse(body: &str) -> Self {
        let mut patterns = Vec::new();
        for raw in body.lines() {
            let line = raw.trim_end_matches('\r');
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut action = RULE_IGNORE;
            let mut pat = line;
            if let Some(rest) = pat.strip_prefix('!') {
                action = RULE_INCLUDE;
                pat = rest;
            }
            // A leading `/` anchors to root; a trailing `/` marks a directory.
            // `anchored` is true if the pattern contains a `/` anywhere (a bare
            // `foo` matches by basename at any depth; `/foo` or `a/b` anchors).
            let anchored = pat.contains('/');
            let pat = pat.strip_prefix('/').unwrap_or(pat);
            let pat = pat.strip_suffix('/').unwrap_or(pat);
            if pat.is_empty() {
                continue;
            }
            let segs = split_glob(pat);
            patterns.push(Rule {
                action,
                anchored,
                segments: segs,
            });
        }
        Dockerignore { patterns }
    }

    /// Load from a `.dockerignore` path (or empty if absent/unreadable).
    pub fn from_file_if_present(context_root: &Path) -> Self {
        let path = context_root.join(".dockerignore");
        match std::fs::read_to_string(&path) {
            Ok(body) => Self::parse(&body),
            Err(_) => Self::parse(""),
        }
    }

    /// Match a context-root-relative path (using `/` separators).
    pub fn matches(&self, rel_path: &str) -> Match {
        let segs: Vec<&str> = rel_path.split('/').collect();
        // Docker: the LAST matching rule wins. Evaluate in reverse so the first
        // match found is the last (most-precedence) rule.
        for rule in self.patterns.iter().rev() {
            if let Some(included) = self.rule_matches(rule, &segs) {
                return if included { Match::Include } else { Match::Ignore };
            }
        }
        Match::None
    }

    fn segment_matches(pat: &str, seg: &str) -> bool {
        if pat == "**" {
            return true;
        }
        if !pat.contains(['*', '?', '[']) {
            return pat == seg;
        }
        glob_match(pat, seg)
    }

    /// Returns `Some(included)` if this rule's action applies to the path,
    /// `None` otherwise.
    fn rule_matches(&self, rule: &Rule, path_segs: &[&str]) -> Option<bool> {
        let pat = &rule.segments;
        if rule.anchored {
            // Match path_segs against pattern segments at the root, allowing
            // `**` to span segments. A prefix match covers a directory.
            if z_match(pat, path_segs).is_some() {
                return Some(rule.action == RULE_INCLUDE);
            }
            None
        } else {
            // Basename pattern (single segment, no slash). Matches a component
            // at any depth.
            let single = &pat[0];
            for seg in path_segs {
                if Self::segment_matches(single, seg) {
                    return Some(rule.action == RULE_INCLUDE);
                }
            }
            None
        }
    }
}

/// `**`-aware pattern match of `pat` segments against `path` segments. Returns
/// Some if matched; also matches when `pat` is a prefix of `path` (covering a
/// directory).
fn z_match(pat: &[String], path: &[&str]) -> Option<()> {
    if pat.is_empty() {
        return Some(());
    }
    // Try to match, allowing `**` to consume 0+ path segments.
    match z_match_rec(pat, path) {
        true => Some(()),
        false => None,
    }
}

fn z_match_rec(pat: &[String], path: &[&str]) -> bool {
    if pat.is_empty() {
        return path.is_empty() || path.last().is_some(); // prefix match allows deeper path
    }
    if pat[0] == "**" {
        // `**` can consume 0..n path segments; then continue with rest.
        // Try consuming 0 or more.
        if z_match_rec(&pat[1..], path) {
            return true;
        }
        if !path.is_empty() {
            return z_match_rec(pat, &path[1..]);
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if !Dockerignore::segment_matches(&pat[0], path[0]) {
        return false;
    }
    z_match_rec(&pat[1..], &path[1..])
}

/// Basic single-segment glob: `*`, `?`, `[...]`, `**` (treated as `*` inside a
/// segment). Uses Dover's `filepath.Match` semantics as close as practical.
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = text.chars().collect();
    glob_rec(&p, &s)
}

fn glob_rec(p: &[char], s: &[char]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        '*' => {
            // Also treat `**` as `*` within a segment.
            // match any run of chars
            let mut start = 0;
            let rest = &p[1..];
            loop {
                if glob_rec(rest, &s[start..]) {
                    return true;
                }
                if start >= s.len() {
                    return false;
                }
                start += 1;
            }
        }
        '?' => {
            if s.is_empty() {
                false
            } else {
                glob_rec(&p[1..], &s[1..])
            }
        }
        '[' => {
            if s.is_empty() {
                return false;
            }
            // find matching ']'
            let mut i = 1;
            let mut neg = false;
            if i < p.len() && (p[i] == '!' || p[i] == '^') {
                neg = true;
                i += 1;
            }
            let mut matched = false;
            let mut first = true;
            while i < p.len() {
                if p[i] == ']' && !first {
                    break;
                }
                // handle ranges a-z
                if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                    let lo = p[i];
                    let hi = p[i + 2];
                    if s[0] >= lo && s[0] <= hi {
                        matched = true;
                    }
                    i += 3;
                } else {
                    if p[i] == s[0] {
                        matched = true;
                    }
                    i += 1;
                }
                first = false;
            }
            // skip to after ']'
            while i < p.len() && p[i] != ']' {
                i += 1;
            }
            if i >= p.len() {
                // unterminated class — treat literal
                return s[0] == '[' && glob_rec(&p[1..], &s[1..]);
            }
            let matches_class = if neg { !matched } else { matched };
            if matches_class {
                glob_rec(&p[i + 1..], &s[1..])
            } else {
                false
            }
        }
        c => {
            if s.is_empty() || s[0] != c {
                false
            } else {
                glob_rec(&p[1..], &s[1..])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_basename_any_depth() {
        let m = Dockerignore::parse("*.tmp\n");
        assert_eq!(m.matches("a.tmp"), Match::Ignore);
        assert_eq!(m.matches("sub/dir/f.tmp"), Match::Ignore);
        assert_eq!(m.matches("keep.txt"), Match::None);
    }

    #[test]
    fn anchored_slash() {
        let m = Dockerignore::parse("/foo\n");
        assert_eq!(m.matches("foo"), Match::Ignore);
        assert_eq!(m.matches("foo/bar"), Match::Ignore);
        assert_eq!(m.matches("sub/foo"), Match::None);
    }

    #[test]
    fn directory_trailing_slash() {
        let m = Dockerignore::parse("build/\n");
        assert_eq!(m.matches("build"), Match::Ignore);
        assert_eq!(m.matches("build/x"), Match::Ignore);
    }

    #[test]
    fn negation_reincludes() {
        let m = Dockerignore::parse("*\n!keep.txt\n");
        // `*` matches the top-level basename of everything; `!keep.txt` wins.
        assert_eq!(m.matches("keep.txt"), Match::Include);
        assert_eq!(m.matches("other.txt"), Match::Ignore);
    }

    #[test]
    fn glob_star_and_question() {
        let m = Dockerignore::parse("*.log\ndata?.bin\n");
        assert_eq!(m.matches("app.log"), Match::Ignore);
        assert_eq!(m.matches("data1.bin"), Match::Ignore);
        assert_eq!(m.matches("data12.bin"), Match::None);
    }

    #[test]
    fn double_star() {
        let m = Dockerignore::parse("**/node_modules\n");
        assert_eq!(m.matches("node_modules"), Match::Ignore);
        assert_eq!(m.matches("a/web/node_modules"), Match::Ignore);
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let m = Dockerignore::parse("# comment\n\n*.txt\n");
        assert_eq!(m.matches("a.txt"), Match::Ignore);
        assert_eq!(m.matches("a.md"), Match::None);
    }

    #[test]
    fn last_rule_wins() {
        let m = Dockerignore::parse("*.txt\n!keep.txt\n");
        assert_eq!(m.matches("x.txt"), Match::Ignore);
        assert_eq!(m.matches("keep.txt"), Match::Include);
    }
}