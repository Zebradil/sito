//! The selection boundary (ADR-0005): serializable input in, ordered fetch
//! plan out. The built-in rules are plain Rust; a future scripting engine
//! replaces the `SelectionEngine` impl without touching the proxy core.

use serde::{Deserialize, Serialize};

use crate::config::Strategy;
use crate::state::UpstreamSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestKind {
    Narinfo,
    Nar,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SelectionInput {
    pub kind: RequestKind,
    pub path: String,
    /// Upstream that served the narinfo naming this NAR, when known.
    pub affinity: Option<usize>,
    pub tiers: Vec<TierInput>,
}

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

fn is_down(input: &SelectionInput, idx: usize) -> bool {
    input
        .tiers
        .iter()
        .flat_map(|t| &t.upstreams)
        .any(|u| u.index == idx && u.healthy == Some(false))
}

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
