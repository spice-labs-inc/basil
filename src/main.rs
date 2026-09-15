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

/// Resolve the DSL file the way `docker build` resolves its Dockerfile: an
/// explicit `-f` path is absolute as-is or relative against the current
/// directory; with no `-f`, the default is `<context>/Basilfile`.
fn resolve_build_file(file: &Option<String>, context: &str) -> String {
    match file.as_deref() {
        Some(f) if std::path::Path::new(f).is_absolute() => f.to_string(),
        Some(f) => std::env::current_dir()
            .map(|cwd| cwd.join(f))
            .unwrap_or_else(|_| std::path::PathBuf::from(f))
            .to_string_lossy()
            .into_owned(),
        None => std::path::Path::new(context)
            .join("Basilfile")
            .to_string_lossy()
            .into_owned(),
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
            let file = resolve_build_file(&args.file, &args.context);
            let src = basil::fsctx::fs_err(std::fs::read_to_string(&file), &file, "read")?;
            let parsed = basil::dsl::parser::parse(&src).map_err(anyhow::Error::msg)?;
            let ctx = std::path::Path::new(&args.context);
            let build_file = std::path::absolute(&file).unwrap_or_else(|_| file.clone().into());
            let file_dir = build_file.parent().unwrap_or(std::path::Path::new("."));
            // Modules resolved offline (pinned bases only — an unpinned module
            // base fails hard, matching the offline closure rule).
            let index = basil::identity::modules::discover_module_index(&parsed.targets, file_dir)
                .map_err(anyhow::Error::msg)?;
            let module_digests = basil::identity::modules::compute_module_digests(
                &index,
                &mut CliResolver,
            )
            .map_err(anyhow::Error::msg)?;
            let tags = basil::identity::closure::compute_closure(
                &parsed.targets,
                &parsed.default_platforms,
                &parsed.global_args,
                ctx,
                &build_file,
                &HashMap::new(),
                &mut CliResolver,
                &module_digests.deliverable,
            )
            .map_err(anyhow::Error::msg)?;
            println!("{} target(s), {} reaction(s)\n", parsed.targets.len(), parsed.reactions.len());
            for t in &parsed.targets {
                let tag = basil::identity::closure::hex(
                    &tags.get(&t.name).copied().unwrap_or([0; 32]),
                );
                let n = format!("  {:<42}  {} instr     tag: {}", t.name, t.instructions.len(), tag);
                println!("{n}");
            }
            Ok(())
        }
        Command::Build(args) => {
            let file = resolve_build_file(&args.file, &args.context);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let inp = basil::solver::orchestrate::BuildInput {
                    file: &file,
                    context: std::path::Path::new(&args.context),
                    event_json: None,
                    target_filter: args.target.as_ref(),
                    force: args.force,
                    cache_to: args.cache_to.as_deref(),
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
            let file = resolve_build_file(&args.file, &args.context);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let event_json: String = match &args.event {
                    Some(p) => basil::fsctx::fs_err(std::fs::read_to_string(p), p, "read")?,
                    None => {
                        let mut s = String::new();
                        use std::io::Read;
                        std::io::stdin().read_to_string(&mut s).map_err(|e| anyhow::anyhow!("{e}"))?;
                        s
                    }
                };
                let inp = basil::solver::orchestrate::BuildInput {
                    file: &file,
                    context: std::path::Path::new(&args.context),
                    event_json: Some(&event_json),
                    target_filter: None,
                    force: args.force,
                    cache_to: args.cache_to.as_deref(),
                };
                let results = basil::solver::orchestrate::handle_event(inp).await?;
                for (k, v) in &results {
                    println!("{k} = {v}");
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Promote(args) => {
            let file = resolve_build_file(&args.file, &args.context);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let promoted = basil::solver::orchestrate::promote_version(
                    &file,
                    std::path::Path::new(&args.context),
                    args.target.as_deref(),
                    &args.version,
                )
                .await?;
                for (k, v) in &promoted {
                    println!("{k} = {v}");
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Pin(args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let src = basil::fsctx::fs_err(std::fs::read_to_string(&args.file), &args.file, "read")?;
                let pinned = basil::solver::pin::pin_file(&src).await.map_err(anyhow::Error::msg)?;
                if pinned == src {
                    println!("basil: no unpinned bases in {}", args.file);
                } else {
                    basil::fsctx::fs_err(std::fs::write(&args.file, pinned), &args.file, "write")?;
                    println!("basil: pinned bases in {}", args.file);
                }
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        }
        Command::Gc(args) => {
            // gc is registry-only; it needs no builder connection.
            let file = resolve_build_file(&args.file, &args.context);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let age_secs = basil::solver::gc::parse_age(&args.age).map_err(anyhow::Error::msg)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                // Resolve the repos to prune: explicit --repo, else the file's
                // deliverable targets (qualified AS names AND REGISTER refs).
                let mut repos: Vec<(String, String)> = Vec::new(); // (host, repo)
                if let Some(fq) = &args.repo {
                    let r = basil::solver::registry::parse_ref(fq, "latest")
                        .map_err(anyhow::Error::msg)?;
                    repos.push((r.host, r.repo));
                } else {
                    let src = basil::fsctx::fs_err(std::fs::read_to_string(&file), &file, "read")?;
                    let parsed = basil::dsl::parser::parse(&src).map_err(anyhow::Error::msg)?;
                    for t in &parsed.targets {
                        // push_ref() = the REGISTER ref, else the AS name — a
                        // bare-AS stage with a REGISTER therefore prunes where
                        // it publishes, exactly like a qualified AS deliverable.
                        let push_ref = t.push_ref();
                        if push_ref.contains('/') {
                            let r = basil::solver::registry::parse_ref(push_ref, "latest")
                                .map_err(anyhow::Error::msg)?;
                            if !repos.iter().any(|(h, rp)| h == &r.host && rp == &r.repo) {
                                repos.push((r.host, r.repo));
                            }
                        }
                    }
                }
                if repos.is_empty() {
                    return Err(anyhow::anyhow!(
                        "no repos to prune: pass --repo or a Basilfile with qualified AS targets or REGISTER refs"
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