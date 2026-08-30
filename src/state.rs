//! In-memory upstream state: health, quality EWMAs, NAR affinity.
//!
//! Cold start by design (ADR-0004): nothing persists, rank falls back to
//! config order until probes and traffic fill the numbers in.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Weight given to the newest sample in [`ewma`]; the remaining 0.7 stays
/// with the accumulated history. Roughly a five-sample memory — fast enough
/// to follow a network change, slow enough to ignore a single slow response.
const EWMA_ALPHA: f64 = 0.3;

/// How many narinfo→upstream affinity entries to hold before dropping them
/// all. Affinity metadata only, not response caching (ADR-0002 still holds).
// ponytail: clear-all eviction; switch to LRU if churn ever thrashes it.
const AFFINITY_CAP: usize = 4096;

/// Fold a new sample into an exponentially weighted moving average.
///
/// A running average is what every quality signal here uses: it costs one
/// float per upstream, keeps no history to bound or evict, and decays old
/// samples on its own, so an upstream that got slow when the laptop left the
/// LAN stops being ranked on its LAN numbers within a handful of requests.
///
/// `prev` of `None` seeds the average with `x` rather than with zero, so a
/// fresh upstream is ranked on its first real measurement instead of climbing
/// out of a warm-up bias.
fn ewma(prev: Option<f64>, x: f64) -> f64 {
    match prev {
        Some(p) => EWMA_ALPHA * x + (1.0 - EWMA_ALPHA) * p,
        None => x,
    }
}

/// Serializable view of one upstream, as fed to the selection engine and
/// dumped by `/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamSnapshot {
    /// Index into the flat config-order upstream list; the id the selection
    /// plan speaks in.
    pub index: usize,
    pub url: String,
    /// Position of the owning tier in config order; 0 is tried first.
    pub tier: usize,
    /// `None` until the first probe answers. `Some(false)` means the
    /// selection engine skips this upstream entirely, so only a probe can
    /// bring it back.
    pub healthy: Option<bool>,
    /// EWMA of probe round-trip time in milliseconds. `None` until a probe
    /// has succeeded at least once.
    pub probe_ms: Option<f64>,
    /// EWMA of narinfo response time in milliseconds — the primary ranking
    /// signal, since it measures the request shape clients actually wait on.
    /// `None` until this upstream has served a narinfo.
    pub narinfo_ms: Option<f64>,
    /// EWMA of NAR download throughput, in megabytes per second
    /// (bytes / 1e6 / seconds). Recorded only for transfers that ran to
    /// completion; `None` until one has.
    pub nar_mbytes_per_sec: Option<f64>,
    /// Requests this upstream answered with a body: every narinfo hit and
    /// every NAR hit.
    pub hits: u64,
    /// Upstream 404s — the path is simply not in this cache. Routine, and not
    /// evidence of anything wrong with the upstream.
    pub misses: u64,
    /// Transport failures (connect, TLS, timeout, non-404 status). Each one
    /// also marks the upstream unhealthy.
    pub errors: u64,
}

/// The single piece of shared mutable state, held behind mutexes and shared
/// by every request thread and the probe thread.
///
/// Readers get cloned snapshots ([`Registry::snapshot`]) rather than a guard,
/// so no caller holds a lock across a network call; the trade is that a plan
/// is computed from state that may be a few microseconds stale, which for
/// ranking is free. Nothing here is persisted — every start is a cold start
/// (ADR-0004).
///
/// Every `index` argument is a flat upstream index as assigned by
/// [`crate::build`]; an out-of-range one panics, and a poisoned mutex panics
/// too, which for a proxy whose whole state is this struct is the honest
/// outcome.
pub struct Registry {
    upstreams: Mutex<Vec<UpstreamSnapshot>>,
    affinity: Mutex<HashMap<String, usize>>,
    /// Process start, as reported by `/status` uptime.
    pub started: Instant,
}

impl Registry {
    /// Build one slot per upstream, flattening the tiers into the shared
    /// index space: `urls_by_tier[t][n]` becomes the next free `index`, and
    /// its position in the outer slice becomes its `tier`.
    pub fn new(urls_by_tier: &[Vec<String>]) -> Self {
        let mut ups = Vec::new();
        for (tier, urls) in urls_by_tier.iter().enumerate() {
            for url in urls {
                ups.push(UpstreamSnapshot {
                    index: ups.len(),
                    url: url.clone(),
                    tier,
                    healthy: None,
                    probe_ms: None,
                    narinfo_ms: None,
                    nar_mbytes_per_sec: None,
                    hits: 0,
                    misses: 0,
                    errors: 0,
                });
            }
        }
        Registry {
            upstreams: Mutex::new(ups),
            affinity: Mutex::new(HashMap::new()),
            started: Instant::now(),
        }
    }

    /// Point-in-time copy of every upstream, ordered by (and indexable with)
    /// the flat upstream index. Callers work off the copy so the lock is not
    /// held while planning or fetching.
    pub fn snapshot(&self) -> Vec<UpstreamSnapshot> {
        self.upstreams.lock().unwrap().clone()
    }

    pub fn url(&self, index: usize) -> String {
        self.upstreams.lock().unwrap()[index].url.clone()
    }

    // ponytail: one mutex over all upstream state; per-upstream locks only if
    // contention ever shows up on a localhost proxy (it won't).
    fn with<R>(&self, index: usize, f: impl FnOnce(&mut UpstreamSnapshot) -> R) -> R {
        f(&mut self.upstreams.lock().unwrap()[index])
    }

    /// Record a probe pass: `Some(ms)` is a reachable upstream and folds into
    /// the probe EWMA, `None` is a failed probe and marks it down. A failed
    /// probe deliberately leaves `probe_ms` alone — the last known latency
    /// stays put so an upstream that comes back is not re-ranked from
    /// scratch.
    pub fn record_probe(&self, index: usize, result: Option<f64>) {
        self.with(index, |u| match result {
            Some(ms) => {
                u.healthy = Some(true);
                u.probe_ms = Some(ewma(u.probe_ms, ms));
            }
            None => u.healthy = Some(false),
        });
    }

    /// A served narinfo: counts as a hit *and* feeds `ms` (milliseconds, from
    /// request start to response headers) into the narinfo EWMA. Callers must
    /// not also call [`Registry::record_hit`] for the same request.
    pub fn record_narinfo_hit(&self, index: usize, ms: f64) {
        self.with(index, |u| {
            u.hits += 1;
            u.narinfo_ms = Some(ewma(u.narinfo_ms, ms));
        });
    }

    /// Fold a completed NAR transfer into the throughput EWMA. `mbytes_per_sec`
    /// is megabytes per second (bytes / 1e6 / seconds), measured over the whole
    /// body. Hit counting is separate: this is only the speed signal.
    pub fn record_nar_throughput(&self, index: usize, mbytes_per_sec: f64) {
        self.with(index, |u| {
            u.nar_mbytes_per_sec = Some(ewma(u.nar_mbytes_per_sec, mbytes_per_sec))
        });
    }

    /// A served response with no timing worth keeping — NAR hits, whose speed
    /// arrives later via [`Registry::record_nar_throughput`].
    pub fn record_hit(&self, index: usize) {
        self.with(index, |u| u.hits += 1);
    }

    /// The upstream answered 404: it simply does not have this path. Not a
    /// health signal — selection moves on to the next upstream and this one
    /// keeps its rank.
    pub fn record_miss(&self, index: usize) {
        self.with(index, |u| u.misses += 1);
    }

    /// A transport error talking to an upstream: count it and mark the
    /// upstream down right away — the kicked probe decides when it is back.
    pub fn record_error(&self, index: usize) {
        self.with(index, |u| {
            u.errors += 1;
            u.healthy = Some(false);
        });
    }

    /// Remember which upstream served the narinfo that named `nar_path`, so
    /// the NAR request that follows goes to the same place — narinfo `URL:`
    /// fields are relative to their own cache, so another upstream's copy is
    /// not addressable by that path (ADR-0003).
    ///
    /// `nar_path` is stored exactly as the narinfo wrote it, without a
    /// leading slash. At `AFFINITY_CAP` entries the whole map is dropped,
    /// which costs at most one extra tier walk per forgotten NAR.
    pub fn set_affinity(&self, nar_path: String, index: usize) {
        let mut map = self.affinity.lock().unwrap();
        if map.len() >= AFFINITY_CAP {
            map.clear();
        }
        map.insert(nar_path, index);
    }

    /// The upstream remembered for `nar_path`, or `None` if it was never
    /// recorded or has since been evicted. `None` is not an error: selection
    /// falls back to the normal tier walk.
    pub fn affinity(&self, nar_path: &str) -> Option<usize> {
        self.affinity.lock().unwrap().get(nar_path).copied()
    }

    /// Live affinity entries, for `/status`.
    pub fn affinity_len(&self) -> usize {
        self.affinity.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_starts_at_first_sample() {
        assert_eq!(ewma(None, 10.0), 10.0);
        let second = ewma(Some(10.0), 20.0);
        assert!(second > 10.0 && second < 20.0);
    }

    #[test]
    fn affinity_cap_clears() {
        let reg = Registry::new(&[vec!["http://a".into()]]);
        for i in 0..AFFINITY_CAP {
            reg.set_affinity(format!("nar/{i}"), 0);
        }
        assert_eq!(reg.affinity_len(), AFFINITY_CAP);
        reg.set_affinity("nar/one-more".into(), 0);
        assert_eq!(reg.affinity_len(), 1);
        assert_eq!(reg.affinity("nar/one-more"), Some(0));
    }

    #[test]
    fn error_marks_down_probe_recovers() {
        let reg = Registry::new(&[vec!["http://a".into()]]);
        reg.record_error(0);
        assert_eq!(reg.snapshot()[0].healthy, Some(false));
        reg.record_probe(0, Some(5.0));
        assert_eq!(reg.snapshot()[0].healthy, Some(true));
    }
}
