# Running and troubleshooting sito

## Running it by hand

```console
$ sito --config ./sito.toml
2026-01-01T10:00:00Z  INFO sito: sito serving listen=127.0.0.1:5001 upstreams=3
```

Point Nix at it for one command without touching system config:

```console
$ NIX_CONFIG="substituters = http://127.0.0.1:5001" nix build .#foo
```

Nix must still trust the signing keys, so `trusted-public-keys` has to cover
every upstream sito might answer from — sito relays signatures untouched and
adds none ([ADR-0006](adr/0006-pass-through-trust.md)). The Nix modules do this
aggregation for you.

## Running it as a service

Both modules keep the daemon alive across crashes and reboots.

**NixOS** — `systemd.services.sito`, `Restart=always`, hardened
(`DynamicUser`, `ProtectSystem=strict`, `NoNewPrivileges`), started after
`network-online.target`.

```console
$ systemctl status sito
$ journalctl -u sito -f
```

**nix-darwin** — `launchd.daemons.sito`, `KeepAlive`, `RunAtLoad`. launchd
discards stderr unless told otherwise, so both streams are written to
`/var/log/sito.log`:

```console
$ tail -f /var/log/sito.log
$ sudo launchctl kickstart -k system/org.nixos.sito   # restart
```

## Logs

`RUST_LOG` (the `logLevel` module option) takes a
[`tracing_subscriber` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html);
the default is `sito=info`.

| Level                | What you get                                                                                      |
| -------------------- | ------------------------------------------------------------------------------------------------- |
| `sito=info` (default)| Startup line, and one line per upstream **state transition** (`upstream state changed url=… up=…`). Quiet by design: nothing per request. |
| `sito=debug`         | Adds the selection plan and chosen upstream per request, plus response codes and client disconnects. This is the level to use when routing looks wrong. |

Warnings appear at any level: `upstream failed` (transport error, with the
error text), `accept failed`, `narinfo body read failed`.

There is deliberately no per-request line at `info` today, which means a
production log shows *that* an upstream flapped but not which requests it
affected. Adding one compact info line per request is a tracked idea — see
[`todo.md`](../todo.md).

## Checking what sito thinks

`/status` is the whole story; every field is documented in
[http-api.md](http-api.md#get-status).

```console
$ curl -s localhost:5001/status | jq '.upstreams[] | {url, healthy, probe_ms, narinfo_ms, hits, misses, errors}'
```

Confirm sito itself is answering (this works even with every upstream down):

```console
$ curl -s localhost:5001/nix-cache-info
StoreDir: /nix/store
WantMassQuery: 1
Priority: 10
```

## Troubleshooting

**Nix downloads from the wrong cache, or is slow.**
`curl -s localhost:5001/status | jq`. If the cache you expected shows
`healthy: false`, the probe cannot reach it — that is a network or upstream
problem, not a routing one. If it shows `healthy: true` but a higher `tier`
number than the one being used, sito is behaving correctly: tier order is
absolute and beats speed. If it is in the same tier and slower on `narinfo_ms`,
it is being deprioritised on measured evidence.

**A cache that is back up is still not used.**
It is used again after the next probe pass, at most `probe-interval-secs` away
(default 15 s). Failures kick an immediate pass, recoveries wait for the timer.
Watch for `upstream state changed … up=true`.

**An upstream that needs authentication is always `healthy: false`.**
The probe treats any non-2xx answer to `GET /nix-cache-info` as unreachable,
401/403 included, and a down upstream is skipped entirely — so there is no way
back for a cache that is up but always rejects an unauthenticated probe. sito
sends no credentials; such caches are not supported today.

**Everything 404s.**
Check `misses` in `/status`. If misses are climbing, sito is reaching the
upstreams and they genuinely lack the paths — normal, and Nix will build
locally. If `errors` are climbing instead, it is connectivity.

**Nix reports a signature error.**
sito never touches signatures; the client does not trust the key that upstream
signs with. Add it to `trusted-public-keys` (or to the upstream's `public-keys`
in `settings` if `manageSubstituters` is on) and rebuild.

**sito will not start.**
The error names the problem and the tier index — see the validation list in
[configuration.md](configuration.md#validation). `bind 127.0.0.1:5001:
Address already in use` means something else holds the port, often an older
instance still running.

**Nix hangs or is slow with sito down.**
It should not: a dead sito means connection-refused on loopback, which is
instant, and Nix falls through to the next substituter or a local build. Slow
failure instead points at sito being *up* and one of its upstreams being a
black hole — check `probe-timeout-secs` and the `upstream failed` warnings.

**Large builds stall at high concurrency.**
`max-inflight` (default 64) bounds concurrent transfers, and a slot is held for
a whole NAR download. Raise it if `nix build -j` with many parallel downloads
plateaus below the link speed.

## Development

```console
$ nix develop                     # cargo, clippy, rustfmt, rust-analyzer
$ cargo test                      # unit tests plus the end-to-end suite
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo fmt --all
$ nix build -L                    # what CI builds; runs the tests again
```

`tests/e2e.rs` starts real mock upstreams and a real sito on ephemeral ports —
tier fallback, affinity, dead upstreams and the read-only surface are covered
there rather than with mocks.
