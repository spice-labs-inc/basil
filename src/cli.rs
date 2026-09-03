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
    /// Resolve unpinned image bases to @sha256 and rewrite the file to pin them.
    Pin(PinArgs),
    /// Prune basil-owned registry tags older than a given age.
    Gc(GcArgs),
}

#[derive(clap::Args)]
pub struct ClosureArgs {
    /// The DSL file to parse.
    #[arg(default_value = "Basilfile")]
    pub file: String,
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(long, default_value = ".")]
    pub context: String,
    /// Restrict the report to one target.
    pub target: Option<String>,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// The DSL file to parse.
    #[arg(default_value = "Basilfile")]
    pub file: String,
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(long, default_value = ".")]
    pub context: String,
    /// Target to build; default is every qualified deliverable target.
    #[arg(short = 't', long)]
    pub target: Option<String>,
}

#[derive(clap::Args)]
pub struct HandleArgs {
    /// The DSL file to parse.
    #[arg(default_value = "Basilfile")]
    pub file: String,
    /// Build context root (where COPY/ADD and `.dockerignore` resolve).
    #[arg(long, default_value = ".")]
    pub context: String,
    /// Path to an event JSON file; read stdin if absent.
    #[arg(long)]
    pub event: Option<String>,
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
    /// The DSL file whose qualified AS targets are pruned (ignored with --repo).
    #[arg(default_value = "Basilfile")]
    pub file: String,
    /// Restrict pruning to one repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// List what would be deleted without deleting.
    #[arg(long)]
    pub dry_run: bool,
}