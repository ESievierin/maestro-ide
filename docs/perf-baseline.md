# Performance baseline

Reference numbers for spotting regressions; refresh them when the toolchain or
a startup-path change makes a real difference. Method: release build
(`npm run tauri build -- --no-bundle`), mock sidecar, isolated `MAESTRO_HOME`,
Windows 10; startup measured as process start → the WebView2 CDP endpoint
answering (the window is up and the frontend is served).

## 2026-08-14 (commit range through the N+37 cycle)

| Metric                         | Value                           |
| ------------------------------ | ------------------------------- |
| Release binary (`maestro.exe`) | 19.6 MB                         |
| Cold startup → webview ready   | 618–759 ms (3 runs, first ~760) |
| Rust suite (`cargo test`)      | ~16–19 s, 332 tests             |
| Frontend suite (`npm test`)    | ~4–6 s, 83 tests                |
| Release rebuild (warm cache)   | ~1.5–3 min                      |

Notes:

- The first run after boot pays WebView2 warm-up (~+150 ms); later runs settle
  around 620 ms.
- The store in the measured profile held ~140 KB; startup includes the config
  seed, telemetry retention sweep, and stale-session failing.
