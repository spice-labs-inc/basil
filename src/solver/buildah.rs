//! buildah execution backend: build the DSL graph by handing a stripped
//! Containerfile to `buildah build` (plain subprocess, no daemon), then tag +
//! push via `buildah push`.
//!
//! The DSL is rendered back to a multi-stage Containerfile whose final stage is
//! the deliverable; `ON`/`TASK`/`PLATFORM`/`OUTPUT` are omitted (side-effects /
//! matching / reverse-copy live in basil, not the image).

use crate::dsl::ast::{Arg, Base, Command, Instruction, Mount, Target};

/// Empty module refs for render tests (module-free files).
#[cfg(test)]
static EMPTY_MODULES: std::collections::BTreeMap<std::string::String, std::string::String> =
    std::collections::BTreeMap::new();

/// The parsed stage graph handed to the buildah backend: the targets in scope
/// plus the file's global-scope `ARG`s. They travel together — the graph is
/// what gets rendered to a Containerfile (a targeted build passes a
/// `dependency_subset` slice with the same file-level args). Module bases
/// render to their resolved `FROM <push-ref>:<tagHash>` — the refs map is
/// keyed by `identity::modules::module_key`, computed against `file_dir` (the
/// importing file's directory, D4).
#[derive(Clone, Copy)]
pub struct Graph<'a> {
    pub targets: &'a [Target],
    pub global_args: &'a [Arg],
    /// Directory of the importing Basilfile — module paths resolve against it.
    pub file_dir: &'a std::path::Path,
    /// Module key -> `FROM`-ready ref (`<push-ref>:<tagHash>`).
    pub modules: &'a std::collections::BTreeMap<String, String>,
}

impl<'a> Graph<'a> {
    pub fn new(
        targets: &'a [Target],
        global_args: &'a [Arg],
        file_dir: &'a std::path::Path,
        modules: &'a std::collections::BTreeMap<String, String>,
    ) -> Self {
        Graph { targets, global_args, file_dir, modules }
    }
}

/// Render the deliverable's stage graph as a multi-stage Containerfile.
/// `targets` is the topologically-ordered set; `deliverable` must be one of
/// them (the final stage). Returns the Containerfile text.
/// Render every stage with its `AS <name>` alias preserved, for a `buildah
/// build --target <stage>` (an intermediate stage must keep its alias so
/// `--target` can find it, unlike the deliverable which is the tagged image).
pub fn render_containerfile_all(g: Graph) -> String {
    let mut out = render_global_args(g.global_args);
    for t in g.targets {
        // Preserve the AS alias on every stage so --target / --from resolve.
        out.push_str(&format!("FROM {} AS {}\n", render_base(&t.base, g.file_dir, g.modules), t.name));
        for instr in &t.instructions {
            out.push_str(&render_instruction(instr));
            out.push('\n');
        }
    }
    out
}

pub fn render_containerfile(g: Graph, deliverable: &str) -> String {
    // File-level (global-scope) ARGs go before the first stage, exactly like a
    // Dockerfile: they default every stage's build arg of the same name, and a
    // stage becomes able to see one only by redeclaring it (bare or defaulted).
    let mut out = render_global_args(g.global_args);
    // Emit every stage (multi-stage deps like COPY --from must resolve), with
    // the deliverable forced to be the last stage (that's what buildah tags).
    let mut order: Vec<&Target> = g.targets.iter().collect();
    if let Some(pos) = order.iter().position(|t| t.name == deliverable) {
        let d = order.remove(pos);
        order.push(d);
    }

    for (i, t) in order.iter().enumerate() {
        let is_deliverable = t.name == deliverable;
        let is_last = i + 1 == order.len();
        let alias = if is_deliverable || is_last {
            String::new() // the tagged image doesn't need an AS alias
        } else {
            format!(" AS {}", t.name)
        };
        out.push_str(&format!("FROM {}{}\n", render_base(&t.base, g.file_dir, g.modules), alias));
        for instr in &t.instructions {
            out.push_str(&render_instruction(instr));
            out.push('\n');
        }
    }
    out
}

/// Render the file-level global `ARG`s (before the first `FROM`). Empty when
/// the file declares none.
fn render_global_args(args: &[Arg]) -> String {
    let mut out = String::new();
    for a in args {
        out.push_str(&render_instruction(&Instruction::Arg(a.clone())));
        out.push('\n');
    }
    out
}

/// Render a `FROM` base. Internal stage deps (`FROM <stage>`) stay as a bare
/// name (buildah's `AS` alias); external images keep their ref. A module base
/// renders as its resolved `FROM <push-ref>:<tagHash>` — the module's image is
/// a real base once built/pushed.
fn render_base(
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
            if let Some(p) = &b.platform {
                s.push_str(&format!("--platform={p} "));
            }
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

fn render_instruction(i: &Instruction) -> String {
    match i {
        Instruction::Arg(a) => match &a.default {
            Some(d) => format!("ARG {}={}", a.name, d),
            None => format!("ARG {}", a.name),
        },
        Instruction::Env(e) => {
            let pairs: Vec<String> = e.pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("ENV {}", pairs.join(" "))
        }
        Instruction::Copy(c) => {
            let mut s = "COPY".to_string();
            if let Some(f) = &c.from {
                s.push_str(&format!(" --from={f}"));
            }
            if let Some(o) = &c.chown {
                s.push_str(&format!(" --chown={o}"));
            }
            if let Some(m) = &c.chmod {
                s.push_str(&format!(" --chmod={m}"));
            }
            s.push_str(" ");
            s.push_str(&c.sources.join(" "));
            s.push(' ');
            s.push_str(&c.dest);
            s
        }
        Instruction::Add(a) => {
            let mut s = "ADD".to_string();
            if let Some(o) = &a.chown {
                s.push_str(&format!(" --chown={o}"));
            }
            if let Some(m) = &a.chmod {
                s.push_str(&format!(" --chmod={m}"));
            }
            s.push(' ');
            s.push_str(&a.sources.join(" "));
            s.push(' ');
            s.push_str(&a.dest);
            s
        }
        Instruction::Run(r) => {
            let mut s = "RUN".to_string();
            for m in &r.mounts {
                s.push_str(&format!(" --mount={}", render_mount(m)));
            }
            if let Some(net) = &r.network {
                s.push_str(&format!(" --network={net}"));
            }
            s.push(' ');
            s.push_str(&match &r.command {
                Command::Shell(sh) => sh.clone(),
                Command::Exec(args) => serde_json::to_string(args).unwrap_or_default(),
            });
            s
        }
        Instruction::Cmd(c) => format!("CMD {}", render_command(c)),
        Instruction::Entrypoint(c) => format!("ENTRYPOINT {}", render_command(c)),
        Instruction::User(u) => format!("USER {u}"),
        Instruction::Workdir(w) => format!("WORKDIR {}", w.path),
        Instruction::Label(labels) => {
            let pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("LABEL {}", pairs.join(" "))
        }
        Instruction::Expose(ports) => format!("EXPOSE {}", ports.join(" ")),
        Instruction::Volume(vols) => format!("VOLUME {}", vols.join(" ")),
        // OUTPUT is a basil post-build step (reverse COPY), not a buildah
        // instruction; it must not be emitted into the Containerfile.
        Instruction::Output(_) => String::new(),
    }
}

fn render_command(c: &Command) -> String {
    match c {
        Command::Shell(s) => s.clone(),
        Command::Exec(args) => serde_json::to_string(args).unwrap_or_default(),
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
    let podman = std::path::PathBuf::from(&home).join(".config").join("containers").join("auth.json");
    let docker = std::path::PathBuf::from(&home).join(".docker").join("config.json");
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        let rt = std::path::PathBuf::from(&d).join("containers").join("auth.json");
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

fn render_mount(m: &Mount) -> String {
    match m {
        Mount::Bind { source, target, read_only } => {
            let mut s = "type=bind".to_string();
            if let Some(src) = source {
                s.push_str(&format!(",source={src}"));
            }
            s.push_str(&format!(",target={target}"));
            if *read_only {
                s.push_str(",readonly");
            }
            s
        }
        Mount::Secret { id, target } => {
            let mut s = format!("type=secret,id={id}");
            if let Some(t) = target {
                s.push_str(&format!(",target={t}"));
            }
            s
        }
        Mount::Ssh { id } => match id {
            Some(i) => format!("type=ssh,id={i}"),
            None => "type=ssh".to_string(),
        },
        Mount::Cache { id, target } => {
            let mut s = format!("type=cache,target={target}");
            if let Some(i) = id {
                s.push_str(&format!(",id={i}"));
            }
            s
        }
    }
}

/// Secret-requesting RUN mounts (`type=secret,id=<id>`) for a deliverable.
fn collect_mount_secrets(targets: &[Target], deliverable: &str) -> Vec<(String, ())> {
    collect_mounts(targets, deliverable)
        .into_iter()
        .filter_map(|m| match m {
            Mount::Secret { id, .. } => Some((id.clone(), ())),
            _ => None,
        })
        .collect()
}

/// Whether any RUN mount uses `type=ssh` (needs the agent forwarded).
fn collect_mount_ssh(targets: &[Target], deliverable: &str) -> bool {
    collect_mounts(targets, deliverable)
        .into_iter()
        .any(|m| matches!(m, Mount::Ssh { .. }))
}

fn collect_mounts(targets: &[Target], deliverable: &str) -> Vec<Mount> {
    targets
        .iter()
        .filter(|t| t.name == deliverable)
        .flat_map(|t| {
            t.instructions.iter().filter_map(|i| match i {
                Instruction::Run(r) => Some(r.mounts.clone()),
                _ => None,
            })
        })
        .flatten()
        .collect()
}

/// Build one deliverable with buildah. Write the Containerfile, run
/// `buildah build --layers -t <name>:<tag> -f <file> <ctx>`, then push.
pub async fn build_deliverable(
    g: Graph<'_>,
    deliverable: &str,
    push_ref: &str,
    tag: &str,
    context: &std::path::Path,
    build_args: &std::collections::HashMap<String, String>,
    insecure: bool,
    force: bool,
) -> anyhow::Result<()> {
    let tag_ref = format!("{push_ref}:{tag}");

    // If the exact content-tagged image already exists in local
    // containers/storage, push it instead of replaying the Containerfile:
    // every step would hit cache and re-commit a byte-identical image, which is
    // pure waste when the artifact itself is already present. The content tag
    // IS the identity, so a local image at this ref is by definition the image
    // this build would produce. `--force` skips this and rebuilds.
    if !force && local_image_exists(&tag_ref).await {
        println!("basil: image {tag_ref} already built locally -- pushing (no rebuild)");
        push_tagged(deliverable, push_ref, tag, &tag_ref, insecure).await?;
        extract_outputs(g, deliverable, &tag_ref, context).await?;
        return Ok(());
    }

    let containerfile = render_containerfile(g, deliverable);

    // Buildah's own output is streamed live to the terminal so the user sees
    // STEP n/n progress; status lines give the phase context. The Containerfile
    // is fed on stdin (`-f -`) — no temp file is written to or removed from the
    // context/repo.
    println!("basil: build {deliverable}:{tag}");
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("build")
        .arg("--layers")
        .arg("-t")
        .arg(&tag_ref)
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // buildah's `build` parses `--build-arg` as a flag only before the trailing
    // positional context arg; placing it after `.arg(context)` makes buildah
    // treat it (and its value) as extra positional args and error. So emit
    // build args before the context path.
    for (k, v) in build_args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    // Forward secrets/ssh requested by any RUN --mount in the deliverable.
    // Secrets read BASIL_SECRET_<ID> (uppercased ID) for the value path;
    // ssh defaults to the agent (no extra value needed). Emitted like build
    // args, before the positional context.
    for (id, _) in collect_mount_secrets(g.targets, deliverable) {
        let env = format!("BASIL_SECRET_{}", id.to_uppercase());
        let path = std::env::var(&env).map_err(|_| {
            anyhow::anyhow!(
                "RUN --mount=type=secret,id={id} needs `{env}` to point at the secret file"
            )
        })?;
        cmd.arg("--secret").arg(format!("id={id},src={path}"));
    }
    if collect_mount_ssh(g.targets, deliverable) {
        cmd.arg("--ssh");
    }
    cmd.arg(context);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning buildah build: {e}"))?;
    // Feed the Containerfile on stdin and close it so buildah sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(containerfile.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("writing Containerfile to buildah stdin: {e}"))?;
        drop(stdin);
    }
    let status = child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("running buildah build: {e}"))?;
    if !status.success() {
        anyhow::bail!("buildah build failed (exit {})", status);
    }
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
async fn local_image_exists(tag_ref: &str) -> bool {
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
async fn push_tagged(
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
    push.arg(tag_ref)
        .arg(format!("docker://{push_ref}:{tag}"))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let pstatus = push
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("running buildah push: {e}"))?;
    if !pstatus.success() {
        anyhow::bail!("buildah push failed (exit {pstatus})");
    }
    println!("basil: pushed {deliverable}:{tag}");
    Ok(())
}

/// Collect a stage's `OUTPUT` (container-src, host-dest) pairs. `stage` is the
/// Extract OUTPUT files from an already-built (or pulled) image. Used on the
/// idempotent-skip path, and by the build path for the deliverable stage.
/// OUTPUT may be declared on any stage: the deliverable uses `image_ref`; an
/// intermediate stage is rebuilt via `buildah build --target <stage>` to get an
/// image to extract from.
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
        // Get an image for the OUTPUT-declaring stage.
        let image = if stage == deliverable {
            image_ref.to_string()
        } else {
            // Intermediate stage: build it as its own image via --target. The
            // Containerfile is fed on stdin (`-f -`); no temp file is written.
            let containerfile = render_containerfile_all(g);
            let local_img = format!("basil-output-{}", std::process::id());
            let mut build = tokio::process::Command::new("buildah");
            build.arg("build")
                .arg("--layers")
                .arg("--target")
                .arg(stage)
                .arg("-t")
                .arg(&local_img)
                .arg("-f")
                .arg("-")
                .arg(context)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
            let mut child = build
                .spawn()
                .map_err(|e| anyhow::anyhow!("spawning buildah build (OUTPUT {stage}): {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(containerfile.as_bytes())
                    .await
                    .map_err(|e| anyhow::anyhow!("writing Containerfile to stdin (OUTPUT {stage}): {e}"))?;
                drop(stdin);
            }
            let status = child
                .wait()
                .await
                .map_err(|e| anyhow::anyhow!("buildah build (OUTPUT {stage}): {e}"))?;
            if !status.success() {
                anyhow::bail!("buildah build for OUTPUT stage `{stage}` failed (exit {status})");
            }
            local_img
        };
        extract_into(&image, stage, &outs, context).await?;
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
                .args(["cp", src, &format!("/hostout/{fname}")])
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
            let status = cmd
                .status()
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

    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &working])
        .output()
        .await;
    result
}

/// Run a task command in `image` via `buildah run` (create a working
/// container, run, then remove).
pub async fn run_task(image: &str, cmd: &Command, env: &[String]) -> anyhow::Result<i32> {
    let running = buildah_from(image).await?;
    let mut run = tokio::process::Command::new("buildah");
    run.arg("run");
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
    let out = run
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("buildah run: {e}"))?;
    let code = out.status.code().unwrap_or(1);
    let _ = tokio::process::Command::new("buildah")
        .args(["rm", &running])
        .output()
        .await;
    Ok(if out.status.success() { 0 } else { code })
}

async fn buildah_from(image: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("buildah")
        .args(["from", image])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("buildah from: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("buildah from {} failed: {}", image, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse;

    #[test]
    fn renders_multi_stage_from_dsl() {
        let src = "FROM golang AS build\n\
RUN go build -o /bin/app\n\
FROM scratch AS 127.0.0.1:5001/app\n\
COPY --from=build /bin/app /app\n\
ENV FOO=bar\n\
CMD [\"/app\"]\n\
ON push(main) BUILD 127.0.0.1:5001/app\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf.starts_with("FROM golang AS build\n"));
        // The deliverable is the final stage, tagged (no AS alias needed).
        assert!(cf.contains("FROM scratch\n"));
        assert!(cf.contains("COPY --from=build /bin/app /app"));
        assert!(cf.contains("CMD [\"/app\"]"));
        assert!(!cf.contains("ON push"));
    }

    #[test]
    fn renders_containerfile_all_preserves_aliases() {
        // For a `buildah build --target <stage>` build of an intermediate stage,
        // every stage must keep its AS alias so --target can find it.
        let src = "FROM golang AS build\n\
RUN go build -o /bin/app\n\
FROM scratch AS app\n\
COPY --from=build /bin/app /app\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile_all(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES));
        assert!(cf.contains("FROM golang AS build\n"), "{cf}");
        assert!(cf.contains("FROM scratch AS app\n"), "{cf}");
        assert!(cf.contains("COPY --from=build /bin/app /app"), "{cf}");
    }

    #[test]
    fn renders_run_mounts_and_network() {
        let src = "FROM alpine AS 127.0.0.1:5001/app\n\
RUN --mount=type=cache,target=/cache --network=none echo hi\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf.contains("RUN --mount=type=cache,target=/cache --network=none echo hi"), "{cf}");
    }

    #[test]
    fn push_authfile_prefers_registry_env() {
        std::env::set_var("REGISTRY_AUTH_FILE", "/tmp/fa.json");
        assert_eq!(push_authfile(), Some(std::path::PathBuf::from("/tmp/fa.json")));
        std::env::remove_var("REGISTRY_AUTH_FILE");
    }

    #[test]
    fn renders_add_chmod_flag() {
        let src = "FROM alpine AS 127.0.0.1:5001/app\n\
ADD --chown=1000:1000 --chmod=0755 a.txt /x\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf.contains("ADD --chown=1000:1000 --chmod=0755 a.txt /x"), "{cf}");
    }

    #[test]
    fn renders_from_platform_buildplatform() {
        let src = "FROM --platform=$BUILDPLATFORM alpine AS 127.0.0.1:5001/app\n\
RUN echo hi\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(
            cf.starts_with("FROM --platform=$BUILDPLATFORM alpine\n"),
            "expected platform selector in FROM:\n{cf}"
        );
    }

    #[test]
    fn renders_global_args_at_top_scope_only() {
        // File-level ARGs are emitted before the first FROM (Dockerfile global
        // scope). A stage must redeclare an ARG itself to see it — basil adds
        // nothing per stage.
        let src = "ARG VERSION=\"\"\n\
FROM alpine AS build\n\
ARG VERSION\n\
RUN echo $VERSION\n\
FROM scratch AS 127.0.0.1:5001/app\n\
COPY --from=build /x /x\n";
        let f = parse(src).unwrap();
        assert_eq!(f.global_args.len(), 1);
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf.starts_with("ARG VERSION=\"\"\n"), "global ARG must be topmost:\n{cf}");
        // The stage's own redeclaration is preserved as written.
        assert!(cf.contains("FROM alpine AS build\nARG VERSION\n"), "{cf}");
        // The global ARG appears exactly once (top scope), not repeated per stage.
        assert_eq!(cf.matches("ARG VERSION=\"\"").count(), 1, "{cf}");
        // No global args -> nothing prepended.
        let plain = parse("FROM alpine AS 127.0.0.1:5001/app\nRUN echo hi\n").unwrap();
        let cf2 = render_containerfile(Graph::new(&plain.targets, &plain.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf2.starts_with("FROM alpine\n"), "{cf2}");
    }

    #[test]
    fn renders_bare_global_arg() {
        let src = "ARG VERSION\nFROM alpine AS 127.0.0.1:5001/app\n";
        let f = parse(src).unwrap();
        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(cf.starts_with("ARG VERSION\nFROM alpine\n"), "{cf}");
        // The all-stages renderer (--target builds) gets them too.
        let all = render_containerfile_all(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES));
        assert!(all.starts_with("ARG VERSION\nFROM alpine AS 127.0.0.1:5001/app\n"), "{all}");
    }

    #[test]
    fn task_clause_never_renders_into_containerfile() {
        // A `TASK` is an event-triggered side-effect, not part of the image.
        // The renderer (which `build_deliverable`/`build` uses) must never bake
        // the task command into the Containerfile.
        let src = "FROM alpine AS 127.0.0.1:5001/app\n\
RUN echo built > /m.txt\n\
TASK echo task-ran\n\
ON push(main) BUILD 127.0.0.1:5001/app\n";
        let f = parse(src).unwrap();
        let app = f.targets.iter().find(|t| t.name == "127.0.0.1:5001/app").unwrap();
        assert!(app.task.is_some(), "task should parse onto the target");

        let cf = render_containerfile(Graph::new(&f.targets, &f.global_args, std::path::Path::new("."), &EMPTY_MODULES), "127.0.0.1:5001/app");
        assert!(
            !cf.contains("task-ran"),
            "TASK command leaked into the Containerfile:\n{cf}"
        );
    }
}