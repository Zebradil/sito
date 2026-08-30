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
    let tiers = upstreams
        .iter()
        .map(|u| format!("[[tier]]\n  [[tier.upstream]]\n  url = \"{u}\"\n"))
        .collect::<String>();
    let cfg = sito::config::Config::parse(&format!("listen = \"127.0.0.1:0\"\n{tiers}")).unwrap();
    let (app, server) = sito::build(&cfg).unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || sito::serve(app, server, 8));
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
