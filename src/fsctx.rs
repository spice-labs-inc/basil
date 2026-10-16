//! Key-value error context for file/filesystem failures.
//!
//! Basil attaches named key-value fields to file errors (via `anyhow_tracing`)
//! so an error like `Permission denied (os error 13)` always carries a
//! `filename` field naming the file it relates to. The context is the field
//! pair — `filename=<path>` — plus an `op` (read/write/create_dir/...) field.
//!
//! `filename=`/`module=` values are displayed relative to the current
//! directory when the path lies under it, so errors read the way an operator
//! navigates the repo; a path outside the cwd uses `..` steps.

use anyhow_tracing::{Error, Result};
use std::path::Path;

/// Render a path relative to the current directory (lexically, no fs access,
/// same model as derived_data's pathdiff). A relative path is passed through
/// untouched; an absolute path under the cwd is stripped to its relative
/// form; one outside the cwd steps up with `..`; with no common ancestor it
/// stays absolute. The cwd itself renders as `.`.
fn rel_to_cwd(p: &Path) -> String {
    if p.is_relative() {
        return p.display().to_string();
    }
    let Ok(cwd) = std::env::current_dir() else {
        return p.display().to_string();
    };
    let cwd_c: Vec<_> = cwd.components().collect();
    let p_c: Vec<_> = p.components().collect();
    let mut i = 0;
    while i < cwd_c.len() && i < p_c.len() && cwd_c[i] == p_c[i] {
        i += 1;
    }
    if i == 0 {
        return p.display().to_string(); // no common ancestor
    }
    let mut out = std::path::PathBuf::new();
    for _ in i..cwd_c.len() {
        out.push("..");
    }
    for c in &p_c[i..] {
        out.push(c.as_os_str());
    }
    let s = out.display().to_string();
    if s.is_empty() {
        ".".to_string()
    } else {
        s
    }
}

/// Wrap an `io::Result`, attaching `filename=<path>` and `op=<op>` fields on
/// failure. `op` describes the operation (e.g. `read`, `write`, `create_dir`).
pub fn fs_err<T>(res: std::io::Result<T>, path: impl AsRef<Path>, op: &str) -> Result<T> {
    let path = path.as_ref();
    res.map_err(|e| {
        anyhow_tracing::anyhow!("{e}")
            .with_field("filename", rel_to_cwd(path))
            .with_field("op", op.to_string())
    })
}

/// Attach arbitrary named key/value fields to an existing error, in the same
/// style as `filename=`/`op=` - for example `module=<key>` when a failure
/// concerns a specific `MODULE` file. The fields ride through to the final
/// error chain and are printed as `key=value` at the boundary.
pub fn with_fields<E: Into<Error>>(err: E, fields: &[(&'static str, String)]) -> Error {
    let mut e: Error = err.into();
    for (k, v) in fields {
        e = e.with_field(k, v.clone());
    }
    e
}

/// Attach `module=<key>` to an existing error: the module whose file a
/// resolution/identity failure concerns. The module key is itself the
/// canonical file path (`<context>/<file>`), so a separate file field would
/// duplicate it.
pub fn with_module<E: Into<Error>>(err: E, key: &str) -> Error {
    with_fields(err, &[("module", rel_to_cwd(std::path::Path::new(key)))])
}

/// Attach `filename=<path>` (and `op`) fields to an existing non-`io::Error`
/// failure (e.g. a `glob` or parse error).
pub fn with_filename<E: Into<Error>>(err: E, path: impl AsRef<Path>, op: &str) -> Error {
    let path = path.as_ref();
    let e: Error = err.into();
    e.with_field("filename", rel_to_cwd(path))
        .with_field("op", op.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_err_attaches_filename_relative_to_cwd() {
        let r: std::io::Result<()> = Err(std::io::Error::from_raw_os_error(13));
        let cwd = std::env::current_dir().unwrap();
        let e = fs_err(r, cwd.join("ctx/secrets.txt"), "read").unwrap_err();
        let text = e.to_string();
        assert!(text.contains("filename=ctx/secrets.txt"), "{text}");
        assert!(!text.contains(cwd.to_str().unwrap()), "{text}");
        assert!(text.contains("op=read"), "{text}");
        assert!(text.contains("Permission denied"), "{text}");
    }

    #[test]
    fn fs_err_keeps_relative_input_as_is() {
        let r: std::io::Result<()> = Err(std::io::Error::from_raw_os_error(13));
        let e = fs_err(r, "Basilfile", "read").unwrap_err();
        let text = e.to_string();
        assert!(text.contains("filename=Basilfile"), "{text}");
    }

    #[test]
    fn with_filename_attaches_fields_relative_to_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let e = with_filename(
            anyhow_tracing::anyhow!("bad glob"),
            cwd.join("src/fsctx.rs"),
            "glob",
        );
        let text = e.to_string();
        assert!(text.contains("filename=src/fsctx.rs"), "{text}");
        assert!(!text.contains(cwd.to_str().unwrap()), "{text}");
        assert!(text.contains("op=glob"), "{text}");
    }

    #[test]
    fn with_fields_attaches_arbitrary_pairs() {
        let e = with_fields(
            anyhow_tracing::anyhow!("could not resolve unpinned base `x`"),
            &[
                ("module", "mod/foo".to_string()),
                ("file", "/ctx/mod/Basilfile".to_string()),
            ],
        );
        let text = e.to_string();
        assert!(text.contains("module=mod/foo"), "{text}");
        assert!(text.contains("file=/ctx/mod/Basilfile"), "{text}");
        assert!(text.contains("could not resolve"), "{text}");
    }

    #[test]
    fn with_module_attaches_module_relative_to_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let key = cwd.join("mod/foo/Basilfile").to_string_lossy().into_owned();
        let e = with_module(anyhow_tracing::anyhow!("boom"), &key);
        let text = e.to_string();
        assert!(text.contains("module=mod/foo/Basilfile"), "{text}");
        assert!(!text.contains(cwd.to_str().unwrap()), "{text}");
        assert!(!text.contains("file="), "{text}");
    }
}
