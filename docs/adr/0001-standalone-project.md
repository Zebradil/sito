# sito is a standalone project, not a kasha component

kasha's ADR-0005 deferred a "selection shim" until the off-network timeout tax
was actually felt. It is felt: with kasha's static substituter list, builds
outside the LAN are effectively unusable without overriding substituters via
`NIX_CONFIG` by hand.

The shim could have been a `kasha shim` subcommand sharing the repo and release
train, but the two programs share no runtime dependency: sito proxies any HTTP
binary caches and is useful without a kasha box; kasha serves fine without sito.
Bundling would couple release cadence and imply a dependency that doesn't exist.
The costs of a separate repo (CI, flake, release plumbing) are accepted.

kasha's `nixosModules.consumer` stays as-is: hosts pinned to the LAN never roam,
so the static list plus low `connect-timeout` remains the right answer there.

Licensed MIT/Apache-2.0 dual, public from the start.
