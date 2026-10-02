//! End-to-end: two mock upstreams behind sito, real HTTP both sides.

use std::io::Read;

use tiny_http::{Response, Server};

const NARINFO: &str = "StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1\n\
URL: nar/deadbeef.nar.xz\n\
Compression: xz\n";
const NAR_BYTES: &[u8] = b"not really a nar, but bytes are bytes";

/// Mock upstream: serves /nix-cache-info, plus the given (path, body) pairs.
fn mock_upstream(routes: Vec<(&'static str, &'static [u8])>) -> String {
    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let path = req.url().to_string();
            if path == "/nix-cache-info" {
                let _ = req.respond(Response::from_string("StoreDir: /nix/store\n"));
            } else if let Some((_, body)) = routes.iter().find(|(p, _)| *p == path) {
                let _ = req.respond(Response::from_data(body.to_vec()));
            } else {
                let _ = req.respond(Response::from_string("nope").with_status_code(404));
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// sito wired to the given upstreams, one tier each, listening on an
/// ephemeral port.
fn sito_at(upstreams: &[String]) -> String {
    sito_with(upstreams, 8)
}

fn sito_with(upstreams: &[String], max_inflight: usize) -> String {
    let tiers = upstreams
        .iter()
        .map(|u| format!("[[tier]]\n  [[tier.upstream]]\n  url = \"{u}\"\n"))
        .collect::<String>();
    let cfg = sito::config::Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nmax-inflight = {max_inflight}\n{tiers}"
    ))
    .unwrap();
    let (app, server) = sito::build(&cfg).unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || sito::serve(app, server));
    format!("http://127.0.0.1:{port}")
}

fn get(url: &str) -> Result<(u16, Vec<u8>), ureq::Error> {
    let mut resp = ureq::get(url).call()?;
    let mut body = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut body).unwrap();
    Ok((resp.status().as_u16(), body))
}

#[test]
fn proxies_narinfo_and_nar_verbatim_with_tier_fallback() {
    // First upstream misses everything; second has the goods — tier order
    // must fall through, and bytes must pass unmodified.
    let empty = mock_upstream(vec![]);
    let full = mock_upstream(vec![
        ("/abc.narinfo", NARINFO.as_bytes()),
        ("/nar/deadbeef.nar.xz", NAR_BYTES),
    ]);
    let base = sito_at(&[empty, full]);

    let (code, body) = get(&format!("{base}/abc.narinfo")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NARINFO.as_bytes());

    let (code, body) = get(&format!("{base}/nar/deadbeef.nar.xz")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NAR_BYTES);

    // Everything misses -> 404.
    match get(&format!("{base}/zzz.narinfo")) {
        Err(ureq::Error::StatusCode(404)) => {}
        other => panic!("expected 404, got {other:?}"),
    }
}

#[test]
fn own_endpoints_and_read_only() {
    let up = mock_upstream(vec![]);
    let base = sito_at(std::slice::from_ref(&up));

    let (code, body) = get(&format!("{base}/nix-cache-info")).unwrap();
    assert_eq!(code, 200);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("StoreDir: /nix/store")
    );

    let (code, body) = get(&format!("{base}/status")).unwrap();
    assert_eq!(code, 200);
    let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["upstreams"].as_array().unwrap().len(), 1);
    assert_eq!(status["upstreams"][0]["url"], up);

    match ureq::put(&format!("{base}/abc.narinfo")).send("x") {
        Err(ureq::Error::StatusCode(405)) => {}
        other => panic!("expected 405, got {other:?}"),
    }
}

#[test]
fn dead_upstream_is_survived() {
    // Port from a listener we immediately drop: connection refused.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    let full = mock_upstream(vec![("/abc.narinfo", NARINFO.as_bytes())]);
    let base = sito_at(&[dead, full]);

    let (code, body) = get(&format!("{base}/abc.narinfo")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NARINFO.as_bytes());
}

#[test]
fn nar_affinity_prefers_the_upstream_that_served_the_narinfo() {
    // Both upstreams have the narinfo, only the second has the NAR. After
    // sito serves the narinfo from the *second* (first one 404s narinfo), the
    // NAR request must follow affinity straight to it.
    let a = mock_upstream(vec![]);
    let b = mock_upstream(vec![
        ("/abc.narinfo", NARINFO.as_bytes()),
        ("/nar/deadbeef.nar.xz", NAR_BYTES),
    ]);
    let base = sito_at(&[a, b]);

    get(&format!("{base}/abc.narinfo")).unwrap();
    let (code, body) = get(&format!("{base}/nar/deadbeef.nar.xz")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NAR_BYTES);

    let (_, status) = get(&format!("{base}/status")).unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status).unwrap();
    assert_eq!(status["affinity_entries"], 1);
}

/// Mock upstream at the raw socket level: answers `/nix-cache-info` properly
/// and every other request with `reply`, then closes the connection — a
/// `Content-Length` larger than `reply` makes that a truncated body.
fn raw_upstream(reply: &'static [u8]) -> String {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let out: &[u8] = if buf[..n].starts_with(b"GET /nix-cache-info ") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\nConnection: close\r\n\r\nStoreDir: /nix/store\n"
            } else {
                reply
            };
            let _ = conn.write_all(out);
        }
    });
    format!("http://{addr}")
}

fn status_of(base: &str) -> serde_json::Value {
    let (_, body) = get(&format!("{base}/status")).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn nar_body_failure_ends_the_response_and_counts_as_error() {
    // Bytes already sent cannot be taken back: the client must get a short
    // body right away rather than wait for bytes that will never come.
    let up = raw_upstream(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nonly a few bytes",
    );
    let base = sito_at(&[up]);

    let (code, body) = get(&format!("{base}/nar/x.nar")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, b"only a few bytes");
    let status = status_of(&base);
    assert_eq!(status["upstreams"][0]["errors"], 1);
    assert!(status["upstreams"][0]["nar_mbytes_per_sec"].is_null());
}

#[test]
fn nar_body_failing_before_first_byte_falls_through() {
    // Headers then nothing: the next upstream still gets its turn, because
    // nothing has reached the client yet.
    let broken =
        raw_upstream(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n");
    let full = mock_upstream(vec![("/nar/x.nar", NAR_BYTES)]);
    let base = sito_at(&[broken, full]);

    let (code, body) = get(&format!("{base}/nar/x.nar")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NAR_BYTES);
    let status = status_of(&base);
    assert_eq!(status["upstreams"][0]["errors"], 1);
    assert_eq!(status["upstreams"][0]["hits"], 0);
    assert_eq!(status["upstreams"][1]["hits"], 1);
}

#[test]
fn failed_affinity_upstream_is_retried_before_404() {
    // Only `flaky` has the NAR, and it drops the first NAR request: the
    // other upstream's 404 must not be the final answer.
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let flaky = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut nar_requests = 0;
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n]).to_string();
            let body: &[u8] = if line.starts_with("GET /nix-cache-info ") {
                b"StoreDir: /nix/store\n"
            } else if line.starts_with("GET /abc.narinfo ") {
                NARINFO.as_bytes()
            } else {
                nar_requests += 1;
                if nar_requests == 1 {
                    continue; // drop the connection unanswered
                }
                NAR_BYTES
            };
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(body);
        }
    });
    let empty = mock_upstream(vec![]);
    let base = sito_at(&[flaky, empty]);

    get(&format!("{base}/abc.narinfo")).unwrap();
    let (code, body) = get(&format!("{base}/nar/deadbeef.nar.xz")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NAR_BYTES);
    let status = status_of(&base);
    assert_eq!(status["upstreams"][0]["errors"], 1);
    assert_eq!(status["upstreams"][1]["misses"], 1);
}

#[test]
fn stuck_nar_does_not_block_narinfo() {
    // One NAR slot, taken by a NAR whose upstream accepted and went silent.
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            if buf[..n].starts_with(b"GET /nix-cache-info ") {
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            } else {
                std::thread::spawn(move || {
                    let _ = conn.read(&mut buf);
                    std::thread::sleep(std::time::Duration::from_secs(120));
                });
            }
        }
    });
    let full = mock_upstream(vec![("/abc.narinfo", NARINFO.as_bytes())]);
    let base = sito_with(&[full, silent], 1);

    let nar = format!("{base}/nar/stuck.nar");
    std::thread::spawn(move || get(&nar));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while status_of(&base)["nar_slots"]["used"] != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "NAR never took its slot"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let (code, body) = get(&format!("{base}/abc.narinfo")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, NARINFO.as_bytes());
    assert_eq!(status_of(&base)["nar_slots"]["max"], 1);
}
