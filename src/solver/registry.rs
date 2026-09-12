//! Minimal registry v2 HTTP client for tag existence + management.
//!
//! Used for the idempotent content-tag existence check (skip a build when the
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
/// The prefix basil puts before every content tag it pushes, so its tags are
/// self-identifying: `basil-<tagHash>` (content) and `basil-cache-*` (cache
/// namespaces). `basil gc` only ever prunes tags in this namespace — never a
/// foreign 64-hex or `cache-*` tag.
pub const BASIL_TAG_PREFIX: &str = "basil-";

/// The cache-tag prefix, above the content prefix.
pub const BASIL_CACHE_PREFIX: &str = "basil-cache-";

/// Convert a bare content hash to the actual registry tag: `basil-<hash>`.
pub fn content_tag(hash: &str) -> String {
    format!("{BASIL_TAG_PREFIX}{hash}")
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
    /// Docker Hub's canonical hosts (`docker.io`, `index.docker.io`) map to
    /// the registry API host `registry-1.docker.io` — querying `docker.io`
    /// returns a 302 to www.docker.com (an HTML page, no digest).
    pub fn base_url(&self) -> String {
        let api_host = match self.host.as_str() {
            "docker.io" | "index.docker.io" => "registry-1.docker.io",
            other => other,
        };
        let scheme = if Self::is_http_host(&api_host) { "http" } else { "https" };
        format!("{scheme}://{api_host}")
    }

    /// A registry client for this image reference's host.
    pub fn client(&self) -> RegistryClient {
        RegistryClient::new(&self.base_url())
    }

    /// A dedup key for this ref (before tag normalization): `host/repo:tag`.
    pub fn dedup_key(&self, tag: &str) -> String {
        format!("{}/{}:{}", self.host, self.repo, tag)
    }

    /// Whether this host is a local/insecure registry (http), mirroring the
    /// scheme choice in `base_url`.
    pub fn is_local(&self) -> bool {
        Self::is_http_host(&self.host)
    }

    fn is_http_host(host: &str) -> bool {
        host.starts_with("localhost") || host.starts_with("127.0.0.1") || !host.contains('.')
    }

    /// Resolve this ref's tag to an image manifest digest via its registry.
    pub async fn resolve_digest(&self) -> Result<String, String> {
        self.client().resolve_digest(&self.repo, &self.tag).await
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
    /// Cached bearer token for this host, fetched lazily on first 401.
    token: tokio::sync::Mutex<Option<String>>,
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
        // Follow redirects (GET/HEAD) deliberately with a bounded limit. The
        // preemptive host normalization avoids the docker.io case; if a base
        // URL still redirects, the caller surfaces the final URL/content-type
        // so a wrong-host redirect is visible rather than silently believed.
        builder = builder.redirect(reqwest::redirect::Policy::limited(10));
        RegistryClient {
            base: base_url.trim_end_matches('/').to_string(),
            headers: HashMap::new(),
            client: builder.build().expect("reqwest client"),
            token: tokio::sync::Mutex::new(None),
        }
    }

    /// Whether a manifest exists for `repo:tag`.
    pub async fn manifest_exists(&self, repo: &str, tag: &str) -> Result<bool, String> {
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let mut req = self.client.head(&url);
        req = self.authed(repo, req).await?;
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        // The registry returns 404 for OCI manifests unless the Accept header
        // lists them; send the manifest media types so the idempotent check detects existing
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
        req = self.authed(repo, req).await?;
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
        req = self.authed(repo, req).await?;
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
        req = self.authed(repo, req).await?;
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
        req = self.authed(repo, req).await?;
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req = req.header(
            reqwest::header::ACCEPT,
            "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json",
        );
        let resp = req.send().await.map_err(|e| format!("registry GET {url}: {e}"))?;
        if !resp.status().is_success() {
            // Expose the failure clearly: final URL (after any redirects) and
            // content-type, so a redirect to a wrong host is visible.
            let final_url = resp.url().to_string();
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            return Err(format!(
                "registry GET {url}: status {} (final {final_url}, content-type `{ct}`)",
                resp.status()
            ));
        }
        let digest = resp
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if digest.is_empty() {
            let final_url = resp.url().to_string();
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            return Err(format!(
                "registry GET {url}: no Docker-Content-Digest (final {final_url}, content-type `{ct}`)"
            ));
        }
        let body = resp.bytes().await.map_err(|e| format!("read manifest: {e}"))?;
        Ok((digest, body.to_vec()))
    }

    /// Resolve `repo:tag` to its manifest digest (pins an unpinned external
    /// base for identity). Returns the `Docker-Content-Digest`.
    pub async fn resolve_digest(&self, repo: &str, tag: &str) -> Result<String, String> {
        let (digest, _) = self.get_manifest(repo, tag).await?;
        if digest.is_empty() {
            return Err(format!(
                "registry did not return a Docker-Content-Digest for {repo}:{tag}"
            ));
        }
        Ok(digest)
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
        req = self.authed(repo, req).await?;
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
        req = self.authed(repo, req).await?;
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("registry PUT {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("registry retag PUT {url}: status {}", resp.status()));
        }
        Ok(digest)
    }

    /// Load the operator's basic-auth credentials for this registry host from
    /// `$DOCKER_CONFIG/config.json` or a Podman auth.json. `(user, pass)`.
    pub fn load_creds(&self) -> Option<(String, String)> {
        let host = self
            .base
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let home = std::env::var("HOME").ok();
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(d) = std::env::var("DOCKER_CONFIG") {
            paths.push(std::path::PathBuf::from(&d).join("config.json"));
        }
        if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
            paths.push(std::path::PathBuf::from(&d).join("containers").join("auth.json"));
        }
        if let Some(h) = &home {
            paths.push(std::path::PathBuf::from(h).join(".docker").join("config.json"));
            paths.push(std::path::PathBuf::from(h).join(".config").join("containers").join("auth.json"));
        }
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(auths) = value.get("auths").and_then(|a| a.as_object()) else {
                continue;
            };
            let key = auths.keys().find(|k| {
                let kk = k.trim_start_matches("https://").trim_start_matches("http://");
                kk == host || kk == format!("{host}/v1/") || kk == format!("{host}/v2/")
            });
            if let Some(k) = key {
                if let Some(b64) = auths[k].get("auth").and_then(|a| a.as_str()) {
                    if let Some(decoded) = base64_decode(b64) {
                        if let Some(col) = decoded.find(':') {
                            return Some((decoded[..col].to_string(), decoded[col + 1..].to_string()));
                        }
                    }
                }
            }
        }
        None
    }

    /// An `Authorization: Bearer` token for `repo`, or `""` if the registry
    /// does not challenge with Bearer. Always attempts the exchange — Docker
    /// Hub issues an *anonymous* token for public pulls, so no creds is fine
    /// for public repos; private repos still need the operator's creds.
    pub async fn token(&self, repo: &str) -> Result<String, String> {
        let mut guard = self.token.lock().await;
        if let Some(t) = &*guard {
            return Ok(t.clone());
        }
        let creds = self.load_creds();
        // Send an unauthenticated repo-scoped request to read the Bearer
        // challenge (realm/service). ghcr/Docker Hub only emit it on a
        // repo-scoped path, not a bare /v2/; fall back to /v2/.
        let challenge_url = format!("{}/v2/{repo}/manifests/latest", self.base);
        let realm = match self.client.get(&challenge_url).send().await {
            Ok(resp) => {
                let ww = resp.headers().get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if parse_bearer_realm(ww).is_some() {
                    parse_bearer_realm(ww)
                } else {
                    // Some registries challenge on the bare /v2/.
                    self.client.get(format!("{}/v2/", self.base)).send().await.ok()
                        .and_then(|r| r.headers().get("www-authenticate")
                            .and_then(|v| v.to_str().ok()).map(String::from))
                        .and_then(|ww| parse_bearer_realm(&ww))
                }
            }
            Err(_) => None,
        };
        let token = match realm {
            Some((realm_url, service)) => {
                // URL-encode the scope (contains '/' and ':') — the token
                // endpoint rejects a raw scope.
                let scope_enc = urlencode(&format!("repository:{repo}:pull"));
                let service_enc = urlencode(&service);
                let token_url = format!("{realm_url}?service={service_enc}&scope={scope_enc}");
                let mut req = self.client.get(&token_url);
                if let Some((user, pass)) = &creds {
                    let basic = base64_encode(&format!("{user}:{pass}"));
                    req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"));
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| format!("registry token `{repo}`: {e}"))?;
                if !resp.status().is_success() {
                    return Err(format!(
                        "registry token exchange for `{repo}`: status {}",
                        resp.status()
                    ));
                }
                let body: serde_json::Value = resp.json().await.map_err(|e| {
                    format!("parse token response `{repo}`: {e}")
                })?;
                body.get("token").and_then(|t| t.as_str()).map(String::from).unwrap_or_default()
            }
            None => String::new(),
        };
        *guard = Some(token.clone());
        Ok(token)
    }

    /// Attach a bearer token (if any) for `repo` onto `req`.
    async fn authed(&self, repo: &str, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder, String> {
        let t = self.token(repo).await?;
        let req = if t.is_empty() {
            req
        } else {
            req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"))
        };
        Ok(req)
    }
}

/// Parse `WWW-Authenticate: Bearer realm="...",service="..."` into (realm, service).
fn parse_bearer_realm(s: &str) -> Option<(String, String)> {
    // `Bearer realm="...",service="...",scope="..."` — drop the scheme.
    let s = s.trim_start();
    let s = s.strip_prefix("Bearer").or_else(|| s.strip_prefix("bearer")).unwrap_or(s);
    let mut realm = None;
    let mut service = None;
    for part in s.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("realm=") {
            realm = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = part.strip_prefix("service=") {
            service = Some(v.trim_matches('"').to_string());
        }
    }
    realm.map(|r| (r, service.unwrap_or_default()))
}

/// Percent-encode a query value (keeps only unreserved chars).
fn urlencode(s: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<String> {
    base64_decode_bytes(s).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn base64_decode_bytes(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn base64_encode(s: &str) -> String {    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/ ";
    let bytes = s.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
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
    fn ir_helpers_dedup_local_client() {
        let r = parse_ref("127.0.0.1:5001/diff", "v1").unwrap();
        assert_eq!(r.dedup_key("v1"), "127.0.0.1:5001/diff:v1");
        assert!(r.is_local());
        // client() is the collapsed constructor used across call sites.
        let _c = r.client();
        let hub = parse_ref("ghcr.io/a/b", "latest").unwrap();
        assert!(!hub.is_local());
    }

    #[test]
    fn ir_resolves_own_digest() {
        // No network test here: construction is cheap; resolve_digest needs a
        // live registry, so just assert the method is reachable on a base.
        let r = parse_ref("127.0.0.1:5001/diff", "latest").unwrap();
        let _ = r.client();
    }

    #[test]
    fn defaults_tag() {
        let r = parse_ref("ghcr.io/a/b", "latest").unwrap();
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parses_bearer_realm_with_scheme_prefix() {
        let ww = "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:spice-labs-inc/build-base:pull\"";
        let (realm, service) = parse_bearer_realm(ww).expect("challenge should parse");
        assert_eq!(realm, "https://ghcr.io/token");
        assert_eq!(service, "ghcr.io");
    }

    #[test]
    fn bearer_no_realm_is_none() {
        assert_eq!(parse_bearer_realm("Basic realm=x"), None);
    }

    #[test]
    fn urlencode_scope() {
        assert_eq!(urlencode("repository:a/b:c"), "repository%3Aa%2Fb%3Ac");
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
