pub mod cli;

use clap::Parser;
use cli::{Cli, Command};
use std::collections::HashMap;

/// CLI resolver: `scratch` and pinned bases resolve offline; unpinned external
/// bases delegate to the registry client (Phase 5). For now the offline path
/// errors on unpinned, matching the design-for-offline closure.
struct CliResolver;
impl basil::identity::closure::BaseResolver for CliResolver {
    fn resolve(
        &mut self,
        raw: &str,
        tag: Option<&str>,
        digest: Option<&str>,
    ) -> Result<String, String> {
        if raw == "scratch" {
            return Ok("scratch-empty".to_string());
        }
        digest.map(|d| d.to_string()).ok_or_else(|| {
            format!("unpinned base `{raw}` ({}): requires registry resolution — pin it with @sha256:...", tag.unwrap_or("no tag"))
        })
    }
}
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Closure(args) => {
            let src = std::fs::read_to_string(&args.file)
                .map_err(|e| anyhow::anyhow!("reading {}: {}", args.file, e))?;
            let file = basil::dsl::parser::parse(&src).map_err(anyhow::Error::msg)?;
            let ctx = std::path::Path::new(&args.context);
            let tags = basil::identity::closure::compute_closure(
                &file.targets,
                &file.default_platforms,
                ctx,
                &HashMap::new(),
                &mut CliResolver,
            )
            .map_err(anyhow::Error::msg)?;
            println!("{} target(s), {} reaction(s)\n", file.targets.len(), file.reactions.len());
            for t in &file.targets {
                let tag = basil::identity::closure::hex(
                    &tags.get(&t.name).copied().unwrap_or([0; 32]),
                );
                let n = format!("  {:<42}  {} instr     tag: {}", t.name, t.instructions.len(), tag);
                println!("{n}");
            }
            Ok(())
        }
        Command::Build(args) => {
            anyhow::bail!("build subcommand not implemented yet: {}", args.file)
        }
        Command::Gc(args) => {
            anyhow::bail!("gc subcommand not implemented yet: {}", args.age)
        }
    }
}