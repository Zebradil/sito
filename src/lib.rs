//! A local always-on Nix substituter proxy for roaming clients.
//!
//! Nix is pointed at one substituter (`http://localhost:<port>`); sito routes
//! each request to the best reachable upstream binary cache, so a laptop that
//! moves between the home LAN and the outside world neither pays a
//! connect-timeout tax nor needs its substituter list edited.
//!
//! The pieces: [`config`] parses the tiers of upstreams, [`state::Registry`]
//! holds their live health and quality signals, [`probe`] keeps reachability
//! fresh, [`select`] turns that state into an ordered selection plan, and
//! [`proxy`] streams the chosen upstream's bytes through unmodified.
//!
//! Vocabulary (upstream, tier, strategy, selection plan, quality signal,
//! probe, pass-through) is defined in `CONTEXT.md`; the rationale for each
//! design choice lives in `docs/adr/`.

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
///
/// Upstreams are flattened here into a single index space, in config order:
/// tier 0's upstreams first, then tier 1's, and so on. That index is the only
/// id the rest of the program uses — [`state::Registry`] stores one slot per
/// index and [`proxy::TierShape`] records which indices belong to which tier,
/// so the two never need to agree on anything but a `usize`.
///
/// The two [`ureq`] agents differ only in timeout policy: `info_agent` is
/// bounded end to end because narinfo responses are small and a slow one is a
/// reason to move on, while `nar_agent` bounds only the connect phase — a
/// multi-gigabyte NAR may legitimately take minutes, so a global deadline
/// would abort healthy transfers.
///
/// Errors if the listen address cannot be bound. The probe thread is already
/// running when that happens; it is detached and harmless in a process that
/// is about to exit.
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

/// Run the accept loop on the calling thread. Blocks forever in normal
/// operation; see [`proxy::serve`] for the failure mode that ends it.
pub fn serve(app: Arc<App>, server: Server, max_inflight: usize) -> Result<()> {
    proxy::serve(app, server, max_inflight)
}
