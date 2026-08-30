//! TOML config: listen address plus ordered tiers of upstreams.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout_secs: u64,
    #[serde(default = "default_max_inflight")]
    pub max_inflight: usize,
    #[serde(default, rename = "tier")]
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Tier {
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<Upstream>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    #[default]
    Sequential,
    Race,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Upstream {
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
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("parse {}", path.display()))
    }

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
