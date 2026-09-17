//! `basil pin`: resolve every unpinned external image base to an `@sha256`
//! digest and rewrite the Basilfile to pin it. Identity then folds the pinned
//! digest directly (no registry read at build time) — a hermetic, immutable
//! base.
//!
//! Only **external** image bases are pinned; internal stage references
//! (`FROM <stage>` naming another target) and `scratch` are left as-is.

use crate::dsl::ast;
use crate::dsl::parser;
use std::collections::HashMap;

/// The text of a `FROM` base exactly as written, reconstructed from the AST.
/// `raw` (no tag/digest) may carry a `$`-var or platform selector; the written
/// form is `raw[:tag][@digest]`.
fn written_base(b: &ast::BaseImage) -> String {
    let mut s = b.raw.clone();
    if let Some(t) = &b.tag {
        s.push(':');
        s.push_str(t);
    }
    if let Some(d) = &b.digest {
        s.push('@');
        s.push_str(d);
    }
    s
}

/// Resolve and pin each unpinned external base in `src`, returning the
/// rewritten Basilfile text. Errors (resolution failure, parse, no base) are
/// surfaced, never silently skipped.
pub async fn pin_file(src: &str) -> Result<String, String> {
    let file = parser::parse(src).map_err(|e| e.to_string())?;

    // Local stage names: a base naming a *different* target is internal, not an
    // image. The self-name case (`FROM alpine` naming a stage `alpine`) stays
    // external (mirrors compute_closure).
    let by_name: std::collections::HashSet<&str> =
        file.targets.iter().map(|t| t.name.as_str()).collect();

    // Resolve each distinct external base ref to its digest once per run.
    let mut pinned: HashMap<String, String> = HashMap::new(); // written_base -> new written_base
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for t in &file.targets {
        let crate::dsl::ast::Base::Image(b) = &t.base else {
            continue; // MODULE bases have no FROM line to pin (D5: no recursion)
        };
        if b.raw == "scratch" || b.digest.is_some() {
            continue; // no digest to add (scratch) or already pinned
        }
        if by_name.contains(b.raw.as_str()) && b.raw != t.name {
            continue; // internal stage — not an image to pin
        }
        let old = written_base(b);
        if pinned.contains_key(&old) {
            continue; // already resolved this distinct ref
        }
        let tag = b.tag.clone().unwrap_or_else(|| "latest".to_string());
        let ir = crate::solver::registry::parse_ref(&b.raw, &tag).map_err(|e| e.to_string())?;
        if !seen.insert(ir.dedup_key(&tag)) {
            continue;
        }
        let digest = ir
            .resolve_digest()
            .await
            .map_err(|e| format!("could not resolve unpinned base `{old}`: {e}"))?;
        // Pin: preserve the tag (if any) and append @digest. Drop the mutable
        // tag when none was written.
        let new = match &b.tag {
            Some(t) => format!("{}:{t}@{digest}", b.raw),
            None => format!("{}@{digest}", b.raw),
        };
        if new != old {
            pinned.insert(old.clone(), new.clone());
            tracing::info!("pin `{old}` -> `{new}`");
        }
    }

    // IMPORT refs are external images too: pin each unpinned one the same way
    // (shared `seen` dedup — a ref that already resolved as a FROM base is not
    // resolved again). Stage-name refs are skipped; compute_closure rejects
    // them with a clear message.
    let mut import_pinned: HashMap<String, String> = HashMap::new(); // written ref -> new
    for t in &file.targets {
        for instr in &t.instructions {
            let crate::dsl::ast::Instruction::Import(im) = instr else { continue };
            if im.digest.is_some() || im.raw == "scratch" {
                continue;
            }
            if by_name.contains(im.raw.as_str()) && im.raw != t.name {
                continue;
            }
            let old = written_ref(&im.raw, im.tag.as_deref());
            if import_pinned.contains_key(&old) {
                continue;
            }
            let tag = im.tag.clone().unwrap_or_else(|| "latest".to_string());
            let ir = crate::solver::registry::parse_ref(&im.raw, &tag).map_err(|e| e.to_string())?;
            if !seen.insert(ir.dedup_key(&tag)) {
                continue;
            }
            let digest = ir
                .resolve_digest()
                .await
                .map_err(|e| format!("could not resolve unpinned IMPORT `{old}`: {e}"))?;
            let new = match &im.tag {
                Some(t) => format!("{}:{t}@{digest}", im.raw),
                None => format!("{}@{digest}", im.raw),
            };
            if new != old {
                import_pinned.insert(old.clone(), new.clone());
                tracing::info!("pin IMPORT `{old}` -> `{new}`");
            }
        }
    }

    // Rewrite each `FROM ...` line in place, replacing the base token. The
    // parser records a target's FROM line number; we locate the base token
    // (after `FROM `, leading `--platform=...` allowed) and swap it.
    let mut lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
    for t in &file.targets {
        let crate::dsl::ast::Base::Image(base_image) = &t.base else { continue };
        if let Some(new_base) = pinned.get(&written_base(base_image)) {
            let idx = t.line.saturating_sub(1); // 0-based
            if let Some(line) = lines.get_mut(idx) {
                let replaced = rewrite_from_base(line, &written_base(base_image), new_base);
                if let Some(r) = replaced {
                    *line = r;
                }
            }
        }
    }

    // Rewrite each `IMPORT ...` line in place, swapping the ref token.
    for t in &file.targets {
        for instr in &t.instructions {
            let crate::dsl::ast::Instruction::Import(im) = instr else { continue };
            let old = written_ref(&im.raw, im.tag.as_deref());
            if let Some(new_ref) = import_pinned.get(&old) {
                let idx = im.line.saturating_sub(1);
                if let Some(line) = lines.get_mut(idx) {
                    if let Some(r) = rewrite_import_line(line, &old, new_ref) {
                        *line = r;
                    }
                }
            }
        }
    }

    Ok(lines.join("\n"))
}

/// `<raw>[:tag]` as written (no digest).
fn written_ref(raw: &str, tag: Option<&str>) -> String {
    match tag {
        Some(t) => format!("{raw}:{t}"),
        None => raw.to_string(),
    }
}
/// Replace the ref token in a single `IMPORT` line (preserving indentation
/// and trailing text). Returns the new line, or `None` if the old ref text
/// isn't found in the expected position.
fn rewrite_import_line(line: &str, old_ref: &str, new_ref: &str) -> Option<String> {
    let lead = line.trim_start();
    if !lead.to_ascii_uppercase().starts_with("IMPORT ") {
        return None;
    }
    let off = line.len() - lead.len();
    let rest = &lead["IMPORT ".len()..];
    let ref_end = rest
        .find(char::is_whitespace)
        .map(|n| "IMPORT ".len() + n)
        .unwrap_or(lead.len());
    if &lead["IMPORT ".len()..ref_end] != old_ref {
        return None;
    }
    Some(format!(
        "{}{new_ref}{}",
        &line[..off + "IMPORT ".len()],
        &line[off + ref_end..]
    ))
}

/// Replace the base token in a single `FROM` line (preserving the `AS` name,
/// any `--platform=` selector, and surrounding text). Returns the new line, or
/// `None` if the old base text isn't found in the expected position.
fn rewrite_from_base(line: &str, old_base: &str, new_base: &str) -> Option<String> {
    let lead = line.trim_start();
    if !lead.to_ascii_uppercase().starts_with("FROM ") {
        return None;
    }
    let off = line.len() - lead.len(); // leading-indent byte count
    const KEYWORD: &str = "FROM ";
    let mut i = KEYWORD.len(); // index into `lead`, past "FROM "
    // Skip a leading `--platform=<sel> ` flag before the base.
    if lead[i..].starts_with("--platform=") {
        let after = &lead[i + "--platform=".len()..];
        let sp = after.find(char::is_whitespace).unwrap_or(after.len());
        i += "--platform=".len() + sp;
        while lead.as_bytes().get(i).map_or(false, |b| b.is_ascii_whitespace()) {
            i += 1;
        }
    }
    let base_end = lead[i..]
        .find(char::is_whitespace)
        .map(|n| i + n)
        .unwrap_or(lead.len());
    let base_tok = &lead[i..base_end];
    if base_tok != old_base {
        return None;
    }
    Some(format!("{}{new_base}{}", &line[..off + i], &line[off + base_end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_import_ref() {
        let line = "IMPORT alpine:3";
        let out = rewrite_import_line(line, "alpine:3", "alpine:3@sha256:abc").unwrap();
        assert_eq!(out, "IMPORT alpine:3@sha256:abc");
    }

    #[test]
    fn rewrites_import_bare_and_preserves_indent() {
        let line = "  IMPORT busybox # toolchain";
        let out = rewrite_import_line(line, "busybox", "busybox@sha256:d").unwrap();
        assert_eq!(out, "  IMPORT busybox@sha256:d # toolchain");
    }

    #[test]
    fn import_line_without_match_is_none() {
        assert_eq!(rewrite_import_line("FROM alpine", "alpine", "alpine@x"), None);
        assert_eq!(rewrite_import_line("IMPORT other", "alpine", "alpine@x"), None);
    }

    #[test]
    fn written_ref_forms() {
        assert_eq!(written_ref("busybox", None), "busybox");
        assert_eq!(written_ref("busybox", Some("1.36")), "busybox:1.36");
    }

    #[test]
    fn pins_plain_base_with_as() {
        let line = "FROM ghcr.io/a/b:v2 AS x";
        let out = rewrite_from_base(line, "ghcr.io/a/b:v2", "ghcr.io/a/b@sha256:abc").unwrap();
        assert_eq!(out, "FROM ghcr.io/a/b@sha256:abc AS x");
    }

    #[test]
    fn pins_platform_base() {
        let line = "FROM --platform=$BUILDPLATFORM alpine AS x";
        let out = rewrite_from_base(line, "alpine", "alpine@sha256:def").unwrap();
        assert_eq!(out, "FROM --platform=$BUILDPLATFORM alpine@sha256:def AS x");
    }

    #[test]
    fn preserves_indent_and_trailing_comment() {
        let line = "  FROM alpine # a base";
        let out = rewrite_from_base(line, "alpine", "alpine@sha256:g").unwrap();
        assert_eq!(out, "  FROM alpine@sha256:g # a base");
    }

    #[test]
    fn non_from_is_none() {
        assert_eq!(rewrite_from_base("COPY a /b", "a", "a@x"), None);
        assert_eq!(rewrite_from_base("FROM something-else", "b", "b@x"), None);
    }
}