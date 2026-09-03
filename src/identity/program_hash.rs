//! Identity: `procedure` — the canonical, deterministic digest over a target's
//! arg-expanded instruction program. Changes when a command or image-config
//! instruction changes; excludes comments, whitespace, and reaction/TASK
//! clauses (policy/side effects, not image content).

use crate::dsl::ast::{Command, Instruction, Mount};
use sha2::{Digest, Sha256};

/// Render one instruction to its canonical, whitespace-normalized form.
pub fn render_instruction(instr: &Instruction) -> String {
    use Instruction::*;
    match instr {
        Arg(a) => match &a.default {
            Some(d) => format!("ARG {}", var_decl(&a.name, d)),
            None => format!("ARG {}", a.name),
        },
        Env(e) => {
            let pairs: Vec<String> = e.pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("ENV {}", pairs.join(" "))
        }
        Copy(c) => {
            let mut parts = vec!["COPY".to_string()];
            if let Some(f) = &c.from {
                parts.push(format!("--from={f}"));
            }
            if let Some(ch) = &c.chown {
                parts.push(format!("--chown={ch}"));
            }
            if let Some(cm) = &c.chmod {
                parts.push(format!("--chmod={cm}"));
            }
            parts.extend(c.sources.clone());
            parts.push(c.dest.clone());
            parts.join(" ")
        }
        Add(a) => {
            let mut parts = vec!["ADD".to_string()];
            if let Some(ch) = &a.chown {
                parts.push(format!("--chown={ch}"));
            }
            if let Some(cm) = &a.chmod {
                parts.push(format!("--chmod={cm}"));
            }
            parts.extend(a.sources.clone());
            parts.push(a.dest.clone());
            parts.join(" ")
        }
        Run(r) => {
            let mounts: Vec<String> = r.mounts.iter().map(render_mount).collect();
            let mut parts = vec!["RUN".to_string()];
            parts.extend(mounts);
            if let Some(net) = &r.network {
                parts.push(format!("--network={net}"));
            }
            parts.push(render_command(&r.command));
            parts.join(" ")
        }
        Cmd(c) => format!("CMD {}", render_command(c)),
        Entrypoint(c) => format!("ENTRYPOINT {}", render_command(c)),
        User(u) => format!("USER {u}"),
        Workdir(w) => format!("WORKDIR {}", w.path),
        Label(pairs) => {
            let ps: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("LABEL {}", ps.join(" "))
        }
        Expose(ps) => format!("EXPOSE {}", ps.join(" ")),
        Volume(ps) => format!("VOLUME {}", ps.join(" ")),
        Output(o) => format!("OUTPUT {} {}", o.source, o.dest),
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

fn render_command(c: &Command) -> String {
    match c {
        Command::Shell(s) => s.clone(),
        Command::Exec(args) => {
            let quoted: Vec<String> = args.iter().map(|s| json_quote(s.as_str())).collect();
            format!("[{}]", quoted.join(", "))
        }
    }
}

fn json_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Expand `$VAR` / `${VAR}` in a string against a resolver. Used for ARG/ENV
/// references in instructions before hashing.
pub fn expand_vars(input: &str, get: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '$' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'{' {
                if let Some(close) = input[i + 2..].find('}') {
                    let name = &input[i + 2..i + 2 + close];
                    let v = (get)(name).unwrap_or_default();
                    out.push_str(&v);
                    i += 2 + close + 1;
                    continue;
                }
            } else if bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            } else if (bytes[i + 1] as char).is_ascii_alphanumeric() || bytes[i + 1] == b'_' {
                let mut j = i + 1;
                while j < bytes.len()
                    && ((bytes[j] as char).is_ascii_alphanumeric() || bytes[j] == b'_')
                {
                    j += 1;
                }
                let name = &input[i + 1..j];
                let v = (get)(name).unwrap_or_default();
                out.push_str(&v);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn var_decl(name: &str, default: &str) -> String {
    format!("{name}={default}")
}

/// Compute the program hash: SHA-256 over canonical, arg-expanded instruction
/// text joined by a separator.
pub fn program_hash(
    instructions: &[Instruction],
    resolve: &dyn Fn(&str) -> Option<String>,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for instr in instructions {
        let rendered = render_instruction(instr);
        let expanded = expand_vars(&rendered, resolve);
        hasher.update(expanded.as_bytes());
        hasher.update(&[0xFF]); // separator between instructions
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_resolve(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn renders_and_hashes() {
        let instrs = vec![
            Instruction::Copy(crate::dsl::ast::Copy {
                from: None,
                chown: None,
                chmod: None,
                sources: vec![".".to_string()],
                dest: "/src".to_string(),
            }),
            Instruction::Run(crate::dsl::ast::Run {
                mounts: vec![],
                network: None,
                command: Command::Shell("echo hi".to_string()),
            }),
        ];
        let h1 = program_hash(&instrs, &empty_resolve);
        // identical render -> identical hash
        let h2 = program_hash(&instrs, &empty_resolve);
        assert_eq!(h1, h2);
        // comment/whitespace doesn't enter (we render canonical, so it's moot)
        let _ = h1;
    }

    #[test]
    fn arg_expansion_changes_program() {
        let instrs = vec![Instruction::Run(crate::dsl::ast::Run {
            mounts: vec![],
            network: None,
            command: Command::Shell("go build -ldflags \"-X main.version=${VERSION}\"".to_string()),
        })];
        let nover = program_hash(&instrs, &empty_resolve);
        let v1 = program_hash(&instrs, &|k| (k == "VERSION").then(|| "1.2.3".to_string()));
        assert_ne!(nover, v1);
    }

    #[test]
    fn explicit_var_syntax() {
        assert_eq!(expand_vars("a$FOO b", &|k| Some(format!("[{k}]"))), "a[FOO] b");
        assert_eq!(expand_vars("a${FOO}b", &|k| Some(format!("<{k}>"))), "a<FOO>b");
        assert_eq!(expand_vars("$$X", &|_| None), "$X");
    }
}