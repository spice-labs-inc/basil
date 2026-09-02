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
            // Set up a tokio runtime and connect to buildkitd (external mode).
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let mut client = buildkit_client::client::BuildKitClient::connect(&args.addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("connecting to {}: {}", args.addr, e))?;
                client.health_check().await.map_err(|e| anyhow::anyhow!("{e}"))?;

                // A session is required for the solve + context filesync.
                let mut session = buildkit_client::session::Session::new();
                let ctx_path = std::path::PathBuf::from(&args.context);
                session.add_file_sync(ctx_path).await;
                session
                    .start(client.control().clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("starting session: {}", e))?;

                // Read event from --event or stdin.
                let event_json = match &args.event {
                    Some(p) => Some(std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{e}"))?),
                    None => {
                        let mut s = String::new();
                        use std::io::Read;
                        std::io::stdin().read_to_string(&mut s).map_err(|e| anyhow::anyhow!("{e}"))?;
                        if s.trim().is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    }
                };

                let inp = basil::solver::orchestrate::BuildInput {
                    file: &args.file,
                    context: std::path::Path::new(&args.context),
                    event_json: event_json.as_deref(),
                    target_filter: args.target.as_ref(),
                };
                // A separate channel for the bridge/orchestrator (tonic Channel
                // is Clone; build one from the same addr).
                let channel = tonic::transport::Endpoint::from_shared(args.addr.clone())
                    .map_err(|e| anyhow::anyhow!("bad addr: {e}"))?
                    .connect()
                    .await
                    .map_err(|e| anyhow::anyhow!("connecting bridge channel: {e}"))?;
                let results = basil::solver::orchestrate::run(
                    channel,
                    &session.get_id(),
                    inp,
                )
                .await?;
                for (k, v) in &results {
                    println!("{k} = {v}");
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Gc(args) => {
            anyhow::bail!("gc subcommand not implemented yet: {}", args.age)
        }
    }
}