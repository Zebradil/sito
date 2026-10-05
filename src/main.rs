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
    use std::io::IsTerminal;
    // Colour only on a terminal: under launchd stderr is a log file, where
    // escape codes get in the way of grep.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
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
    sito::serve(app, server)
}
