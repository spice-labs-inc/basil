//! AST for the basil DSL — a Dockerfile superset plus `TASK`, `PLATFORM`, and
//! top-level `ON` reactions.
//!
//! The design (docs/DESIGN.md) fixes an instruction subset. Anything outside it
//! is a parse-time error, so the parser must be able to reject heredocs,
//! `COPY --link`, `RUN --network`/`--security`, `SHELL`, `STOPSIGNAL`,
//! `HEALTHCHECK`, `ONBUILD`, and per-instruction `--platform`. The AST models
//! only the supported subset; rejection happens during parsing.

/// A parsed basil file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Targets in source order. The first non-comment `FROM` their base may be
    /// an external image or another target's name.
    pub targets: Vec<Target>,
    /// Top-level reactions.
    pub reactions: Vec<Reaction>,
    /// File-wide default platforms (`PLATFORM` before the first `FROM`).
    pub default_platforms: Vec<Platform>,
}

/// One named stage (`FROM <base> AS <name>`, or an implicit single stage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The stage's name — the `AS` alias. A target referenced by a `BUILD`
    /// reaction is a *deliverable* and its name is a fully-qualified image ref
    /// (e.g. `ghcr.io/mycompany/app`); otherwise it is an internal/task name.
    /// Whether a target is a deliverable is decided later by `BUILD` membership.
    pub name: String,
    /// The stage's base image.
    pub base: BaseImage,
    /// Build steps in source order (no `FROM`; the base is separate).
    pub instructions: Vec<Instruction>,
    /// Optional `TASK` clause — side-effect commands run on the resolved base
    /// image outside the build.
    pub task: Option<TaskCmd>,
    /// Per-target platform list (`PLATFORM` inside this stage), overriding the
    /// file-wide default.
    pub platforms: Option<Vec<Platform>>,
    /// 1-based source line of the target's `FROM`.
    pub line: usize,
}

/// The base of a target's `FROM`.
///
/// A bare base (e.g. `alpine`) is ambiguous at parse time: `FROM <x>` resolves
/// like Dockerfile — a local `AS` alias shadows a registry reference — so a
/// base matching a target name is that stage, otherwise an image pull. The
/// parser records the structural reference; target-vs-external resolution
/// happens later (Phase 2, `FROM`-digest folding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseImage {
    /// The base as written after `FROM` (e.g. `alpine`, `ghcr.io/a/b:1.2`).
    pub raw: String,
    /// Explicit tag (`:3.23`), if any.
    pub tag: Option<String>,
    /// Pinned digest (`@sha256:...`), if any. Present = hermetic.
    pub digest: Option<String>,
}

/// A build instruction inside a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    Arg(Arg),
    Env(Env),
    Copy(Copy),
    Add(Add),
    Run(Run),
    Cmd(Command),
    Entrypoint(Command),
    User(String),
    Workdir(Workdir),
    Label(Vec<(String, String)>),
    Expose(Vec<String>),
    Volume(Vec<String>),
    /// `OUTPUT <container-src> <host-dest>` — reverse COPY: copy a file out of
    /// the built image onto the host (relative dest only).
    Output(Output),
}

/// `ARG name[=default]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub name: String,
    pub default: Option<String>,
    pub line: usize,
}

/// `ENV K=V [K2=V2 ...]` or `ENV K V` (single-pair form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub pairs: Vec<(String, String)>,
}

/// `COPY [--from=<src>] [--chown=...] [--chmod=...] <src>... <dest>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub from: Option<String>,
    pub chown: Option<String>,
    pub chmod: Option<String>,
    pub sources: Vec<String>,
    pub dest: String,
}

/// `OUTPUT <container-src> <host-dest>` — a reverse COPY: after building, copy
/// a file out of the image's root filesystem to the host. `dest` is relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Path inside the built image (its rootfs).
    pub source: String,
    /// Host destination, relative to the build context. Only relative paths are
    /// allowed (no absolute host path — prevents escaping the context).
    pub dest: String,
}

/// `ADD` — same shape as `COPY`; a URL source marks the target non-hermetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    pub chown: Option<String>,
    pub sources: Vec<String>,
    pub dest: String,
}

/// `RUN [--mount=type=...] <command>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub mounts: Vec<Mount>,
    pub command: Command,
}

/// A `--mount` specification on a `RUN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// `--mount=type=bind,source=S,target=T[,readonly]`
    Bind {
        source: Option<String>,
        target: String,
        read_only: bool,
    },
    /// `--mount=type=secret,id=ID[,target=T]`
    Secret { id: String, target: Option<String> },
    /// `--mount=type=ssh[,id=ID]`
    Ssh { id: Option<String> },
    /// `--mount=type=cache[,id=ID],target=T`
    Cache { id: Option<String>, target: String },
}

/// A command in shell or exec form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `CMD echo hi`, `RUN /bin/sh -c 'x'` — via the image shell.
    Shell(String),
    /// `CMD ["prog", "arg"]`, `ENTRYPOINT ["/app"]` — direct execution.
    Exec(Vec<String>),
}

/// `WORKDIR <path>`; `WORKDIR` with a resolved `$VAR` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workdir {
    pub path: String,
}

/// A `TASK` clause — the side-effect command: the command itself, like `RUN`,
/// run as a one-shot container on the target's resolved base image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCmd {
    pub command: Command,
}

/// An `ON` reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub event: EventMatch,
    /// `BUILD <target>...` — build + tag + push (deliverables).
    pub build: Vec<String>,
    /// `DO <target>...` — run each target's `TASK` clause.
    pub do_list: Vec<String>,
    /// 1-based source line.
    pub line: usize,
}

/// The matched event token and its refiners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMatch {
    pub token: EventToken,
    /// The selector values, e.g. `push(main, develop)` → `["main", "develop"]`.
    pub selectors: Vec<String>,
}

/// The event token in an `ON` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventToken {
    PrOpened,
    PrSynchronized,
    PrMerged,
    Push,
    Tag,
    /// An unrecognized token (e.g. `image-published`), resolved later or deferred.
    Custom(String),
}

/// A platform `<os>/<arch>[/<variant>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub variant: Option<String>,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.variant {
            Some(v) => write!(f, "{}/{}/{}", self.os, self.arch, v),
            None => write!(f, "{}/{}", self.os, self.arch),
        }
    }
}