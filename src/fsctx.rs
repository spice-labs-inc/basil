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
}