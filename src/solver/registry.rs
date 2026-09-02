//! Minimal registry v2 HTTP client for tag existence + management.
//!
//! Used for the Level 1 content-tag existence check (skip a solve when the
//! deliverable's tag already exists) and for `basil gc` (list/prune basil's
//! own tags). Auth uses the operator's Docker/Podman credentials in the real
//! client (Phase 8); for now anonymous access is supported (sufficient for
//! local registries and public hubs without auth).

use std::collections::HashMap;

/// A parsed image reference: `[host[:port]/]repo[:tag]`.
#[derive(Debug, Clone)]
pub struct ImageRef {
    /// Registry host (with port), default `index.docker.io`.
    pub host: String,
    /// The repo path (e.g. `library/alpine` or `mycompany/app`).
    pub repo: String,
    /// The tag (default `latest`).
    pub tag: String,
}

/// Default registry host when none is specified.
const DOCKER_HUB: &str = "index.docker.io";

/// Parse an RFC3339 timestamp (UTC or numeric offset) into unix seconds.
/// Minimal: handles `YYYY-MM-DDThh:mm:ss[.fff](Z|±hh:mm|±hhmm)`.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |i: usize, len: usize| s[i..i + len].parse::<i64>().ok();
    let year = num(0, 4)?;
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    // Time starts after 'T' (index 11).
    let hours = num(11, 2)?;
    let mins = num(14, 2)?;
    let secs = num(17, 2)?;

    let mut offset: i64 = 0;
    let mut idx = 19; // past seconds
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
    }
    if idx < bytes.len() {
        match bytes[idx] {
            b'Z' | b'z' => {}
            b'+' | b'-' => {
                let sign = if bytes[idx] == b'-' { -1 } else { 1 };
                // ±hh:mm or ±hhmm
                let rest = &s[idx + 1..];
                let (hstr, mstr) = if rest.len() >= 5 && rest.as_bytes().get(2) == Some(&b':') {
                    (&rest[..2], &rest[3..5])
                } else {
                    (&rest[..2], &rest[2..4])
                };
                let oh: i64 = hstr.parse().ok()?;
                let om: i64 = mstr.parse().ok()?;
                offset = sign * (oh * 3600 + om * 60);
            }
            _ => return None,
        }
    }

    // Days to unix (civil algorithm) for the UTC date.
    let y = year;
    let m = month;
    let d = day;
    if !(1970..=3000).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs_utc = days * 86400 + hours * 3600 + mins * 60 + secs;
    Some((secs_utc - offset) as u64)
}
pub fn parse_ref(name: &str, default_tag: &str) -> Result<ImageRef, String> {
    let s = name;
    let (host, path) = match s.find('/') {
        Some(i) => {
            let h = &s[..i];
            if h.contains(':') || h.contains('.') || h == "localhost" {
                // host:port / host / localhost
                (&s[..i], &s[i + 1..])
            } else {
                // bare repo name like `alpine` → docker hub library
                (DOCKER_HUB, &name[..])
            }
        }
        None => (DOCKER_HUB, name),
    };

    // Split tag from the repo path (last `:` with no `/` after).
    let (repo, tag) = match path.rfind(':') {
        Some(i) if !path[i + 1..].contains('/') => (&path[..i], &path[i + 1..]),
        _ => (path, default_tag),
    };

    let repo = if host == DOCKER_HUB && !repo.contains('/') {
        format!("library/{repo}")
    } else {
        repo.to_string()
    };

    Ok(ImageRef {
        host: host.to_string(),
        repo,
        tag: tag.to_string(),
    })
}

impl ImageRef {
    /// The registry base URL. Localhost/127.0.0.1 use http; others https.
    pub fn base_url(&self) -> String {
        let scheme = if self.host.starts_with("localhost") || self.host.starts_with("127.0.0.1") || !self.host.contains('.') {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}", self.host)
    }

    /// The manifest URL for the tag.
    pub fn manifest_url(&self) -> String {
        format!("{}/v2/{}/manifests/{}", self.base_url(), self.repo, self.tag)
    }
}

/// A client for one registry host, tracking auth headers.
pub struct RegistryClient {
    base: String,
    headers: HashMap<String, String>,
    client: reqwest::Client,
}

impl RegistryClient {
    pub fn new(base_url: &str) -> Self {
        // Certificate verification is bypassed only when the operator opts in
        // via BASIL_INSECURE_REGISTRY (e.g. a self-signed test registry). Never
        // accept invalid certs by default.
        let insecure = std::env::var("BASIL_INSECURE_REGISTRY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
            .unwrap_or(false);
        let mut builder = reqwest::Client::builder();
        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        RegistryClient {
            base: base_url.trim_end_matches('/').to_string(),
            headers: HashMap::new(),
            client: builder.build().expect("reqwest client"),
        }
    }

    /// Whether a manifest exists for `repo:tag`.
    pub async fn manifest_exists(&self, repo: &str, tag: &str) -> Result<bool, String> {
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let mut req = self.client.head(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        // The registry returns 404 for OCI manifests unless the Accept header
        // lists them; send the manifest media types so Level-1 detects existing
        // tags correctly.
        req = req.header(
            reqwest::header::ACCEPT,
            "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json",
        );
        let resp = req.send().await.map_err(|e| format!("registry HEAD {url}: {e}"))?;
        let ok = resp.status().is_success();
        if !ok {
            tracing::debug!("registry HEAD {url} -> {}", resp.status());
        }
        Ok(ok)
    }

    /// List all tags for a repo.
    pub async fn list_tags(&self, repo: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/v2/{repo}/tags/list", self.base);
        let mut req = self.client.get(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry tags {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry tags {url}: status {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("parse tags: {e}"))?;
        Ok(body
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default())
    }

    /// Delete a tag (manifest) from a repo.
    pub async fn delete_tag(&self, repo: &str, tag: &str) -> Result<(), String> {
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let mut req = self.client.delete(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry delete {url}: {e}"))?;
        let status = resp.status().as_u16();
        if (400..=499).contains(&status) && status != 404 {
            // Some registries (e.g. registry:2) only accept DELETE by manifest
            // digest, not by tag. Resolve the tag to a digest and delete that.
            let (digest, _) = self.get_manifest(repo, tag).await?;
            if digest.is_empty() {
                return Err(format!("registry delete {url}: status {status} and could not resolve digest"));
            }
            return self.delete_digest(repo, &digest).await;
        }
        Ok(())
    }

    /// Delete a manifest by digest (the registry's canonical prune unit).
    pub async fn delete_digest(&self, repo: &str, digest: &str) -> Result<(), String> {
        let url = format!("{}/v2/{repo}/manifests/{digest}", self.base);
        let mut req = self.client.delete(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry delete {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry delete {url}: status {}", resp.status()));
        }
        Ok(())
    }

    /// Get the manifest bytes for `repo:tag`, parsing its digest from the
    /// `Docker-Content-Digest` response header (needed to retag).
    pub async fn get_manifest(
        &self,
        repo: &str,
        tag: &str,
    ) -> Result<(String, Vec<u8>), String> {
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let mut req = self.client.get(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req = req.header(
            reqwest::header::ACCEPT,
            "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json",
        );
        let resp = req.send().await.map_err(|e| format!("registry GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry GET {url}: status {}", resp.status()));
        }
        let digest = resp
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.bytes().await.map_err(|e| format!("read manifest: {e}"))?;
        Ok((digest, body.to_vec()))
    }

    /// Resolve the image's `created` time (unix seconds) for `repo:tag`, if an
    /// OCI config `created` field is parseable.
    pub async fn get_config_created(&self, repo: &str, tag: &str) -> Result<Option<u64>, String> {
        let (_, manifest) = self.get_manifest(repo, tag).await?;
        let m: serde_json::Value =
            serde_json::from_slice(&manifest).map_err(|e| format!("parse manifest: {e}"))?;
        let cfg_digest = m.get("config").and_then(|c| c.get("digest")).and_then(serde_json::Value::as_str);
        let Some(cfg_digest) = cfg_digest else {
            return Ok(None);
        };
        let url = format!("{}/v2/{repo}/blobs/{cfg_digest}", self.base);
        let mut req = self.client.get(&url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry config {url}: {e}"))?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("parse config blob: {e}"))?;
        if let Some(c) = body.get("created").and_then(serde_json::Value::as_str) {
            return Ok(parse_rfc3339(c));
        }
        Ok(None)
    }

    /// Re-tag: copy the manifest under `src` to `tag`, checking immutability.
    /// Returns the source digest it was anchored to.
pub async fn retag(&self, repo: &str, src: &str, tag: &str) -> Result<String, String> {        // Immutability: an existing `<repo>:<tag>` pointing at a different
        // manifest is refused rather than overwritten.
        if self.manifest_exists(repo, tag).await? {
            let (exist_digest, _) = self.get_manifest(repo, tag).await?;
            let (src_digest, _) = self.get_manifest(repo, src).await?;
            if !exist_digest.is_empty() && !src_digest.is_empty() && exist_digest != src_digest {
                return Err(format!(
                    "refusing to overwrite immutable version tag `{repo}:{tag}` (points at {exist_digest}, not {src_digest})"
                ));
            }
            if exist_digest == src_digest {
                return Ok(src_digest); // already there
            }
        }
        let (digest, body) = self.get_manifest(repo, src).await?;
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let mut req = self
            .client
            .put(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
            .body(body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry PUT {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry retag PUT {url}: status {}", resp.status()));
        }
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_docker_name() {
        let r = parse_ref("alpine", "latest").unwrap();
        assert_eq!(r.host, "index.docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parses_qualified_with_tag() {
        let r = parse_ref("ghcr.io/mycompany/app:v1", "latest").unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repo, "mycompany/app");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn parses_localhost_port() {
        let r = parse_ref("127.0.0.1:5001/diff", "latest").unwrap();
        assert_eq!(r.host, "127.0.0.1:5001");
        assert_eq!(r.repo, "diff");
        assert_eq!(r.tag, "latest");
        assert!(r.base_url().starts_with("http://"));
    }

    #[test]
    fn defaults_tag() {
        let r = parse_ref("ghcr.io/a/b", "latest").unwrap();
        assert_eq!(r.tag, "latest");
    }
}
#[cfg(test)]
mod rfc3339_tests {
    use super::*;
    #[test]
    fn parses_fractional_and_plain() {
        // 2024-01-15 00:00:00 UTC = 1705276800.
        assert_eq!(parse_rfc3339("2024-01-15T00:00:00.123456789Z"), Some(1705276800));
        assert_eq!(parse_rfc3339("2024-01-15T00:00:00Z"), Some(1705276800));
        assert_eq!(parse_rfc3339("2024-01-15T01:00:00+01:00"), Some(1705276800));
        assert_eq!(parse_rfc3339("garbage"), None);
    }
}
