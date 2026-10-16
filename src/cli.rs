use clap::{Parser, Subcommand};

/// Basil — a plain-process build tool driven by declarative build files.
#[derive(Parser)]
#[command(name = "basil", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Parse the DSL and report each target's input closure.
    Closure(ClosureArgs),
    /// Build a deliverable target directly (no event). With -t, build that
    /// target; otherwise build every qualified deliverable target.
    Build(BuildArgs),
    /// Read an event from stdin (or --event) and run the matching reactions.
    Handle(HandleArgs),
    /// Run a non-published target's `TASK` clause in a one-shot container,
    /// without an event and without publishing anything (a manual `DO` for
    /// non-published targets). Registered (REGISTER-clause) targets are
    /// excluded
    Run(RunArgs),
    /// Set the immutable `v<version>` tags on the deliverables' published images.
    Promote(PromoteArgs),
    /// Exit 0 if any of the given targets would change (its closure hash is
    /// not yet published at its registry ref), 1 if none changed, 2 on
    /// error. A change-detection gate for CI.
    IsChanged(IsChangedArgs),
    /// Resolve unpinned image bases to @sha256 and rewrite the file to pin them.
    Pin(PinArgs),
    /// Prune basil-owned registry tags older than a given age.
    Gc(GcArgs),
}

#[derive(clap::Args)]
pub struct ClosureArgs {
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Restrict the report to one target.
    pub target: Option<String>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Target to build; default is every qualified deliverable target.
    #[arg(short = 't', long)]
    pub target: Option<String>,
    /// Rebuild and re-push even when every content tag already exists in the
    /// registry (bypasses the idempotent check).
    #[arg(long)]
    pub force: bool,
    /// Where intermediate stages are cached (`basil-cache-*` tags); defaults to
    /// `BASIL_CACHE_REF`, then the deliverable's own repo.
    #[arg(long, value_name = "REPO")]
    pub cache_to: Option<String>,
}

#[derive(clap::Args)]
pub struct HandleArgs {
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Path to an event JSON file; read stdin if absent.
    #[arg(long)]
    pub event: Option<String>,
    /// Rebuild and re-push even when every content tag already exists in the
    /// registry (bypasses the idempotent check).
    #[arg(long)]
    pub force: bool,
    /// Where intermediate stages are cached (`basil-cache-*` tags); defaults to
    /// `BASIL_CACHE_REF`, then the deliverable's own repo.
    #[arg(long, value_name = "REPO")]
    pub cache_to: Option<String>,
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// The non-published target to run (its AS name; REGISTERed targets are
    /// excluded).
    pub target: String,
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Where dependency stage cache images go (`basil-cache-*` tags); defaults
    /// to `BASIL_CACHE_REF`, then the target's own repo when it is qualified.
    #[arg(long, value_name = "REPO")]
    pub cache_to: Option<String>,
}

#[derive(clap::Args)]
pub struct PromoteArgs {
    /// The version to tag (e.g. `1.2.3` or `v1.2.3`); each qualifified
    /// deliverable gets an immutable `v<version>` tag pointing at its current
    /// content tag.
    pub version: String,
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Promote only this deliverable (its AS name or the REGISTER ref);
    /// default is every qualified deliverable target.
    #[arg(short = 't', long)]
    pub target: Option<String>,
}

#[derive(clap::Args)]
pub struct PinArgs {
    /// The DSL file to pin (rewritten in place).
    #[arg(default_value = "Basilfile")]
    pub file: String,
}

#[derive(clap::Args)]
pub struct GcArgs {
    /// Age with unit (e.g. `30d`, `4w`, `1y`).
    pub age: String,
    /// Build context root (where `.dockerignore` resolves).
    #[arg(default_value = ".")]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` path resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Restrict pruning to one repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// List what would be deleted without deleting.
    #[arg(long)]
    pub dry_run: bool,
    /// Leave the content tags (basil-<tagHash>, the published deliverable
    /// identity) untouched; prune only the intermediate-stage cache
    /// namespace (basil-cache-*).
    #[arg(long)]
    pub cache_only: bool,
}

#[derive(clap::Args)]
pub struct IsChangedArgs {
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(default_value = ".", required = true)]
    pub context: String,
    /// DSL file to parse; defaults to `<context>/Basilfile` (docker-style).
    /// A relative `-f` flag resolves against the current directory.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<String>,
    /// Target AS names (or REGISTER refs) to check; at least one required.
    #[arg(required = true, value_name = "TARGET")]
    pub targets: Vec<String>,
}
