//! LLB emitter: translate a target's instruction graph into a `pb::Definition`.
//!
//! Mirrors buildkit's `client/llb` State model: each stage accumulates a
//! rootfs output; `FROM` seeds it with a source or a `--from` stage ref; `RUN`
//! produces the next rootfs via an exec; `COPY`/`ADD` via a file op; and the
//! final output is the stage's deliverable rootfs. Image config (`ENV`, `CMD`,
//! `ENTRYPOINT`, `USER`, `WORKDIR`, `LABEL`, `EXPOSE`, `VOLUME`) is composed
//! separately (in `image_config`) and delivered to the export via the gateway
//! frontend (Phase 4b), since buildkit only reads it from solve-result metadata.

pub mod llb;

use crate::dsl::ast::{BaseImage, Command, Instruction, Mount, Target};
use crate::identity::program_hash::expand_vars;
use buildkit_client::proto::pb;
use llb::{ExtraMount, Graph, Output};
use std::collections::HashMap;

/// The emitted result for a target.
pub struct Emitted {
    pub target: String,
    /// The final rootfs output reference (vertex index + output index).
    pub output: Output,
    /// The composed image config (JSON-serializable later).
    pub config: ImageConfig,
}

/// The composed OCI/Docker image config for a target, per instructions.
#[derive(Debug, Clone, Default)]
pub struct ImageConfig {
    pub env: Vec<String>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub user: Option<String>,
    pub working_dir: String,
    pub labels: Vec<(String, String)>,
    pub exposed_ports: Vec<String>,
    pub volumes: Vec<String>,
}

/// The emitted definition plus per-target configs.
pub struct EmitResult {
    pub graph: Graph,
    pub targets: Vec<Emitted>,
}

fn normalize_image_ref(raw: &str, tag: Option<&str>, digest: Option<&str>) -> String {
    // Normalize a bare name to a fully-qualified reference (buildkit's
    // containerimage source requires it; a bare `alpine` URL-parses as
    // host:port and breaks). docker/library names become docker.io/library/...
    let named = if raw.contains('.') || raw.contains(':') || raw == "localhost" {
        raw.to_string()
    } else {
        format!("docker.io/library/{raw}")
    };
    let mut s = named;
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

/// Emit the LLB for one target placed in `graph`, resolving stage deps from
/// `stage_outputs`. Returns the target's final output + composed config.
/// `build_args` are build-arg overrides (e.g. `VERSION` on a tag event); they
/// become available to ARG resolution and `$VAR` expansion in RUN/COPY/etc.
pub fn emit_target(
    graph: &mut Graph,
    target: &Target,
    stage_outputs: &HashMap<String, Output>,
    context_local_id: &str,
    session_id: &str,
    platform: Option<pb::Platform>,
    build_args: &HashMap<String, String>,
) -> Result<Emitted, String> {
    let mut state: Option<Output> = match &target.base {
        BaseImage { raw, tag, digest } => {
            // `scratch` → empty rootfs (no output yet).
            if raw.eq_ignore_ascii_case("scratch") {
                None
            } else if let Some(stage_out) = stage_outputs.get(raw) {
                Some(*stage_out)
            } else {
                // External image. Normalize to a fully-qualified ref (buildkit
                // source needs `docker.io/library/...`, not a bare name) and
                // include the tag/digest.
                let refstr = normalize_image_ref(raw, tag.as_deref(), digest.as_deref());
                Some(llb::source_image(graph, &refstr, platform.clone()))
            }
        }
    };

    let mut config = ImageConfig::default();
    // Accumulate env/workdir/user for exec meta (RUN/commands inherit).
    let mut env: HashMap<String, String> = HashMap::new();
    // ARG values (default or build-arg override), available for `$VAR` expansion
    // in RUN/COPY and provided as exec env unless shadowed by ENV.
    let mut arg_values: HashMap<String, String> = HashMap::new();
    let mut workdir = "/".to_string();
    let mut user = String::new();

    config.working_dir = "/".to_string();

    for instr in &target.instructions {
        match instr {
            Instruction::Arg(a) => {
                // ARG affects program hash and build args, not the rootfs. Its
                // value (override, else default) is available for expansion.
                let value = build_args
                    .get(&a.name)
                    .cloned()
                    .or_else(|| a.default.clone());
                if let Some(v) = value {
                    arg_values.insert(a.name.clone(), v);
                } else {
                    arg_values.remove(&a.name);
                }
            }
            Instruction::Env(e) => {
                for (k, v) in &e.pairs {
                    env.insert(k.clone(), v.clone());
                    // also accumulated into config below
                }
                config.env.extend(e.pairs.iter().map(|(k, v)| format!("{k}={v}")));
            }
            Instruction::Copy(c) => {
                let source = if let Some(from) = &c.from {
                    match stage_outputs.get(from) {
                        Some(o) => *o,
                        None => return Err(format!("COPY --from refers to unknown stage `{from}`")),
                    }
                } else {
                    // context copy: local source provides the files.
                    if context_local_id.is_empty() {
                        return Err("COPY from context requires a context source".to_string());
                    }
                    llb::source_local(graph, context_local_id, session_id, platform.clone())
                };
                // The sources are copied from `source` (context dir or stage FS).
                state = Some(llb::file_op(
                    graph,
                    state,
                    source,
                    c.sources.clone(),
                    c.dest.clone(),
                    c.chown.clone(),
                    c.chmod.clone(),
                    false,
                    platform.clone(),
                ));
            }
            Instruction::Add(a) => {
                // ADD from context; URLs (non-hermetic) are a separate path —
                // v1 rejects remote ADD at parse (TODO) or treats URL in program.
                let source = llb::source_local(graph, context_local_id, session_id, platform.clone());
                state = Some(llb::file_op(
                    graph,
                    state,
                    source,
                    a.sources.clone(),
                    a.dest.clone(),
                    a.chown.clone(),
                    None,
                    true,
                    platform.clone(),
                ));
            }
            Instruction::Run(r) => {
                let (args, cmd_env) = command_to_meta(&r.command);
                // Expand `$VAR`/`${VAR}` in the command against ARG values then env.
                let resolve = |name: &str| env.get(name).cloned().or_else(|| arg_values.get(name).cloned());
                let args: Vec<String> = args.iter().map(|a| expand_vars(a, &resolve)).collect();
                let mut extra = Vec::new();
                for m in &r.mounts {
                    extra.push(mount_to_extra(graph, m, session_id, platform.clone()));
                }
                // exec env = accumulated ENV + ARG values not shadowed + cmd env
                let mut exec_env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                for (k, v) in &arg_values {
                    if !env.contains_key(k) {
                        exec_env.push(format!("{k}={v}"));
                    }
                }
                exec_env.extend(cmd_env);
                state = Some(llb::run(
                    graph,
                    state,
                    args,
                    exec_env,
                    workdir.clone(),
                    user.clone(),
                    platform.clone(),
                    extra,
                ));
            }
            Instruction::Cmd(c) => {
                config.cmd = Some(command_to_vec(c));
            }
            Instruction::Entrypoint(c) => {
                config.entrypoint = Some(command_to_vec(c));
            }
            Instruction::User(u) => {
                config.user = Some(u.clone());
                user = u.clone();
            }
            Instruction::Workdir(w) => {
                config.working_dir = w.path.clone();
                workdir = w.path.clone();
            }
            Instruction::Label(pairs) => {
                config.labels.extend(pairs.clone());
            }
            Instruction::Expose(ports) => {
                config.exposed_ports.extend(ports.clone());
            }
            Instruction::Volume(vols) => {
                config.volumes.extend(vols.clone());
            }
        }
    }

    // Default ENV PATH if not set.
    if !config.env.iter().any(|e| e.starts_with("PATH=")) {
        config.env.insert(0, "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
    }

    Ok(Emitted {
        target: target.name.clone(),
        output: state.ok_or_else(|| {
            format!("target `{}` is an empty scratch stage with no output to export", target.name)
        })?,
        config,
    })
}

/// Emit a whole file: process targets in dependency order.
pub fn emit_file(
    targets: &[Target],
    context_local_id: &str,
    session_id: &str,
    platform: Option<pb::Platform>,
    build_args: &HashMap<String, String>,
) -> Result<EmitResult, String> {
    let mut graph = Graph::new();
    let mut stage_outputs: HashMap<String, Output> = HashMap::new();
    let mut emitted = Vec::new();

    // Resolve in order — assume targets are already topologically ordered (the
    // identity layer validates stage order). Emit each stage.
    for t in targets {
        let e = emit_target(
            &mut graph,
            t,
            &stage_outputs,
            context_local_id,
            session_id,
            platform.clone(),
            build_args,
        )?;
        stage_outputs.insert(t.name.clone(), e.output);
        emitted.push(e);
    }

    Ok(EmitResult { graph, targets: emitted })
}

/// Convert a shell/exec command to exec args. Shell form runs via the image's
/// shell (`/bin/sh -c <cmd>`).
fn command_to_meta(cmd: &Command) -> (Vec<String>, Vec<String>) {
    match cmd {
        Command::Shell(s) => (vec!["/bin/sh".to_string(), "-c".to_string(), s.clone()], vec![]),
        Command::Exec(args) => (args.clone(), vec![]),
    }
}

fn command_to_vec(cmd: &Command) -> Vec<String> {
    match cmd {
        Command::Shell(s) => vec!["/bin/sh".to_string(), "-c".to_string(), s.clone()],
        Command::Exec(args) => args.clone(),
    }
}

fn mount_to_extra(graph: &mut Graph, m: &Mount, session_id: &str, platform: Option<pb::Platform>) -> ExtraMount {
    match m {
        Mount::Bind { source, target, read_only } => {
            let src = source
                .as_deref()
                .filter(|s| !s.is_empty() && *s != ".")
                .map(|s| llb::source_local(graph, s, session_id, platform));
            ExtraMount {
                source: src,
                dest: target.clone(),
                readonly: *read_only,
            }
        }
        // secret/ssh/cache mounts are represented via pb.Mount types not exec
        // bind inputs; v1 supports them through the session. Represent cache
        // mounts as a passthrough (placeholder) for now — full support is 4b.
        Mount::Secret { target, .. } => ExtraMount {
            source: None,
            dest: target.clone().unwrap_or_default(),
            readonly: true,
        },
        Mount::Ssh { .. } => ExtraMount {
            source: None,
            dest: String::new(),
            readonly: true,
        },
        Mount::Cache { target, .. } => ExtraMount {
            source: None,
            dest: target.clone(),
            readonly: false,
        },
    }
}#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse;

    fn emit(dsl: &str, id: &str) -> EmitResult {
        let file = parse(dsl).unwrap();
        emit_file(&file.targets, id, "", None, &HashMap::new()).unwrap()
    }

    #[test]
    fn from_image_produces_source_plus_exec() {
        let r = emit("FROM alpine:3.23\nRUN echo hi\n", "ctx");
        // [source_image, exec]
        assert_eq!(r.graph.op_count(), 2);
        if let Some(pb::op::Op::Source(s)) = r.graph.op(0).op.as_ref() {
            assert!(s.identifier.starts_with("docker-image://docker.io/library/alpine:3.23"));
        } else {
            panic!("op 0 should be a source");
        }
    }

    #[test]
    fn version_build_arg_expands_run_args_and_env() {
        let file = parse("FROM alpine\nARG VERSION\nRUN echo v$VERSION\n").unwrap();
        let mut args = HashMap::new();
        args.insert("VERSION".to_string(), "1.2.3".to_string());
        let r = emit_file(&file.targets, "ctx", "", None, &args).unwrap();
        let n = r.graph.op_count();
        // Last op is the exec (RUN echo v$VERSION).
        let exec = r.graph.op(n - 1).op.as_ref().unwrap();
        if let pb::op::Op::Exec(e) = exec {
            let args = e.meta.as_ref().unwrap().args.clone();
            let env = e.meta.as_ref().unwrap().env.clone();
            // The shell command has $VERSION expanded to 1.2.3, and VERSION is
            // injected as exec env.
            assert!(
                args.iter().any(|a| a.contains("1.2.3")),
                "RUN args should have $VERSION expanded: {args:?}"
            );
            assert!(env.iter().any(|kv| kv == "VERSION=1.2.3"));
        } else {
            panic!("last op should be an exec");
        }
    }

    #[test]
    fn context_copy_uses_local_source_and_fileop() {
        let r = emit("FROM alpine\nCOPY . /src\n", "ctx");
        // [source_image, source_local, fileop]
        assert_eq!(r.graph.op_count(), 3);
        // op 2 is the file op
        assert!(matches!(r.graph.op(2).op.as_ref(), Some(pb::op::Op::File(_))));
    }

    #[test]
    fn stage_dep_copy_uses_stage_output() {
        let r = emit(
            "FROM golang AS build\nRUN go build\n\nFROM scratch AS app\nCOPY --from=build /bin/app /app\n",
            "ctx",
        );
        let n = r.graph.op_count();
        // The final op is the file op for COPY --from=build; its destination is
        // scratch (empty, no input edge) so it carries just the build-stage
        // source input.
        let last = r.graph.op(n - 1);
        assert!(matches!(last.op.as_ref(), Some(pb::op::Op::File(_))));
        assert_eq!(last.inputs.len(), 1);
    }

    #[test]
    fn run_uses_sh_c_and_writes_root_output() {
        let r = emit("FROM alpine\nRUN echo build\n", "ctx");
        let exec = r.graph.op(1);
        if let Some(pb::op::Op::Exec(e)) = exec.op.as_ref() {
            let meta = e.meta.as_ref().unwrap();
            assert_eq!(meta.args, vec!["/bin/sh", "-c", "echo build"]);
            // root mount "/" is writable with output 0
            let root = e.mounts.iter().find(|m| m.dest == "/").unwrap();
            assert_eq!(root.output, 0);
            assert!(!root.readonly);
        } else {
            panic!("op 1 should be exec");
        }
    }

    #[test]
    fn image_config_composed() {
        let file = parse(
            "FROM alpine\nENV FOO=bar\nWORKDIR /app\nUSER 1000\nCMD [\"/bin/run\"]\nLABEL a=b\n",
        )
        .unwrap();
        let mut graph = Graph::new();
        let e = emit_target(&mut graph, &file.targets[0], &HashMap::new(), "ctx", "", None, &HashMap::new()).unwrap();
        assert!(e.config.env.iter().any(|v| v == "FOO=bar"));
        assert_eq!(e.config.working_dir, "/app");
        assert_eq!(e.config.user.as_deref(), Some("1000"));
        assert_eq!(e.config.cmd, Some(vec!["/bin/run".to_string()]));
        assert!(e.config.labels.contains(&("a".to_string(), "b".to_string())));
    }

    #[test]
    fn command_shell_form_maps_to_sh_c() {
        let file = parse("FROM alpine\nCMD echo hi\n").unwrap();
        let mut graph = Graph::new();
        let e = emit_target(&mut graph, &file.targets[0], &HashMap::new(), "ctx", "", None, &HashMap::new()).unwrap();
        assert_eq!(
            e.config.cmd,
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string()
            ])
        );
    }
}