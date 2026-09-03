//! `derivedData(T)` — the exact context files a target's instructions read.
//!
//! Resolved at parse time from `COPY`/`ADD` sources and bind-mount
//! `from=context` sources, filtered by `.dockerignore` (Docker's rules), with
//! globs expanded against the local context. `COPY .` therefore means the whole
//! tree explicitly — there is no implicit default. The collected file set is
//! hashed as a deterministic OCI-style synthetic snapshot.

use super::dockerignore::{Dockerignore, Match};
use crate::dsl::ast::{Instruction, Target};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// A context file read by a target, with its content-bearing metadata.
///
/// How an instruction's source contributes to the closure. Only context-based
/// sources (COPY/ADD local, bind-mount from context) count; `--from=<stage>` or
/// `--from=<image>` and non-context mounts contribute nothing here.
/// Expand one source token (may contain a glob) to the set of files under the
/// context it names.
fn expand_source(
    src: &str,
    context: &Path,
    ignore: &Dockerignore,
    out: &mut BTreeSet<String>,
) -> Result<(), String> {
    let full = context.join(src);
    let pattern = full.to_string_lossy().to_string();
    // If src contains glob metachars, match; else treat as exact path.
    let has_meta = src.contains('*') || src.contains('?') || (src.contains('[') && src.contains(']'));
    if has_meta {
        let matches = glob::glob(&pattern).map_err(|e| format!("bad glob `{src}`: {e}"))?;
        let mut any = false;
        for entry in matches {
            let entry = entry.map_err(|e| format!("glob error for `{src}`: {e}"))?;
            any = true;
            collect_path(&entry, context, ignore, out)?;
        }
        if !any {
            // No glob match: Docker allows empty wildcard for COPY --from only;
            // for context, an unmatched glob is an error in a real build. For
            // identity, an unmatched glob contributes nothing, but we surface it
            // here as an explicit entry so it isn't silently dropped.
            out.insert(format!("<unmatched-glob>{src}"));
        }
    } else {
        let p = context.join(src);
        collect_path(&p, context, ignore, out)?;
    }
    Ok(())
}

/// Add a path (file or dir) and any children to the output set, dockerignore-filtered.
fn collect_path(
    p: &Path,
    context: &Path,
    ignore: &Dockerignore,
    out: &mut BTreeSet<String>,
) -> Result<(), String> {
    let rel = pathdiff(p, context, "").map_err(|e| format!("path `{p:?}` outside context: {e}"))?;
    // A source equal to the context root (`.`, `./`, `/`) means the whole tree.
    // pathdiff returns "" for the root; walk its children instead.
    if rel.is_empty() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(p)
            .map_err(|e| format!("reading context root: {e}"))?
            .filter_map(|r| r.ok().map(|d| d.path()))
            .collect();
        entries.sort();
        for child in entries {
            collect_path(&child, context, ignore, out)?;
        }
        return Ok(());
    }
    let rel_str = rel.replace('\\', "/");
    // Never include the context root itself.
    if rel_str.is_empty() {
        return Ok(());
    }
    match ignore.matches(&rel_str) {
        Match::Ignore => return Ok(()),
        Match::Include | Match::None => {}
    }
    if p.is_dir() {
        // A directory source: include the dir and everything under it.
        out.insert(rel_str.clone());
        let mut entries: Vec<PathBuf> = std::fs::read_dir(p)
            .map_err(|e| format!("reading dir `{p:?}`: {e}"))?
            .filter_map(|r| r.ok().map(|d| d.path()))
            .collect();
        entries.sort();
        for child in entries {
            collect_path(&child, context, ignore, out)?;
        }
    } else if p.is_file() {
        out.insert(rel_str);
    } else {
        // Symlink or missing. Symlinks: include as-is (the tar preserves the
        // link). Missing: surface for honest identity.
        if p.symlink_metadata().is_ok() {
            out.insert(rel_str);
        } else {
            out.insert(format!("<missing>{rel_str}"));
        }
    }
    Ok(())
}

/// Compute the relative path of `p` from `context`.
fn pathdiff(p: &Path, context: &Path, sep: &str) -> Result<String, String> {
    let _ = sep;
    let ctx = context.canonicalize().map_err(|e| format!("context `{context:?}`: {e}"))?;
    let abs = p
        .canonicalize()
        .map_err(|e| format!("path `{p:?}`: {e}"))?;
    let rel = abs
        .strip_prefix(&ctx)
        .map_err(|_| format!("`{p:?}` is not under context `{ctx:?}`"))?;
    let mut parts = Vec::new();
    for c in rel.components() {
        if let Component::Normal(s) = c {
            parts.push(s.to_string_lossy().to_string());
        }
    }
    Ok(parts.join("/"))
}

/// Which sources a target's instructions read from the context.
fn context_sources(target: &Target, context: &Path) -> Vec<String> {
    let mut srcs = Vec::new();
    // The target's own BASE `FROM <external>` is a base digest, not context data.
    for instr in &target.instructions {
        match instr {
            Instruction::Copy(c) if c.from.is_none() => {
                // context copy; but COPY --from=<stage>/<image> reads from a
                // stage, not context. Sources that look like `-` (stdin) skip.
                srcs.extend(c.sources.iter().filter(|s| s.as_str() != "-").cloned());
            }
            Instruction::Copy(_) => {
                // COPY --from=... — no context contribution.
            }
            Instruction::Add(a) => {
                // Local ADD sources contribute to context; URL sources are
                // non-hermetic (the URL string is in programHash, remote
                // content never fetched).
                for s in &a.sources {
                    if looks_like_url(s) {
                        continue;
                    }
                    srcs.push(s.clone());
                }
            }
            Instruction::Run(r) => {
                // bind mounts from context contribute; others (secret/ssh/cache)
                // do not.
                for m in &r.mounts {
                    if let Instruction::Run(_) = instr {}
                    match m {
                        crate::dsl::ast::Mount::Bind { source: Some(s), .. } => {
                            if s == "-" {
                                continue;
                            }
                            // bind mount source may be from context; the design
                            // counts `--mount=type=bind,from=context,source=...`.
                            srcs.push(s.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let _ = context;
    srcs
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
}

/// Minimal tar writer producing a deterministic snapshot (no name/mtime in
/// header, uid/gid 0, sorted entries, gzip fixed level).
fn snapshot_digest(files: &BTreeSet<String>, context: &Path) -> anyhow::Result<[u8; 32]> {
    use sha2::Digest;

    // Build a deterministic tar in memory.
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        builder.mode(tar::HeaderMode::Deterministic);
        for rel in files {
            if rel.starts_with('<') {
                continue; // synthetic markers are not real files
            }
            let full = context.join(rel);
            let meta = match std::fs::metadata(&full) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mut header = tar::Header::new_ustar();
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            if meta.is_dir() {
                header.set_entry_type(tar::EntryType::Directory);
                let mut name = rel.clone();
                if !name.ends_with('/') {
                    name.push('/');
                }
                header.set_path(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
                header.set_mode(0o755);
                builder
                    .append_data(&mut header, &name, std::io::empty())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            } else {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_path(rel).map_err(|e| anyhow::anyhow!("{e}"))?;
                header.set_size(meta.len());
                header.set_mode(0o644);
                let mut f = std::fs::File::open(&full).map_err(|e| anyhow::anyhow!("{e}"))?;
                builder
                    .append_data(&mut header, rel, &mut f)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
        builder.finish().map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // gzip at a fixed level (6) for determinism.
    let mut gz =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    gz.write_all(&tar_bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    let gz_bytes = gz.finish().map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(&gz_bytes);
    Ok(hasher.finalize().into())
}

/// Compute the derived-data for a target: the sorted set of context files it
/// reads, returned in a deterministic form.
pub fn derived_file_set(target: &Target, context: &Path) -> anyhow::Result<BTreeSet<String>> {
    let ignore = Dockerignore::from_file_if_present(context);
    let mut out = BTreeSet::new();
    for src in context_sources(target, context) {
        expand_source(&src, context, &ignore, &mut out)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(out)
}

/// Compute `derivedData(T)` as a digest.
pub fn derived_data_digest(target: &Target, context: &Path) -> anyhow::Result<[u8; 32]> {
    let files = derived_file_set(target, context)?;
    snapshot_digest(&files, context)
}

/// Compute `derivedData(T)` and the total context bytes it reads, in one walk
/// (avoids re-deriving the file set twice when the caller wants both).
pub fn derived_data_digest_and_bytes(
    target: &Target,
    context: &Path,
) -> anyhow::Result<([u8; 32], u64)> {
    let files = derived_file_set(target, context)?;
    let mut total = 0u64;
    for rel in &files {
        if rel.starts_with('<') {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(context.join(rel)) {
            total = total.saturating_add(meta.len());
        }
    }
    Ok((snapshot_digest(&files, context)?, total))
}

/// Total bytes of the context files a target's derived-data reads. Used to
/// warn when a build hashes a large context (e.g. an unexcluded big blob).
pub fn derived_data_bytes(target: &Target, context: &Path) -> anyhow::Result<u64> {
    let (_, b) = derived_data_digest_and_bytes(target, context)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse;
    use std::fs;

    fn ctx(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    #[test]
    fn detects_copy_context_files() {
        let dir = std::env::temp_dir().join(format!("basil-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "a.txt", "aaa");
        ctx(&dir, "b.txt", "bbb");
        fs::create_dir_all(dir.join("sub")).unwrap();
        ctx(&dir.join("sub"), "c.txt", "ccc");
        let dsl = "FROM alpine\nCOPY a.txt /x\n";
        let f = parse(dsl).unwrap();
        let files = derived_file_set(&f.targets[0], &dir).unwrap();
        assert!(files.contains("a.txt"));
        assert!(!files.contains("b.txt"));
        assert!(!files.contains("sub/c.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_whole_tree_copy() {
        let dir = std::env::temp_dir().join(format!("basil-test2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "a.txt", "a");
        ctx(&dir.join("sub"), "b.txt", "b");
        let dsl = "FROM alpine\nCOPY . /src\n";
        let f = parse(dsl).unwrap();
        let files = derived_file_set(&f.targets[0], &dir).unwrap();
        assert!(files.contains("a.txt"));
        assert!(files.contains("sub/b.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dockerignore_filters_copy() {
        let dir = std::env::temp_dir().join(format!("basil-test3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, ".dockerignore", "node_modules\n*.tmp\n");
        ctx(&dir, "a.txt", "a");
        ctx(&dir, "node_modules/x.js", "x");
        ctx(&dir, "f.tmp", "f");
        let dsl = "FROM alpine\nCOPY . /src\n";
        let f = parse(dsl).unwrap();
        let files = derived_file_set(&f.targets[0], &dir).unwrap();
        assert!(files.contains("a.txt"));
        assert!(!files.contains("f.tmp"));
        assert!(!files.contains("node_modules/x.js"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_source_expands() {
        let dir = std::env::temp_dir().join(format!("basil-test4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "a.json", "{}");
        ctx(&dir, "b.json", "{}");
        ctx(&dir, "c.txt", "x");
        let dsl = "FROM alpine\nCOPY *.json /x/\n";
        let f = parse(dsl).unwrap();
        let files = derived_file_set(&f.targets[0], &dir).unwrap();
        assert!(files.contains("a.json"));
        assert!(files.contains("b.json"));
        assert!(!files.contains("c.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_mount_source_counts() {
        let dir = std::env::temp_dir().join(format!("basil-test5-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "Makefile", "all:\n");
        let dsl = "FROM alpine\nRUN --mount=type=bind,source=Makefile,target=/m/Makefile make\n";
        let f = parse(dsl).unwrap();
        let files = derived_file_set(&f.targets[0], &dir).unwrap();
        assert!(files.contains("Makefile"));
        let _ = fs::remove_dir_all(&dir);
    }
}