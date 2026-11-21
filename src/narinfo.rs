//! Narinfo rewriting and the self-describing NAR URLs it produces (ADR-0009).
//!
//! A narinfo's `URL:` is relative to the cache that served it, and Nix keeps
//! narinfos in its own disk cache for up to 30 days, keyed by the substituter
//! it asked: sito. So a NAR request may arrive long after the narinfo, after a
//! restart, or while the upstream that served it is down. sito therefore
//! rewrites the `URL:` to carry everything it needs to serve the NAR later
//! without remembering anything:
//!
//! ```text
//! URL: nar/sito/v1/<upstream key>/<store hash>/<NarHash>/<codec>/<original URL>
//! Compression: none
//! ```
//!
//! and serves every NAR decompressed. A NAR's identity is then its `NarHash`:
//! any upstream whose narinfo for the same store path carries the same
//! `NarHash` holds the same NAR, whatever its compression or URL layout, so it
//! can stand in when the original upstream cannot serve it.
//!
//! None of the rewritten fields are signed — a narinfo signature covers the
//! store path, `NarHash`, `NarSize` and references — and Nix still checks the
//! NAR it receives against `NarHash`, so the client's verification is
//! unchanged (ADR-0006).

/// Prefix of every rewritten NAR URL, relative to the cache root. Under
/// `nar/` so the route table needs no new entry; `v1` leaves room to change
/// the layout without misreading URLs Nix still holds from an older sito.
const PREFIX: &str = "nar/sito/v1/";

/// NAR compressions sito can decode. A narinfo with any other compression,
/// including bzip2 (Nix's default when the field is missing), is relayed
/// unmodified and its NAR fetched verbatim, with no substitute upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    None,
    Xz,
    Zstd,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Codec> {
        match s {
            "none" => Some(Codec::None),
            "xz" => Some(Codec::Xz),
            "zstd" => Some(Codec::Zstd),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::Xz => "xz",
            Codec::Zstd => "zstd",
        }
    }
}

/// The narinfo fields sito reads.
#[derive(Debug, PartialEq, Eq)]
pub struct Narinfo<'a> {
    pub url: &'a str,
    /// As written, or `bzip2` when absent, which is how Nix reads it.
    pub compression: &'a str,
    pub nar_hash: &'a str,
}

/// Read `URL`, `Compression` and `NarHash`; `None` without a URL or NarHash.
pub fn parse(text: &str) -> Option<Narinfo<'_>> {
    let (mut url, mut compression, mut nar_hash) = (None, "bzip2", None);
    for (key, value) in text.lines().filter_map(field) {
        match key {
            "URL" => url = Some(value),
            "Compression" => compression = value,
            "NarHash" => nar_hash = Some(value),
            _ => {}
        }
    }
    Some(Narinfo {
        url: url.filter(|u| !u.is_empty())?,
        compression,
        nar_hash: nar_hash.filter(|h| !h.is_empty())?,
    })
}

fn field(line: &str) -> Option<(&str, &str)> {
    line.split_once(':').map(|(k, v)| (k, v.trim()))
}

/// Where a rewritten NAR URL points.
#[derive(Debug, PartialEq, Eq)]
pub struct NarTarget {
    /// [`upstream_key`] of the upstream that served the narinfo.
    pub upstream_key: String,
    /// Hash part of the store path, which names its narinfo on any upstream.
    pub store_hash: String,
    /// The narinfo's `NarHash`, which any substitute must match.
    pub nar_hash: String,
    /// Compression of the NAR at `path`.
    pub codec: Codec,
    /// The original NAR path on that upstream, leading slash included.
    pub path: String,
}

/// Rewrite a narinfo served for `/<store_hash>.narinfo` by the upstream with
/// key `upstream_key`: `URL:` becomes a self-describing NAR URL,
/// `Compression:` becomes `none`, and `FileHash:` and `FileSize:`, which
/// describe the compressed file, are dropped. Every other line is kept as is,
/// signatures included.
///
/// `None` means relay the narinfo unmodified: not UTF-8, no `URL` or
/// `NarHash`, a compression sito cannot decode, or an absolute `URL`, which
/// Nix fetches directly without asking sito.
pub fn rewrite(body: &[u8], store_hash: &str, upstream_key: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let info = parse(text)?;
    let codec = Codec::parse(info.compression)?;
    if info.url.contains("://") {
        return None;
    }
    let url = format!(
        "{PREFIX}{upstream_key}/{store_hash}/{}/{}/{}",
        escape(info.nar_hash),
        codec.as_str(),
        info.url
    );
    let mut out = String::with_capacity(text.len() + url.len());
    for line in text.lines() {
        match field(line).map(|(k, _)| k) {
            Some("URL") => out.push_str(&format!("URL: {url}\n")),
            Some("Compression") => out.push_str("Compression: none\n"),
            Some("FileHash" | "FileSize") => {}
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    Some(out.into_bytes())
}

/// Decode a request path produced by [`rewrite`]; `None` for any other path,
/// such as a NAR URL from a narinfo Nix cached before sito rewrote them.
pub fn parse_target(request_path: &str) -> Option<NarTarget> {
    let rest = request_path.strip_prefix('/')?.strip_prefix(PREFIX)?;
    // The original URL comes last and keeps its own slashes and query.
    let mut parts = rest.splitn(5, '/');
    let mut next = || parts.next().filter(|p| !p.is_empty());
    let (key, store_hash, nar_hash, codec, path) = (next()?, next()?, next()?, next()?, next()?);
    Some(NarTarget {
        upstream_key: key.to_string(),
        store_hash: store_hash.to_string(),
        nar_hash: unescape(nar_hash)?,
        codec: Codec::parse(codec)?,
        path: format!("/{path}"),
    })
}

/// The store hash a narinfo request path names, if it is a well-formed
/// `/<hash>.narinfo`. A malformed one is relayed but never rewritten, since
/// the hash goes into the NAR URL.
pub fn store_hash(request_path: &str) -> Option<&str> {
    let hash = request_path.strip_prefix('/')?.strip_suffix(".narinfo")?;
    (hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(hash)
}

/// A short id for an upstream that stays the same across restarts and config
/// reorders: FNV-1a over its URL, trailing slash ignored. Nix holds rewritten
/// URLs for weeks, so a flat upstream index, which shifts when an upstream is
/// added, would point them at the wrong cache. An unknown key, after the URL
/// itself changed, only costs the NAR its preferred upstream.
pub fn upstream_key(url: &str) -> String {
    let hash = url
        .trim_end_matches('/')
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Percent-encode everything but `[A-Za-z0-9:._-]`, so a hash in SRI form,
/// whose base64 may hold `/` and `+`, stays one path segment.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b":._-".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "1rxb1dhql5jg8mg2k3wlq813a4gjlvk4";
    const NARINFO: &str = "StorePath: /nix/store/1rxb1dhql5jg8mg2k3wlq813a4gjlvk4-hello-1\n\
        URL: nar/0abc.nar.xz\n\
        Compression: xz\n\
        FileHash: sha256:0abc\n\
        FileSize: 100\n\
        NarHash: sha256:1def\n\
        NarSize: 300\n\
        References: \n\
        Sig: cache-1:c2ln\n";

    #[test]
    fn rewrite_round_trips_through_parse_target() {
        let out = rewrite(NARINFO.as_bytes(), HASH, "k").unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "StorePath: /nix/store/1rxb1dhql5jg8mg2k3wlq813a4gjlvk4-hello-1\n\
             URL: nar/sito/v1/k/1rxb1dhql5jg8mg2k3wlq813a4gjlvk4/sha256:1def/xz/nar/0abc.nar.xz\n\
             Compression: none\n\
             NarHash: sha256:1def\n\
             NarSize: 300\n\
             References: \n\
             Sig: cache-1:c2ln\n"
        );
        let url = parse(&out).unwrap().url;
        assert_eq!(
            parse_target(&format!("/{url}")).unwrap(),
            NarTarget {
                upstream_key: "k".into(),
                store_hash: HASH.into(),
                nar_hash: "sha256:1def".into(),
                codec: Codec::Xz,
                path: "/nar/0abc.nar.xz".into(),
            }
        );
    }

    #[test]
    fn original_query_and_sri_hash_survive() {
        let info = "URL: nar/abc.nar?hash=xyz\nCompression: none\nNarHash: sha256-a/b+c=\n";
        let out = String::from_utf8(rewrite(info.as_bytes(), HASH, "k").unwrap()).unwrap();
        let url = parse(&out).unwrap().url;
        assert!(url.contains("/sha256-a%2Fb%2Bc%3D/"), "{url}");
        let target = parse_target(&format!("/{url}")).unwrap();
        assert_eq!(target.nar_hash, "sha256-a/b+c=");
        assert_eq!(target.codec, Codec::None);
        assert_eq!(target.path, "/nar/abc.nar?hash=xyz");
    }

    #[test]
    fn undecodable_or_absolute_narinfos_are_left_alone() {
        let bzip2 = "URL: nar/a.nar.bz2\nCompression: bzip2\nNarHash: sha256:1\n";
        let implicit_bzip2 = "URL: nar/a.nar.bz2\nNarHash: sha256:1\n";
        let absolute = "URL: https://elsewhere/a.nar\nCompression: none\nNarHash: sha256:1\n";
        let no_hash = "URL: nar/a.nar\nCompression: none\n";
        for info in [bzip2, implicit_bzip2, absolute, no_hash] {
            assert_eq!(rewrite(info.as_bytes(), HASH, "k"), None, "{info}");
        }
    }

    #[test]
    fn other_nar_paths_are_not_targets() {
        assert_eq!(parse_target("/nar/0abc.nar.xz"), None);
        assert_eq!(parse_target("/nar/sito/v1/k/h/sha256:1/xz/"), None);
        assert_eq!(parse_target("/nar/sito/v1/k/h/sha256:1/br/nar/a"), None);
    }

    #[test]
    fn store_hash_wants_a_well_formed_narinfo_path() {
        assert_eq!(store_hash(&format!("/{HASH}.narinfo")), Some(HASH));
        assert_eq!(store_hash("/short.narinfo"), None);
        assert_eq!(
            store_hash("/../../aaaaaaaaaaaaaaaaaaaaaaaaaaaaa.narinfo"),
            None
        );
    }

    #[test]
    fn upstream_key_ignores_trailing_slash() {
        assert_eq!(upstream_key("https://a/"), upstream_key("https://a"));
        assert_ne!(upstream_key("https://a"), upstream_key("https://b"));
        assert_eq!(upstream_key("https://a").len(), 16);
    }
}
