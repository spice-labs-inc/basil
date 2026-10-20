//! buildah execution backend: drive `buildah` per-instruction over a working
//! container (plain subprocess, no daemon, no Containerfile), then tag + push
//! via `buildah push`.
//!
//! Every stage is built imperatively — `buildah from` the resolved base,
//! `buildah config` for image-config instructions, `buildah copy`/`buildah
//! add` for COPY/ADD, `buildah run` for RUN, `buildah add --from <img> <ctr>
//! / /` for an `IMPORT`, `buildah commit` to the target ref — see
//! [`crate::solver::imperative`]. Basil-only directives (`ON`, `TASK`,
//! `PLATFORM`, `OUTPUT`, `REGISTER`, `GIVEN`, `MODULE`) never enter the build:
//! they live in basil, not the image.

use crate::dsl::ast::{Arg, Base, Command, Instruction, Target};

/// The parsed stage graph handed to the buildah backend: the targets in scope
/// plus the file's global-scope `ARG`s. They travel together — the graph is
/// what identity was computed over (a targeted build passes a
/// `dependency_subset` slice with the same file-level args). Module bases
/// resolve to their `FROM <push-ref>:<tagHash>` — the refs map is keyed by
/// `identity::modules::module_key`, computed against `file_dir` (the importing
/// file's directory).
#[derive(Clone, Copy)]
pub struct Graph<'a> {
    pub targets: &'a [Target],
    pub global_args: &'a [Arg],
    /// The file-wide default platform list (PLATFORM before the first FROM),
    /// for targets without their own PLATFORM declaration.
    pub default_platforms: &'a [crate::dsl::ast::Platform],
    /// Directory of the importing Basilfile — module paths resolve against it.
    pub file_dir: &'a std::path::Path,
    /// Module key -> `FROM`-ready ref (`<push-ref>:<tagHash>`).
    pub modules: &'a std::collections::BTreeMap<String, String>,
    /// Stage name -> `<cache-to>:basil-cache-<stageHash>` for every
    /// non-deliverable stage in scope. Internal `FROM <stage>` bases build
    /// from this ref (the stage's published cache image) so the deliverable
    /// pulls its intermediates instead of rebuilding them.
    pub stage_cache: &'a std::collections::BTreeMap<String, String>,
    /// (stage, commit seq) -> `<cache-to>:basil-cache-<prefixHash>` for every
    /// `COMMIT` instruction: the layer-cache checkpoint sealing the
    /// instruction prefix up to that point. The executor resumes from the
    /// deepest existing checkpoint instead of re-executing the prefix.
    pub checkpoints: &'a std::collections::BTreeMap<(String, usize), String>,
}

impl<'a> Graph<'a> {
    pub fn new(
        targets: &'a [Target],
        global_args: &'a [Arg],
        default_platforms: &'a [crate::dsl::ast::Platform],
        file_dir: &'a std::path::Path,
        modules: &'a std::collections::BTreeMap<String, String>,
        stage_cache: &'a std::collections::BTreeMap<String, String>,
        checkpoints: &'a std::collections::BTreeMap<(String, usize), String>,
    ) -> Self {
        Graph {
            targets,
            global_args,
            default_platforms,
            file_dir,
            modules,
            stage_cache,
            checkpoints,
        }
    }
}

/// A stage's base resolved to the argv the builder consumes: an optional
/// `--platform` selector and the image ref as separate pieces (they must not
/// be glued together — `buildah from` parses them as distinct arguments).
pub(crate) struct ResolvedBase {
    pub platform: Option<String>,
    pub image: String,
}

/// Resolve a stage's base to the image ref the builder starts from: an
/// internal `FROM <stage>` becomes the stage's published cache ref (when an
/// entry exists and the base has no platform/tag/digest of its own), a module
/// base its `FROM <push-ref>:<tagHash>`, an external image stays as written.
pub(crate) fn resolve_base_ref(
    base: &Base,
    file_dir: &std::path::Path,
    modules: &std::collections::BTreeMap<String, String>,
    stage_cache: &std::collections::BTreeMap<String, String>,
) -> ResolvedBase {
    match base {
        Base::Module(_) => ResolvedBase {
            platform: None,
            image: written_base(base, file_dir, modules),
        },
        Base::Image(b) => {
            if let Some(cached) = stage_cache.get(&b.raw) {
                if b.platform.is_none() && b.tag.is_none() && b.digest.is_none() {
                    return ResolvedBase {
                        platform: None,
                        image: cached.clone(),
                    };
                }
            }
            ResolvedBase {
                platform: b.platform.clone(),
                image: written_base(base, file_dir, modules),
            }
        }
    }
}

/// The written image ref of a base: `raw[:tag][@digest]`; a module base becomes
/// its resolved `FROM <push-ref>:<tagHash>` — the module's image is a real
/// base once built/pushed.
fn written_base(
    base: &Base,
    file_dir: &std::path::Path,
    modules: &std::collections::BTreeMap<String, String>,
) -> String {
    match base {
        Base::Module(m) => {
            let key = crate::identity::modules::module_key(file_dir, m).unwrap_or_default();
            modules
                .get(&key)
                .cloned()
                .unwrap_or_else(|| format!("MODULE:{}", m.path))
        }
        Base::Image(b) => {
            let mut s = String::new();
            if b.raw == "scratch" {
                s.push_str("scratch");
            } else {
                s.push_str(&b.raw);
            }
            if let Some(tag) = &b.tag {
                s.push(':');
                s.push_str(tag);
            }
            if let Some(d) = &b.digest {
                s.push('@');
                s.push_str(d);
            }
            s
        }
    }
}

/// The auth file to give `buildah push --authfile`: an explicit
/// `REGISTRY_AUTH_FILE`, else Podman's auth.json, else Docker's config.json
/// (buildah/containers-image read the same `auths` map). `None` =
/// anonymous push.
fn push_authfile() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("REGISTRY_AUTH_FILE") {
        return Some(std::path::PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok()?;
    let podman = std::path::PathBuf::from(&home)
        .join(".config")
        .join("containers")
        .join("auth.json");
    let docker = std::path::PathBuf::from(&home)
        .join(".docker")
        .join("config.json");
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        let rt = std::path::PathBuf::from(&d)
            .join("containers")
            .join("auth.json");
        if rt.exists() {
            return Some(rt);
        }
    }
    if podman.exists() {
        Some(podman)
    } else if docker.exists() {
        Some(docker)
    } else {
        None
    }
}

/// Build one deliverable with buildah, imperatively: `build_stage_imperative`
/// drives `buildah from`/`config`/`copy`/`run`/`add`/`commit` per instruction
/// (the only execution model — no Containerfile is ever rendered), then the
/// committed image is pushed.
///
/// `build_file` is the Basilfile path, excluded from context copies exactly
/// like identity excludes it. `build_args` are `--build-arg` values
/// (substituted into instructions and exposed to `RUN` as env).
pub async fn build_deliverable(
    g: Graph<'_>,
    deliverable: &str,
    push_ref: &str,
    tag: &str,
    context: &std::path::Path,
    build_file: &std::path::Path,
    build_args: &std::collections::HashMap<String, String>,
    insecure: bool,
    force: bool,
) -> anyhow::Result<()> {
    // The registry tag is namespaced (`basil-<tagHash>`), so basil's tags are
    // self-identifying for gc and never collide with a foreign 64-hex tag.
    let tag = &crate::solver::registry::content_tag(tag);
    let tag_ref = format!("{push_ref}:{tag}");

    // If the exact content-tagged image already exists in local
    // containers/storage, push it instead of replaying the build: replaying
    // would re-commit a byte-identical image, which is pure waste when the
    // artifact itself is already present. The content tag IS the identity, so
    // a local image at this ref is by definition the image this build would
    // produce. `--force` skips this and rebuilds.
    if !force && local_image_exists(&tag_ref).await {
        println!("basil: image {tag_ref} already built locally -- pushing (no rebuild)");
        push_tagged(deliverable, push_ref, tag, &tag_ref, insecure).await?;
        extract_outputs(g, deliverable, &tag_ref, context).await?;
        return Ok(());
    }

    crate::solver::imperative::build_stage_imperative(
        g,
        deliverable,
        context,
        build_file,
        &tag_ref,
        build_args,
    )
    .await?;
    println!("basil: built {deliverable}:{tag}");

    push_tagged(deliverable, push_ref, tag, &tag_ref, insecure).await?;

    // Reverse COPY: extract any OUTPUT files (from the stage that declares
    // them) to the host. No-op if there are none.
    extract_outputs(g, deliverable, &tag_ref, context).await?;
    Ok(())
}

/// Is the exact `<push-ref>:<tag>` image present in local containers/storage?
/// `buildah inspect --type image` resolves a local ref and never pulls, so a
/// non-zero exit is "not present locally".
pub(crate) async fn local_image_exists(tag_ref: &str) -> bool {
    tokio::process::Command::new("buildah")
        .arg("inspect")
        .arg("--type")
        .arg("image")
        .arg(tag_ref)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Push an already-tagged local image to `<push_ref>:<tag>`.
pub(crate) async fn push_tagged(
    deliverable: &str,
    push_ref: &str,
    tag: &str,
    tag_ref: &str,
    insecure: bool,
) -> anyhow::Result<()> {
    // Push the tagged image to the deliverable's registry.
    println!("basil: push {push_ref}:{tag}");
    let mut push = tokio::process::Command::new("buildah");
    push.arg("push");
    if insecure {
        push.arg("--tls-verify=false");
    }
    // Auth for the push: buildah defaults to the Podman auth file. If that
    // isn't present but the operator has Docker creds, fall back to them so a
    // `docker login`-only registry still pushes.
    if let Some(authfile) = push_authfile() {
        push.arg("--authfile").arg(&authfile);
    }
    push.arg(tag_ref).arg(format!("docker://{push_ref}:{tag}"));
    let pstatus = run_buildah_relayed(push, "buildah push")
        .await
        .map_err(|e| anyhow::anyhow!("running buildah push: {e}"))?;
    if !pstatus.success() {
        anyhow::bail!("buildah push failed (exit {pstatus})");
    }
    println!("basil: pushed {deliverable}:{tag}");
    Ok(())
}

/// Extract OUTPUT files from an already-built (or pulled) image. Used on the
/// idempotent-skip path, and by the build path for the deliverable stage.
/// OUTPUT may be declared on any stage: the deliverable uses `image_ref`; an
/// intermediate stage extracts from its published cache image.
pub async fn extract_outputs_from_image(
    image_ref: &str,
    context: &std::path::Path,
    g: Graph<'_>,
    deliverable: &str,
) -> anyhow::Result<()> {
    extract_outputs(g, deliverable, image_ref, context).await
}

/// Reverse COPY: extract every `OUTPUT` (container-src, host-dest) out of the
/// image of the stage that declares it, onto the host. Uses
/// `buildah run --volume <hostdir>:/hostout` to bind the host destination dir
/// into the working container and `cp` the file out (no rootless `buildah
/// mount`/`unshare` needed). Host dests are relative to the context and already
/// validated relative at parse time.
async fn extract_outputs(
    g: Graph<'_>,
    deliverable: &str,
    image_ref: &str,
    context: &std::path::Path,
) -> anyhow::Result<()> {
    // Group OUTPUTs by the stage that declares them.
    let mut by_stage: std::collections::BTreeMap<&str, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for t in g.targets {
        let outs: Vec<(String, String)> = t
            .instructions
            .iter()
            .filter_map(|i| match i {
                Instruction::Output(o) => Some((o.source.clone(), o.dest.clone())),
                _ => None,
            })
            .collect();
        if !outs.is_empty() {
            by_stage.insert(t.name.as_str(), outs);
        }
    }
    for (stage, outs) in by_stage {
        // The deliverable extracts from its pushed image; an intermediate stage
        // extracts from its published cache image (`basil-cache-<stageHash>`),
        // which ensure_stages_cached has probed/built/pushed before extraction
        // on every path. buildah_from pulls it into local storage, so this is a
        // pull, never a rebuild — and the pulled image is the cache, not a
        // throwaway.
        let image = if stage == deliverable {
            image_ref.to_string()
        } else {
            g.stage_cache.get(stage).cloned().ok_or_else(|| {
                // stage_cache is total over non-deliverable subset stages and
                // ensure_stages_cached reconciled them before extraction, so a
                // miss here is a basil bug (an ordering/coverage break), never
                // operator error - say so instead of faking a registry pull.
                anyhow::anyhow!(
                    "internal invariant broken: stage `{stage}` declares OUTPUT but has no cache ref; ensure_stages_cached must cover every OUTPUT stage before extraction"
                )
            })?
        };
        extract_into(&image, stage, &outs, context).await?;
    }
    Ok(())
}

/// Remove a locally-built image by ref or ID. Used after pushing a stage's
/// cache image. `-f` because the image may carry leftover tags from earlier
/// runs (identical cached content), and the image is throwaway — refusing on
/// "more than one tag" would leak it.
pub(crate) async fn remove_local_image(image: &str) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("buildah")
        .args(["rmi", "-f", image])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("buildah rmi: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah rmi {image} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Extract one stage's `(src, dest)` OUTPUTs from `image` into `context`.
///
/// The container binds the *destination's parent directory* (on the same
/// device as the host destination) at `/hostout` and `cp`s the source straight
/// into it — so the file lands at `host_dest` directly. No temp staging dir is
/// used, so it can never hit a cross-device `rename` (EXDEV).
async fn extract_into(
    image: &str,
    stage: &str,
    outputs: &[(String, String)],
    context: &std::path::Path,
) -> anyhow::Result<()> {
    let working = buildah_from(image).await?;

    let result = async {
        for (src, dest) in outputs {
            let host_dest = context.join(dest);
            let parent = host_dest
                .parent()
                .ok_or_else(|| anyhow::anyhow!("OUTPUT dest `{dest}` has no parent"))?;
            crate::fsctx::fs_err(std::fs::create_dir_all(parent), parent, "create_dir")?;
            // buildah requires an absolute host path for --volume.
            let abs_parent = std::path::absolute(parent)
                .map_err(|e| anyhow::anyhow!("resolving OUTPUT dir `{}`: {e}", parent.display()))?;
            let fname = host_dest
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("OUTPUT dest `{dest}` has no filename"))?;
            let mut cmd = tokio::process::Command::new("buildah");
            cmd.arg("run")
                .arg("--volume")
                .arg(format!("{}:/hostout", abs_parent.display()))
                .arg(&working)
                .args(["cp", src, &format!("/hostout/{fname}")]);
            let status = run_buildah_relayed(cmd, "buildah run (OUTPUT)")
                .await
                .map_err(|e| anyhow::anyhow!("buildah run (OUTPUT {stage}): {e}"))?;
            if !status.success() {
                anyhow::bail!("OUTPUT failed to extract `{src}` (exit {status})");
            }
            println!("basil: output {src} -> {dest}");
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let rm = tokio::process::Command::new("buildah")
        .args(["rm", &working])
        .output()
        .await;
    match rm {
        Ok(o) if o.status.success() => {}
        // Report a failed working-container removal instead of silently
        // leaking it; the extraction result still wins so the real failure is
        // what surfaces.
        Ok(o) => tracing::warn!(
            "removing OUTPUT container `{working}`: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("removing OUTPUT container `{working}`: {e}"),
    }
    result
}

/// Render the `buildah run` arguments for a task's `--mount` declarations.
/// Mirrors the executor's RUN mounts: a sourced bind mount resolves relative
/// to the build context (an absolute source is used as-is, since `Path::join`
/// replaces on absolute input); secrets come from the `BASIL_SECRET_<ID>` env
/// and bind-mount as a file (buildah run has no `--secret`); ssh is refused
/// (buildah run has no ssh mount). Never touches image identity.
fn task_mount_args(
    mounts: &[crate::dsl::ast::Mount],
    context: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for m in mounts {
        match m {
            crate::dsl::ast::Mount::Bind {
                source,
                target,
                read_only,
            } => {
                let src = match source {
                    Some(s) => context.join(s).display().to_string(),
                    None => String::new(),
                };
                let mut spec = format!("type=bind,source={src},target={target}");
                if *read_only {
                    spec.push_str(",readonly");
                }
                out.push(format!("--mount={spec}"));
            }
            crate::dsl::ast::Mount::Cache { id, target } => {
                let mut spec = format!("type=cache,target={target}");
                if let Some(i) = id {
                    spec.push_str(&format!(",id={i}"));
                }
                out.push(format!("--mount={spec}"));
            }
            crate::dsl::ast::Mount::Secret { id, target } => {
                let env = format!("BASIL_SECRET_{}", id.to_uppercase());
                let src = std::env::var(&env).map_err(|_| {
                    anyhow::anyhow!(
                        "TASK --mount=type=secret,id={id} needs `{env}` to point at the secret file"
                    )
                })?;
                let t = target
                    .clone()
                    .unwrap_or_else(|| format!("/run/secrets/{id}"));
                out.push(format!("--volume={src}:{t}"));
            }
            crate::dsl::ast::Mount::Ssh { .. } => {
                anyhow::bail!(
                    "TASK --mount=type=ssh is not supported (buildah run has no ssh mount)"
                );
            }
        }
    }
    Ok(out)
}

/// Run a task command in `image` via `buildah run` (create a working
/// container, run, then remove), with the task's `--mount` declarations
/// applied and task stdout/stderr relayed to basil's own (never a tty).
pub async fn run_task(
    image: &str,
    cmd: &Command,
    env: &[String],
    mounts: &[crate::dsl::ast::Mount],
    context: &std::path::Path,
) -> anyhow::Result<i32> {
    let running = buildah_from(image).await?;
    let mut run = tokio::process::Command::new("buildah");
    run.arg("run");
    for m in task_mount_args(mounts, context)? {
        run.arg(m);
    }
    for e in env {
        run.arg("--env").arg(e);
    }
    run.arg(&running);
    match cmd {
        Command::Shell(s) => {
            run.args(["/bin/sh", "-c", s]);
        }
        Command::Exec(args) => {
            run.args(args);
        }
    }
    let status = run_buildah_relayed(run, "task run").await?;
    let code = status.code().unwrap_or(1);
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &running])
        .output()
        .await;
    Ok(if status.success() { 0 } else { code })
}

/// Spawn a buildah command with stdout/stderr as **pipes**, relayed 1:1 to
/// basil's own stdout/stderr, and stdin closed. The child never sees a
/// terminal: build steps cannot become interactive (no progress spinners,
/// colors, or readline) and can never consume the operator's terminal input
/// — a build step that reads stdin gets EOF, not the user's keystrokes.
/// Output order between the two streams is not guaranteed to interleave
/// exactly as a shared tty would.
pub(crate) async fn run_buildah_relayed(
    mut cmd: tokio::process::Command,
    what: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {what}: {e}"))?;
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let mut basil_out = tokio::io::stdout();
    let mut basil_err = tokio::io::stderr();
    let (ro, re) = tokio::join!(
        tokio::io::copy(&mut so, &mut basil_out),
        tokio::io::copy(&mut se, &mut basil_err),
    );
    ro.map_err(|e| anyhow::anyhow!("{what}: relaying stdout: {e}"))?;
    re.map_err(|e| anyhow::anyhow!("{what}: relaying stderr: {e}"))?;
    child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("{what}: {e}"))
}

/// `buildah from <image>` — a working container for a plain image ref
/// (deliverable pulls, OUTPUT extraction, task runs). Localhost/insecure
/// hosts get `--tls-verify=false` (buildah autodetects localhost for `build`
/// but not for `from`/`pull`).
pub(crate) async fn buildah_from(image: &str) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("from");
    if let Ok(ir) = crate::solver::registry::parse_ref(image, "latest") {
        if ir.is_local() {
            cmd.arg("--tls-verify=false");
        }
    }
    let out = cmd
        .arg(image)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("buildah from: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah from {} failed: {}",
            image,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_authfile_prefers_registry_env() {
        std::env::set_var("REGISTRY_AUTH_FILE", "/tmp/fa.json");
        assert_eq!(
            push_authfile(),
            Some(std::path::PathBuf::from("/tmp/fa.json"))
        );
        std::env::remove_var("REGISTRY_AUTH_FILE");
    }

    #[test]
    fn task_mount_bind_resolves_relative_to_context() {
        let args = task_mount_args(
            &[crate::dsl::ast::Mount::Bind {
                source: Some("sub".to_string()),
                target: "/workspace".to_string(),
                read_only: false,
            }],
            std::path::Path::new("/ctx"),
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["--mount=type=bind,source=/ctx/sub,target=/workspace"]
        );
    }

    #[test]
    fn task_mount_bind_absolute_source_passes_through() {
        let args = task_mount_args(
            &[crate::dsl::ast::Mount::Bind {
                source: Some("/abs/path".to_string()),
                target: "/data".to_string(),
                read_only: true,
            }],
            std::path::Path::new("/ctx"),
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["--mount=type=bind,source=/abs/path,target=/data,readonly"]
        );
    }

    #[test]
    fn task_mount_cache_and_secret_render() {
        std::env::set_var("BASIL_SECRET_TOKEN", "/tmp/secret-file");
        let args = task_mount_args(
            &[
                crate::dsl::ast::Mount::Cache {
                    id: Some("c1".to_string()),
                    target: "/cache".to_string(),
                },
                crate::dsl::ast::Mount::Secret {
                    id: "token".to_string(),
                    target: Some("/run/secrets/t".to_string()),
                },
            ],
            std::path::Path::new("/ctx"),
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "--mount=type=cache,target=/cache,id=c1",
                "--volume=/tmp/secret-file:/run/secrets/t"
            ]
        );
        std::env::remove_var("BASIL_SECRET_TOKEN");
    }

    #[test]
    fn task_mount_secret_without_env_fails() {
        std::env::remove_var("BASIL_SECRET_MISSING");
        let e = task_mount_args(
            &[crate::dsl::ast::Mount::Secret {
                id: "missing".to_string(),
                target: None,
            }],
            std::path::Path::new("/ctx"),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("BASIL_SECRET_MISSING"), "{e:#}");
    }

    #[test]
    fn task_mount_ssh_is_refused() {
        let e = task_mount_args(
            &[crate::dsl::ast::Mount::Ssh { id: None }],
            std::path::Path::new("/ctx"),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("ssh"), "{e:#}");
    }
}
