//! TOML config: listen address plus ordered tiers of upstreams.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Every key is kebab-case in TOML (`probe-interval-secs`) and unknown keys
/// are a parse error, so a typo fails loudly at startup instead of silently
/// taking a default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// `host:port` to bind. Default `127.0.0.1:5001` — localhost only,
    /// because sito performs no authentication and grants its clients
    /// whatever the upstreams grant it.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Seconds between probe passes. Default 15; it bounds how long a stale
    /// health verdict can survive after the machine changes networks
    /// (ADR-0004).
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
    /// Per-probe deadline in seconds, connect through response. Default 3: an
    /// upstream that cannot answer `/nix-cache-info` inside it counts as down
    /// for this pass.
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout_secs: u64,
    /// Hard cap on concurrently served requests. Default 64. The cap is
    /// backpressure, not rejection — see [`crate::proxy::serve`].
    #[serde(default = "default_max_inflight")]
    pub max_inflight: usize,
    /// Tiers in the order they are tried. Written as repeated `[[tier]]`
    /// tables, each holding repeated `[[tier.upstream]]` tables. Defaults to
    /// empty, which [`Config::parse`] then rejects.
    #[serde(default, rename = "tier")]
    pub tiers: Vec<Tier>,
}

/// An ordered group of upstreams sharing one selection strategy. Tiers are
/// tried in order and the first tier that produces a hit wins, so the tier is
/// the unit of policy: "my own caches first, the public one only on miss".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Tier {
    #[serde(default)]
    pub strategy: Strategy,
    /// Upstreams in config order, which is also the tie-break order used
    /// before any quality signal exists.
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<Upstream>,
}

/// How a tier queries its upstreams for one request. `race` is parsed but
/// rejected by [`Config::parse`]: the schema carries it from day one so
/// adding it later is not a breaking config change (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Try upstreams one at a time in current rank order; the first hit wins,
    /// a 404 moves on to the next. The only strategy v1 implements.
    #[default]
    Sequential,
    /// Reserved: parallel fan-out, first positive answer wins. Unimplemented.
    Race,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Upstream {
    /// Base URL of the binary cache, `http://` or `https://`. A trailing
    /// slash is tolerated; request paths are appended to it verbatim.
    pub url: String,
    /// Signing keys the client must trust for this upstream. sito never
    /// verifies signatures itself (pass-through trust); the field exists so
    /// one config file can drive both the proxy and the nix module's
    /// `trusted-public-keys`.
    #[serde(default)]
    pub public_keys: Vec<String>,
}

fn default_listen() -> String {
    "127.0.0.1:5001".into()
}
fn default_probe_interval() -> u64 {
    15
}
fn default_probe_timeout() -> u64 {
    3
}
fn default_max_inflight() -> usize {
    64
}

impl Config {
    /// Read and parse a TOML config file. Errors if the file is unreadable or
    /// fails [`Config::parse`].
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Parse TOML and reject configs that would leave sito with nothing
    /// useful to do or with a promise it cannot keep: no upstreams anywhere,
    /// a tier asking for [`Strategy::Race`], or an upstream URL that is not
    /// `http://` or `https://`. Validation happens once at startup — config
    /// reload is restart-only (ADR-0005).
    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(raw)?;
        if cfg.tiers.iter().all(|t| t.upstreams.is_empty()) {
            bail!("no upstreams configured");
        }
        for (i, tier) in cfg.tiers.iter().enumerate() {
            if tier.strategy == Strategy::Race {
                bail!("tier {i}: strategy 'race' is not implemented yet");
            }
            for u in &tier.upstreams {
                if !u.url.starts_with("http://") && !u.url.starts_with("https://") {
                    bail!("tier {i}: upstream url must be http(s): {}", u.url);
                }
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
listen = "127.0.0.1:5001"

[[tier]]
strategy = "sequential"

  [[tier.upstream]]
  url = "http://box.lan:5000"
  public-keys = ["znix.zebradil.dev:AAAA"]

  [[tier.upstream]]
  url = "https://znix.zebradil.dev"

[[tier]]
  [[tier.upstream]]
  url = "https://cache.nixos.org"
  public-keys = ["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="]
"#;

    #[test]
    fn parses_sample_with_defaults() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:5001");
        assert_eq!(cfg.probe_interval_secs, 15);
        assert_eq!(cfg.tiers.len(), 2);
        assert_eq!(cfg.tiers[0].upstreams.len(), 2);
        assert_eq!(cfg.tiers[1].strategy, Strategy::Sequential);
        assert_eq!(cfg.tiers[0].upstreams[0].public_keys.len(), 1);
        assert!(cfg.tiers[0].upstreams[1].public_keys.is_empty());
    }

    #[test]
    fn race_parses_but_is_rejected() {
        let raw = r#"
[[tier]]
strategy = "race"
  [[tier.upstream]]
  url = "http://a:1"
"#;
        let err = Config::parse(raw).unwrap_err().to_string();
        assert!(err.contains("race"), "{err}");
    }

    #[test]
    fn empty_config_is_rejected() {
        assert!(Config::parse("").is_err());
    }

    #[test]
    fn non_http_url_is_rejected() {
        let raw = r#"
[[tier]]
  [[tier.upstream]]
  url = "s3://bucket"
"#;
        assert!(Config::parse(raw).is_err());
    }
}
