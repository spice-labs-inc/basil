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
    /// File-level (global-scope) `ARG` declarations before the first `FROM`.
    /// These default every target's build args of the same name; a stage only
    /// sees the value after it redeclares the arg (bare or defaulted) itself,
    /// mirroring Dockerfile global-scope semantics.
    pub global_args: Vec<Arg>,
}

/// One named stage (`FROM <base> AS <name>`, or an implicit single stage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The stage's name — the `AS` alias. A target referenced by a `BUILD`
    /// reaction is a *deliverable* and its name is a fully-qualified image ref
    /// (e.g. `ghcr.io/mycompany/app`); otherwise it is an internal/task name.
    /// Whether a target is a deliverable is decided later by `BUILD` membership.
    pub name: String,
    /// `REGISTER <fullname>` — the fully-qualified image ref this deliverable is
    /// pushed to, when the target's `name` is a short stage name. When present
    /// the target is addressed by `name` (stage ref, `-t`, `BUILD`) but pushed
    /// to `register`. It does NOT fold into the tagHash — content keeps its
    /// digest across registries — it only selects *where* to look up/push it.
    pub register: Option<String>,
    /// `GIVEN <path>…` — explicit closure inputs. When present the target's
    /// *derived-data* component is computed from exactly these context paths
    /// (files/dirs/globs, dockerignore-filtered) instead of from its `COPY`/`ADD`
    /// sources. The build recipe (RUN/COPY/ADD) still builds the image and still
    /// contributes via `programHash`; `GIVEN` only overrides which context files
    /// feed the tagHash. It is an identity-only directive (not rendered to the
    /// builder).
    pub given: Option<Vec<String>>,
    /// The stage's base — `FROM` image or `MODULE` dependency. Exactly one of
    /// the base forms is set (the parser enforces it).
    pub base: Base,
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

impl Target {
    /// The fully-qualified image ref this target pushes to: `register` when
    /// present, else `name`. Delivers the digest-independent location that
    /// content is looked up at. The stage is addressed by `name`; this returns
    /// where the result image is published.
    pub fn push_ref(&self) -> &str {
        self.register.as_deref().unwrap_or(&self.name)
    }

    /// The `FROM` image base, when this target's base is an image (not a
    /// `MODULE`). Module-base targets return `None`.
    pub fn base_image(&self) -> Option<&BaseImage> {
        match &self.base {
            Base::Image(b) => Some(b),
            Base::Module(_) => None,
        }
    }
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
    /// `FROM --platform=<sel>` platform selector, if any. `$BUILDPLATFORM` /
    /// `$TARGETPLATFORM` are build-staged vars buildah expands at build time;
    /// a literal `linux/amd64` selects that platform.
    pub platform: Option<String>,
}

/// A `MODULE <path> [-f <file>] [TARGET <name>] [AS <name>]` base: the stage's
/// base image is another basil build — its own context + Basilfile.
///
/// The module is a separate closure domain: its deliverable's `tagHash` is
/// folded into the importer's identity and the importer's Containerfile renders
/// `FROM <module-register>:<module-tagHash> AS <alias>`. Build semantics are
/// use-or-build: if that content tag exists in the module's registry the image
/// is pulled, otherwise the module is built and pushed first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRef {
    /// Module context dir, resolved against the importing file's directory
    /// (absolute paths used as-is).
    pub path: String,
    /// The module's Basilfile name; defaults to `<path>/Basilfile`.
    pub file: Option<String>,
    /// Optional deliverable selector (`TARGET <name>`); when absent the module
    /// file must declare exactly one qualified deliverable.
    pub target: Option<String>,
    /// DSL line number (error context).
    pub line: usize,
}

/// A target's base: either a `FROM <image>` or a `MODULE <path>` dependency.
/// Exactly one is set — the parser enforces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Image(BaseImage),
    Module(ModuleRef),
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
    /// `IMPORT <image>` — inline the rootfs content of another image into the
    /// build at this point. A basil-only instruction: a Containerfile has no
    /// such statement, so a stage containing one is built imperatively
    /// (buildah from/config/copy/run/add) instead of rendered to `buildah bud`.
    Import(ImportImage),
    /// `COMMIT` — checkpoint the build so far into the layer cache: the
    /// working container is sealed as a `basil-cache-<prefixHash>` image (its
    /// identity is the closure of the instruction prefix up to this point). A
    /// later build resumes from the deepest matching checkpoint instead of
    /// re-executing the prefix. A cache-policy directive: it never changes
    /// image content, contributes nothing to the program hash (like `OUTPUT`),
    /// and the final committed image is identical with or without it.
    Commit,
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

/// `IMPORT <image>` — the image to inline at this point in the build. The
/// import image's root filesystem content is copied over the working rootfs
/// (imported files overwrite existing paths), then the build continues.
///
/// Identity folds the import's *resolved manifest digest* (`import.<n>`), so an
/// unpinned ref (`IMPORT foo:latest`) re-hashes when the tag moves. Only
/// external images are supported — a ref naming a local stage is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportImage {
    /// The image ref as written after `IMPORT` (e.g. `alpine`, `ghcr.io/a/b:1.2`).
    pub raw: String,
    /// Explicit tag (`:3.23`), if any.
    pub tag: Option<String>,
    /// Pinned digest (`@sha256:...`), if any. Present = hermetic.
    pub digest: Option<String>,
    /// 1-based source line (error context).
    pub line: usize,
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
    pub chmod: Option<String>,
    pub sources: Vec<String>,
    pub dest: String,
}

/// `RUN [--mount=...] <command>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub mounts: Vec<Mount>,
    /// `RUN --network=<none|host|default>`.
    pub network: Option<String>,
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
    /// A GitHub `release` event. Bare `ON release` matches any release; a
    /// selector narrows it (e.g. `release(published)`, `release(prerelease)`).
    Release,
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