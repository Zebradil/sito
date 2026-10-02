//! HTTP surface: streaming pass-through proxy (ADR-0002), read-only, plus
//! sito's own `/nix-cache-info` and `/status`.

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::probe::Prober;
use crate::select::{Plan, RequestKind, SelectionEngine, SelectionInput, TierInput};
use crate::state::Registry;

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
    /// NAR downloads: bounded connect, unbounded body.
    pub nar_agent: ureq::Agent,
}

/// Static tier shape from config: which upstream indices belong to which
/// tier, in order. Fixed at startup — only the per-upstream state in
/// [`Registry`] changes at runtime.
pub struct TierShape {
    pub strategy: crate::config::Strategy,
    /// Flat upstream indices, in config order.
    pub indices: Vec<usize>,
}

/// Consecutive accept failures tolerated before [`serve`] gives up. A
/// transient failure (fd exhaustion under load) recovers well inside this;
/// a listener that is actually dead trips it in ten seconds.
const MAX_ACCEPT_FAILURES: u32 = 100;

/// Serve until the listener dies. Thread per request with a hard in-flight
/// cap, kasha-style: a slot is held for the whole transfer.
///
/// Blocks the calling thread and, in normal operation, never returns. The one
/// exit is `MAX_ACCEPT_FAILURES` accept failures in a row, which means the
/// listener is gone and no future request can arrive. Failure to *spawn* a
/// handler is not fatal: that request is shed and the loop carries on.
pub fn serve(app: Arc<App>, server: Server, max_inflight: usize) -> Result<()> {
    let slots = Arc::new(Slots::new(max_inflight));
    let mut failures = 0u32;
    loop {
        let req = match server.recv() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                failures += 1;
                if failures >= MAX_ACCEPT_FAILURES {
                    anyhow::bail!("listener failed {failures} times in a row: {e}");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        failures = 0;
        let slot = slots.acquire();
        let app = app.clone();
        let spawned = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let _slot = slot;
                handle(&app, req);
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "spawn failed, shedding request");
        }
    }
}

/// The whole route table.
///
/// sito answers two paths itself: `/nix-cache-info`, authored locally because
/// the upstreams' own priorities and store dirs are none of the client's
/// business (ADR-0006), and `/status`, a JSON dump of live state. `*.narinfo`
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
        "upstreams": app.registry.snapshot(),
    });
    let mut resp = Response::from_string(body.to_string());
    resp.add_header(header("Content-Type", "application/json"));
    respond(req, resp);
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
/// completes, and reports how a transfer ended when it did not end well.
///
/// A transfer ends one of three ways, told apart on drop:
///
/// - **EOF** — the body completed. Throughput feeds the EWMA, and a transfer
///   that was long and slow is logged as a warning.
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
            if secs >= SLOW_NAR_MIN_SECS && rate < SLOW_NAR_MBYTES_PER_SEC {
                tracing::warn!(
                    url = self.url,
                    bytes = self.bytes,
                    secs = secs as u64,
                    mbytes_per_sec = format!("{rate:.2}"),
                    "slow NAR transfer"
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

/// Counting semaphore over in-flight requests, the whole backpressure
/// mechanism. Nothing else bounds memory or upstream fan-out.
struct Slots {
    used: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
    max: usize,
}

impl Slots {
    /// `max` is clamped to at least 1, since a zero cap would deadlock the
    /// accept loop on the first request.
    fn new(max: usize) -> Self {
        Slots {
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max: max.max(1),
        }
    }

    /// Take a slot, blocking the caller — the accept loop — until one frees
    /// up. Blocking there rather than rejecting is the point: the kernel
    /// backlog queues the excess and Nix sees a slow proxy instead of a
    /// failing one.
    fn acquire(self: &Arc<Self>) -> Slot {
        let mut used = self
            .freed
            .wait_while(self.used.lock().unwrap(), |used| *used >= self.max)
            .unwrap();
        *used += 1;
        Slot(Arc::clone(self))
    }
}

/// A held slot, released on drop. It lives in the handler thread for the
/// whole request including the NAR body, so the cap counts bytes in flight,
/// not just requests being parsed.
struct Slot(Arc<Slots>);

impl Drop for Slot {
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
