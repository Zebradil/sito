use anyhow::Result;
use clap::Parser;

/// Local Nix substituter proxy: one localhost endpoint, the best reachable
/// upstream behind it.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Path to the TOML config file.
    #[arg(long, env = "SITO_CONFIG")]
    config: std::path::PathBuf,

    /// Override the listen address from the config file.
    #[arg(long, env = "SITO_LISTEN")]
    listen: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sito=info".into()),
        )
        .init();
    let args = Args::parse();
    let mut cfg = sito::config::Config::load(&args.config)?;
    if let Some(listen) = args.listen {
        cfg.listen = listen;
    }
    let (app, server) = sito::build(&cfg)?;
    tracing::info!(
        listen = cfg.listen,
        upstreams = app.registry.snapshot().len(),
        "sito serving"
    );
    sito::serve(app, server, cfg.max_inflight)
}
