---
title: Monitor sito over time
description: Ship sito's /metrics from a roaming machine to VictoriaMetrics or Prometheus, and graph how each build fetched.
---

sito keeps no history: `/status` and `/metrics` show the numbers since the process started, nothing more. This guide
sets up a pipeline that keeps them, from a machine that is often away from the network the metrics store lives on,
then lists the metrics and the queries worth a panel.

## Prerequisites

- sito running, by hand or through the [NixOS or nix-darwin module](../nix-modules/). Commands below assume the default
  `127.0.0.1:5001`.
- A metrics store that accepts Prometheus remote write: VictoriaMetrics, or Prometheus with
  `--web.enable-remote-write-receiver`.
- A route from the machine to the store when it is home, or over a VPN such as Tailscale.

## How the pipeline works

A store at home cannot scrape a laptop that is somewhere else, so collection runs on the laptop and pushes:

```text
laptop                                      home
sito /metrics ◀── scrape every 10 s ── vmagent ──── remote write ───▶ VictoriaMetrics ──▶ Grafana
                                           │        (when reachable)
                                           └─ on-disk queue while the store is out of reach
```

[vmagent](https://docs.victoriametrics.com/victoriametrics/vmagent/) scrapes sito over loopback, so collection never
stops. While the store is unreachable, samples queue on disk. Once the store is back, they are sent with their original
timestamps, and the graphs show no gap for the time away.

## 1. Run vmagent next to sito

### nix-darwin

The sito module runs vmagent as a second launchd daemon:

```nix
{
  services.sito.vmagent = {
    enable = true;
    remoteWriteUrl = "http://metrics.example.ts.net:8428/api/v1/write";
    extraArgs = [ "-remoteWrite.label=host=laptop" ];
  };
}
```

The daemon is `org.nixos.sito-vmagent`, logging to `/var/log/sito-vmagent.log`. It scrapes sito's `settings.listen`
address, and its own status pages listen on `127.0.0.1:8429` only. A broken scrape config fails `darwin-rebuild`,
because the module checks it with `vmagent -dryRun` at build time.

| Option | Default | Meaning |
| --- | --- | --- |
| `vmagent.enable` | `false` | Run the daemon. |
| `vmagent.remoteWriteUrl` | none | Remote write endpoint, for example VictoriaMetrics' `/api/v1/write`. |
| `vmagent.scrapeInterval` | `"10s"` | How often vmagent scrapes sito. |
| `vmagent.maxDiskUsage` | `"1GB"` | Cap on the on-disk queue; past it the oldest samples are dropped. |
| `vmagent.dataDir` | `"/var/lib/sito-vmagent"` | Where unsent samples wait. |
| `vmagent.extraArgs` | `[]` | Extra vmagent flags: labels, remote-write credentials. |
| `vmagent.package` | `pkgs.vmagent` | vmagent build to run. |

For basic auth on the store, add `-remoteWrite.basicAuth.username=…` and `-remoteWrite.basicAuth.passwordFile=…` to
`extraArgs`. vmagent re-reads the password file every second.

### NixOS

NixOS ships its own `services.vmagent`; give it a scrape job for sito:

```nix
{
  services.vmagent = {
    enable = true;
    remoteWrite.url = "http://metrics.example.ts.net:8428/api/v1/write";
    prometheusConfig.scrape_configs = [
      {
        job_name = "sito";
        scrape_interval = "10s";
        static_configs = [ { targets = [ "127.0.0.1:5001" ]; } ];
      }
    ];
    extraArgs = [
      "-remoteWrite.label=host=laptop"
      "-remoteWrite.maxDiskUsagePerURL=1GB"
      "-httpListenAddr=127.0.0.1:8429"
    ];
  };
}
```

The queue lives in the unit's cache directory, `/var/cache/vmagent`, and survives restarts. vmagent's disk use is
unlimited unless `-remoteWrite.maxDiskUsagePerURL` caps it. Without `-httpListenAddr`, vmagent listens on every
interface.

### Anything else

Any Prometheus-compatible scraper works: point a job at `http://127.0.0.1:5001/metrics`. Only a scraper running on the
same machine as sito keeps collecting while the machine is away.

## 2. Size the queue and the store

With four upstreams sito exposes about 120 series; each upstream adds about 30. In a measured run vmagent queued them
at 2.2 KB per scrape: about 19 MB a day at a 10 s interval, so a 1GB queue holds about seven weeks offline. A shorter
interval adds timing detail within a build, at the cost of queue space. The counters and histograms record every
request either way.

The store decides how old a sample it accepts, and a sample it refuses is lost:

- **VictoriaMetrics** rejects samples older than `-retentionPeriod` (default one month), or `-maxBackfillAge` if set
  lower. Keep retention longer than the longest time away.
- **Prometheus** rejects samples older than its in-memory head, roughly the last few hours, as `out of bounds`. Set
  `storage.tsdb.out_of_order_time_window` in the Prometheus config to cover the longest time away, for example `1w`.
  With the default settings, a sample six hours old was rejected; with the window set, it was stored.

## 3. Check that samples arrive

On the machine, vmagent's own metrics say whether the queue drains:

```sh
curl -s http://127.0.0.1:8429/metrics | grep -E '^vmagent_remotewrite_(pending_data_bytes|requests_total)'
```

`vmagent_remotewrite_pending_data_bytes` grows while the store is out of reach and falls back once it answers. On the
store, query `sito_uptime_seconds`; it should be there for every host label you set.

VictoriaMetrics shows a sample in queries about 30 s after it arrives (`-search.latencyOffset`). Backfilled samples
appear in queries within seconds of arriving, cached ranges included.

## Metrics

`/metrics` serves sito's numbers in the Prometheus text format. Metric names are kept stable, unlike the `/status`
shape.

| Metric | Type | Meaning |
| --- | --- | --- |
| `sito_uptime_seconds` | gauge | Seconds since sito started. |
| `sito_affinity_entries` | gauge | As `affinity_entries` in `/status`. |
| `sito_nar_slots_used`, `sito_nar_slots_max` | gauge | As `nar_slots` in `/status`. |
| `sito_upstream_healthy` | gauge | `1` or `0`; absent until the first probe answers. |
| `sito_upstream_requests_total` | counter | Requests per upstream, by `result`: `hit`, `miss` or `error`. |
| `sito_upstream_narinfo_seconds` | histogram | Narinfo latency, request to response headers. Buckets from 10 ms to 2.5 s. |
| `sito_upstream_nar_bytes_per_second` | histogram | Throughput of completed NAR transfers. Buckets from 0.5 MB/s to 100 MB/s. |
| `sito_upstream_nar_bytes_total` | counter | NAR body bytes passed to clients, including transfers that broke off. |
| `sito_upstream_probe_ewma_seconds` | gauge | `probe_ms`, in seconds. |
| `sito_upstream_narinfo_ewma_seconds` | gauge | `narinfo_ms`, in seconds. |
| `sito_upstream_nar_ewma_bytes_per_second` | gauge | `nar_mbytes_per_sec`, in bytes per second. |

Per-upstream metrics carry `url` and `tier` labels. An EWMA gauge is absent until its first sample. Counters reset when
sito restarts, which `rate()` and `increase()` absorb.

The EWMA gauges show what the ranker saw at scrape time. A build often finishes inside one scrape interval, so they say
little about it. The counters and histograms record every request between two scrapes, so use them for anything
about a build.

## Queries

Each of these was run against VictoriaMetrics fed by vmagent; Prometheus accepts the same PromQL. Swap the `[1h]`
window for Grafana's `$__range` or `$__rate_interval` in a dashboard.

**At home or away.** The LAN upstream's health is the location signal sito already has. Use it as a state timeline,
or to shade other panels:

```promql
sito_upstream_healthy{url="http://box.lan:5000"}
```

**Narinfo latency, p95 per upstream.** Lookups are most of what a build waits on:

```promql
histogram_quantile(0.95, sum by (url, le) (increase(sito_upstream_narinfo_seconds_bucket[1h])))
```

**NAR throughput, median per upstream:**

```promql
histogram_quantile(0.5, sum by (url, le) (increase(sito_upstream_nar_bytes_per_second_bucket[1h])))
```

**Share of NARs slower than 2 MB/s.** A bucket bound (`le`) picks the threshold:

```promql
sum by (url) (increase(sito_upstream_nar_bytes_per_second_bucket{le="2000000"}[1h]))
  / sum by (url) (increase(sito_upstream_nar_bytes_per_second_count[1h]))
```

**Bytes served per upstream**, and the download rate while a build runs:

```promql
sum by (url) (increase(sito_upstream_nar_bytes_total[1h]))
sum by (url) (rate(sito_upstream_nar_bytes_total[1m]))
```

**Hit ratio per upstream.** A cache that mostly misses still costs a round trip for every path in its tier:

```promql
sum by (url) (increase(sito_upstream_requests_total{result="hit"}[1h]))
  / sum by (url) (increase(sito_upstream_requests_total{result=~"hit|miss"}[1h]))
```

**Errors per upstream:**

```promql
sum by (url) (increase(sito_upstream_requests_total{result="error"}[1h]))
```

## Logs for single downloads

Metrics aggregate; the log has every NAR. At the default `sito=info`, each completed download writes one line:

```text
INFO sito::proxy: NAR transfer done url="https://cache.nixos.org/nar/1q5d….nar.zst" bytes=38457 secs=0.02 mbytes_per_sec=2.27
```

A download that took 30 s or more at under 1 MB/s logs `slow NAR transfer` at warn instead.
[Troubleshoot sito](../troubleshooting/#2-read-the-log) lists every log message.
