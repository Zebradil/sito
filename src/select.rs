//! The selection boundary (ADR-0005): serializable input in, ordered fetch
//! plan out. The built-in rules are plain Rust; a future scripting engine
//! replaces the `SelectionEngine` impl without touching the proxy core.

use serde::{Deserialize, Serialize};

use crate::config::Strategy;
use crate::state::UpstreamSnapshot;

/// The two request shapes sito routes. Everything else it either answers
/// itself or 404s, so the engine never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestKind {
    /// `/<hash>.narinfo` — small metadata lookup, timed as the ranking
    /// signal, and the request whose answer establishes NAR affinity.
    Narinfo,
    /// `/nar/…` — the store path payload; potentially huge, streamed.
    Nar,
}

/// Everything the engine is allowed to see about one request. Plain
/// serializable data by design: an alternative engine receives exactly this
/// and nothing more (ADR-0005).
#[derive(Debug, Serialize, Deserialize)]
pub struct SelectionInput {
    pub kind: RequestKind,
    /// Request path as received, leading slash included.
    pub path: String,
    /// Upstream that served the narinfo naming this NAR, when known.
    pub affinity: Option<usize>,
    /// Tiers in config order — index 0 is the tier to try first.
    pub tiers: Vec<TierInput>,
}

/// One tier's policy and the live state of its members, in config order.
#[derive(Debug, Serialize, Deserialize)]
pub struct TierInput {
    pub strategy: Strategy,
    pub upstreams: Vec<UpstreamSnapshot>,
}

/// Ordered upstream indices to try; empty means answer 404 without asking
/// anyone.
#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub attempts: Vec<usize>,
}

/// The seam an alternative engine slots into (ADR-0005). The contract:
/// `plan` is a pure function of `input` — no I/O, no hidden state, no
/// reaching around the boundary into the registry — it is called on every
/// request from arbitrary threads, so it must be cheap and `Send + Sync`, and
/// it may return any subset of the offered upstream indices in any order,
/// including none.
pub trait SelectionEngine: Send + Sync {
    fn plan(&self, input: &SelectionInput) -> Plan;
}

/// Built-in rules: affinity first, then tiers in config order; within a tier
/// upstreams ranked by observed latency (narinfo EWMA, falling back to probe
/// EWMA), unmeasured ones keeping config order at the back; upstreams marked
/// down are skipped entirely — the probe loop owns bringing them back.
pub struct DefaultEngine;

impl SelectionEngine for DefaultEngine {
    fn plan(&self, input: &SelectionInput) -> Plan {
        let mut attempts = Vec::new();
        if let Some(idx) = input.affinity
            && !is_down(input, idx)
        {
            attempts.push(idx);
        }
        for tier in &input.tiers {
            let mut ranked: Vec<&UpstreamSnapshot> = tier
                .upstreams
                .iter()
                .filter(|u| u.healthy != Some(false))
                .collect();
            ranked.sort_by(|a, b| {
                score(a)
                    .partial_cmp(&score(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for u in ranked {
                if !attempts.contains(&u.index) {
                    attempts.push(u.index);
                }
            }
        }
        Plan { attempts }
    }
}

/// Whether `idx` is known-down. An index no tier mentions reads as not down;
/// it simply never gets appended by the tier walk that follows.
fn is_down(input: &SelectionInput, idx: usize) -> bool {
    input
        .tiers
        .iter()
        .flat_map(|t| &t.upstreams)
        .any(|u| u.index == idx && u.healthy == Some(false))
}

/// Rank key within a tier, in milliseconds; lower is better. Real traffic
/// beats probe timing because it measures the request shape clients wait on.
///
/// An upstream with neither measurement scores [`f64::MAX`], which sinks it
/// behind everything measured; since the sort is stable, unmeasured upstreams
/// keep their config order relative to each other, which is the cold-start
/// behaviour ADR-0004 asks for.
fn score(u: &UpstreamSnapshot) -> f64 {
    u.narinfo_ms.or(u.probe_ms).unwrap_or(f64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(index: usize, tier: usize) -> UpstreamSnapshot {
        UpstreamSnapshot {
            index,
            url: format!("http://u{index}"),
            tier,
            healthy: None,
            probe_ms: None,
            narinfo_ms: None,
            nar_mbps: None,
            hits: 0,
            misses: 0,
            errors: 0,
        }
    }

    fn input(tiers: Vec<Vec<UpstreamSnapshot>>) -> SelectionInput {
        SelectionInput {
            kind: RequestKind::Narinfo,
            path: "/x.narinfo".into(),
            affinity: None,
            tiers: tiers
                .into_iter()
                .map(|upstreams| TierInput {
                    strategy: Strategy::Sequential,
                    upstreams,
                })
                .collect(),
        }
    }

    #[test]
    fn cold_start_follows_config_order() {
        let plan = DefaultEngine.plan(&input(vec![vec![snap(0, 0), snap(1, 0)], vec![snap(2, 1)]]));
        assert_eq!(plan.attempts, vec![0, 1, 2]);
    }

    #[test]
    fn down_upstreams_are_skipped() {
        let mut a = snap(0, 0);
        a.healthy = Some(false);
        let plan = DefaultEngine.plan(&input(vec![vec![a, snap(1, 0)]]));
        assert_eq!(plan.attempts, vec![1]);
    }

    #[test]
    fn all_down_means_empty_plan() {
        let mut a = snap(0, 0);
        a.healthy = Some(false);
        let plan = DefaultEngine.plan(&input(vec![vec![a]]));
        assert!(plan.attempts.is_empty());
    }

    #[test]
    fn faster_upstream_ranks_first_within_tier() {
        let mut slow = snap(0, 0);
        slow.narinfo_ms = Some(200.0);
        let mut fast = snap(1, 0);
        fast.narinfo_ms = Some(5.0);
        let plan = DefaultEngine.plan(&input(vec![vec![slow, fast]]));
        assert_eq!(plan.attempts, vec![1, 0]);
    }

    #[test]
    fn tier_order_beats_speed() {
        let mut slow_tier0 = snap(0, 0);
        slow_tier0.narinfo_ms = Some(500.0);
        let mut fast_tier1 = snap(1, 1);
        fast_tier1.narinfo_ms = Some(1.0);
        let plan = DefaultEngine.plan(&input(vec![vec![slow_tier0], vec![fast_tier1]]));
        assert_eq!(plan.attempts, vec![0, 1]);
    }

    #[test]
    fn affinity_goes_first_unless_down() {
        let mut inp = input(vec![vec![snap(0, 0), snap(1, 0)]]);
        inp.kind = RequestKind::Nar;
        inp.affinity = Some(1);
        assert_eq!(DefaultEngine.plan(&inp).attempts, vec![1, 0]);

        inp.tiers[0].upstreams[1].healthy = Some(false);
        assert_eq!(DefaultEngine.plan(&inp).attempts, vec![0]);
    }

    #[test]
    fn boundary_types_round_trip_through_json() {
        let inp = input(vec![vec![snap(0, 0)]]);
        let json = serde_json::to_string(&inp).unwrap();
        let back: SelectionInput = serde_json::from_str(&json).unwrap();
        let plan = DefaultEngine.plan(&back);
        let _: Plan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(plan.attempts, vec![0]);
    }
}
