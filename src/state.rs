//! In-memory upstream state: health, quality EWMAs, NAR affinity.
//!
//! Cold start by design (ADR-0004): nothing persists, rank falls back to
//! config order until probes and traffic fill the numbers in.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

const EWMA_ALPHA: f64 = 0.3;

/// How many narinfo→upstream affinity entries to hold before dropping them
/// all. Affinity metadata only, not response caching (ADR-0002 still holds).
// ponytail: clear-all eviction; switch to LRU if churn ever thrashes it.
const AFFINITY_CAP: usize = 4096;

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
    pub tier: usize,
    /// `None` until the first probe answers.
    pub healthy: Option<bool>,
    pub probe_ms: Option<f64>,
    pub narinfo_ms: Option<f64>,
    pub nar_mbps: Option<f64>,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
}

pub struct Registry {
    upstreams: Mutex<Vec<UpstreamSnapshot>>,
    affinity: Mutex<HashMap<String, usize>>,
    pub started: Instant,
}

impl Registry {
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
                    nar_mbps: None,
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

    pub fn record_probe(&self, index: usize, result: Option<f64>) {
        self.with(index, |u| match result {
            Some(ms) => {
                u.healthy = Some(true);
                u.probe_ms = Some(ewma(u.probe_ms, ms));
            }
            None => u.healthy = Some(false),
        });
    }

    pub fn record_narinfo_hit(&self, index: usize, ms: f64) {
        self.with(index, |u| {
            u.hits += 1;
            u.narinfo_ms = Some(ewma(u.narinfo_ms, ms));
        });
    }

    pub fn record_nar_throughput(&self, index: usize, mbps: f64) {
        self.with(index, |u| u.nar_mbps = Some(ewma(u.nar_mbps, mbps)));
    }

    pub fn record_hit(&self, index: usize) {
        self.with(index, |u| u.hits += 1);
    }

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

    pub fn set_affinity(&self, nar_path: String, index: usize) {
        let mut map = self.affinity.lock().unwrap();
        if map.len() >= AFFINITY_CAP {
            map.clear();
        }
        map.insert(nar_path, index);
    }

    pub fn affinity(&self, nar_path: &str) -> Option<usize> {
        self.affinity.lock().unwrap().get(nar_path).copied()
    }

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
