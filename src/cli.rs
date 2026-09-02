use clap::{Parser, Subcommand};

/// Basil — a plain-process, buildkit-graph-owning build tool.
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
    /// Build deliverable targets for a delivered event (stdin JSON).
    Build(BuildArgs),
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
    /// Event JSON delivered on stdin (GitHub Actions payload).
    #[arg(long)]
    pub event: Option<String>,
    /// buildkitd gRPC address (defaults to $BUILDKIT_HOST).
    #[arg(long, env = "BUILDKIT_HOST", default_value = "tcp://127.0.0.1:8818")]
    pub addr: String,
    /// Manual trigger: run a specific target's reaction.
    #[arg(short = 't', long)]
    pub target: Option<String>,
}

#[derive(clap::Args)]
pub struct GcArgs {
    /// Age with unit (e.g. `30d`, `4w`, `1y`).
    pub age: String,
    /// Restrict pruning to one repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// List what would be deleted without deleting.
    #[arg(long)]
    pub dry_run: bool,
}