# Quality is passive traffic metrics plus active probes, held in memory only

Ranking upstreams needs two signals with different failure modes:

- **Passive** — latency and throughput measured on real requests. Free and
  honest, but stale the moment the machine changes networks while idle: the
  first request after waking in a café would be routed on dead data.
- **Active** — a timed `GET /nix-cache-info` per upstream, run on start, on a
  short interval (~15s default), and immediately after a request failure.
  Cheap, and directly answers the roaming question: "is the LAN box reachable
  right now".

sito uses both: probes gate reachability (an upstream marked down is skipped
entirely), passive metrics order the reachable ones (e.g. private remote cache
vs cache.nixos.org by observed speed).

State is in-memory; every start is a cold start with rank falling back to
config order until probes and traffic fill it in. The on-start probe settles
reachability in under a second, and yesterday's throughput numbers from a
different network are as likely wrong as right.
<!-- ponytail: cold start, persist rank state only if it measurably misroutes -->

Possible refinement, undecided: a burst re-probe on the first request after an
idle gap, to shrink the stale window below the probe interval.

OS network-change events (SCNetworkReachability, netlink) are deliberately not
used: platform-specific surface to save seconds the interval already bounds.
