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
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport, time,
};

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
/// reason to move on, while `nar_agent` has no overall deadline — a
/// multi-gigabyte NAR may legitimately take minutes, so a global deadline
/// would abort healthy transfers. It is bounded per phase instead (see
/// [`nar_agent`]).
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
    let nar_agent = nar_agent(NAR_IDLE_TIMEOUT);

    let app = Arc::new(App {
        registry,
        engine: Box::new(DefaultEngine),
        prober,
        tiers,
        info_agent,
        nar_agent,
        nar_slots: proxy::Slots::new(cfg.max_inflight),
    });
    let server =
        Server::http(&cfg.listen).map_err(|e| anyhow::anyhow!("bind {}: {e}", cfg.listen))?;
    Ok((app, server))
}

/// Longest a NAR fetch may wait for its next bytes from the upstream, headers
/// included. Kept well under Nix's `stalled-download-timeout` (300 s by
/// default) so sito notices a dead flow, frees its thread and demotes the
/// upstream before Nix gives up and retries on top of it.
const NAR_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The NAR agent: DNS and connect bounded at 5 s each, then every wait for
/// upstream bytes capped at `idle` through [`IdleConnector`].
///
/// ureq 3 has no idle timeout of its own — `timeout_recv_body` is a deadline
/// for the whole body — and without one an upstream flow that goes silent
/// without a FIN or RST (a VPN drop, say) blocks its reader forever, since
/// sito never writes to the upstream and so never learns the peer is gone.
/// The resolve bound matters for the same reason: with no resolve timeout set,
/// ureq calls `getaddrinfo` synchronously, which a broken VPN DNS can hang.
fn nar_agent(idle: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_resolve(Some(Duration::from_secs(5)))
        .timeout_connect(Some(Duration::from_secs(5)))
        .build();
    ureq::Agent::with_parts(
        config,
        IdleConnector {
            inner: DefaultConnector::default(),
            idle,
        },
        DefaultResolver::default(),
    )
}

/// Wraps ureq's default connector chain so every transport it produces is an
/// [`IdleTransport`]. Built on ureq's `unversioned` API, which may change in a
/// minor release; a ureq bump that breaks this fails to compile rather than
/// misbehaving.
#[derive(Debug)]
struct IdleConnector {
    inner: DefaultConnector,
    idle: Duration,
}

impl Connector for IdleConnector {
    type Out = IdleTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<IdleTransport>, ureq::Error> {
        Ok(self
            .inner
            .connect(details, chained)?
            .map(|inner| IdleTransport {
                inner,
                idle: self.idle,
            }))
    }
}

/// A transport whose reads give up after `idle` without bytes. ureq sets the
/// socket read timeout from the `NextTimeout` it passes in on every wait, so
/// capping that value is all it takes. An idle timeout surfaces as ureq's
/// timeout error, labelled with whichever configured timeout was due next
/// (usually `global`, as none is set) rather than as an idle timeout.
#[derive(Debug)]
struct IdleTransport {
    inner: Box<dyn Transport>,
    idle: Duration,
}

impl Transport for IdleTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let after = match timeout.after {
            time::Duration::Exact(d) if d < self.idle => timeout.after,
            _ => time::Duration::Exact(self.idle),
        };
        self.inner.await_input(NextTimeout { after, ..timeout })
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// Run the accept loop on the calling thread. Blocks forever in normal
/// operation; see [`proxy::serve`] for the failure mode that ends it.
pub fn serve(app: Arc<App>, server: Server) -> Result<()> {
    proxy::serve(app, server)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// An upstream that accepts, writes `reply` and then holds the
    /// connection open without another byte, like a flow lost to a VPN drop.
    fn silent_upstream(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                conn.write_all(reply).unwrap();
                std::thread::spawn(move || {
                    let _ = conn.read(&mut [0; 4096]);
                    std::thread::sleep(Duration::from_secs(30));
                });
            }
        });
        format!("http://{addr}/nar/x.nar")
    }

    #[test]
    fn idle_timeout_bounds_headers_and_body() {
        let agent = super::nar_agent(Duration::from_millis(300));

        let start = Instant::now();
        assert!(agent.get(&silent_upstream(b"")).call().is_err());
        assert!(start.elapsed() < Duration::from_secs(5));

        let url = silent_upstream(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
        let mut resp = agent.get(&url).call().unwrap();
        let start = Instant::now();
        let mut body = Vec::new();
        assert!(resp.body_mut().as_reader().read_to_end(&mut body).is_err());
        assert_eq!(body, b"abc");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
