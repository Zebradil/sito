//! HTTP surface: streaming pass-through proxy (ADR-0002), read-only, plus
//! sito's own `/nix-cache-info`, `/status` and `/metrics`.

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::probe::Prober;
use crate::select::{Plan, RequestKind, SelectionEngine, SelectionInput, TierInput};
use crate::state::{
    Histogram, NAR_BYTES_PER_SEC_BOUNDS, NARINFO_SECS_BOUNDS, Registry, UpstreamSnapshot,
};

/// Everything a request thread needs, shared immutably behind an `Arc`. The
/// only mutable state is inside [`Registry`], which does its own locking.
pub struct App {
    pub registry: Arc<Registry>,
    /// Consulted per request; swapping this out is the whole point of
    /// [`crate::select`]'s boundary.
    pub engine: Box<dyn SelectionEngine>,
    /// Kicked when an upstream fails a request, so health catches up without
    /// waiting for the next interval.
    pub prober: Prober,
    /// Tiers in config order, holding the flat upstream indices to look up in
    /// [`Registry`].
    pub tiers: Vec<TierShape>,
    /// narinfo and probe-sized requests: bounded end to end.
    pub info_agent: ureq::Agent,
    /// NAR downloads: bounded DNS, connect and idle time, no overall
    /// deadline.
    pub nar_agent: ureq::Agent,
    /// Caps concurrent NAR transfers (`max-inflight`).
    pub(crate) nar_slots: Slots,
}

/// Static tier shape from config: which upstream indices belong to which
/// tier, in order. Fixed at startup — only the per-upstream state in
/// [`Registry`] changes at runtime.
pub struct TierShape {
    pub strategy: crate::config::Strategy,
    /// Flat upstream indices, in config order.
    pub indices: Vec<usize>,
}

/// Serve until the listener dies, one thread per request.
///
/// The loop itself never waits on anything but `recv`: the concurrency cap
/// applies to NAR transfers only and is taken inside the handler (see
/// [`Slots`]), so stuck NARs can never stop narinfo lookups or `/status` from
/// being dispatched.
///
/// Blocks the calling thread and, in normal operation, never returns. The one
/// exit is an accept failure: tiny_http's accept thread hands over that error
/// and then stops, closing the listener, so `recv` would block forever on a
/// port nobody can reach. Returning lets launchd or systemd restart sito.
/// Failure to *spawn* a handler is not fatal: that request is shed and the
/// loop carries on.
pub fn serve(app: Arc<App>, server: Server) -> Result<()> {
    loop {
        let req = server.recv().context("listener failed")?;
        let app = app.clone();
        let spawned = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || handle(&app, req));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "spawn failed, shedding request");
        }
    }
}

/// The whole route table.
///
/// sito answers three paths itself: `/nix-cache-info`, authored locally
/// because the upstreams' own priorities and store dirs are none of the
/// client's business (ADR-0006), `/status`, a JSON dump of live state, and
/// `/metrics`, the same state for Prometheus-compatible scrapers. `*.narinfo`
/// and `/nar/*` are forwarded to upstreams. Everything else is 404.
///
/// Status codes sito emits on its own behalf: 404 for an unrecognised path,
/// 404 when every upstream in the plan missed or failed, and 405 for anything
/// but GET or HEAD (sito is read-only, ADR-0002).
fn handle(app: &App, req: Request) {
    let method = req.method().clone();
    let path = req.url().to_string();
    match (&method, path.as_str()) {
        (Method::Get | Method::Head, "/nix-cache-info") => respond(
            req,
            Response::from_string("StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 10\n"),
        ),
        (Method::Get | Method::Head, "/status") => status(app, req),
        (Method::Get | Method::Head, "/metrics") => metrics(app, req),
        (Method::Get | Method::Head, p) if p.ends_with(".narinfo") => {
            forward(app, req, RequestKind::Narinfo)
        }
        (Method::Get | Method::Head, p) if p.starts_with("/nar/") => {
            forward(app, req, RequestKind::Nar)
        }
        (Method::Get | Method::Head, _) => respond(req, text(404, "not found")),
        _ => respond(req, text(405, "sito is read-only")),
    }
}

/// `/status`: uptime, affinity map size, and every [`UpstreamSnapshot`] as
/// JSON. Diagnostics for a human — the shape is not a stable API.
///
/// [`UpstreamSnapshot`]: crate::state::UpstreamSnapshot
fn status(app: &App, req: Request) {
    let body = serde_json::json!({
        "uptime_secs": app.registry.started.elapsed().as_secs(),
        "affinity_entries": app.registry.affinity_len(),
        "nar_slots": {"used": app.nar_slots.used(), "max": app.nar_slots.max},
        "upstreams": app.registry.snapshot(),
    });
    let mut resp = Response::from_string(body.to_string());
    resp.add_header(header("Content-Type", "application/json"));
    respond(req, resp);
}

/// `/metrics`: the `/status` numbers in the Prometheus text exposition format,
/// for a scraper such as vmagent that keeps the history sito does not.
///
/// Unlike `/status`, metric names are meant to stay stable, since dashboards
/// and alerts are written against them. Upstreams are labelled by `url` and
/// `tier`. `healthy` is omitted until the first probe answers, and each EWMA
/// until its first sample, so "unknown" never reads as 0. Counters reset on
/// restart, which `rate()` handles.
fn metrics(app: &App, req: Request) {
    let ups = app.registry.snapshot();
    let mut out = String::new();
    let gauge = |out: &mut String, name, help, v: f64| {
        family(out, name, "gauge", help, [(String::new(), v)])
    };
    gauge(
        &mut out,
        "sito_uptime_seconds",
        "Seconds since sito started.",
        app.registry.started.elapsed().as_secs_f64(),
    );
    gauge(
        &mut out,
        "sito_affinity_entries",
        "NAR paths remembered with the upstream that served their narinfo.",
        app.registry.affinity_len() as f64,
    );
    gauge(
        &mut out,
        "sito_nar_slots_used",
        "NAR transfers in flight.",
        app.nar_slots.used() as f64,
    );
    gauge(
        &mut out,
        "sito_nar_slots_max",
        "The max-inflight cap on NAR transfers.",
        app.nar_slots.max as f64,
    );

    let labels = |u: &UpstreamSnapshot| format!("url=\"{}\",tier=\"{}\"", escape(&u.url), u.tier);
    let per = |f: fn(&UpstreamSnapshot) -> Option<f64>| {
        ups.iter()
            .filter_map(|u| f(u).map(|v| (format!("{{{}}}", labels(u)), v)))
            .collect::<Vec<_>>()
    };
    family(
        &mut out,
        "sito_upstream_healthy",
        "gauge",
        "1 after a successful probe, 0 after a failed probe or request.",
        per(|u| u.healthy.map(|h| h as u8 as f64)),
    );
    family(
        &mut out,
        "sito_upstream_requests_total",
        "counter",
        "Requests answered by an upstream, by result: hit, miss (404) or error.",
        ups.iter().flat_map(|u| {
            [("hit", u.hits), ("miss", u.misses), ("error", u.errors)]
                .map(|(result, n)| (format!("{{{},result=\"{result}\"}}", labels(u)), n as f64))
        }),
    );
    let histogram = |f: fn(&UpstreamSnapshot) -> &Histogram, bounds: &[f64]| {
        ups.iter()
            .flat_map(|u| {
                let (h, l) = (f(u), labels(u));
                let mut cumulative = 0;
                let les = bounds.iter().map(f64::to_string).chain(["+Inf".into()]);
                les.zip(h.buckets)
                    .map(|(le, n)| {
                        cumulative += n;
                        (format!("_bucket{{{l},le=\"{le}\"}}"), cumulative as f64)
                    })
                    .chain([
                        (format!("_sum{{{l}}}"), h.sum),
                        (format!("_count{{{l}}}"), h.count as f64),
                    ])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    family(
        &mut out,
        "sito_upstream_narinfo_seconds",
        "histogram",
        "Narinfo latency on real traffic, request to response headers.",
        histogram(|u| &u.narinfo_secs, &NARINFO_SECS_BOUNDS),
    );
    family(
        &mut out,
        "sito_upstream_nar_bytes_per_second",
        "histogram",
        "Throughput of completed NAR transfers.",
        histogram(|u| &u.nar_bytes_per_sec, &NAR_BYTES_PER_SEC_BOUNDS),
    );
    family(
        &mut out,
        "sito_upstream_nar_bytes_total",
        "counter",
        "NAR body bytes passed to clients.",
        per(|u| Some(u.nar_bytes as f64)),
    );
    family(
        &mut out,
        "sito_upstream_probe_ewma_seconds",
        "gauge",
        "Moving average of the probe round trip, as ranked on.",
        per(|u| u.probe_ms.map(|ms| ms / 1000.0)),
    );
    family(
        &mut out,
        "sito_upstream_narinfo_ewma_seconds",
        "gauge",
        "Moving average of narinfo latency, as ranked on.",
        per(|u| u.narinfo_ms.map(|ms| ms / 1000.0)),
    );
    family(
        &mut out,
        "sito_upstream_nar_ewma_bytes_per_second",
        "gauge",
        "Moving average of completed NAR transfer throughput.",
        per(|u| u.nar_mbytes_per_sec.map(|m| m * 1e6)),
    );

    let mut resp = Response::from_string(out);
    resp.add_header(header("Content-Type", "text/plain; version=0.0.4"));
    respond(req, resp);
}

/// Append one metric family: its HELP and TYPE lines, then one sample per
/// `(tail, value)`, where the tail follows the family name: an optional
/// suffix such as `_sum`, then the label set as `{...}`, either may be empty.
fn family(
    out: &mut String,
    name: &str,
    kind: &str,
    help: &str,
    samples: impl IntoIterator<Item = (String, f64)>,
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (tail, v) in samples {
        let _ = writeln!(out, "{name}{tail} {v}");
    }
}

/// Escape a label value for the text exposition format.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Assemble the engine's input from one registry snapshot, so every tier in
/// the plan is ranked against the same instant.
///
/// Affinity is looked up for NAR requests only — a narinfo request is what
/// *creates* affinity, and asking a specific upstream for one would defeat
/// the ranking. The lookup key is the path without its leading slash, because
/// that is the form the narinfo `URL:` field uses: those fields are relative
/// to their own cache, which is also why the NAR must come from the upstream
/// that served the narinfo (ADR-0003).
fn selection_input(app: &App, kind: RequestKind, path: &str) -> SelectionInput {
    let snaps = app.registry.snapshot();
    let affinity = match kind {
        RequestKind::Nar => app.registry.affinity(path.trim_start_matches('/')),
        RequestKind::Narinfo => None,
    };
    SelectionInput {
        kind,
        path: path.to_string(),
        affinity,
        tiers: app
            .tiers
            .iter()
            .map(|t| TierInput {
                strategy: t.strategy,
                upstreams: t.indices.iter().map(|&i| snaps[i].clone()).collect(),
            })
            .collect(),
    }
}

/// Walk the selection plan until an upstream answers, then send its response.
///
/// Each attempt ends one of three ways (see [`attempt`]): a hit, which is
/// sent and stops the walk — no second upstream is consulted; a miss, which
/// moves on; or a failure, which also moves on.
///
/// An exhausted plan — including an empty one, when every upstream is down —
/// is a 404. That is the correct answer for Nix: it moves to the next
/// substituter or builds locally (ADR-0002).
///
/// Except when the NAR's affinity upstream *failed* rather than missed: the
/// other upstreams' 404s say nothing about a path relative to another cache,
/// so sito first retries the affinity upstream a few times to ride out a
/// short flap. It answers 404 only after that, rather than a 5xx that Nix
/// would retry: once Nix's own retries ran out, a 5xx on a NAR fails the
/// whole build unless `--fallback` is set, while a 404 lets Nix build that
/// path locally.
fn forward(app: &App, req: Request, kind: RequestKind) {
    let path = req.url().to_string();
    let head = *req.method() == Method::Head;
    let input = selection_input(app, kind, &path);
    let Plan { attempts } = app.engine.plan(&input);
    tracing::debug!(path, ?attempts, "plan");
    let _slot = (kind == RequestKind::Nar).then(|| app.nar_slots.acquire());

    let mut affinity_failed = None;
    for idx in attempts {
        match attempt(app, idx, &path, kind, head) {
            Attempt::Hit(resp) => return respond(req, resp),
            Attempt::Failed if input.affinity == Some(idx) => affinity_failed = Some(idx),
            Attempt::Miss | Attempt::Failed => {}
        }
    }
    if let Some(idx) = affinity_failed {
        for _ in 0..AFFINITY_RETRIES {
            std::thread::sleep(AFFINITY_RETRY_DELAY);
            match attempt(app, idx, &path, kind, head) {
                Attempt::Hit(resp) => return respond(req, resp),
                Attempt::Miss => break,
                Attempt::Failed => {}
            }
        }
    }
    respond(req, text(404, "no upstream has this path"));
}

/// Extra tries at a failed affinity upstream, [`AFFINITY_RETRY_DELAY`] apart:
/// about 15 s of waiting, enough for the VPN-dependent flaps seen in practice.
const AFFINITY_RETRIES: u32 = 3;
const AFFINITY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// How one upstream answered one request.
enum Attempt {
    /// A response ready to send. For a GET the body is already known to be
    /// flowing: the narinfo is read whole, and the NAR's first chunk is in.
    Hit(Response<Box<dyn Read + Send>>),
    /// The upstream answered 404: it does not have this path. Costs the
    /// upstream nothing but the counter.
    Miss,
    /// A transport error, a non-404 error status, or a body that failed
    /// before its first byte. Counts an error, marks the upstream down and
    /// kicks the prober so recovery does not wait for the interval.
    Failed,
}

/// Fetch `path` from upstream `idx` and turn the answer into a response for
/// the client, recording what happened in the registry.
///
/// Nothing reaches the client until the body has started, which is what lets
/// a body that fails early — an upstream that sends headers and then stalls
/// until the idle timeout — fall through to the next upstream instead of
/// becoming a broken 200. Once NAR bytes have been sent there is no switching
/// upstreams; see [`MeteredReader`] for how a later failure is surfaced.
///
/// The upstream's status code is mirrored, and only `Content-Type` is carried
/// over; upstream caching, CORS and vendor headers are dropped, since the
/// client is a Nix daemon on localhost that reads neither. No client request
/// headers travel the other way, so an upstream never sees a `Range` or
/// `If-None-Match` it could answer 206 or 304 to.
///
/// - **HEAD** answers with those headers plus the upstream's
///   `Content-Length`, and no body.
/// - **Narinfo** bodies are buffered whole, with a 1 MiB cap, so the `URL:`
///   field can seed NAR affinity — the bytes sent are still exactly the bytes
///   received (pass-through trust, ADR-0006). The length sito advertises is
///   the buffer's own, since forwarding the upstream's would leave a client
///   waiting on bytes a truncated body never sends; a narinfo past the cap is
///   served short and rejected by the client as malformed. Real ones are well
///   under a kilobyte.
/// - **NAR** bodies stream through a [`MeteredReader`], always chunked: no
///   `Content-Length` is sent, even when the upstream gave one. tiny_http
///   cannot close a client connection mid-response, so with a fixed length a
///   failed upstream would leave Nix waiting for the missing bytes until its
///   own `stalled-download-timeout`. A chunked body instead ends at once, and
///   Nix rejects the short NAR straight away.
fn attempt(app: &App, idx: usize, path: &str, kind: RequestKind, head: bool) -> Attempt {
    let base = app.registry.url(idx);
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let agent = match kind {
        RequestKind::Narinfo => &app.info_agent,
        RequestKind::Nar => &app.nar_agent,
    };
    let start = Instant::now();
    let call = if head {
        agent.head(&url).call()
    } else {
        agent.get(&url).call()
    };
    let upstream = match call {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => {
            app.registry.record_miss(idx);
            return Attempt::Miss;
        }
        Err(e) => {
            tracing::warn!(url, error = %e, "upstream failed");
            app.registry.record_error(idx);
            app.prober.kick();
            return Attempt::Failed;
        }
    };
    let headers_ms = start.elapsed().as_secs_f64() * 1000.0;

    let status = tiny_http::StatusCode(upstream.status().as_u16());
    let headers: Vec<Header> = upstream
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|v| header("Content-Type", v))
        .into_iter()
        .collect();
    let len: Option<usize> = upstream
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let (body, len): (Box<dyn Read + Send>, _) = if head {
        (Box::new(std::io::empty()), len)
    } else {
        match kind {
            RequestKind::Narinfo => {
                let mut body = Vec::new();
                if let Err(e) = upstream
                    .into_body()
                    .into_reader()
                    .take(1 << 20)
                    .read_to_end(&mut body)
                {
                    tracing::warn!(url, error = %e, "narinfo body read failed");
                    app.registry.record_error(idx);
                    app.prober.kick();
                    return Attempt::Failed;
                }
                if let Some(nar_path) = narinfo_url_field(&body) {
                    app.registry.set_affinity(nar_path, idx);
                }
                let n = body.len();
                (Box::new(std::io::Cursor::new(body)), Some(n))
            }
            RequestKind::Nar => {
                let mut reader = MeteredReader {
                    inner: upstream.into_body().into_reader(),
                    registry: app.registry.clone(),
                    prober: app.prober.clone(),
                    idx,
                    url: url.clone(),
                    start,
                    bytes: 0,
                    eof: false,
                    failed: false,
                };
                let mut first = vec![0; 64 * 1024];
                let n = reader.read(&mut first).unwrap_or(0);
                if reader.failed {
                    // Already recorded and logged by the reader.
                    return Attempt::Failed;
                }
                first.truncate(n);
                (Box::new(std::io::Cursor::new(first).chain(reader)), None)
            }
        }
    };

    match kind {
        RequestKind::Narinfo => app.registry.record_narinfo_hit(idx, headers_ms),
        RequestKind::Nar => app.registry.record_hit(idx),
    }
    tracing::debug!(url, upstream = base, "hit");
    Attempt::Hit(Response::new(status, headers, body, len, None))
}

/// Extract the `URL:` field (the NAR path a client will fetch next) from a
/// narinfo body.
fn narinfo_url_field(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("URL:"))
        .map(|v| v.trim().to_string())
}

/// Below this throughput a completed NAR transfer that took at least
/// [`SLOW_NAR_MIN_SECS`] is logged as slow. Small NARs are dominated by
/// latency, hence the duration floor.
const SLOW_NAR_MBYTES_PER_SEC: f64 = 1.0;
const SLOW_NAR_MIN_SECS: f64 = 30.0;

/// Counts NAR bytes on the way through, records throughput once the body
/// completes, and logs one line per transfer saying how it ended.
///
/// A transfer ends one of three ways, told apart on drop:
///
/// - **EOF** — the body completed. Throughput feeds the EWMA and the
///   histogram, and the transfer is logged at info with its size, duration
///   and rate — at warn instead if it was long and slow. Narinfo lookups get
///   no such line: a build makes thousands of them, and their latency is in
///   the histogram.
/// - **Upstream read error** — including the idle timeout. Counted as an
///   upstream error and the upstream is marked down and the prober kicked,
///   exactly like a failed request: an upstream that stalls mid-body is as
///   broken as one that refuses connections. Logged at warn. The client gets
///   a short body; Nix cannot resume it (sito sends no `Accept-Ranges`) and
///   rejects the truncated NAR.
/// - **Neither** — the client hung up first (Nix cancels downloads it no
///   longer needs). Not the upstream's fault, so nothing is recorded; logged
///   at info.
///
/// Requiring `eof` before recording throughput is what keeps the number
/// honest: a client that disconnects mid-NAR leaves elapsed time counting
/// bytes that were never sent, which would read as a slow upstream and demote
/// a perfectly good one. Elapsed time is measured from the request start, so
/// connect and header latency are charged to throughput too — deliberate,
/// since that is what the transfer actually cost.
struct MeteredReader {
    inner: ureq::BodyReader<'static>,
    registry: Arc<Registry>,
    prober: Prober,
    idx: usize,
    url: String,
    start: Instant,
    bytes: u64,
    eof: bool,
    failed: bool,
}

impl Read for MeteredReader {
    /// An upstream error is recorded and then reported to the caller as end
    /// of body, never as an error. The NAR goes out chunked, and tiny_http
    /// flushes the terminating chunk only when the body ends without an
    /// error, so passing the error on would leave the client waiting for
    /// bytes until its own stall timeout.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.failed {
            return Ok(0);
        }
        match self.inner.read(buf) {
            Ok(0) => {
                self.eof = true;
                Ok(0)
            }
            Ok(n) => {
                self.bytes += n as u64;
                self.registry.record_nar_bytes(self.idx, n as u64);
                Ok(n)
            }
            Err(e) => {
                self.failed = true;
                tracing::warn!(
                    url = self.url,
                    bytes = self.bytes,
                    secs = self.start.elapsed().as_secs(),
                    error = %e,
                    "NAR upstream read failed"
                );
                self.registry.record_error(self.idx);
                self.prober.kick();
                Ok(0)
            }
        }
    }
}

impl Drop for MeteredReader {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        if self.eof && secs > 0.0 && self.bytes > 0 {
            let rate = self.bytes as f64 / 1e6 / secs;
            self.registry.record_nar_throughput(self.idx, rate);
            let slow = secs >= SLOW_NAR_MIN_SECS && rate < SLOW_NAR_MBYTES_PER_SEC;
            let round = |v: f64| (v * 100.0).round() / 100.0;
            let (secs, mbytes_per_sec) = (round(secs), round(rate));
            if slow {
                tracing::warn!(
                    url = self.url,
                    bytes = self.bytes,
                    secs,
                    mbytes_per_sec,
                    "slow NAR transfer"
                );
            } else {
                tracing::info!(
                    url = self.url,
                    bytes = self.bytes,
                    secs,
                    mbytes_per_sec,
                    "NAR transfer done"
                );
            }
        } else if !self.eof && !self.failed {
            tracing::info!(
                url = self.url,
                bytes = self.bytes,
                secs = secs as u64,
                "NAR transfer abandoned by client"
            );
        }
    }
}

/// Counting semaphore over concurrent NAR transfers, the backpressure that
/// bounds memory and upstream fan-out.
///
/// NARs only: narinfo and sito's own endpoints are cheap and time-bounded,
/// and putting them behind the same cap is what let stuck NAR transfers
/// starve lookups. The slot is taken in the handler thread, not the accept
/// loop, so a full cap delays only the NARs waiting for it; a waiting thread
/// holds nothing but its stack, and the Nix daemon's own connection cap
/// (`http-connections`, 25 by default) bounds how many there are.
pub(crate) struct Slots {
    used: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
    max: usize,
}

/// How long a NAR may wait for a slot before sito logs that the cap is full.
const SLOT_WAIT_WARN: std::time::Duration = std::time::Duration::from_secs(5);

impl Slots {
    /// `max` is clamped to at least 1, since a zero cap would block every NAR
    /// forever.
    pub(crate) fn new(max: usize) -> Self {
        Slots {
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max: max.max(1),
        }
    }

    /// Take a slot, blocking until one frees up. Blocking rather than
    /// rejecting is the point: Nix sees a slow proxy instead of a failing one.
    /// A wait past [`SLOT_WAIT_WARN`] is logged once, since a cap that stays
    /// full usually means transfers are stuck, not busy.
    fn acquire(&self) -> Slot<'_> {
        let full = |used: &mut usize| *used >= self.max;
        let (mut used, wait) = self
            .freed
            .wait_timeout_while(self.used.lock().unwrap(), SLOT_WAIT_WARN, full)
            .unwrap();
        if wait.timed_out() {
            tracing::warn!(max = self.max, "all NAR slots busy, waiting");
            used = self.freed.wait_while(used, full).unwrap();
        }
        *used += 1;
        Slot(self)
    }

    fn used(&self) -> usize {
        *self.used.lock().unwrap()
    }
}

/// A held slot, released on drop. It lives for the whole NAR request
/// including the body, so the cap counts transfers in flight, not just
/// requests being planned.
struct Slot<'a>(&'a Slots);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

/// Send a response and log it. Writing to the socket blocks until the body is
/// drained, so for a NAR this is where most of a request's wall time goes —
/// and where its slot is still held.
///
/// A write failure is a client that hung up, which is normal (Nix cancels
/// downloads it no longer needs) and is logged, not propagated.
fn respond<R: Read>(req: Request, resp: Response<R>) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let code = resp.status_code().0;
    if let Err(e) = req.respond(resp) {
        tracing::debug!(error = %e, "client went away");
    }
    tracing::debug!(%method, url, code, "request");
}

/// A plain-text response sito authors itself. The body is for humans reading
/// logs; Nix only looks at the status code.
fn text(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code)
}

/// # Panics
///
/// If `name` or `value` is not a valid header. Every call site passes either
/// a literal or a value already validated as a header by the upstream's HTTP
/// stack, so this cannot fire.
fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}
