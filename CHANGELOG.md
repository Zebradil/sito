# Changelog

## [0.2.0](https://github.com/Zebradil/sito/compare/v0.1.0...v0.2.0) (2026-10-02)


### Features

* add nixosModule and darwinModule ([0df777c](https://github.com/Zebradil/sito/commit/0df777cedfa830e030332ba24140f16c6edb976d))
* implement proxy core v1 ([23e3a16](https://github.com/Zebradil/sito/commit/23e3a16c83d99d43b702fc2e00e68199ed0c2486))
* **module:** named tiers that merge across modules ([#21](https://github.com/Zebradil/sito/issues/21)) ([7b02eff](https://github.com/Zebradil/sito/commit/7b02effec25c52451476cb3f53d63e7f79488b3d))
* read the cache push URL from a variable ([#9](https://github.com/Zebradil/sito/issues/9)) ([519d898](https://github.com/Zebradil/sito/commit/519d8988f233b1e2c805fa734145ca01986be5ef))


### Fixes

* address findings from the documentation pass ([4e646a3](https://github.com/Zebradil/sito/commit/4e646a341da04685aff3d1be3076d94ec28aab5e))
* **darwin:** coerce configFile to string in ProgramArguments ([0d1f479](https://github.com/Zebradil/sito/commit/0d1f4790fe3e5ebe7bbb4abaa372e1167aaaa348))
* **darwin:** write daemon logs to /var/log/sito.log ([a3ff1fc](https://github.com/Zebradil/sito/commit/a3ff1fc2c50adc528c7294108df80268791b74da))
* **deps:** update rust crate toml to v1 ([#2](https://github.com/Zebradil/sito/issues/2)) ([130d04a](https://github.com/Zebradil/sito/commit/130d04ac9a670575f676880847ef4c1f4545da6f))
* make launchd daemon wait for /nix/store to get mounted ([5476642](https://github.com/Zebradil/sito/commit/5476642d29d1a696ad4b4b3eb63c043adb258e45))
* **proxy:** bound idle NAR upstream reads at 60s ([a7098f2](https://github.com/Zebradil/sito/commit/a7098f282776a81308f4cc2a7d5ee5995967bc1a))
* **proxy:** keep NAR slots off the accept loop and expose usage ([ac0b75d](https://github.com/Zebradil/sito/commit/ac0b75df33b0974c2261a6ce4b47be399e09ea1c))
* **proxy:** surface NAR body failures fast and count them ([d53e3aa](https://github.com/Zebradil/sito/commit/d53e3aa1ad8a64629cae853b9c678247b2ef0f87))
* **select:** try the affinity upstream even when marked down ([1a1111a](https://github.com/Zebradil/sito/commit/1a1111a956cf646a5bb8a96e4f6a2bb1f3ae75c3))


### Documentation

* add HTTP API, configuration, architecture and operations references ([2083966](https://github.com/Zebradil/sito/commit/2083966470993d427910cb839089562f15fc7e4c))
* capture sito design as CONTEXT + ADRs ([edfa682](https://github.com/Zebradil/sito/commit/edfa682b22f29710f72aea3f20dfa563e52fcc50))
* expand rustdoc across all modules ([b1f07ac](https://github.com/Zebradil/sito/commit/b1f07ac4e4478ec0b314197900e4252e112f015c))
* note missing info-level per-request logging ([8fbf418](https://github.com/Zebradil/sito/commit/8fbf418b12919e26276f4b4ebf8128ff17b13d3d))
* record deferred auth-gated upstream support ([6c66bcf](https://github.com/Zebradil/sito/commit/6c66bcfde427812e7147a7b00913c56c39f6267c))
