pub mod config;
pub mod probe;
pub mod proxy;
pub mod select;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tiny_http::Server;

use crate::proxy::{App, TierShape};
use crate::select::DefaultEngine;
use crate::state::Registry;

/// Bind the listener and assemble the running pieces (probe thread included).
/// Split from [`serve`] so tests can bind port 0 and learn the real address.
pub fn build(cfg: &config::Config) -> Result<(Arc<App>, Server)> {
    let urls_by_tier: Vec<Vec<String>> = cfg
        .tiers
        .iter()
        .map(|t| t.upstreams.iter().map(|u| u.url.clone()).collect())
        .collect();
    let registry = Arc::new(Registry::new(&urls_by_tier));

    let mut tiers = Vec::new();
    let mut next = 0usize;
    for t in &cfg.tiers {
        let indices = (next..next + t.upstreams.len()).collect::<Vec<_>>();
        next += t.upstreams.len();
        tiers.push(TierShape {
            strategy: t.strategy,
            indices,
        });
    }

    let prober = probe::spawn(
        registry.clone(),
        Duration::from_secs(cfg.probe_interval_secs),
        Duration::from_secs(cfg.probe_timeout_secs),
    );

    let info_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let nar_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .build()
        .into();

    let app = Arc::new(App {
        registry,
        engine: Box::new(DefaultEngine),
        prober,
        tiers,
        info_agent,
        nar_agent,
    });
    let server =
        Server::http(&cfg.listen).map_err(|e| anyhow::anyhow!("bind {}: {e}", cfg.listen))?;
    Ok((app, server))
}

pub fn serve(app: Arc<App>, server: Server, max_inflight: usize) -> Result<()> {
    proxy::serve(app, server, max_inflight)
}
