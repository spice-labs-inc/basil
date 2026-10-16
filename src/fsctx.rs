//! Key-value error context for file/filesystem failures.
//!
//! Basil attaches named key-value fields to file errors (via `anyhow_tracing`)
//! so an error like `Permission denied (os error 13)` always carries a
//! `filename` field naming the file it relates to. The context is the field
//! pair — `filename=<path>` — plus an `op` (read/write/create_dir/...) field.

use anyhow_tracing::{Error, Result};
use std::path::Path;

/// Wrap an `io::Result`, attaching `filename=<path>` and `op=<op>` fields on
/// failure. `op` describes the operation (e.g. `read`, `write`, `create_dir`).
pub fn fs_err<T>(res: std::io::Result<T>, path: impl AsRef<Path>, op: &str) -> Result<T> {
    let path = path.as_ref();
    res.map_err(|e| {
        anyhow_tracing::anyhow!("{e}")
            .with_field("filename", path.display().to_string())
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
    with_fields(err, &[("module", key.to_string())])
}

/// Attach `filename=<path>` (and `op`) fields to an existing non-`io::Error`
/// failure (e.g. a `glob` or parse error).
pub fn with_filename<E: Into<Error>>(err: E, path: impl AsRef<Path>, op: &str) -> Error {
    let path = path.as_ref();
    let e: Error = err.into();
    e.with_field("filename", path.display().to_string())
        .with_field("op", op.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_err_attaches_filename() {
        let r: std::io::Result<()> = Err(std::io::Error::from_raw_os_error(13));
        let e = fs_err(r, "/tmp/secret.txt", "read").unwrap_err();
        let text = e.to_string();
        assert!(text.contains("filename=/tmp/secret.txt"), "{text}");
        assert!(text.contains("op=read"), "{text}");
        assert!(text.contains("Permission denied"), "{text}");
    }

    #[test]
    fn with_filename_attaches_fields() {
        let e = with_filename(anyhow_tracing::anyhow!("bad glob"), "/a/b", "glob");
        let text = e.to_string();
        assert!(text.contains("filename=/a/b"), "{text}");
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
    fn with_module_attaches_module() {
        let e = with_module(anyhow_tracing::anyhow!("boom"), "mod/bar");
        let text = e.to_string();
        assert!(text.contains("module=mod/bar"), "{text}");
        assert!(!text.contains("file="), "{text}");
    }
}
