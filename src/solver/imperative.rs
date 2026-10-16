//! The buildah execution model: every stage is built by driving `buildah`
//! per-instruction over a working container — no Containerfile is rendered at
//! all. Basil does what a Containerfile cannot: `IMPORT` inlines another
//! image's rootfs at an arbitrary point (`buildah add --from <img> <ctr> / /`,
//! imported files overwrite existing paths — layer-append semantics).
//!
//! Instruction map: `buildah from` the resolved base (platform + ref as
//! separate argv), `buildah config` for image-config instructions,
//! `buildah copy`/`buildah add` for COPY/ADD (dockerignore-honest per-file
//! copies, Docker dest placement reconstructed), `buildah run` for RUN,
//! `buildah add --from` for IMPORT, `buildah commit` to the target ref.
//!
//! ARG/ENV substitution mirrors `programHash` exactly (the same accumulated
//! `ENV` map and resolved `ARG` values), so the built image matches the hashed
//! program — identity stays the contract for what gets built.

use crate::dsl::ast::{Command, Instruction, Mount, Target};
use crate::identity::closure::{target_env, Args};
use crate::identity::program_hash::expand_vars;
use crate::solver::buildah::Graph;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// ARG/ENV substitution context, mirroring closure.rs's `VarCtx` resolution
/// order (accumulated `ENV` first, then resolved `ARG` values) so expansion
/// here matches `programHash` byte-for-byte.
struct SubstCtx {
    args: Args,
    env: std::collections::BTreeMap<String, String>,
}

impl SubstCtx {
    fn new(
        target: &Target,
        global_args: &[crate::dsl::ast::Arg],
        overrides: &HashMap<String, String>,
    ) -> Self {
        SubstCtx {
            args: Args::from_target(target, global_args, overrides),
            env: target_env(target),
        }
    }

    fn resolve(&self, name: &str) -> Option<String> {
        if let Some(v) = self.env.get(name) {
            return Some(v.clone());
        }
        self.args.resolved(name)
    }

    fn expand(&self, s: &str) -> String {
        expand_vars(s, &|k| self.resolve(k))
    }
}

/// `buildah from <base>`, with the base's `--platform` selector and image ref
/// passed as separate argv (gluing them breaks buildah's arg parser) and
/// `--tls-verify=false` for local/insecure hosts. The build-staged platform
/// variables (`$BUILDPLATFORM`/`$TARGETPLATFORM`) expand to the host platform
/// — the same value `buildah bud` would use when no `--platform` flag is given;
/// any other `$VAR` in a selector is a hard error (it could not be resolved
/// faithfully).
async fn buildah_from_resolved(
    base: &crate::solver::buildah::ResolvedBase,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("from");
    if let Some(p) = &base.platform {
        let p = expand_platform_selector(p);
        if p.contains('$') {
            anyhow::bail!(
                "FROM --platform={p}: unexpandable platform variable (only $BUILDPLATFORM / \
                 $TARGETPLATFORM, resolved to the host platform, or a literal platform)"
            );
        }
        cmd.arg(format!("--platform={p}"));
    }
    if insecure_buildah_ref(&base.image) {
        cmd.arg("--tls-verify=false");
    }
    let out = cmd
        .arg(&base.image)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah from: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah from {} failed: {}",
            base.image,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Expand the build-staged platform variables to the host platform (the value
/// `buildah bud` assumes when basil passes no `--platform` flag).
fn expand_platform_selector(sel: &str) -> String {
    let host = host_platform();
    sel.replace("${BUILDPLATFORM}", &host)
        .replace("$BUILDPLATFORM", &host)
        .replace("${TARGETPLATFORM}", &host)
        .replace("$TARGETPLATFORM", &host)
}

fn host_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    format!("{}/{}", std::env::consts::OS, arch)
}

/// Build `stage` (from + per-instruction ops) and commit it to `local_ref` —
/// an image ref in local containers/storage that the caller pushes (and
/// possibly removes) afterwards. Used for both intermediate-stage cache builds
/// and the deliverable itself.
///
/// `COMMIT` checkpoints: before building, the executor probes this stage's
/// checkpoint refs (`<cache-to>:basil-cache-<prefixHash>`, derived from the
/// exact instruction prefix each one seals) and resumes from the deepest one
/// that exists in its registry — the sealed prefix is not re-executed. Sealing
/// happens on the way: a `COMMIT` whose ref is missing commits + pushes the
/// current container state as the checkpoint image (kept local for later
/// runs).
pub(crate) async fn build_stage_imperative(
    g: Graph<'_>,
    stage: &str,
    context: &Path,
    build_file: &Path,
    local_ref: &str,
    build_args: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let target = g
        .targets
        .iter()
        .find(|t| t.name == stage)
        .ok_or_else(|| anyhow::anyhow!("imperative build: stage `{stage}` not in scope"))?;
    let base = crate::solver::buildah::resolve_base_ref(
        &target.base,
        g.file_dir,
        g.modules,
        g.stage_cache,
    );
    // COMMIT positions (indices into the instruction list), 1-based seq.
    let commit_indices: Vec<usize> = target
        .instructions
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, Instruction::Commit))
        .map(|(i, _)| i)
        .collect();
    // Resume from the deepest checkpoint already sealed in its registry (its
    // hash is recomputed per run, so existence implies the prefix matches).
    let mut resume_seq: Option<usize> = None;
    for (k, _) in commit_indices.iter().enumerate() {
        let seq = k + 1;
        if let Some(cref) = g.checkpoints.get(&(stage.to_string(), seq)) {
            if checkpoint_ref_exists(cref).await {
                resume_seq = Some(seq);
            }
        }
    }
    let container: String;
    // `None` = no checkpoint resumed; `Some(bound)` = instructions at or below
    // `bound` are already sealed inside the resumed checkpoint image.
    let skip_through: Option<usize> = resume_seq.map(|seq| commit_indices[seq - 1]);
    match resume_seq {
        Some(seq) => {
            let cref = g.checkpoints[&(stage.to_string(), seq)].clone();
            println!("basil: build {stage} resuming from checkpoint {seq} ({cref})");
            // Checkpoints live in local containers/storage only; buildah from
            // resolves locally (never pulls for a present local image).
            container = crate::solver::buildah::buildah_from(&cref).await?;
        }
        None => {
            println!("basil: build {stage} from {}", base.image);
            container = buildah_from_resolved(&base).await?;
        }
    }
    // Instructions run, then the container is committed to `local_ref`. The
    // working container is removed on every path (success or failure) so a
    // failed build never leaks one.
    let outcome = async {
        execute_instructions(
            g,
            target,
            &container,
            context,
            build_file,
            build_args,
            stage,
            skip_through,
            resume_seq.unwrap_or(0),
        )
        .await?;
        println!("basil: commit {stage} -> {local_ref}");
        buildah_commit(&container, local_ref, stage).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let rm = tokio::process::Command::new("buildah")
        .args(["rm", &container])
        .output()
        .await;
    match rm {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::warn!(
            "removing imperative container `{container}`: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("removing imperative container `{container}`: {e}"),
    }
    outcome
}

/// `buildah commit --format=oci <container> <ref>`; fails with context.
async fn buildah_commit(container: &str, ref_str: &str, what: &str) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("buildah")
        .arg("commit")
        .arg("--format=oci")
        .arg(container)
        .arg(ref_str)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah commit: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah commit {what} -> {ref_str} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a checkpoint is present in local containers/storage (the buildah
/// layer cache) — the resume probe. Never a registry read: checkpoint images
/// are local-only by design (docker-style group caching; nothing is pushed).
async fn checkpoint_ref_exists(ref_str: &str) -> bool {
    crate::solver::buildah::local_image_exists(ref_str).await
}

/// Seal the current container state as checkpoint `cref` — commit it into
/// local containers/storage only, never pushed: the buildah layer cache at
/// group granularity. A later build resumes from it with `buildah from`
/// (local-first).
async fn seal_checkpoint(container: &str, stage: &str, cref: &str) -> anyhow::Result<()> {
    println!("basil: stage {stage}: checkpointing -> {cref} (local layer cache)");
    buildah_commit(container, cref, stage).await
}

/// Replay a stage's instructions against the working container, in source
/// order. ARG updates the substitution context only; OUTPUT is a post-build
/// step (extract_outputs handles it from the committed/published image).
/// `commit_indices`/`skip_through`/`resume_seq` drive `COMMIT` handling:
/// instructions at or below `skip_through` are already sealed inside the
/// resumed checkpoint image, and every later `COMMIT` seals its prefix to a
/// `basil-cache-<prefixHash>` image when that ref is missing.
async fn execute_instructions(
    g: Graph<'_>,
    target: &Target,
    container: &str,
    context: &Path,
    build_file: &Path,
    build_args: &HashMap<String, String>,
    stage: &str,
    skip_through: Option<usize>,
    resume_seq: usize,
) -> anyhow::Result<()> {
    let ctx = SubstCtx::new(target, g.global_args, build_args);
    let mut commit_seq = resume_seq; // commits at <= skip_through were resumed through
    for (i, instr) in target.instructions.iter().enumerate() {
        if skip_through.is_some_and(|bound| i <= bound) {
            continue; // sealed inside the checkpoint image we started from
        }
        match instr {
            Instruction::Arg(_) => {
                // ARG affects substitution (and RUN env, below) only — it is
                // never written into the image config.
            }
            Instruction::Commit => {
                commit_seq += 1;
                let cref = g.checkpoints.get(&(stage.to_string(), commit_seq)).ok_or_else(|| {
                    anyhow::anyhow!(
                        "COMMIT #{commit_seq} in stage `{stage}`: no checkpoint identity was computed"
                    )
                })?;
                if !checkpoint_ref_exists(cref).await {
                    seal_checkpoint(container, stage, cref).await?;
                }
                println!("basil: stage {stage}: checkpoint {commit_seq} ({cref})");
            }
            Instruction::Env(e) => {
                // ENV accumulates into the container config; values expand
                // against the same ctx programHash uses.
                let mut cmd = tokio::process::Command::new("buildah");
                cmd.arg("config");
                for (k, v) in &e.pairs {
                    cmd.arg("--env")
                        .arg(format!("{}={}", ctx.expand(k), ctx.expand(v)));
                }
                cmd.arg(container);
                run_quiet(cmd, "config --env").await?;
            }
            Instruction::Cmd(c) => {
                run_quiet(
                    config_cmd(container, "--cmd", &exec_form(c, &ctx)),
                    "config --cmd",
                )
                .await?;
            }
            Instruction::Entrypoint(c) => {
                run_quiet(
                    config_cmd(container, "--entrypoint", &exec_form(c, &ctx)),
                    "config --entrypoint",
                )
                .await?;
            }
            Instruction::User(u) => {
                let v = ctx.expand(u);
                run_quiet(config_cmd(container, "--user", &v), "config --user").await?;
            }
            Instruction::Workdir(w) => {
                let v = ctx.expand(&w.path);
                run_quiet(
                    config_cmd(container, "--workingdir", &v),
                    "config --workingdir",
                )
                .await?;
            }
            Instruction::Label(pairs) => {
                let mut cmd = tokio::process::Command::new("buildah");
                cmd.arg("config");
                for (k, v) in pairs {
                    cmd.arg("--label")
                        .arg(format!("{}={}", ctx.expand(k), ctx.expand(v)));
                }
                cmd.arg(container);
                run_quiet(cmd, "config --label").await?;
            }
            Instruction::Expose(ports) => {
                let mut cmd = tokio::process::Command::new("buildah");
                cmd.arg("config");
                for p in ports {
                    cmd.arg("--port").arg(ctx.expand(p));
                }
                cmd.arg(container);
                run_quiet(cmd, "config --port").await?;
            }
            Instruction::Volume(vols) => {
                let mut cmd = tokio::process::Command::new("buildah");
                cmd.arg("config");
                for v in vols {
                    cmd.arg("--volume").arg(ctx.expand(v));
                }
                cmd.arg(container);
                run_quiet(cmd, "config --volume").await?;
            }
            Instruction::Copy(c) if c.from.is_none() => {
                // Context copy, dockerignore-honest: copy exactly the filtered
                // file set identity hashed, with Docker's dest placement.
                for (abs, dest) in copy_pairs(&c.sources, &c.dest, context, build_file)? {
                    copy_files_into(container, &[abs], &dest, &c.chown, &c.chmod, "copy").await?;
                }
            }
            Instruction::Copy(c) => {
                // COPY --from=<stage|image>: the stage resolves to its
                // published cache image (ensure_stages_cached has reconciled
                // it); an external ref is used as written. No dockerignore
                // applies (image-sourced).
                let from = c.from.as_deref().unwrap_or_default();
                let from_ref = g
                    .stage_cache
                    .get(from)
                    .cloned()
                    .unwrap_or_else(|| from.to_string());
                ensure_from_image_local(&from_ref).await?;
                for src in &c.sources {
                    let dest = { c.dest.clone() };
                    let mut cmd = tokio::process::Command::new("buildah");
                    cmd.arg("copy").arg("--from").arg(from_ref.clone());
                    if let Some(ch) = &c.chown {
                        cmd.arg("--chown").arg(ctx.expand(ch));
                    }
                    if let Some(cm) = &c.chmod {
                        cmd.arg("--chmod").arg(ctx.expand(cm));
                    }
                    cmd.arg(container).arg(ctx.expand(src)).arg(dest);
                    run_quiet(cmd, "copy --from").await?;
                }
            }
            Instruction::Add(a) => {
                for src in &a.sources {
                    if looks_like_url(src) {
                        // Non-hermetic, like the bud path: ADD <url> fetches at
                        // build time (the URL string is in programHash; remote
                        // content never is).
                        let mut cmd = tokio::process::Command::new("buildah");
                        cmd.arg("add");
                        if let Some(ch) = &a.chown {
                            cmd.arg("--chown").arg(ctx.expand(ch));
                        }
                        if let Some(cm) = &a.chmod {
                            cmd.arg("--chmod").arg(ctx.expand(cm));
                        }
                        cmd.arg(container).arg(src.clone()).arg(a.dest.clone());
                        run_quiet(cmd, "add <url>").await?;
                        continue;
                    }
                    // ADD is a directory/globbing copy like COPY: every file in
                    // the dockerignore-filtered set lands, dest-reconstructed
                    // per file. (A single-file source must still be one file.)
                    for (abs, dest) in
                        copy_pairs(std::slice::from_ref(src), &a.dest, context, build_file)?
                    {
                        copy_files_into(container, &[abs], &dest, &a.chown, &a.chmod, "add")
                            .await?;
                    }
                }
            }
            Instruction::Run(r) => {
                run_step(target, container, &ctx, r, context).await?;
            }
            Instruction::Output(_) => {
                // Post-build reverse COPY; extract_outputs runs it from the
                // committed/published image. Nothing to do here.
            }
            Instruction::Import(im) => {
                let ref_str = image_ref(&im.raw, im.tag.as_deref(), im.digest.as_deref());
                let ref_str = ctx.expand(&ref_str);
                println!("basil: (import) inline {ref_str}");
                ensure_from_image_local(&ref_str).await?;
                let mut add = tokio::process::Command::new("buildah");
                add.arg("add")
                    .arg("--from")
                    .arg(&ref_str)
                    .arg(container)
                    .arg("/")
                    .arg("/");
                let status =
                    crate::solver::buildah::run_buildah_relayed(add, "buildah add (IMPORT)")
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("spawning buildah add (IMPORT {ref_str}): {e}")
                        })?;
                if !status.success() {
                    anyhow::bail!("IMPORT {ref_str} failed (exit {status})");
                }
            }
        }
    }
    Ok(())
}

/// `buildah config <flag> <value> <container>`.
fn config_cmd(container: &str, flag: &str, value: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("config").arg(flag).arg(value).arg(container);
    cmd
}

/// Run a buildah command capturing output; fail with the trimmed stderr.
async fn run_quiet(mut cmd: tokio::process::Command, what: &str) -> anyhow::Result<()> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah {what}: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// One RUN step: `buildah run [--network=] [--mount=…] [--volume=…] …` with the
/// declared build args exposed as env (mirroring buildah bud, which makes
/// build args available to RUN) and the same secret/cache/bind mounts the
/// Containerfile path supports. Shell form runs via `/bin/sh -c`, exec form
/// directly — exactly what `buildah bud` would produce.
async fn run_step(
    target: &Target,
    container: &str,
    ctx: &SubstCtx,
    r: &crate::dsl::ast::Run,
    context: &Path,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("run");
    if let Some(net) = &r.network {
        cmd.arg(format!("--network={net}"));
    }
    for m in &r.mounts {
        match m {
            Mount::Bind {
                source,
                target,
                read_only,
            } => {
                // A sourced bind mount reads from the build context (the bud
                // `from=context` semantic); an unsourced one is a host path.
                let src = match source {
                    Some(s) => context.join(s).display().to_string(),
                    None => String::new(),
                };
                let mut spec = format!("type=bind,source={src},target={target}");
                if *read_only {
                    spec.push_str(",readonly");
                }
                cmd.arg(format!("--mount={spec}"));
            }
            Mount::Cache { id, target } => {
                let mut spec = format!("type=cache,target={target}");
                if let Some(i) = id {
                    spec.push_str(&format!(",id={i}"));
                }
                cmd.arg(format!("--mount={spec}"));
            }
            Mount::Secret { id, target } => {
                // buildah run has no --secret (verified on 1.39.x); a single
                // file bind mount gives the same semantics — the file is only
                // present during this run and never lands in the image.
                let env = format!("BASIL_SECRET_{}", id.to_uppercase());
                let src = std::env::var(&env).map_err(|_| {
                    anyhow::anyhow!(
                        "RUN --mount=type=secret,id={id} needs `{env}` to point at the secret file"
                    )
                })?;
                let t = target
                    .clone()
                    .unwrap_or_else(|| format!("/run/secrets/{id}"));
                cmd.arg("--volume").arg(format!("{src}:{t}"));
            }
            Mount::Ssh { .. } => {
                anyhow::bail!(
                    "RUN --mount=type=ssh is not supported in IMPORT stages (buildah run has no ssh mount) — use a plain stage"
                );
            }
        }
    }
    // Build args are exposed to RUN as env, like buildah bud's build-arg env.
    for (name, value) in &ctx.args.map {
        if let Some(v) = value {
            cmd.arg("--env").arg(format!("{name}={v}"));
        }
    }
    cmd.arg(container);
    cmd.arg("--");
    match &r.command {
        Command::Shell(s) => {
            cmd.args(["/bin/sh", "-c", &ctx.expand(s)]);
        }
        Command::Exec(args) => {
            let expanded: Vec<String> = args.iter().map(|a| ctx.expand(a)).collect();
            cmd.args(&expanded);
        }
    }
    let status = crate::solver::buildah::run_buildah_relayed(cmd, "buildah run")
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah run ({}): {e}", target.name))?;
    if !status.success() {
        anyhow::bail!("buildah run failed (exit {status})");
    }
    Ok(())
}

/// COPY/ADD of a single local file (or one already-matched set) into the
/// container image at `dest`. `sources` are absolute host paths.
async fn copy_files_into(
    container: &str,
    sources: &[PathBuf],
    dest: &str,
    chown: &Option<String>,
    chmod: &Option<String>,
    verb: &str,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg(verb);
    if let Some(ch) = chown {
        cmd.arg("--chown").arg(ch);
    }
    if let Some(cm) = chmod {
        cmd.arg("--chmod").arg(cm);
    }
    cmd.arg(container);
    for s in sources {
        cmd.arg(s);
    }
    cmd.arg(dest);
    run_quiet(cmd, verb).await
}

/// Docker's COPY placement, reconstructed per source: an exact file lands at
/// `dest`; a directory/glob/`.` source (or any multi-source or trailing-slash
/// `dest`) copies into `dest`, preserving the source's relative subtree. Each
/// returned pair is (absolute host path, image dest). Sources are filtered by
/// `.dockerignore` via the same enumeration identity hashes, so the built
/// image contains exactly the hashed file set (and the build file stays
/// excluded, like bud's stdin-fed Containerfile).
pub(crate) fn copy_pairs(
    sources: &[String],
    dest: &str,
    context: &Path,
    build_file: &Path,
) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let dest_dir = dest.ends_with('/')
        || sources.len() > 1
        || sources
            .iter()
            .any(|s| looks_like_url(s) || has_glob_meta(s) || s == "." || context.join(s).is_dir());
    let mut out = Vec::new();
    for src in sources {
        if looks_like_url(src) {
            continue; // URLs are handled by the caller (ADD only)
        }
        let set = crate::identity::derived_data::derived_file_set_for_sources(
            std::slice::from_ref(src),
            context,
            build_file,
        )?;
        let is_file = !has_glob_meta(src) && context.join(src).is_file();
        for f in &set {
            if let Some(missing) = f.strip_prefix("<missing>") {
                anyhow::bail!(
                    "COPY/ADD source `{src}` does not exist in the context (missing `{missing}`)"
                );
            }
            if let Some(glob) = f.strip_prefix("<unmatched-glob>") {
                anyhow::bail!("COPY/ADD source `{glob}` matched no files");
            }
            // Directories are tree nodes in the derived set, not copy units;
            // parent dirs are created implicitly by the file copies below.
            if context.join(f).is_dir() {
                continue;
            }
            let rel = if is_file {
                f.rsplit('/').next().unwrap_or(f).to_string()
            } else {
                relative_to_source(src, f)
            };
            let image_dest = if dest_dir {
                format!("{}/{}", dest.trim_end_matches('/'), rel)
            } else {
                dest.to_string()
            };
            out.push((context.join(f), image_dest));
        }
    }
    Ok(out)
}

/// The path of `f` (a context-relative file) relative to its source token:
/// `src`'s static prefix (before any glob meta), `"sub/"` in `sub/*.go`;
/// `"."` and a bare pattern have no prefix, so files keep their full path.
fn relative_to_source(src: &str, f: &str) -> String {
    if src == "." {
        return f.to_string();
    }
    let meta = src.find(['*', '?', '[']).unwrap_or(src.len());
    let prefix = src[..meta].trim_end_matches('/');
    if prefix.is_empty() {
        return f.to_string();
    }
    f.strip_prefix(&format!("{prefix}/"))
        .map(str::to_string)
        .unwrap_or_else(|| f.to_string())
}

fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || (s.contains('[') && s.contains(']'))
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
}

/// Shell-form CMD/ENTRYPOINT becomes `["/bin/sh","-c",<cmd>]` — the exact
/// exec form `buildah bud` produces (`buildah config --cmd "echo hi"` would
/// store the literal string as a one-element array, so basil converts).
fn exec_form(c: &Command, ctx: &SubstCtx) -> String {
    match c {
        Command::Shell(s) => {
            let expanded = ctx.expand(s);
            serde_json::to_string(&["/bin/sh", "-c", expanded.as_str()]).unwrap_or_default()
        }
        Command::Exec(args) => {
            let expanded: Vec<String> = args.iter().map(|a| ctx.expand(a)).collect();
            serde_json::to_string(&expanded).unwrap_or_default()
        }
    }
}

/// Whether a ref must be pulled with `--tls-verify=false` for buildah: a
/// local/insecure host (http, like the localhost test registry), or the
/// operator's `BASIL_INSECURE_REGISTRY` opt-in for self-signed certs.
fn insecure_buildah_ref(ref_str: &str) -> bool {
    crate::solver::registry::parse_ref(ref_str, "")
        .map(|ir| ir.is_local())
        .unwrap_or(false)
        || std::env::var("BASIL_INSECURE_REGISTRY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
            .unwrap_or(false)
}

/// Ensure a `--from` source ref is present in local containers/storage before
/// `buildah copy/add --from` uses it: those verbs pull a missing image
/// themselves, but their internal pull cannot talk to a localhost/insecure
/// registry over http (the verbs have no `--tls-verify` flag) — so a local
/// pull with the flag runs first. Non-local refs (https) and already-local
/// images skip the pre-pull.
async fn ensure_from_image_local(ref_str: &str) -> anyhow::Result<()> {
    let present = tokio::process::Command::new("buildah")
        .arg("inspect")
        .arg("--type")
        .arg("image")
        .arg(ref_str)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if present || !insecure_buildah_ref(ref_str) {
        return Ok(());
    }
    let out = tokio::process::Command::new("buildah")
        .arg("pull")
        .arg("--tls-verify=false")
        .arg(ref_str)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning buildah pull {ref_str}: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "buildah pull {ref_str} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `<raw>[:tag][@digest]`.
fn image_ref(raw: &str, tag: Option<&str>, digest: Option<&str>) -> String {
    let mut s = raw.to_string();
    if let Some(t) = tag {
        s.push(':');
        s.push_str(t);
    }
    if let Some(d) = digest {
        s.push('@');
        s.push_str(d);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::Target;
    use crate::dsl::parser::parse;

    fn ctx_dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("basil-imp-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, content) in files {
            let full = dir.join(name);
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        dir
    }

    fn first_copy(target: &Target) -> &crate::dsl::ast::Copy {
        match target
            .instructions
            .iter()
            .find(|i| matches!(i, Instruction::Copy(c) if c.from.is_none()))
        {
            Some(Instruction::Copy(c)) => c,
            _ => panic!("no context COPY"),
        }
    }

    #[test]
    fn platform_vars_expand_to_host() {
        let host = host_platform();
        assert_eq!(expand_platform_selector("$BUILDPLATFORM"), host);
        assert_eq!(expand_platform_selector("${BUILDPLATFORM}"), host);
        assert_eq!(
            expand_platform_selector("linux/$TARGETPLATFORM"),
            format!("linux/{host}")
        );
        assert_eq!(expand_platform_selector("linux/amd64"), "linux/amd64");
        // Unknown vars stay literal; buildah_from_resolved rejects the `$`.
        assert_eq!(expand_platform_selector("$NOPE"), "$NOPE");
    }

    #[test]
    fn single_file_lands_at_dest() {
        let dir = ctx_dir("one", &[("a.txt", "a")]);
        let f = parse("FROM alpine\nCOPY a.txt /x\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, dir.join("a.txt"));
        assert_eq!(
            pairs[0].1, "/x",
            "single file + no trailing slash -> the file itself"
        );
    }

    #[test]
    fn trailing_slash_dest_is_dir() {
        let dir = ctx_dir("trail", &[("a.txt", "a")]);
        let f = parse("FROM alpine\nCOPY a.txt /x/\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x/",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        assert_eq!(pairs[0].1, "/x/a.txt");
    }

    #[test]
    fn dir_copy_preserves_subtree() {
        let dir = ctx_dir(
            "dir",
            &[
                ("src/a.txt", "a"),
                ("src/sub/b.txt", "b"),
                ("other.txt", "o"),
            ],
        );
        let f = parse("FROM alpine\nCOPY src /app\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/app",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        let mut got: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, d)| (a.display().to_string(), d.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    dir.join("src/a.txt").display().to_string(),
                    "/app/a.txt".to_string()
                ),
                (
                    dir.join("src/sub/b.txt").display().to_string(),
                    "/app/sub/b.txt".to_string()
                ),
            ],
            "dir copy must preserve src's subtree under dest"
        );
    }

    #[test]
    fn dot_copy_whole_tree() {
        let dir = ctx_dir("dot", &[("a.txt", "a"), ("sub/b.txt", "b")]);
        let f = parse("FROM alpine\nCOPY . /app\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/app",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        let mut got: Vec<&str> = pairs.iter().map(|(_, d)| d.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["/app/a.txt", "/app/sub/b.txt"]);
    }

    #[test]
    fn dockerignore_filters_copy() {
        let dir = ctx_dir(
            "ign",
            &[
                (".dockerignore", "*.tmp\n"),
                ("keep.txt", "k"),
                ("drop.tmp", "d"),
            ],
        );
        let f = parse("FROM alpine\nCOPY . /app\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/app",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        let dests: Vec<String> = pairs.iter().map(|(_, d)| d.clone()).collect();
        assert!(dests.contains(&"/app/keep.txt".to_string()));
        assert!(
            !dests.contains(&"/app/drop.tmp".to_string()),
            "dockerignored file must not be copied: {dests:?}"
        );
    }

    #[test]
    fn glob_copy_keeps_no_prefix() {
        let dir = ctx_dir(
            "glob",
            &[("a.json", "{}"), ("b.json", "{}"), ("c.txt", "x")],
        );
        let f = parse("FROM alpine\nCOPY *.json /x/\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x/",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        let mut got: Vec<&str> = pairs.iter().map(|(_, d)| d.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["/x/a.json", "/x/b.json"]);
    }

    #[test]
    fn explicit_dockerignored_source_errors() {
        let dir = ctx_dir("excl", &[(".dockerignore", "a.txt\n"), ("a.txt", "a")]);
        let f = parse("FROM alpine\nCOPY a.txt /x\n").unwrap();
        let err = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("dockerignore"), "{err:#}");
    }

    #[test]
    fn build_file_is_excluded_from_copy() {
        // The Basilfile sits inside the context, like the fixture. Identity
        // excludes it (programHash represents it); the imperative copy must too.
        let dir = ctx_dir(
            "self",
            &[("Basilfile", "FROM alpine\nCOPY . /x\n"), ("a.txt", "a")],
        );
        let f = parse("FROM alpine\nCOPY . /x\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x",
            &dir,
            std::path::Path::new("Basilfile"),
        )
        .unwrap();
        let abs: Vec<PathBuf> = pairs.iter().map(|(a, _)| a.clone()).collect();
        assert!(abs.contains(&dir.join("a.txt")));
        assert!(!abs.contains(&dir.join("Basilfile")));
    }

    #[test]
    fn multi_source_dest_is_dir() {
        let dir = ctx_dir("multi", &[("a.txt", "a"), ("b.txt", "b")]);
        let f = parse("FROM alpine\nCOPY a.txt b.txt /x\n").unwrap();
        let pairs = copy_pairs(
            &first_copy(&f.targets[0]).sources,
            "/x",
            &dir,
            std::path::Path::new("__bf__"),
        )
        .unwrap();
        let mut got: Vec<&str> = pairs.iter().map(|(_, d)| d.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["/x/a.txt", "/x/b.txt"]);
    }

    #[test]
    fn shell_cmd_becomes_exec_form() {
        let c = Command::Shell("echo hi $FOO".to_string());
        let ctx = SubstCtx::new(
            &parse("FROM alpine\nENV FOO=bar\n").unwrap().targets[0],
            &[],
            &HashMap::new(),
        );
        assert_eq!(exec_form(&c, &ctx), r#"["/bin/sh","-c","echo hi bar"]"#);
        let exec = Command::Exec(vec!["/app".to_string(), "$FOO".to_string()]);
        assert_eq!(exec_form(&exec, &ctx), r#"["/app","bar"]"#);
    }
}
