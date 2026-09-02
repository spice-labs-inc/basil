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
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let inp = basil::solver::orchestrate::BuildInput {
                    file: &args.file,
                    context: std::path::Path::new(&args.context),
                    event_json: None,
                    target_filter: args.target.as_ref(),
                };
                let results = basil::solver::orchestrate::build_targets(inp).await?;
                for (k, v) in &results {
                    println!("{k} = {v}");
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Handle(args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let event_json: String = match &args.event {
                    Some(p) => std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{e}"))?,
                    None => {
                        let mut s = String::new();
                        use std::io::Read;
                        std::io::stdin().read_to_string(&mut s).map_err(|e| anyhow::anyhow!("{e}"))?;
                        s
                    }
                };
                let inp = basil::solver::orchestrate::BuildInput {
                    file: &args.file,
                    context: std::path::Path::new(&args.context),
                    event_json: Some(&event_json),
                    target_filter: None,
                };
                let results = basil::solver::orchestrate::handle_event(inp).await?;
                for (k, v) in &results {
                    println!("{k} = {v}");
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Gc(args) => {
            // gc is registry-only; it needs no builder connection.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let age_secs = basil::solver::gc::parse_age(&args.age).map_err(anyhow::Error::msg)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                // Resolve the repos to prune: explicit --repo, else the file's
                // fully-qualified `AS` targets.
                let mut repos: Vec<(String, String)> = Vec::new(); // (host, repo)
                if let Some(fq) = &args.repo {
                    let r = basil::solver::registry::parse_ref(fq, "latest")
                        .map_err(anyhow::Error::msg)?;
                    repos.push((r.host, r.repo));
                } else {
                    let src = std::fs::read_to_string(&args.file)
                        .map_err(|e| anyhow::anyhow!("reading {}: {}", args.file, e))?;
                    let parsed = basil::dsl::parser::parse(&src).map_err(anyhow::Error::msg)?;
                    for t in &parsed.targets {
                        if t.name.contains('/') {
                            let r = basil::solver::registry::parse_ref(&t.name, "latest")
                                .map_err(anyhow::Error::msg)?;
                            if !repos.iter().any(|(h, rp)| h == &r.host && rp == &r.repo) {
                                repos.push((r.host, r.repo));
                            }
                        }
                    }
                }
                if repos.is_empty() {
                    return Err(anyhow::anyhow!(
                        "no repos to prune: pass --repo or a Basilfile with qualified AS targets"
                    ));
                }

                for (i, (host, repo)) in repos.iter().enumerate() {
                    let client = basil::solver::registry::RegistryClient::new(
                        &basil::solver::registry::ImageRef {
                            host: host.clone(),
                            repo: repo.clone(),
                            tag: String::new(),
                        }
                        .base_url(),
                    );
                    let deleted = basil::solver::gc::gc_repo(&client, repo, age_secs, now, args.dry_run)
                        .await
                        .map_err(anyhow::Error::msg)?;
                    println!("gc[{}]: {repo} -> {} tag(s) {}", i, deleted.len(), if args.dry_run { "(dry-run)" } else { "deleted" });
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
    }
}