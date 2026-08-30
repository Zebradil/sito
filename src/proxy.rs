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

pub struct App {
    pub registry: Arc<Registry>,
    pub engine: Box<dyn SelectionEngine>,
    pub prober: Prober,
    pub tiers: Vec<TierShape>,
    /// narinfo and probe-sized requests: bounded end to end.
    pub info_agent: ureq::Agent,
    /// NAR downloads: bounded connect, unbounded body.
    pub nar_agent: ureq::Agent,
}

/// Static tier shape from config: which upstream indices belong to which
/// tier, in order.
pub struct TierShape {
    pub strategy: crate::config::Strategy,
    pub indices: Vec<usize>,
}

const MAX_ACCEPT_FAILURES: u32 = 100;

/// Serve until the listener dies. Thread per request with a hard in-flight
/// cap, kasha-style: a slot is held for the whole transfer.
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

fn forward(app: &App, req: Request, kind: RequestKind) {
    let path = req.url().to_string();
    let head = *req.method() == Method::Head;
    let input = selection_input(app, kind, &path);
    let Plan { attempts } = app.engine.plan(&input);
    tracing::debug!(path, ?attempts, "plan");

    for idx in attempts {
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
        match call {
            Ok(upstream_resp) => {
                match kind {
                    RequestKind::Narinfo => {
                        app.registry
                            .record_narinfo_hit(idx, start.elapsed().as_secs_f64() * 1000.0);
                    }
                    RequestKind::Nar => app.registry.record_hit(idx),
                }
                tracing::debug!(url, upstream = base, "hit");
                return relay(app, req, upstream_resp, idx, kind, head, start);
            }
            Err(ureq::Error::StatusCode(404)) => {
                app.registry.record_miss(idx);
                continue;
            }
            Err(e) => {
                tracing::warn!(url, error = %e, "upstream failed");
                app.registry.record_error(idx);
                app.prober.kick();
                continue;
            }
        }
    }
    respond(req, text(404, "no upstream has this path"));
}

/// Stream an upstream response through unmodified. Narinfo bodies are tiny
/// and get buffered so the `URL:` field can seed NAR affinity — the bytes
/// sent are still exactly the bytes received (pass-through trust, ADR-0006).
fn relay(
    app: &App,
    req: Request,
    mut upstream: ureq::http::Response<ureq::Body>,
    idx: usize,
    kind: RequestKind,
    head: bool,
    start: Instant,
) {
    let mut headers = Vec::new();
    for name in ["Content-Type", "Content-Length"] {
        if let Some(v) = upstream.headers().get(name)
            && let Ok(v) = v.to_str()
        {
            headers.push(header(name, v));
        }
    }
    let len: Option<usize> = upstream
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    if head {
        let mut resp = Response::empty(200);
        for h in headers {
            resp.add_header(h);
        }
        return respond(req, resp);
    }

    match kind {
        RequestKind::Narinfo => {
            let mut body = Vec::new();
            if let Err(e) = upstream
                .body_mut()
                .as_reader()
                .take(1 << 20)
                .read_to_end(&mut body)
            {
                tracing::warn!(error = %e, "narinfo body read failed");
                return respond(req, text(502, "upstream read failed"));
            }
            if let Some(nar_path) = narinfo_url_field(&body) {
                app.registry.set_affinity(nar_path, idx);
            }
            let mut resp = Response::from_data(body);
            for h in headers {
                resp.add_header(h);
            }
            respond(req, resp);
        }
        RequestKind::Nar => {
            let reader = MeteredReader {
                inner: upstream.into_body().into_reader(),
                registry: app.registry.clone(),
                idx,
                start,
                bytes: 0,
                eof: false,
            };
            let mut resp = Response::new(200.into(), headers, reader, len, None);
            if len.is_none() {
                // Unknown length: tiny_http falls back to chunked encoding.
                resp = resp.with_chunked_threshold(1);
            }
            respond(req, resp);
        }
    }
}

/// Extract the `URL:` field (the NAR path a client will fetch next) from a
/// narinfo body.
fn narinfo_url_field(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("URL:"))
        .map(|v| v.trim().to_string())
}

/// Counts NAR bytes on the way through and records throughput once the body
/// completes; aborted transfers record nothing.
struct MeteredReader {
    inner: ureq::BodyReader<'static>,
    registry: Arc<Registry>,
    idx: usize,
    start: Instant,
    bytes: u64,
    eof: bool,
}

impl Read for MeteredReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        if n == 0 {
            self.eof = true;
        }
        Ok(n)
    }
}

impl Drop for MeteredReader {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        if self.eof && secs > 0.0 && self.bytes > 0 {
            self.registry
                .record_nar_throughput(self.idx, self.bytes as f64 / 1e6 / secs);
        }
    }
}

struct Slots {
    used: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
    max: usize,
}

impl Slots {
    fn new(max: usize) -> Self {
        Slots {
            used: std::sync::Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max: max.max(1),
        }
    }

    fn acquire(self: &Arc<Self>) -> Slot {
        let mut used = self
            .freed
            .wait_while(self.used.lock().unwrap(), |used| *used >= self.max)
            .unwrap();
        *used += 1;
        Slot(Arc::clone(self))
    }
}

struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

fn respond<R: Read>(req: Request, resp: Response<R>) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let code = resp.status_code().0;
    if let Err(e) = req.respond(resp) {
        tracing::debug!(error = %e, "client went away");
    }
    tracing::debug!(%method, url, code, "request");
}

fn text(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code)
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}
