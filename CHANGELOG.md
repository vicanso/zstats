# Changelog

## [0.3.0] - 2026-08-13

### ⛰️  Features

- Derived IO/mem/CPU fields, metrics docs, and CPU-time recording - ([255ea84](https://github.com/vicanso/zedis/commit/255ea84c7cfeb035923c9278b9ea7467f45224a1))
- Builtin template for the whole-app alert rules - ([ea4b5d3](https://github.com/vicanso/zedis/commit/ea4b5d35ccddae40999c30ec2b5bf6fbd326a5de))
- Whole-app alert rules, semantic status lines, group IO and uid - ([93ddc1c](https://github.com/vicanso/zedis/commit/93ddc1c196de7139f86d2606928af183f8c58cc6))
- Install.sh, version targets, and a --version flag - ([7955bac](https://github.com/vicanso/zedis/commit/7955bac2973417d2a9adb6f04c9e0c11ce5bf6c2))
- Disk and memory-pressure alerts, battery metric, delivery logging - ([0017d27](https://github.com/vicanso/zedis/commit/0017d2720d0390e8729787edb0e61d25dae39314))
- [**breaking**] Lib frontend layers, app groups, two-tier alerts, new metrics - ([0477656](https://github.com/vicanso/zedis/commit/0477656ea8bc1e273a50ae48773ca75b609adee8))
- Timestamped daemon logging via tracing-subscriber - ([d67a21f](https://github.com/vicanso/zedis/commit/d67a21f5d42915181a5b364cfbd7047bc798b627))
- [**breaking**] Config-directory driven CLI with human-friendly durations - ([efa0a31](https://github.com/vicanso/zedis/commit/efa0a31b92720bc3f3f71e318aecb64a93708e67))
- Add temperature sensors (15s refresh) and simplify PROC columns - ([b883bbe](https://github.com/vicanso/zedis/commit/b883bbe0038c69046eddb2565e6d53e07d4dfa46))
- Scale process boost by core load - ([e4ad561](https://github.com/vicanso/zedis/commit/e4ad5617107bbbc1315db684033ce821dedf5e1b))
- Let attach detach with d/q like watch mode - ([e26bf7e](https://github.com/vicanso/zedis/commit/e26bf7e9ea323120b3f47636aabcd0480edbaf2d))
- Hot-reload alert config; add cargo install package metadata - ([ab3db87](https://github.com/vicanso/zedis/commit/ab3db8770795b23841f2846c2f3d7a1a5e8976c3))
- Richer metrics, cheaper selection, persistent alert config - ([d68e5b5](https://github.com/vicanso/zedis/commit/d68e5b502c91e244f65740057915126f205b5187))
- Daemon mode with history replay and per-process alerting - ([43c3836](https://github.com/vicanso/zedis/commit/43c3836d90ab178995a15ee490e4418b1adc9b87))
- In-place watch TUI, smarter PROC view, cheaper collection - ([8cb9fd4](https://github.com/vicanso/zedis/commit/8cb9fd4d2730461933ff41b5ee7a4f3f3faefa9e))
- Optional disk/network collection and aligned CLI tables - ([a0cec34](https://github.com/vicanso/zedis/commit/a0cec3492c435bc27d1ab8bb94545732dcd91c0b))
- Init commit - ([a4f3586](https://github.com/vicanso/zedis/commit/a4f3586a285a039dbeea9a53ef997887a7d30af1))

### 🐛 Bug Fixes

- Align CI with make check, self-provision targets, trim daemon threads - ([464b837](https://github.com/vicanso/zedis/commit/464b837a2125eff1a012e162d7a3ac8cfa32e116))

### ⚙️ Miscellaneous Tasks

- Version 0.2.0 - ([0d0f3de](https://github.com/vicanso/zedis/commit/0d0f3de7c12158a7b18d0ce723a9c5cc4e64d92f))
- Publish workflow — tagged releases and gated nightly builds - ([77a3faf](https://github.com/vicanso/zedis/commit/77a3faf5fd863820e24eda88e8eca56e78767acb))

## [0.2.0] - 2026-08-12

### ⛰️  Features

- Builtin template for the whole-app alert rules - ([ea4b5d3](https://github.com/vicanso/zedis/commit/ea4b5d35ccddae40999c30ec2b5bf6fbd326a5de))
- Whole-app alert rules, semantic status lines, group IO and uid - ([93ddc1c](https://github.com/vicanso/zedis/commit/93ddc1c196de7139f86d2606928af183f8c58cc6))
- Install.sh, version targets, and a --version flag - ([7955bac](https://github.com/vicanso/zedis/commit/7955bac2973417d2a9adb6f04c9e0c11ce5bf6c2))
- Disk and memory-pressure alerts, battery metric, delivery logging - ([0017d27](https://github.com/vicanso/zedis/commit/0017d2720d0390e8729787edb0e61d25dae39314))
- [**breaking**] Lib frontend layers, app groups, two-tier alerts, new metrics - ([0477656](https://github.com/vicanso/zedis/commit/0477656ea8bc1e273a50ae48773ca75b609adee8))
- Timestamped daemon logging via tracing-subscriber - ([d67a21f](https://github.com/vicanso/zedis/commit/d67a21f5d42915181a5b364cfbd7047bc798b627))
- [**breaking**] Config-directory driven CLI with human-friendly durations - ([efa0a31](https://github.com/vicanso/zedis/commit/efa0a31b92720bc3f3f71e318aecb64a93708e67))
- Add temperature sensors (15s refresh) and simplify PROC columns - ([b883bbe](https://github.com/vicanso/zedis/commit/b883bbe0038c69046eddb2565e6d53e07d4dfa46))
- Scale process boost by core load - ([e4ad561](https://github.com/vicanso/zedis/commit/e4ad5617107bbbc1315db684033ce821dedf5e1b))
- Let attach detach with d/q like watch mode - ([e26bf7e](https://github.com/vicanso/zedis/commit/e26bf7e9ea323120b3f47636aabcd0480edbaf2d))
- Hot-reload alert config; add cargo install package metadata - ([ab3db87](https://github.com/vicanso/zedis/commit/ab3db8770795b23841f2846c2f3d7a1a5e8976c3))
- Richer metrics, cheaper selection, persistent alert config - ([d68e5b5](https://github.com/vicanso/zedis/commit/d68e5b502c91e244f65740057915126f205b5187))
- Daemon mode with history replay and per-process alerting - ([43c3836](https://github.com/vicanso/zedis/commit/43c3836d90ab178995a15ee490e4418b1adc9b87))
- In-place watch TUI, smarter PROC view, cheaper collection - ([8cb9fd4](https://github.com/vicanso/zedis/commit/8cb9fd4d2730461933ff41b5ee7a4f3f3faefa9e))
- Optional disk/network collection and aligned CLI tables - ([a0cec34](https://github.com/vicanso/zedis/commit/a0cec3492c435bc27d1ab8bb94545732dcd91c0b))
- Init commit - ([a4f3586](https://github.com/vicanso/zedis/commit/a4f3586a285a039dbeea9a53ef997887a7d30af1))

### 🐛 Bug Fixes

- Align CI with make check, self-provision targets, trim daemon threads - ([464b837](https://github.com/vicanso/zedis/commit/464b837a2125eff1a012e162d7a3ac8cfa32e116))

### ⚙️ Miscellaneous Tasks

- Publish workflow — tagged releases and gated nightly builds - ([77a3faf](https://github.com/vicanso/zedis/commit/77a3faf5fd863820e24eda88e8eca56e78767acb))

