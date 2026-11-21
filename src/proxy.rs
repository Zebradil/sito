//! HTTP surface: streaming proxy (ADR-0002), read-only, plus sito's own
//! `/nix-cache-info`, `/status` and `/metrics`. Narinfos are rewritten and
//! NARs served decompressed (ADR-0009); see [`crate::narinfo`].

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::narinfo::{self, Codec, NarTarget};
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
    /// [`narinfo::upstream_key`] of each upstream, by flat upstream index.
    pub upstream_keys: Vec<String>,
    /// narinfo and probe-sized requests: bounded end to end.
    pub info_agent: ureq::Agent,
    /// NAR downloads: bounded DNS, connect and idle time, no overall
    /// deadline.
    pub nar_agent: ureq::Agent,
    /// Caps concurrent NAR transfers (`max-inflight`).
    pub(crate) nar_slots: Slots,
    /// Per-upstream NAR caps (an upstream's own `max-inflight`), by flat
    /// upstream index; `None` where the upstream sets none.
    pub(crate) upstream_slots: Vec<Option<Slots>>,
    /// Rewritten-URL NAR requests their own upstream did not serve
    /// (ADR-0009), served by another upstream with the same `NarHash`.
    pub(crate) nar_fallbacks_served: AtomicU64,
    /// The same, answered 404 because no upstream could serve them.
    pub(crate) nar_fallbacks_failed: AtomicU64,
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

/// `/status`: uptime, NAR slot use, and every [`UpstreamSnapshot`] as
/// JSON. Diagnostics for a human — the shape is not a stable API.
///
/// [`UpstreamSnapshot`]: crate::state::UpstreamSnapshot
fn status(app: &App, req: Request) {
    let body = serde_json::json!({
        "uptime_secs": app.registry.started.elapsed().as_secs(),
        "nar_slots": {"used": app.nar_slots.used(), "max": app.nar_slots.max},
        // By flat upstream index, like `upstreams`; null where none is set.
        "upstream_nar_slots": app.upstream_slots.iter().map(|s| s.as_ref().map(|s| {
            serde_json::json!({"used": s.used(), "max": s.max})
        })).collect::<Vec<_>>(),
        "nar_fallbacks": {
            "served": app.nar_fallbacks_served.load(Ordering::Relaxed),
            "failed": app.nar_fallbacks_failed.load(Ordering::Relaxed),
        },
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
    family(
        &mut out,
        "sito_nar_fallbacks_total",
        "counter",
        "Rewritten NAR requests their own upstream did not serve, by result: served by another upstream with the same NarHash, or failed (404).",
        [
            ("served", &app.nar_fallbacks_served),
            ("failed", &app.nar_fallbacks_failed),
        ]
        .map(|(result, n)| {
            (
                format!("{{result=\"{result}\"}}"),
                n.load(Ordering::Relaxed) as f64,
            )
        }),
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
        "NAR body bytes received from upstreams, before decompression.",
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
/// Affinity exists for rewritten NAR URLs only: it is the upstream whose
/// key the URL carries, the one that served the narinfo (ADR-0009). A key no
/// configured upstream has, because its URL changed since, leaves the NAR
/// without one, and every upstream is then asked for a substitute.
fn selection_input(
    app: &App,
    kind: RequestKind,
    path: &str,
    target: Option<&NarTarget>,
) -> SelectionInput {
    let snaps = app.registry.snapshot();
    let affinity = target.and_then(|t| app.upstream_keys.iter().position(|k| *k == t.upstream_key));
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
/// Except when the NAR's affinity upstream *failed* rather than missed and no
/// other upstream holds the same NAR: sito first retries the affinity
/// upstream a few times to ride out a short flap. It answers 404 only after
/// that, rather than a 5xx that Nix would retry: once Nix's own retries ran
/// out, a 5xx on a NAR fails the whole build unless `--fallback` is set,
/// while a 404 lets Nix build that path locally.
///
/// An attempt at an upstream that is marked down and whose own NAR cap is
/// full counts as failed without connecting (see [`upstream_slot`]), so a
/// flapping upstream costs each request its own tries and the retry window,
/// never a place in a queue of other requests' connect timeouts.
fn forward(app: &App, req: Request, kind: RequestKind) {
    let path = req.url().to_string();
    let head = *req.method() == Method::Head;
    let target = match kind {
        RequestKind::Nar => narinfo::parse_target(&path),
        RequestKind::Narinfo => None,
    };
    let input = selection_input(app, kind, &path, target.as_ref());
    let Plan { attempts } = app.engine.plan(&input);
    tracing::debug!(path, ?attempts, "plan");
    let _slot = (kind == RequestKind::Nar).then(|| app.nar_slots.acquire());

    let try_one = |idx| match upstream_slot(app, idx, kind) {
        Some(_slot) => attempt(app, idx, &path, kind, head, target.as_ref()),
        None => Attempt::Failed,
    };
    let mut affinity_failed = None;
    for idx in attempts {
        match try_one(idx) {
            Attempt::Hit(resp) => {
                if target
                    .as_ref()
                    .is_some_and(|t| app.upstream_keys[idx] != t.upstream_key)
                {
                    app.nar_fallbacks_served.fetch_add(1, Ordering::Relaxed);
                }
                return respond(req, resp);
            }
            Attempt::Failed if input.affinity == Some(idx) => affinity_failed = Some(idx),
            Attempt::Miss | Attempt::Failed => {}
        }
    }
    if let Some(idx) = affinity_failed {
        for _ in 0..AFFINITY_RETRIES {
            std::thread::sleep(AFFINITY_RETRY_DELAY);
            match try_one(idx) {
                Attempt::Hit(resp) => return respond(req, resp),
                Attempt::Miss => break,
                Attempt::Failed => {}
            }
        }
    }
    if target.is_some() {
        app.nar_fallbacks_failed.fetch_add(1, Ordering::Relaxed);
    }
    respond(req, text(404, "no upstream has this path"));
}

/// Take a slot under upstream `idx`'s own NAR cap, if it sets one. The
/// caller keeps it until the response is sent, so it counts the transfer, not
/// just the request; on a miss or failure it is released before the next
/// upstream is tried.
///
/// `Some(None)` means no cap applies. `None` means the upstream is marked down
/// and its cap is full, on arrival or when a slot frees up: the caller skips
/// the attempt. Waiting would only queue the request behind connect timeouts
/// to a host that is likely unreachable, with Nix seeing no bytes meanwhile;
/// with a free slot the attempt still goes ahead, since a probe verdict can be
/// stale during a flap.
///
/// shortcut: a request waiting on a healthy upstream sends Nix nothing, so a
/// wait past Nix's `stalled-download-timeout` (300 s) aborts it and Nix
/// retries; revisit if a low cap with a large closure makes that common.
fn upstream_slot(app: &App, idx: usize, kind: RequestKind) -> Option<Option<Slot<'_>>> {
    if kind != RequestKind::Nar {
        return Some(None);
    }
    match &app.upstream_slots[idx] {
        None => Some(None),
        Some(slots) => {
            let slot = slots.acquire_unless(|| app.registry.is_down(idx));
            if slot.is_none() {
                tracing::info!(
                    url = slots.scope,
                    "upstream down and its NAR slots busy, skipping"
                );
            }
            slot.map(Some)
        }
    }
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
    /// upstream nothing but the counter. Also a substitute narinfo whose
    /// `NarHash` differs, or whose NAR sito cannot decode.
    Miss,
    /// A transport error, a non-404 error status, or a body that failed
    /// before its first byte. Counts an error, marks the upstream down and
    /// kicks the prober so recovery does not wait for the interval.
    Failed,
}

/// Ask upstream `idx` for the request at `path` and turn the answer into a
/// response for the client, recording what happened in the registry.
///
/// A NAR request comes in one of two shapes:
///
/// - A **rewritten URL** (ADR-0009). At the upstream it names, sito fetches
///   the original path. At any other upstream, it first fetches the narinfo
///   for the same store path, and goes on only if its `NarHash` matches: then
///   that upstream's NAR is the same NAR, at its own path and in its own
///   compression. Either way the client gets it decompressed, as the
///   rewritten narinfo's `Compression: none` promised.
/// - **Any other path**, such as one from a narinfo Nix cached before sito
///   rewrote them, or one whose compression sito cannot decode. It is
///   fetched as is from each upstream in turn and relayed verbatim; caches
///   that share the `nar/<filehash>` layout will have it or 404 cheaply.
fn attempt(
    app: &App,
    idx: usize,
    path: &str,
    kind: RequestKind,
    head: bool,
    target: Option<&NarTarget>,
) -> Attempt {
    match (kind, target) {
        (RequestKind::Narinfo, _) => attempt_narinfo(app, idx, path, head),
        (RequestKind::Nar, None) => attempt_nar(app, idx, path, Codec::None, head),
        (RequestKind::Nar, Some(t)) if app.upstream_keys[idx] == t.upstream_key => {
            attempt_nar(app, idx, &t.path, t.codec, head)
        }
        (RequestKind::Nar, Some(t)) => match substitute(app, idx, t) {
            Ok((path, codec)) => attempt_nar(app, idx, &path, codec, head),
            Err(a) => a,
        },
    }
}

/// Fetch a narinfo and rewrite it (see [`narinfo::rewrite`]); one that cannot
/// be rewritten is relayed unmodified.
///
/// The body is buffered whole, with a 1 MiB cap. The length sito advertises
/// is the rewritten body's own; a narinfo past the cap is served short and
/// rejected by the client as malformed. Real ones are well under a kilobyte.
/// A HEAD answer carries no `Content-Length`, since the rewritten length is
/// unknown without the body; Nix only reads the status of a narinfo HEAD.
fn attempt_narinfo(app: &App, idx: usize, path: &str, head: bool) -> Attempt {
    let (upstream, headers_ms) = match call(app, idx, path, RequestKind::Narinfo, head) {
        Ok(r) => r,
        Err(a) => return a,
    };
    let (status, headers) = relay_headers(app, idx, &upstream);
    if head {
        app.registry.record_narinfo_hit(idx, headers_ms);
        return Attempt::Hit(Response::new(
            status,
            headers,
            Box::new(std::io::empty()),
            None,
            None,
        ));
    }
    let body = match read_narinfo(app, idx, path, upstream) {
        Some(b) => b,
        None => return Attempt::Failed,
    };
    let body = narinfo::store_hash(path)
        .and_then(|h| narinfo::rewrite(&body, h, &app.upstream_keys[idx]))
        .unwrap_or(body);
    app.registry.record_narinfo_hit(idx, headers_ms);
    tracing::debug!(path, upstream = app.registry.url(idx), "hit");
    let n = body.len();
    Attempt::Hit(Response::new(
        status,
        headers,
        Box::new(std::io::Cursor::new(body)),
        Some(n),
        None,
    ))
}

/// Find upstream `idx`'s copy of the NAR `target` names: its narinfo for the
/// same store path must carry the same `NarHash`. Returns that NAR's path and
/// compression, or the [`Attempt`] to give up with.
fn substitute(app: &App, idx: usize, target: &NarTarget) -> Result<(String, Codec), Attempt> {
    let path = format!("/{}.narinfo", target.store_hash);
    let (upstream, headers_ms) = call(app, idx, &path, RequestKind::Narinfo, false)?;
    let body = read_narinfo(app, idx, &path, upstream).ok_or(Attempt::Failed)?;
    app.registry.record_narinfo_hit(idx, headers_ms);
    let info = std::str::from_utf8(&body).ok().and_then(narinfo::parse);
    match info {
        Some(i) if i.nar_hash == target.nar_hash && !i.url.contains("://") => {
            match Codec::parse(i.compression) {
                Some(codec) => Ok((format!("/{}", i.url), codec)),
                None => Err(Attempt::Miss),
            }
        }
        _ => {
            tracing::debug!(
                path,
                upstream = app.registry.url(idx),
                "no substitute: NarHash differs or narinfo unusable"
            );
            Err(Attempt::Miss)
        }
    }
}

/// Fetch a NAR and stream it to the client, decompressed from `codec`.
///
/// Nothing reaches the client until the first decompressed bytes are in,
/// which is what lets a body that fails early — an upstream that sends
/// headers and then stalls until the idle timeout — fall through to the next
/// upstream instead of becoming a broken 200. Once NAR bytes have been sent
/// there is no switching upstreams; see [`MeteredReader`] for how a later
/// failure is surfaced.
///
/// The body goes out always chunked: no `Content-Length` is sent, even when
/// the upstream gave one. tiny_http cannot close a client connection
/// mid-response, so with a fixed length a failed upstream would leave Nix
/// waiting for the missing bytes until its own `stalled-download-timeout`. A
/// chunked body instead ends at once, and Nix rejects the short NAR straight
/// away. A HEAD answer carries the upstream's `Content-Length` only when
/// nothing is decompressed.
fn attempt_nar(app: &App, idx: usize, path: &str, codec: Codec, head: bool) -> Attempt {
    let (upstream, _) = match call(app, idx, path, RequestKind::Nar, head) {
        Ok(r) => r,
        Err(a) => return a,
    };
    let (status, headers) = relay_headers(app, idx, &upstream);
    let url = format!("{}{}", app.registry.url(idx).trim_end_matches('/'), path);
    if head {
        let len = upstream
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .filter(|_| codec == Codec::None);
        app.registry.record_hit(idx);
        return Attempt::Hit(Response::new(
            status,
            headers,
            Box::new(std::io::empty()),
            len,
            None,
        ));
    }
    let reader = MeteredReader {
        inner: upstream.into_body().into_reader(),
        registry: app.registry.clone(),
        prober: app.prober.clone(),
        idx,
        url: url.clone(),
        start: Instant::now(),
        bytes: 0,
        eof: false,
        failed: false,
    };
    let mut body = match Decode::new(codec, reader) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(url, error = %e, "NAR decoder setup failed");
            return Attempt::Failed;
        }
    };
    let mut first = vec![0; 64 * 1024];
    let n = body.read(&mut first).unwrap_or(0);
    if n == 0 {
        // An upstream failure is already recorded and logged by the reader;
        // a NAR is never empty, so an empty body is no answer either.
        return Attempt::Failed;
    }
    first.truncate(n);
    app.registry.record_hit(idx);
    tracing::debug!(url, "hit");
    Attempt::Hit(Response::new(
        status,
        headers,
        Box::new(std::io::Cursor::new(first).chain(body)),
        None,
        None,
    ))
}

/// GET or HEAD `path` on upstream `idx`, with the agent for `kind`. A 404 or
/// a failure is recorded and comes back as the [`Attempt`] to give up with;
/// a success comes with its time to response headers, in milliseconds.
fn call(
    app: &App,
    idx: usize,
    path: &str,
    kind: RequestKind,
    head: bool,
) -> Result<(ureq::http::Response<ureq::Body>, f64), Attempt> {
    let url = format!("{}{}", app.registry.url(idx).trim_end_matches('/'), path);
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
    match call {
        Ok(r) => Ok((r, start.elapsed().as_secs_f64() * 1000.0)),
        Err(ureq::Error::StatusCode(404)) => {
            app.registry.record_miss(idx);
            Err(Attempt::Miss)
        }
        Err(e) => {
            tracing::warn!(url, error = %e, "upstream failed");
            app.registry.record_error(idx);
            app.prober.kick();
            Err(Attempt::Failed)
        }
    }
}

/// The status and headers relayed to the client. The upstream's status code
/// is mirrored, and only `Content-Type` is carried over; upstream caching,
/// CORS and vendor headers are dropped, since the client is a Nix daemon on
/// localhost that reads neither. sito adds `X-Sito-Upstream` with the
/// configured URL of the upstream that answered, so `curl -I` shows where a
/// path came from without debug logs. No client request headers travel the
/// other way, so an upstream never sees a `Range` or `If-None-Match` it could
/// answer 206 or 304 to.
fn relay_headers(
    app: &App,
    idx: usize,
    upstream: &ureq::http::Response<ureq::Body>,
) -> (tiny_http::StatusCode, Vec<Header>) {
    let status = tiny_http::StatusCode(upstream.status().as_u16());
    let mut headers: Vec<Header> = upstream
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|v| header("Content-Type", v))
        .into_iter()
        .collect();
    // Config URLs are not validated as header values; skip the header rather than panic.
    headers
        .extend(Header::from_bytes(&b"X-Sito-Upstream"[..], app.registry.url(idx).as_bytes()).ok());
    (status, headers)
}

/// Read a narinfo body whole, up to 1 MiB; `None` after recording a failed
/// read against the upstream.
fn read_narinfo(
    app: &App,
    idx: usize,
    path: &str,
    upstream: ureq::http::Response<ureq::Body>,
) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    match upstream
        .into_body()
        .into_reader()
        .take(1 << 20)
        .read_to_end(&mut body)
    {
        Ok(_) => Some(body),
        Err(e) => {
            tracing::warn!(path, upstream = app.registry.url(idx), error = %e, "narinfo body read failed");
            app.registry.record_error(idx);
            app.prober.kick();
            None
        }
    }
}

/// A NAR body on its way to the client, decompressed from the upstream's
/// compression.
///
/// Like [`MeteredReader`], it never returns an error: a corrupt stream is
/// logged and ends the body, so the chunked response terminates at once and
/// Nix rejects the short NAR. Once the decoder reports the end, the rest of
/// the upstream body is drained, so the reader sees its EOF and the transfer
/// counts as complete; decoders stop at the end of their stream without
/// reading on to the end of the body.
struct Decode {
    inner: Decoder,
    done: bool,
}

enum Decoder {
    None(MeteredReader),
    Xz(liblzma::read::XzDecoder<MeteredReader>),
    Zstd(zstd::stream::read::Decoder<'static, std::io::BufReader<MeteredReader>>),
}

impl Decode {
    fn new(codec: Codec, reader: MeteredReader) -> std::io::Result<Self> {
        let inner = match codec {
            Codec::None => Decoder::None(reader),
            // Concatenated streams, as Nix itself decodes them.
            Codec::Xz => Decoder::Xz(liblzma::read::XzDecoder::new_multi_decoder(reader)),
            Codec::Zstd => Decoder::Zstd(zstd::stream::read::Decoder::new(reader)?),
        };
        Ok(Decode { inner, done: false })
    }

    fn metered(&mut self) -> &mut MeteredReader {
        match &mut self.inner {
            Decoder::None(r) => r,
            Decoder::Xz(d) => d.get_mut(),
            Decoder::Zstd(d) => d.get_mut().get_mut(),
        }
    }
}

impl Read for Decode {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        let read = match &mut self.inner {
            Decoder::None(r) => r.read(buf),
            Decoder::Xz(d) => d.read(buf),
            Decoder::Zstd(d) => d.read(buf),
        };
        match read {
            Ok(0) => {
                self.done = true;
                let _ = std::io::copy(self.metered(), &mut std::io::sink());
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(e) => {
                self.done = true;
                let reader = self.metered();
                // A truncated upstream body also surfaces here, as an early
                // end of stream; the reader has already logged that one.
                if !reader.failed {
                    reader.failed = true;
                    tracing::warn!(url = reader.url, error = %e, "NAR decode failed");
                }
                Ok(0)
            }
        }
    }
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
    /// What the cap covers, for the busy warning: `global` or an upstream URL.
    scope: String,
}

/// How long a NAR may wait for a slot before sito logs that the cap is full.
const SLOT_WAIT_WARN: std::time::Duration = std::time::Duration::from_secs(5);

impl Slots {
    /// `max` is clamped to at least 1, since a zero cap would block every NAR
    /// forever.
    pub(crate) fn new(max: usize, scope: impl Into<String>) -> Self {
        Slots {
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max: max.max(1),
            scope: scope.into(),
        }
    }

    /// Take a slot, blocking until one frees up. Blocking rather than
    /// rejecting is the point: Nix sees a slow proxy instead of a failing one.
    fn acquire(&self) -> Slot<'_> {
        self.acquire_unless(|| false).expect("never gives up")
    }

    /// Take a slot, blocking until one frees up, unless `give_up` holds while
    /// none is free: then `None`, without waiting further. `give_up` is checked
    /// on arrival and each time a slot frees up. A wait past
    /// [`SLOT_WAIT_WARN`] is logged once, since a cap that stays full usually
    /// means transfers are stuck, not busy.
    fn acquire_unless(&self, give_up: impl Fn() -> bool) -> Option<Slot<'_>> {
        let mut wait = |used: &mut usize| *used >= self.max && !give_up();
        let (mut used, res) = self
            .freed
            .wait_timeout_while(self.used.lock().unwrap(), SLOT_WAIT_WARN, &mut wait)
            .unwrap();
        if res.timed_out() {
            tracing::warn!(
                max = self.max,
                scope = self.scope,
                "all NAR slots busy, waiting"
            );
            used = self.freed.wait_while(used, &mut wait).unwrap();
        }
        if *used >= self.max {
            return None;
        }
        *used += 1;
        Some(Slot(self))
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
        // All, not one: once the upstream is down, every waiter must wake to
        // give up, not stay queued behind whichever one takes this slot.
        self.0.freed.notify_all();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn down_upstream_with_busy_slot_does_not_wait() {
        let slots = Slots::new(1, "test");
        let _held = slots.acquire();
        let start = Instant::now();
        assert!(slots.acquire_unless(|| true).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn down_upstream_with_free_slot_is_still_tried() {
        let slots = Slots::new(1, "test");
        assert!(slots.acquire_unless(|| true).is_some());
    }

    #[test]
    fn healthy_upstream_waits_for_its_slot() {
        let slots = Slots::new(1, "test");
        std::thread::scope(|s| {
            let held = slots.acquire();
            let start = Instant::now();
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                drop(held);
            });
            assert!(slots.acquire_unless(|| false).is_some());
            assert!(start.elapsed() >= Duration::from_millis(100));
        });
    }

    #[test]
    fn queue_drains_once_upstream_goes_down() {
        // Three requests queued behind a connect attempt; the attempt fails,
        // marks the upstream down and frees the slot. The freed slot goes to
        // one waiter, the rest give up instead of queueing behind it.
        let slots = Slots::new(1, "test");
        let down = AtomicBool::new(false);
        let got = std::thread::scope(|s| {
            let held = slots.acquire();
            let waiters: Vec<_> = (0..3)
                .map(|_| s.spawn(|| slots.acquire_unless(|| down.load(Ordering::SeqCst))))
                .collect();
            std::thread::sleep(Duration::from_millis(100));
            down.store(true, Ordering::SeqCst);
            drop(held);
            let got: Vec<_> = waiters.into_iter().map(|w| w.join().unwrap()).collect();
            got.iter().filter(|g| g.is_some()).count()
        });
        assert_eq!(got, 1);
        assert_eq!(slots.used(), 0);
    }
}
