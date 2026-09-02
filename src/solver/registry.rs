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

/// Parse an image reference, normalizing bare names to `docker.io/library/...`.
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
        RegistryClient {
            base: base_url.trim_end_matches('/').to_string(),
            headers: HashMap::new(),
            client: reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("reqwest client"),
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
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            return Err(format!("registry delete {url}: status {}", resp.status()));
        }
        Ok(())
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