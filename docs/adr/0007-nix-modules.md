# Nix modules manage the substituter list by default

Targets: `x86_64-linux` (nixosModule) and `aarch64-darwin` (darwinModule) — the
roaming laptop is the primary beneficiary, so darwin support is not optional.

Both modules run the daemon (systemd/launchd, keep-alive) and, with
`manageSubstituters = true` (the default), set
`substituters = [ "http://localhost:<port>" ]` and aggregate every configured
upstream's public keys into `trusted-public-keys`. Enabling sito without wiring
Nix to it would be a surprising default; users who curate their substituter
list by hand flip one bool and add the localhost entry themselves.

No extra fallback substituter is added by default: sito already queries every
upstream, so a second static entry means a duplicate remote roundtrip on every
true miss — a permanent tax for the rare case of a dead daemon. When sito is
down, localhost answers connection-refused instantly (no timeout tax) and Nix
falls back to building. An `extraFallbackSubstituters` option serves the
cautious.
