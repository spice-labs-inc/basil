//! buildah execution backend: build the DSL graph by handing a stripped
//! Containerfile to `buildah build` (plain subprocess, no daemon), then tag +
//! push via `buildah push`.
//!
//! See docs/DESIGN-buildah-backend.md. The DSL is rendered back to a
//! multi-stage Containerfile whose final stage is the deliverable; `ON`/`TASK`/
//! `PLATFORM` are omitted (side-effects / matching live in basil, not the image).

use crate::dsl::ast::{BaseImage, Command, Instruction, Target};

/// Render the deliverable's stage graph as a multi-stage Containerfile.
/// `targets` is the topologically-ordered set; `deliverable` must be one of
/// them (the final stage). Returns the Containerfile text.
pub fn render_containerfile(targets: &[Target], deliverable: &str) -> String {
    // Emit every stage (multi-stage deps like COPY --from must resolve), with
    // the deliverable forced to be the last stage (that's what buildah tags).
    let mut order: Vec<&Target> = targets.iter().collect();
    if let Some(pos) = order.iter().position(|t| t.name == deliverable) {
        let d = order.remove(pos);
        order.push(d);
    }

    let mut out = String::new();
    for (i, t) in order.iter().enumerate() {
        let is_deliverable = t.name == deliverable;
        let is_last = i + 1 == order.len();
        let alias = if is_deliverable || is_last {
            String::new() // the tagged image doesn't need an AS alias
        } else {
            format!(" AS {}", t.name)
        };
        out.push_str(&format!("FROM {}{}\n", render_base(&t.base), alias));
        for instr in &t.instructions {
            out.push_str(&render_instruction(instr));
            out.push('\n');
        }
    }
    out
}

/// Render a `FROM` base. Internal stage deps (`FROM <stage>`) stay as a bare
/// name (buildah's `AS` alias); external images keep their ref. For non-final
/// stages we add `AS <name>` only when the name shadows/is referenced; for
/// simplicity every non-deliverable stage gets `AS <name>` so `--from` works.
fn render_base(base: &BaseImage) -> String {
    let mut s = String::new();
    if base.raw == "scratch" {
        s.push_str("scratch");
    } else {
        s.push_str(&base.raw);
    }
    if let Some(tag) = &base.tag {
        s.push(':');
        s.push_str(tag);
    }
    if let Some(d) = &base.digest {
        s.push('@');
        s.push_str(d);
    }
    s
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
            s.push(' ');
            s.push_str(&a.sources.join(" "));
            s.push(' ');
            s.push_str(&a.dest);
            s
        }
        Instruction::Run(r) => match &r.command {
            Command::Shell(sh) => format!("RUN {}", sh),
            Command::Exec(args) => format!("RUN {}", serde_json::to_string(args).unwrap_or_default()),
        },
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
    }
}

fn render_command(c: &Command) -> String {
    match c {
        Command::Shell(s) => s.clone(),
        Command::Exec(args) => serde_json::to_string(args).unwrap_or_default(),
    }
}

/// Build one deliverable with buildah. Write the Containerfile, run
/// `buildah build --layers -t <name>:<tag> -f <file> <ctx>`, then push.
pub async fn build_deliverable(
    targets: &[Target],
    deliverable: &str,
    tag: &str,
    context: &std::path::Path,
    build_args: &std::collections::HashMap<String, String>,
    insecure: bool,
) -> anyhow::Result<()> {
    let containerfile = render_containerfile(targets, deliverable);
    let cf_path = context.join(format!(".Basilfile.buildah.{}", std::process::id()));
    std::fs::write(&cf_path, containerfile)
        .map_err(|e| anyhow::anyhow!("writing containerfile: {e}"))?;

    let tag_ref = format!("{deliverable}:{tag}");
    let mut cmd = tokio::process::Command::new("buildah");
    cmd.arg("build")
        .arg("--layers")
        .arg("-t")
        .arg(&tag_ref)
        .arg("-f")
        .arg(&cf_path)
        .arg(context);
    for (k, v) in build_args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running buildah build: {e}"))?;
    std::fs::remove_file(&cf_path).ok();
    if !out.status.success() {
        anyhow::bail!(
            "buildah build failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Push the tagged image to the deliverable's registry.
    let mut push = tokio::process::Command::new("buildah");
    push.arg("push");
    if insecure {
        push.arg("--tls-verify=false");
    }
    push.arg(&tag_ref)
        .arg(format!("docker://{deliverable}:{tag}"));
    let pout = push
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running buildah push: {e}"))?;
    if !pout.status.success() {
        anyhow::bail!(
            "buildah push failed:\n{}",
            String::from_utf8_lossy(&pout.stderr)
        );
    }
    Ok(())
}

/// Run a task command in `image` via `buildah run` (create a working
/// container, run, then remove).
pub async fn run_task(image: &str, cmd: &Command, env: &[String]) -> anyhow::Result<i32> {
    let running = buildah_from(image).await?;
    let mut run = tokio::process::Command::new("buildah");
    run.arg("run").arg(&running);
    for e in env {
        run.arg("--env").arg(e);
    }
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
        let cf = render_containerfile(&f.targets, "127.0.0.1:5001/app");
        assert!(cf.starts_with("FROM golang AS build\n"));
        // The deliverable is the final stage, tagged (no AS alias needed).
        assert!(cf.contains("FROM scratch\n"));
        assert!(cf.contains("COPY --from=build /bin/app /app"));
        assert!(cf.contains("CMD [\"/app\"]"));
        assert!(!cf.contains("ON push"));
    }
}