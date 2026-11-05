# gdebug specification

Research date: **2026-11-05**. Documentation only; no debugger implementation or dependency changes accompany these files.

- [Specification](SPEC.md): scope, architecture, operations, protocol, CLI, DAP/editor integration, and acceptance gates.
- [Research and evidence](RESEARCH.md): inspected framework revision, upstream sources, verified facts versus hypotheses.
- [RSGDLL issue 1](RSGDLL_ISSUE_1.md): resolved upstream; native Lua debug API bindings and ABI probes.
- [RSGDLL issue 2](RSGDLL_ISSUE_2.md): resolved upstream; protected native-hook entry and paused-state inspection.
- [RSGDLL issue 3](RSGDLL_ISSUE_3.md): closed/out of scope; no safe supported pre-unwind interface, so exception breakpoints were removed.

## Decisions confirmed with the user

| Area | Decision |
| --- | --- |
| Targets | Linux x86-64, Linux x86, Windows x86-64, Windows x86 |
| Realms | Server and client; not menu |
| Attach | Module is installed and loaded beforehand; no injection |
| Transport | Local loopback; remote access through a user-managed SSH tunnel |
| Frontends | Persistent JSONL CLI for agents, VS Code, and Zed |
| Commands | Explicitly typed GLua statement chunks and Source console commands |
| Advanced v1 | Conditional, hit-count, and function breakpoints; logpoints |
| Exception breakpoints | Excluded from v1; preserve normal GMod error reporting and protected debugger-command errors |
| Evaluation | Trusted full GLua; calls/side effects allowed; no guaranteed timeout |
| Existing debug hook | Temporary replacement only with explicit consent; save/restore it; warn about disabled watchdog/profilers |

The specification fixes the implementation defaults; measured tuning is allowed where explicitly noted. Scope changes still require a user decision.

## Readiness

**Finalized specification; ready to begin implementation in the order defined in SPEC.md §12. Not runtime-qualified or release-ready.**

The inspected rsgdll revision `0a71f1bb2d6f267927135b04be6992465e4ea1ad` includes the APIs/fixtures addressing issues 1 and 2 and is now the specification's HTTPS Git pin. Issue 3 has a negative feasibility outcome; the user-approved removal of exception breakpoints retires G3. No exception-breakpoint workaround is required.

Start with the native hook/pause/inspection integration (G1/G2), then core/CLI, DAP/editor integration, and full release qualification. Hook ABI, hook-entry error safety, coroutine behavior, watchdog/JIT restoration, and teardown still need gdebug in-engine proof. Upstream code inspection and existing Linux64 evidence do not qualify all eight platform/realm combinations.

A breakpoint blocks the engine thread, potentially freezing both realms in a listen server. Evaluation can hang that thread. These are development-target tools, not production-safe introspection.

## Sources

The full linked source list and evidence qualifications are in [RESEARCH.md](RESEARCH.md#sources).
