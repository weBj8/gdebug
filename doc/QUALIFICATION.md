# Qualification record

Host: AMD Ryzen 9 5950X, Linux 7.2.11 (CachyOS), rsgdll `0a71f1bb2d6f267927135b04be6992465e4ea1ad`.

| Combination | Status | Evidence |
|---|---|---|
| linux64 server (srcds_run_x64) | PASS | attach, conditional + hit-count bp, stack/scopes/locals/env paging, evaluate (incl. error), stopped GLua write, console command, step over/out, function bp (`hook.Call`), logpoint, pause, detach restored hook+JIT; kill -9 CLI while stopped → realm resumed, re-attach ok; `changelevel` → session terminated, new generation/descriptor, re-attach ok |
| linux32 server (srcds_run) | PASS | same smoke (break/stack/locals/evaluate/step out/pause/console/detach restore) |
| linux32/64 client | NOT RUN | needs GMod client launch |
| win32/win64 server/client | NOT RUN | builds only (`gdebug.dll`, `gdebug.exe` for i686/x86_64 msvc) |
| VS Code / Zed UI | NOT RUN | DAP covered by `crates/gdebug-cli/tests/e2e.rs`; Zed ext builds for wasm32-wasip2 |

Idle-stopped cached `status` latency (linux64 and linux32 server, CLI round trip): 0.1–0.3 ms
(target ≤ 100 ms). Pause-to-stopped on a 50 ms timer fixture: 13 ms.

GMod had no prior Lua debug hook on the server at attach (`replacedHook: false`), so the
watchdog-resume-after-long-pause gate is not exercised by these runs.

Automated: `cargo test --workspace` (protocol unit tests, stock-LuaJIT session, CLI JSONL + DAP e2e).
