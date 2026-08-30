//! Active reachability probes (ADR-0004): timed `GET /nix-cache-info` per
//! upstream — on start, on an interval, and kicked immediately after a
//! request failure.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use crate::state::Registry;

/// Handle to the running probe thread. Holding one does not keep the thread
/// alive and dropping one does not stop it; the only thing it can do is ask
/// for an early pass.
pub struct Prober {
    kick: SyncSender<()>,
}

impl Prober {
    /// Ask for an out-of-band probe pass; a pass already pending absorbs the
    /// kick.
    pub fn kick(&self) {
        let _ = self.kick.try_send(());
    }
}

/// Start the probe loop on its own detached thread: one pass immediately, so
/// health is settled before the first request, then a pass every `interval`
/// or sooner if [`Prober::kick`] fires. The thread never exits and is not
/// joined — the process outlives it by construction.
///
/// `timeout` is the whole per-probe budget, connect through response; an
/// upstream that overruns it counts as down for that pass. Probes run on
/// their own agent so this deadline cannot leak onto request traffic.
///
/// # Panics
///
/// If the thread cannot be spawned. A sito that cannot probe cannot route.
pub fn spawn(registry: Arc<Registry>, interval: Duration, timeout: Duration) -> Prober {
    let (tx, rx): (SyncSender<()>, Receiver<()>) = sync_channel(1);
    std::thread::Builder::new()
        .name("probe".into())
        .spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .build()
                .into();
            loop {
                probe_all(&agent, &registry);
                // Sleep that a kick can cut short.
                let _ = rx.recv_timeout(interval);
            }
        })
        .expect("spawn probe thread");
    Prober { kick: tx }
}

/// One pass over every upstream, sequentially. Reachable means a 2xx on
/// `/nix-cache-info` within the timeout: ureq reports any other status as an
/// error, so a host that answers 404 there is not a usable binary cache and
/// is treated as down.
///
/// Passes are frequent and mostly boring, so the info-level line fires only
/// when a verdict actually flips; steady state stays silent and a network
/// change shows up as a handful of lines.
fn probe_all(agent: &ureq::Agent, registry: &Registry) {
    for u in registry.snapshot() {
        let url = format!("{}/nix-cache-info", u.url.trim_end_matches('/'));
        let start = Instant::now();
        let result = match agent.get(&url).call() {
            Ok(_) => Some(start.elapsed().as_secs_f64() * 1000.0),
            Err(e) => {
                tracing::debug!(url, error = %e, "probe failed");
                None
            }
        };
        let came_up = result.is_some() && u.healthy != Some(true);
        let went_down = result.is_none() && u.healthy != Some(false);
        registry.record_probe(u.index, result);
        if came_up || went_down {
            tracing::info!(url = u.url, up = result.is_some(), "upstream state changed");
        }
    }
}
