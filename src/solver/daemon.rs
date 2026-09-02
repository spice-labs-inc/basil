//! BuildKit daemon lifecycle: external or ephemeral.
//!
//! Basil is a plain-process tool: it talks to a running `buildkitd` over gRPC.
//! When the user provides `BUILDKIT_HOST` / `--addr` the daemon is external
//! (managed elsewhere). Otherwise basil can spawn an ephemeral rootless
//! `buildkitd` for the duration of the build, using the operator's machine, and
//! tear it down afterward. No demo containers, no shared daemon.

use std::process::{Child, Command, Stdio};

/// How the daemon is provisioned.
pub enum Daemon {
    /// A pre-existing daemon the user pointed us at — never managed by basil.
    External(String),
    /// An ephemeral `buildkitd` basil started; dropping it stops the child.
    Ephemeral {
        /// The gRPC address (unix socket) the daemon listens on.
        addr: String,
        child: Child,
        root: std::path::PathBuf,
    },
}

impl Daemon {
    /// Resolve the daemon. If `extern_addr`/`BUILDKIT_HOST` is set, use it
    /// externally; else spawn an ephemeral rootless `buildkitd`.
    pub async fn resolve(
        extern_addr: Option<&str>,
        buildkitd: Option<&str>,
    ) -> anyhow::Result<Daemon> {
        // External wins if the user supplied an address.
        if let Some(a) = extern_addr {
            if !a.is_empty() {
                return Ok(Daemon::External(a.to_string()));
            }
        }
        Self::spawn_ephemeral(buildkitd).await
    }

    /// Spawn an ephemeral rootless `buildkitd` with a temp state root.
    async fn spawn_ephemeral(buildkitd: Option<&str>) -> anyhow::Result<Daemon> {
        let bin = buildkitd_binary(buildkitd).ok_or_else(|| {
            anyhow::anyhow!(
                "no buildkitd available: set BUILDKIT_HOST/--addr to an external \
                 daemon, or provide a buildkitd binary via BASIL_BUILDKITD / PATH"
            )
        })?;

        // A per-invocation state dir + unix socket.
        let root = std::env::temp_dir().join(format!("basil-buildkitd-{}", rand_suffix()));
        std::fs::create_dir_all(&root).ok();

        let mut cmd = Command::new(&bin);
        let addr = format!("unix://{}", root.join("buildkitd.sock").display());
        cmd.arg("--addr").arg(&addr).arg("--root").arg(&root);
        // Rootless: no privileges required with the default worker code paths.
        cmd.env("BUILDKIT_ROOTLESS", "1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("spawning '{}' for ephemeral buildkitd: {e}", bin.display())
        })?;

        // Wait for the socket to appear (bounded).
        let sock = root.join("buildkitd.sock");
        for _ in 0..100 {
            if child.try_wait()?.is_some() {
                return Err(anyhow::anyhow!("ephemeral buildkitd exited during startup"));
            }
            if sock.exists() {
                use std::io::Write as _;
                std::io::stdout().flush().ok();
                return Ok(Daemon::Ephemeral { addr, child, root });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("ephemeral buildkitd did not create its socket in time"))
    }

    /// The gRPC address (tcp:// or unix://) to connect to.
    pub fn addr(&self) -> &str {
        match self {
            Daemon::External(a) => a,
            Daemon::Ephemeral { addr, .. } => addr,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Daemon::Ephemeral { child, root, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
            std::fs::remove_dir_all(root).ok();
        }
    }
}

fn buildkitd_binary(explicit: Option<&str>) -> Option<std::path::PathBuf> {
    if let Some(p) = explicit {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
        return None;
    }
    // Prefer a basil-adjacent vendored binary, then PATH.
    let candidates = [
        std::env::var("BASIL_BUILDKITD").ok().map(std::path::PathBuf::from),
        None,
    ];
    for c in candidates.into_iter().flatten() {
        if c.exists() {
            return Some(c);
        }
    }
    // PATH lookup.
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("buildkitd"))
            .find(|p| p.exists())
    })
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", n)
}