//! `derivedData(T)` — the exact context files a target's instructions read.
//!
//! Resolved at parse time from `COPY`/`ADD` sources and bind-mount
//! `from=context` sources, filtered by `.dockerignore` (Docker's rules), with
//! globs expanded against the local context. `COPY .` therefore means the whole
//! tree explicitly — there is no implicit default. The git metadata directory
//! (`.git`, at any depth) is always excluded, like Docker's context archive.
//!
//! The collected file set is digested as a synthetic **git tree object** built
//! with git's own object model: a subtree whose content the local git index
//! already covers — fully tracked, unmodified, and entirely contained in the
//! derived set — contributes git's precomputed tree/blob hashes instead of
//! re-hashing file contents; partial (dockerignore/GIVEN/glob-filtered) or
//! dirty (modified/untracked) subtrees are hashed from the working tree. A
//! context outside any git repository has nothing borrowable and is fully
//! computed — the same object model, so the digest stays deterministic either
//! way. Because the digest commits to git's modes, the executable bit of a
//! context file now changes identity (the old tar snapshot forced fixed modes).

use super::dockerignore::{Dockerignore, Match};
use crate::dsl::ast::{Instruction, Target};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use sha1::Digest as Sha1Digest;
use sha2::Digest as Sha2Digest;

/// Git tree-entry modes (git's canonical octal strings, no leading zero).
const MODE_FILE: &str = "100644";
const MODE_EXEC: &str = "100755";
const MODE_LINK: &str = "120000";
const MODE_TREE: &str = "40000";

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
) -> anyhow::Result<()> {
    let full = context.join(src);
    let pattern = full.to_string_lossy().to_string();
    // If src contains glob metachars, match; else treat as exact path.
    let has_meta = src.contains('*') || src.contains('?') || (src.contains('[') && src.contains(']'));
    if has_meta {
        let matches = glob::glob(&pattern)
            .map_err(|e| crate::fsctx::with_filename(anyhow::anyhow!("bad glob: {e}"), &full, "glob"))?;
        let mut any = false;
        for entry in matches {
            let entry = entry.map_err(|e| {
                crate::fsctx::with_filename(anyhow::anyhow!("glob error: {e}"), &full, "glob")
            })?;
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
        // An explicit (non-glob) source that's excluded by .dockerignore is a
        // hard error, matching buildah/Docker ("no items matching glob ...
        // filtered out using .dockerignore"). Silently dropping it would hash an
        // identity for an input the build will reject. The always-excluded
        // `.git` metadata follows the same rule.
        if let Ok(rel) = pathdiff(&p, context, "") {
            let rel_str = rel.replace('\\', "/");
            if !rel_str.is_empty() {
                if is_git_metadata_path(&rel_str) {
                    anyhow::bail!(
                        "`{src}` is inside `.git`, which is always excluded from the build context"
                    );
                }
                if let Match::Ignore = ignore.matches(&rel_str) {
                    anyhow::bail!(
                        "`{src}` is excluded by .dockerignore but explicitly listed as an ADD/COPY source"
                    );
                }
            }
        }
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
) -> anyhow::Result<()> {
    let rel = pathdiff(p, context, "").map_err(|e| anyhow::anyhow!("path `{p:?}` outside context: {e}"))?;
    // A source equal to the context root (`.`, `./`, `/`) means the whole tree.
    // pathdiff returns "" for the root; walk its children instead.
    if rel.is_empty() {
        let mut entries: Vec<PathBuf> = crate::fsctx::fs_err(std::fs::read_dir(p), p, "read_dir")?
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
    // The git metadata directory is never a build input (Docker excludes it
    // from the context archive at any depth too): skip it and never descend.
    if is_git_metadata_path(&rel_str) {
        return Ok(());
    }
    match ignore.matches(&rel_str) {
        Match::Ignore => return Ok(()),
        Match::Include | Match::None => {}
    }
    // A symlink is its own build input: git represents links as a 120000 blob
    // of the target string, and buildah copies them as links — never dive the
    // target.
    if p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        out.insert(rel_str);
        return Ok(());
    }
    if p.is_dir() {
        // A directory source: include the dir and everything under it.
        out.insert(rel_str.clone());
        let mut entries: Vec<PathBuf> = crate::fsctx::fs_err(std::fs::read_dir(p), p, "read_dir")?
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

/// Normalize a path's components lexically (resolve `.`/`..` textually, without
/// touching the filesystem, so symlinks keep their own path).
fn lexical_components<'a>(path: &'a Path) -> Vec<Component<'a>> {
    let mut out: Vec<Component<'a>> = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Compute the relative path of `p` from `context`, lexically: a symlink's
/// identity is the link itself (git's 120000 entries; buildah copies links as
/// links), never the target it points at.
fn pathdiff(p: &Path, context: &Path, sep: &str) -> Result<String, String> {
    let _ = sep;
    let ctx = std::path::absolute(context).map_err(|e| format!("context `{context:?}`: {e}"))?;
    let abs = std::path::absolute(p).map_err(|e| format!("path `{p:?}`: {e}"))?;
    let ctx_c = lexical_components(&ctx);
    let abs_c = lexical_components(&abs);
    if abs_c.len() < ctx_c.len() || abs_c[..ctx_c.len()] != ctx_c {
        return Err(format!("`{p:?}` is not under context `{context:?}`"));
    }
    let mut parts = Vec::new();
    for c in &abs_c[ctx_c.len()..] {
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

/// Is any path component named `.git` (the metadata dir or a worktree-pointer
/// file)? Such paths are always excluded from identity, like Docker's context
/// archive.
fn is_git_metadata_path(rel: &str) -> bool {
    rel.split('/').any(|c| c == ".git")
}

// ---------------------------------------------------------------------------
// Git-tree digest
// ---------------------------------------------------------------------------

/// The enclosing repository's object hash ("sha1" default; sha256-format repos
/// use sha256). Computed objects must match the repo's format so borrowed and
/// computed hashes stay interchangeable.
#[derive(Clone, Copy)]
enum Algo {
    Sha1,
    Sha256,
}

impl Algo {
    /// `H("<type> <len>\0" ++ payload)` — git's object hash.
    fn object(self, type_: &str, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(type_.len() + 32 + payload.len());
        buf.extend_from_slice(type_.as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(payload.len().to_string().as_bytes());
        buf.push(0);
        buf.extend_from_slice(payload);
        match self {
            Algo::Sha1 => {
                let mut h: sha1::Sha1 = Default::default();
                Sha1Digest::update(&mut h, &buf);
                Sha1Digest::finalize(h).as_slice().to_vec()
            }
            Algo::Sha256 => {
                let mut h: sha2::Sha256 = Default::default();
                Sha2Digest::update(&mut h, &buf);
                Sha2Digest::finalize(h).as_slice().to_vec()
            }
        }
    }
}

/// Git state under the build context, snapshotted once per closure computation.
///
/// An empty state (no enclosing work tree, or any probe failure) makes every
/// tree "dirty": nothing borrows and the digest is fully computed.
pub struct RepoState {
    algo: Algo,
    /// Context relative to the repo toplevel ("" when the context IS the
    /// toplevel; `sub` for a context at `<toplevel>/sub`).
    prefix: String,
    /// `<prefix>/`, for stripping toplevel-relative paths back to context space.
    prefix_slash: String,
    /// Tracked blobs under the context: toplevel-relative path -> (mode, sha).
    index: BTreeMap<String, (String, Vec<u8>)>,
    /// Directory subtree hashes: toplevel-relative dir path -> tree sha. The
    /// context root itself is always present (the `write-tree` root when the
    /// context is the toplevel, or its own `ls-tree` entry otherwise).
    trees: BTreeMap<String, Vec<u8>>,
    /// Worktree-changed paths (modified/deleted/untracked/typechange) —
    /// context-relative.
    dirty: BTreeSet<String>,
    /// Index paths whose worktree copy cannot be trusted (skip-worktree /
    /// assume-unchanged / unmerged) — toplevel-relative.
    unreliable: BTreeSet<String>,
}

impl RepoState {
    fn empty() -> Self {
        RepoState {
            algo: Algo::Sha1,
            prefix: String::new(),
            prefix_slash: String::new(),
            index: BTreeMap::new(),
            trees: BTreeMap::new(),
            dirty: BTreeSet::new(),
            unreliable: BTreeSet::new(),
        }
    }

    /// Context-relative path -> toplevel-relative path.
    fn trel<'a>(&self, rel: &'a str) -> Cow<'a, str> {
        if self.prefix.is_empty() {
            Cow::Borrowed(rel)
        } else {
            Cow::Owned(format!("{}/{}", self.prefix, rel))
        }
    }

    /// Toplevel-relative path -> context-relative path (None if not under the
    /// context).
    fn ctx_rel<'a>(&self, top: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            return Some(top);
        }
        top.strip_prefix(&self.prefix_slash)
    }

    /// May `dir`'s content be borrowed wholesale from the index tree?
    ///
    /// All of: the dir exists in the index tree; every derived member under it
    /// is tracked and every tracked entry under it is in the derived set
    /// (nothing filtered, nothing untracked); no worktree changes under it; no
    /// skip-worktree/assume-unchanged entries; and no index blob that conflicts
    /// with a directory the walk dived into (gitlink, symlink-to-dir).
    fn dir_borrowable(&self, dir: &str, files: &BTreeSet<String>) -> bool {
        let top = self.trel(dir);
        if !self.trees.contains_key(top.as_ref()) {
            return false;
        }
        // Every set member under `dir` must be tracked (file blob or subtree).
        for m in under(files, dir) {
            if m.starts_with('<') {
                continue; // synthetic markers are never real paths
            }
            let t = self.trel(m);
            if !self.index.contains_key(t.as_ref()) && !self.trees.contains_key(t.as_ref()) {
                return false;
            }
        }
        // Every tracked blob under `dir` must be covered by the set,
        // worktree-clean, and not encoded as a blob where the walk saw a
        // directory (gitlink, symlink-to-dir).
        for (path, (mode, _)) in under_map(&self.index, top.as_ref()) {
            if self.unreliable.contains(path) {
                return false;
            }
            let Some(ctx) = self.ctx_rel(path) else { return false };
            if !files.contains(ctx) {
                return false;
            }
            if !matches!(mode.as_str(), MODE_FILE | MODE_EXEC | MODE_LINK) {
                return false;
            }
            if has_children(files, ctx) {
                return false;
            }
        }
        // Every tracked subtree under `dir` must be covered by the set (a
        // filtered tracked dir would otherwise be smuggled in by the borrow).
        for (path, _) in under_map(&self.trees, top.as_ref()) {
            let Some(ctx) = self.ctx_rel(path) else { return false };
            if !files.contains(ctx) {
                return false;
            }
        }
        // No worktree changes under `dir`.
        !under(&self.dirty, dir).next().is_some()
    }
}

/// Probe the git state under `context`: index tree, tracked blobs/subtrees,
/// worktree changes, and the object format. Probes read metadata only — never
/// file contents. Failures (not a work tree, corrupt index, git absent) yield
/// an empty state: nothing borrows, everything is computed.
pub fn discover_git(context: &Path) -> RepoState {
    let empty = RepoState::empty();
    let Some(meta) = git(context, &["rev-parse", "--show-toplevel", "--show-prefix", "--show-object-format"])
    else {
        return empty;
    };
    let mut lines = meta.lines();
    let toplevel = lines.next().unwrap_or("").trim_end();
    if toplevel.is_empty() {
        return empty;
    }
    let prefix = lines.next().unwrap_or("").trim_end_matches('/').to_string();
    let algo = match lines.next() {
        Some(f) if f.trim_end() == "sha256" => Algo::Sha256,
        _ => Algo::Sha1,
    };
    let Some(root) = git(context, &["write-tree"]) else { return empty };
    let Some(root_sha) = hex_decode(root.trim_end()) else { return empty };
    let mut st = RepoState {
        algo,
        prefix: prefix.clone(),
        prefix_slash: if prefix.is_empty() { String::new() } else { format!("{}/", prefix) },
        index: BTreeMap::new(),
        trees: BTreeMap::new(),
        dirty: BTreeSet::new(),
        unreliable: BTreeSet::new(),
    };
    // Recursive listing under the context (includes the context's own subtree
    // entry when the context is not the toplevel).
    let mut args: Vec<&str> = vec!["ls-tree", "-r", "-t", "-z", root.trim_end()];
    if !prefix.is_empty() {
        args.push("--");
        args.push(&prefix);
    }
    let Some(listing) = git(context, &args) else { return empty };
    for record in listing.split('\0') {
        if record.is_empty() {
            continue;
        }
        let Some((meta, path)) = record.split_once('\t') else { continue };
        let mut parts = meta.splitn(3, ' ');
        let (Some(mode), Some(ty), Some(sha_hex)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(sha) = hex_decode(sha_hex) else { continue };
        if ty == "tree" {
            st.trees.insert(path.to_string(), sha);
        } else {
            st.index.insert(path.to_string(), (mode.to_string(), sha));
        }
    }
    if prefix.is_empty() {
        st.trees.insert(String::new(), root_sha);
    }
    // Worktree changes under the context (relative to the index).
    let Some(status) = git(
        context,
        &["status", "--porcelain", "-z", "--untracked-files=all", "--no-renames", "--", "."],
    ) else {
        return empty;
    };
    for rec in status.split('\0') {
        if rec.len() > 3 && rec.as_bytes()[2] == b' ' {
            st.dirty.insert(rec[3..].to_string());
        }
    }
    // skip-worktree / assume-unchanged / unmerged entries: their worktree copy
    // is not a reliable representation of the index blob.
    let Some(flags) = git(context, &["ls-files", "-v", "-z"]) else { return empty };
    for rec in flags.split('\0') {
        if rec.len() > 2 && rec.as_bytes()[1] == b' ' {
            let flag = rec.as_bytes()[0];
            if matches!(flag, b'S' | b'h' | b'M') {
                st.unreliable.insert(rec[2..].to_string());
            }
        }
    }
    st
}

/// Run `git -C <context> <args>` and return stdout on success.
fn git(context: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(context)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Decode a hex object id to raw bytes (40 for sha1, 64 for sha256).
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        out.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
    }
    Some(out)
}

/// Exclusive upper bound for a `prefix/` range: prefix with its last byte
/// incremented ('/' → '0'), the smallest string no path under the prefix can
/// reach.
fn upper_bound(prefix: &str) -> String {
    debug_assert!(prefix.ends_with('/'));
    let mut b = prefix.as_bytes().to_vec();
    *b.last_mut().unwrap() += 1;
    String::from_utf8(b).unwrap()
}

/// Iterate members of `set` living under context-relative `dir` ("" = context
/// root → all members).
fn under<'a>(set: &'a BTreeSet<String>, dir: &str) -> impl Iterator<Item = &'a str> + 'a {
    let lower: String = if dir.is_empty() { String::new() } else { format!("{}/", dir) };
    let upper: String = if dir.is_empty() { "\u{10ffff}".to_string() } else { upper_bound(&lower) };
    set.range(lower..upper).map(|s| s.as_str())
}

/// Iterate entries of `map` living under toplevel-relative `top` ("" =
/// everything).
fn under_map<'a, V>(map: &'a BTreeMap<String, V>, top: &str) -> impl Iterator<Item = (&'a str, &'a V)> + 'a {
    let lower: String = if top.is_empty() { String::new() } else { format!("{}/", top) };
    let upper: String = if top.is_empty() { "\u{10ffff}".to_string() } else { upper_bound(&lower) };
    map.range(lower..upper).map(|(k, v)| (k.as_str(), v))
}

/// Does the set contain any member under `rel/`?
fn has_children(files: &BTreeSet<String>, rel: &str) -> bool {
    under(files, rel).next().is_some()
}

/// Digest the derived file set as a synthetic git tree rooted at the context,
/// returning `(digest, bytes_read)` — the latter counts only content basil
/// actually reads from the working tree while hashing; subtrees/blobs covered
/// by a borrowed git tree hash contribute zero, so a large clean tracked blob
/// never trips the large-context warning.
fn tree_digest(files: &BTreeSet<String>, context: &Path, repo: &RepoState) -> anyhow::Result<(Vec<u8>, u64)> {
    let mut bytes_read = 0u64;
    let digest = digest_dir("", files, context, repo, &mut bytes_read)?;
    Ok((digest, bytes_read))
}

/// Digest one directory node: its borrowed index-tree hash when borrowable,
/// else a composed git tree object over its children (git order: dir names
/// sort as if they carried a trailing '/').
fn digest_dir(
    dir: &str,
    files: &BTreeSet<String>,
    context: &Path,
    repo: &RepoState,
    bytes_read: &mut u64,
) -> anyhow::Result<Vec<u8>> {
    if repo.dir_borrowable(dir, files) {
        let top = repo.trel(dir);
        tracing::debug!("identity: subtree `{dir}` covered by git tree hash (not read)");
        return Ok(repo.trees[top.as_ref()].clone());
    }
    // First-segment children in set order (the set's byte order is already
    // git's: "a.txt" < "a/").
    let base = if dir.is_empty() { 0 } else { dir.len() + 1 };
    let mut names: Vec<(String, bool)> = Vec::new();
    for m in under(files, dir) {
        if m.starts_with('<') {
            continue;
        }
        let rest = &m[base..];
        let (name, tail) = match rest.find('/') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        if names.last().map(|(n, _)| n != name).unwrap_or(true) {
            let full = if dir.is_empty() {
                context.join(name)
            } else {
                context.join(dir).join(name)
            };
            let is_dir = tail || std::fs::symlink_metadata(&full).map(|m| m.is_dir()).unwrap_or(false);
            names.push((name.to_string(), is_dir));
        }
    }
    let mut entries: Vec<u8> = Vec::new();
    for (name, is_dir) in &names {
        let rel = if dir.is_empty() { name.clone() } else { format!("{}/{}", dir, name) };
        let (mode, sha) = if *is_dir {
            (String::from(MODE_TREE), digest_dir(&rel, files, context, repo, bytes_read)?)
        } else {
            leaf_digest(&rel, context, repo, bytes_read)?
        };
        entries.extend_from_slice(mode.as_bytes());
        entries.push(b' ');
        entries.extend_from_slice(name.as_bytes());
        entries.push(0);
        entries.extend_from_slice(&sha);
    }
    Ok(repo.algo.object("tree", &entries))
}

/// Digest one file leaf: the tracked, unmodified index blob when borrowable,
/// else the working-tree content with git's mode convention (any exec bit ->
/// 100755; symlinks encode their target). Only the compute path reads bytes.
fn leaf_digest(
    rel: &str,
    context: &Path,
    repo: &RepoState,
    bytes_read: &mut u64,
) -> anyhow::Result<(String, Vec<u8>)> {
    if let Some((mode, sha)) = repo.index.get(repo.trel(rel).as_ref()) {
        if matches!(mode.as_str(), MODE_FILE | MODE_EXEC | MODE_LINK)
            && !repo.dirty.contains(rel)
            && !repo.unreliable.contains(repo.trel(rel).as_ref())
        {
            tracing::debug!("identity: `{rel}` covered by git index blob (not read)");
            return Ok((mode.clone(), sha.clone()));
        }
    }
    let full = context.join(rel);
    let meta = crate::fsctx::fs_err(std::fs::symlink_metadata(&full), &full, "stat")?;
    let (mode, content) = if meta.file_type().is_symlink() {
        let target = crate::fsctx::fs_err(std::fs::read_link(&full), &full, "read_link")?;
        (String::from(MODE_LINK), target.as_os_str().as_bytes().to_vec())
    } else {
        let mode = if meta.permissions().mode() & 0o111 != 0 { MODE_EXEC } else { MODE_FILE };
        let content = crate::fsctx::fs_err(std::fs::read(&full), &full, "open")?;
        (String::from(mode), content)
    };
    tracing::debug!("identity: read `{rel}` ({} bytes)", content.len());
    *bytes_read += content.len() as u64;
    Ok((mode, repo.algo.object("blob", &content)))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute the derived-data for a target: the sorted set of context files it
/// reads, returned in a deterministic form.
///
/// When the target declares `GIVEN <path>…`, that explicit path set replaces
/// the COPY/ADD-derived set — `derivedData(T)` means exactly the given inputs.
pub fn derived_file_set(target: &Target, context: &Path, build_file: &Path) -> anyhow::Result<BTreeSet<String>> {
    let ignore = Dockerignore::from_file_if_present(context);
    let mut out = BTreeSet::new();
    // GIVEN overrides the input set: hash exactly those context paths instead
    // of the build-resolved sources. Still dockerignore-filtered (a GIVEN path
    // excluded by .dockerignore is a real input mismatch, so it errors via
    // expand_source's explicit-source rule).
    let sources: Vec<String> = match &target.given {
        Some(given) => given.clone(),
        None => context_sources(target, context),
    };
    for src in &sources {
        expand_source(src, context, &ignore, &mut out)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    // Exclude the build file that produced this target graph: its content is
    // already represented by programHash, so hashing it here would make identity
    // self-referential (editing the recipe would change the derived-data hash as
    // well as the program hash). Resolve both sides absolute for a fair compare.
    let build_path = std::path::absolute(context.join(build_file))
        .unwrap_or_else(|_| build_file.to_path_buf());
    out.retain(|rel| {
        let abs = std::path::absolute(context.join(rel)).unwrap_or_else(|_| context.join(rel));
        abs != build_path
    });
    Ok(out)
}

/// Compute `derivedData(T)` as a git-tree-object digest.
pub fn derived_data_digest(
    target: &Target,
    context: &Path,
    build_file: &Path,
    repo: &RepoState,
) -> anyhow::Result<Vec<u8>> {
    let files = derived_file_set(target, context, build_file)?;
    Ok(tree_digest(&files, context, repo)?.0)
}

/// Compute `derivedData(T)` and the total context bytes basil actually reads
/// while hashing it (git-borrowed content is never read and counts 0).
pub fn derived_data_digest_and_bytes(
    target: &Target,
    context: &Path,
    build_file: &Path,
    repo: &RepoState,
) -> anyhow::Result<(Vec<u8>, u64)> {
    let files = derived_file_set(target, context, build_file)?;
    tree_digest(&files, context, repo)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
fn tmpdir(tag: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    let dir = std::env::temp_dir().join(format!("basil-git-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
fn write_ctx(dir: &std::path::Path, name: &str, content: &str) {
    let full = dir.join(name);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

#[cfg(test)]
fn git_ok() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
fn git_run(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Init a repo at `dir`, committing `files` (rel path -> content); `exec`
/// lists paths made executable; `link` lists (rel, target) symlinks.
#[cfg(test)]
fn make_repo(dir: &std::path::Path, files: &[(&str, &str)], exec: &[&str], links: &[(&str, &str)]) {
    use std::os::unix::fs::symlink;
    for (name, content) in files {
        write_ctx(dir, name, content);
    }
    for name in exec {
        let full = dir.join(name);
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (name, target) in links {
        symlink(target, dir.join(name)).unwrap();
    }
    git_run(dir, &["init", "-q"]);
    git_run(dir, &["config", "user.email", "t@t"]);
    git_run(dir, &["config", "user.name", "t"]);
    git_run(dir, &["add", "-A"]);
    // Disable commit signing for the fixture: the ambient config may enable
    // SSH signing with an agent socket that isn't reachable from the test
    // process, which would fail the fixture's own git commit.
    git_run(dir, &["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
}

#[cfg(test)]
fn head_tree(dir: &std::path::Path) -> String {
    git_run(dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string()
}

#[test]
fn whole_clean_tree_digest_equals_git_tree() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t1");
    make_repo(
        &dir,
        &[
            ("a.txt", "hello"),
            ("sub/b.txt", "x"),
            ("sub/deep/c.txt", "nested"),
            ("run.sh", "#!/bin/sh\necho hi\n"),
        ],
        &["run.sh"],
        &[("link", "a.txt")],
    );
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let dig = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    // `.git` is always excluded from the walk, so a clean repo-root context is
    // exactly its tracked tree: the root borrows and the digest equals git's
    // own tree hash byte-for-byte, with no .dockerignore needed.
    assert_eq!(hex(&dig), head_tree(&dir), "clean full-tree digest must equal git's own tree hash");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn git_metadata_is_never_hashed() {
    // A plain context with a `.git`-named directory (no enclosing repo): its
    // contents must be invisible to identity, exactly like Docker's context
    // archive which drops `.git` at any depth.
    let dir = tmpdir("t1b");
    write_ctx(&dir, "a.txt", "hello");
    write_ctx(&dir, ".git/HEAD", "ref: refs/heads/main\n");
    write_ctx(&dir, ".git/config", "[core]\n");
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let d1 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    let (_, bytes) = derived_data_digest_and_bytes(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_eq!(bytes, "hello".len() as u64, "only the real file is read");
    write_ctx(&dir, ".git/HEAD", "ref: refs/heads/other\n");
    write_ctx(&dir, ".git/objects/x", "churn");
    let repo2 = discover_git(&dir);
    let d2 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(d1, d2, ".git content must never feed identity");
    // Editing the real file must still move the digest.
    write_ctx(&dir, "a.txt", "hello world");
    let repo3 = discover_git(&dir);
    let d3 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo3).unwrap();
    assert_ne!(d3, d1);
    // An explicit source under .git is a hard error, like a dockerignored one.
    let err = derived_file_set(
        &crate::dsl::parser::parse("FROM alpine\nCOPY .git/HEAD /x\n").unwrap().targets[0],
        &dir,
        std::path::Path::new("__buildfile__"),
    )
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains(".git"), "expected the .git source to be named:\n{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dirty_and_untracked_files_break_the_borrow() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t2");
    make_repo(&dir, &[("a.txt", "hello"), ("b.txt", "x")], &[], &[]);
    let clean = head_tree(&dir);
    // Uncommitted edit: computed from the worktree.
    write_ctx(&dir, "a.txt", "hello world");
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let dirty1 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_ne!(hex(&dirty1), clean);
    // Deterministic across recomputation.
    let repo2 = discover_git(&dir);
    let dirty2 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(dirty1, dirty2);
    // Untracked file: also dirty, and its content feeds the digest.
    write_ctx(&dir, "u.txt", "untracked");
    let repo3 = discover_git(&dir);
    let with_u = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo3).unwrap();
    assert_ne!(with_u, dirty1);
    write_ctx(&dir, "u.txt", "changed untracked");
    let repo4 = discover_git(&dir);
    let with_u2 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo4).unwrap();
    assert_ne!(with_u2, with_u);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gitignored_or_dockerignored_files() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t3");
    make_repo(&dir, &[("a.txt", "hello")], &[], &[]);
    let clean = head_tree(&dir);
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    // .gitignore'ed but present + no .dockerignore: the file is in the walked
    // set, so identity covers it (computed; the digest must not borrow past it).
    write_ctx(&dir, ".gitignore", "u.txt\n");
    write_ctx(&dir, "u.txt", "ignored");
    let repo = discover_git(&dir);
    let with_u = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_ne!(hex(&with_u), clean);
    // Once dockerignore excludes those too (plus the ignore files themselves),
    // the set is exactly the tracked tree again: the digest returns to
    // equality with git's tree (.git is excluded automatically).
    write_ctx(&dir, ".dockerignore", "u.txt\n.gitignore\n.dockerignore\n");
    let repo2 = discover_git(&dir);
    let back = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(hex(&back), clean);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dockerignored_tracked_file_is_not_smuggled() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t4");
    make_repo(&dir, &[("a.txt", "hello"), ("b.txt", "x")], &[], &[]);
    let clean = head_tree(&dir);
    // b.txt is a build input to git, but .dockerignore excludes it from the
    // build: identity must NOT carry it (no borrow), and edits to it must not
    // churn the tag.
    write_ctx(&dir, ".dockerignore", "b.txt\n");
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let d1 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_ne!(hex(&d1), clean);
    write_ctx(&dir, "b.txt", "excluded from build");
    let repo2 = discover_git(&dir);
    let d2 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(d1, d2, "editing a dockerignored tracked file must not change identity");
    write_ctx(&dir, "a.txt", "hello again");
    let repo3 = discover_git(&dir);
    let d3 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo3).unwrap();
    assert_ne!(d3, d1, "editing an included file must change identity");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn given_full_listing_equals_git_tree() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t5");
    make_repo(
        &dir,
        &[("a.txt", "hello"), ("run.sh", "#!/bin/sh\necho hi\n"), ("sub/b.txt", "x")],
        &["run.sh"],
        &[],
    );
    // GIVEN naming every tracked file = the whole-tree set: borrow applies and
    // the digest equals git's tree hash.
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\nGIVEN a.txt run.sh sub/b.txt\n").unwrap();
    let repo = discover_git(&dir);
    let dig = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_eq!(hex(&dig), head_tree(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn given_subtree_borrows_and_wraps() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t6");
    make_repo(&dir, &[("a.txt", "hello"), ("sub/b.txt", "x")], &[], &[]);
    // GIVEN sub/ dives that subtree only: the borrowed `sub` tree hash is
    // wrapped in a synthetic root tree with a single entry.
    let f = crate::dsl::parser::parse("FROM alpine\nGIVEN sub/\n").unwrap();
    let repo = discover_git(&dir);
    let dig = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    let sub_tree = git_run(&dir, &["rev-parse", "HEAD:sub"]).trim().to_string();
    let sub_raw = hex_decode(&sub_tree).unwrap();
    let mut entry = Vec::new();
    entry.extend_from_slice(b"40000 sub\0");
    entry.extend_from_slice(&sub_raw);
    let mut h: sha1::Sha1 = Default::default();
    Sha1Digest::update(&mut h, b"tree ");
    Sha1Digest::update(&mut h, entry.len().to_string().as_bytes());
    Sha1Digest::update(&mut h, &[0]);
    Sha1Digest::update(&mut h, &entry);
    let expected: Vec<u8> = Sha1Digest::finalize(h).as_slice().to_vec();
    assert_eq!(dig, expected, "wrapped subtree hash mismatch");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn context_inside_repo_uses_prefix() {
    if !git_ok() {
        return;
    }
    let dir = tmpdir("t7");
    make_repo(&dir, &[("outer.txt", "o"), ("ctx/a.txt", "hello"), ("ctx/sub/b.txt", "x")], &[], &[]);
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let ctx = dir.join("ctx");
    let repo = discover_git(&ctx);
    let dig = derived_data_digest(&f.targets[0], &ctx, std::path::Path::new("__buildfile__"), &repo).unwrap();
    // context == <toplevel>/ctx → the digest is exactly the ctx subtree hash.
    let expect = git_run(&dir, &["rev-parse", "HEAD:ctx"]).trim().to_string();
    assert_eq!(hex(&dig), expect);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_repo_digest_matches_git_blob_composition() {
    if !git_ok() {
        return;
    }
    // A plain directory (no git) hashes blobs and trees the same way git
    // would: digest the working files, then make the directory a repo and
    // dockerignore the repo's own artifact (.dockerignore itself — it stays
    // untracked, so git's tree does not contain it either; `.git` is always
    // excluded). All three digests must be identical: computed == borrowed
    // composition, and both == git's own tree hash for the same content.
    let dir = tmpdir("t8");
    write_ctx(&dir, "a.txt", "hello");
    write_ctx(&dir, "sub/b.txt", "x");
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    assert!(repo.index.is_empty(), "plain dir must have nothing borrowable");
    let plain = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    make_repo(&dir, &[], &[], &[]);
    write_ctx(&dir, ".dockerignore", ".dockerignore\n");
    let repo2 = discover_git(&dir);
    let in_git = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(hex(&plain), hex(&in_git), "computed digest must equal the git composition");
    assert_eq!(hex(&plain), head_tree(&dir), "both must equal git's own tree hash");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn size_counts_only_unborrowed_bytes() {
    if !git_ok() {
        return;
    }
    // A big tracked file is covered by a borrowed git tree hash: basil never
    // reads it, so the derived-data byte count (the large-context warning
    // input) must stay 0. Once it becomes dirty, the bytes it occupies are
    // counted.
    let big = "x".repeat(1 << 20); // 1 MiB
    let dir = tmpdir("t11");
    make_repo(&dir, &[("big.bin", &big), ("small.txt", "s")], &[], &[]);
    write_ctx(&dir, ".dockerignore", ".dockerignore\n");
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let (_, bytes) = derived_data_digest_and_bytes(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    assert_eq!(bytes, 0, "clean tracked content is borrowed, nothing is read");
    write_ctx(&dir, "big.bin", &format!("{big}!"));
    let repo2 = discover_git(&dir);
    let (_, bytes2) = derived_data_digest_and_bytes(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_eq!(bytes2, (big.len() + 1) as u64, "the dirty blob's read bytes are counted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exec_bit_changes_identity() {
    if !git_ok() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tmpdir("t9");
    make_repo(&dir, &[("tool.sh", "#!/bin/sh\n")], &[], &[]);
    let f = crate::dsl::parser::parse("FROM alpine\nCOPY . /x\n").unwrap();
    let repo = discover_git(&dir);
    let d1 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    // chmod +x on a tracked file is a worktree change AND feeds a different
    // mode into the tree: identity must move.
    std::fs::set_permissions(dir.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo2 = discover_git(&dir);
    let d2 = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo2).unwrap();
    assert_ne!(d1, d2, "the executable bit must be part of identity");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_dir_given_gets_synthetic_empty_tree() {
    // git cannot represent empty directories; a GIVEN dive over one must still
    // fold a deterministic zero-entry tree into the root.
    let dir = tmpdir("t10");
    std::fs::create_dir_all(dir.join("emptydir")).unwrap();
    write_ctx(&dir, "a.txt", "hello");
    let f = crate::dsl::parser::parse("FROM alpine\nGIVEN emptydir/ a.txt\n").unwrap();
    let repo = discover_git(&dir);
    let dig = derived_data_digest(&f.targets[0], &dir, std::path::Path::new("__buildfile__"), &repo).unwrap();
    // Expected root: "100644 a.txt" (blob of "hello") then "40000 emptydir"
    // -> the synthesized empty tree (git cannot represent empty directories).
    let mut h: sha1::Sha1 = Default::default();
    Sha1Digest::update(&mut h, b"blob 5\0hello");
    let a_blob: Vec<u8> = Sha1Digest::finalize(h).as_slice().to_vec();
    let mut h2: sha1::Sha1 = Default::default();
    Sha1Digest::update(&mut h2, b"tree 0\0");
    let empty_tree: Vec<u8> = Sha1Digest::finalize(h2).as_slice().to_vec();
    let mut entry = Vec::new();
    entry.extend_from_slice(b"100644 a.txt\0");
    entry.extend_from_slice(&a_blob);
    entry.extend_from_slice(b"40000 emptydir\0");
    entry.extend_from_slice(&empty_tree);
    let mut h3: sha1::Sha1 = Default::default();
    Sha1Digest::update(&mut h3, b"tree ");
    Sha1Digest::update(&mut h3, entry.len().to_string().as_bytes());
    Sha1Digest::update(&mut h3, &[0]);
    Sha1Digest::update(&mut h3, &entry);
    let expected: Vec<u8> = Sha1Digest::finalize(h3).as_slice().to_vec();
    assert_eq!(dig, expected);
    let _ = std::fs::remove_dir_all(&dir);
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
        let files = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
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
        let files = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
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
        let files = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
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
        let files = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
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
        let files = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
        assert!(files.contains("Makefile"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_source_excluded_by_dockerignore_errors() {
        // An ADD/COPY source explicitly listed AND excluded by .dockerignore is
        // a hard error (buildah: "no items matching glob ... filtered out using
        // .dockerignore"), not a silent skip — identity must not hash an input
        // the build will reject.
        let dir = std::env::temp_dir().join(format!("basil-test-ignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, ".dockerignore", "rustfmt.toml\n");
        ctx(&dir, "rustfmt.toml", "# unused\n");
        ctx(&dir, "other.txt", "o");
        let dsl = "FROM alpine\nADD rustfmt.toml other.txt /x/\n";
        let f = parse(dsl).unwrap();
        let err = derived_file_set(&f.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("rustfmt.toml") && text.contains(".dockerignore"),
            "expected the explicit+ignored source to be named:\n{text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn given_overrides_copy_derived_set() {
        // GIVEN declares the closure inputs explicitly: COPY . is ignored for
        // identity; only the given paths feed derivedData.
        let dir = std::env::temp_dir().join(format!("basil-test-given-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "a.txt", "aaa");
        ctx(&dir, "b.txt", "bbb");
        ctx(&dir, "src/c.rs", "ccc");
        // No GIVEN: COPY . reads the whole tree.
        let without = parse("FROM alpine\nCOPY . /x\n").unwrap();
        let w = derived_file_set(&without.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
        assert!(w.contains("a.txt") && w.contains("src/c.rs"));
        // With GIVEN: only the given path is hashed, COPY . is ignored.
        let with = parse("FROM alpine\nCOPY . /x\nGIVEN b.txt\n").unwrap();
        let g = derived_file_set(&with.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g.contains("b.txt"), "{g:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn given_dir_and_glob_expand() {
        let dir = std::env::temp_dir().join(format!("basil-test-given2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "x/a.txt", "a");
        ctx(&dir, "x/b.txt", "b");
        ctx(&dir, "x/sub/c.txt", "c");
        ctx(&dir, "y.txt", "y");
        let with = parse("FROM alpine\nGIVEN x/ *.txt\n").unwrap();
        let g = derived_file_set(&with.targets[0], &dir, std::path::Path::new("__buildfile__")).unwrap();
        assert!(g.contains("x/a.txt") && g.contains("x/b.txt"), "{g:?}");
        assert!(g.contains("x/sub/c.txt"), "{g:?}"); // dir dives recursively
        assert!(g.contains("y.txt"), "{g:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_file_is_excluded_from_derived_set() {
        // The build file (the recipe that produced the target graph) must not be
        // a content input: its effect is captured by programHash, so hashing it
        // would make identity self-referential. `ADD .` over a context that
        // contains the Basilfile must drop it from the derived set.
        let dir = std::env::temp_dir().join(format!("basil-test-bf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ctx(&dir, "Basilfile", "FROM alpine\nCOPY marker /m\n");
        ctx(&dir, "marker.txt", "m");
        let dsl = "FROM alpine\nCOPY . /x\n";
        let f = parse(dsl).unwrap();
        let build_file = dir.join("Basilfile");
        let files = derived_file_set(&f.targets[0], &dir, &build_file).unwrap();
        assert!(files.contains("marker.txt"), "{files:?}");
        assert!(!files.contains("Basilfile"), "build file must be excluded: {files:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}