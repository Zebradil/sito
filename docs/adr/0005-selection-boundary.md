# Selection logic is Rust behind a serializable boundary; scripting deferred

The selection rules are where experimentation happens, so the iteration loop
matters. Options considered: rules as Rust code (iterate via rebuild, which is
the normal `nixos-rebuild`/`darwin-rebuild` loop anyway), a config-file policy
DSL, or an embedded scripting engine (the OpenResty/Envoy "policy as code"
pattern — Lua, rhai, or WASM against a host API).

v1: rules are Rust behind a small trait, with a few TOML knobs (probe interval,
thresholds). A DSL is a language nobody else will learn, and scripting is
premature before the built-in rules prove insufficient.

The door stays open structurally: the engine's input (request context, upstream
states and metrics) and output (the selection plan — an ordered list of fetch
attempts) are plain serializable data. Any future engine (`rhai`, `mlua`,
`wasmtime`) slots in at that seam without touching the proxy core. Nothing in
the core may reach around the boundary.

Config is TOML (plus flags/env overrides), reload is restart-only: the nix
module regenerates config and restarts the service on rebuild, so live-reload
would buy state-migration bugs for nothing.
