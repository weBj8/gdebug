# Feasibility: observe uncaught engine Lua errors before stack unwinding

**Status: closed — negative feasibility result; feature removed from gdebug v1.** The user accepted dropping exception breakpoints because a safe, stable pre-unwind interface cannot be guaranteed. G3 is retired and this issue is no longer a release blocker.

Upstream's [feasibility report at `0a71f1bb2d6f267927135b04be6992465e4ea1ad`](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/docs/uncaught-errors.md) records a Linux64 server probe: `OnLuaError` runs after unwind, arbitrary error objects are not preserved, and pre-unwind interception would require private/version-specific engine detours. rsgdll exposes no uncaught-error observer. Other target/realm combinations were not verified by that probe.

The original request below is retained as rationale, not work required for v1. Normal GMod error reporting and protected debugger-operation errors remain required; no engine detouring workaround is requested.

**Type:** investigation / possible framework integration  
**Inspected revision:** `e7edbecdf798acf566748e3a443f2d227bd703f3`  
**Affected areas:** GMod engine Lua error boundary, protected-call integration, lifecycle/ABI  
**Original priority:** mandatory feature, now withdrawn by the agreed scope change

## Originally requested behavior (withdrawn)

A downstream debugger needs to stop on **uncaught runtime Lua errors reaching GMod's engine boundary**, while the faulting function's live stack, locals, and upvalues remain inspectable.

Errors caught by script `pcall`/`xpcall` are intentionally excluded. This is not a first-chance/all-errors request. Compilation diagnostics, OOM, and native/process crashes need truthful reporting, not a promise of a normal inspectable Lua exception stop.

After continue/detach, the original engine error handling must execute exactly once, with the original error object/return behavior and ordinary console/hook reporting preserved.

## Why ordinary debugger hooks do not provide this

- Lua 5.1 debug hooks expose call, return/tail-return, line, and count events—not error events.
- A `lua_pcall` error handler can inspect useful fault information before unwind, but installing one only around debugger-owned calls does not cover arbitrary engine timer/net/hook execution.
- Reading the error after `lua_pcall` returns is too late for the original live stack.
- `GM:OnLuaError` supplies diagnostics and a stack-trace table. Its documented API does not promise a pre-unwind callback or live locals.
- Wrapping global `error`/`assert` misses nil indexing, arithmetic failures, and other VM-generated errors; replacing script `pcall`/`xpcall` changes application semantics.
- `lua_atpanic` handles unprotected fatal paths, not ordinary engine-protected runtime errors.
- Hooking exported `lua_error` alone is not proof of intercepting internal VM error paths.

## Baseline framework evidence

- `protected.rs` and `trampoline.rs` protect/report rsgdll's own Lua calls and exports; they are not general observers of engine-owned execution.
- `crates/rsgdll-sys/src/vtables.rs` leaves `ErrorFromLua` as a padding slot. Its existence suggests an investigation point, **not** evidence that it is safely hookable or runs before unwind. Merely typing that slot would not install an observer.
- No public pre-unwind engine error callback was found in the inspected facade/core/sys code.

## Original request to maintainer (closed)

First answer feasibility with a minimal in-engine probe:

1. Is there an existing supported GMod/rsgdll integration point at the actual engine protected-call/error-handler boundary, before Lua frames are unwound?
2. Can it distinguish an error reaching that boundary from one handled by script code, across timer/net/hook entry paths?
3. Can a synchronous owner-thread observer pause, inspect the fault state/coroutine, and run protected debugger evaluation without recursively reporting debugger errors?
4. Can the observer return without replacing/suppressing the original handler or causing double reporting?
5. What is the stable ABI/lifecycle contract on Linux/Windows × x86/x86-64, server/client realms?

If a stable point exists, expose the smallest opt-in observer/registration API and its protected-entry contract. It should carry the live state/generation, original error object, and enough context to locate fault frames. Keep breakpoint policy and networking downstream.

If this needs version-specific engine detours, private VM offsets, or is not safely supportable, report that explicitly with evidence and the affected targets. Do not present a post-unwind diagnostic callback as equivalent. An honest negative feasibility result is useful and triggers a downstream scope decision.

## Original proof fixture / done criteria

- Throw an uncaught nil-index error inside nested Lua functions with known local/upvalue values from an actual engine callback.
- Before original handling/unwind, observe the correct faulting stack and read those live values. A preformatted traceback alone fails this criterion.
- Repeat with arithmetic errors, explicit `error`, a non-string error object, timer callbacks, engine hook callbacks, and a real net receiver.
- Pausing on the error keeps the engine thread stopped; continue returns to the original handler and produces the same reporting/return behavior as without the observer.
- `pcall`/`xpcall`-caught errors do not notify the uncaught observer. A later genuine rethrow reaching the engine boundary does.
- Debugger-originated compile/evaluate/condition errors do not recurse into an exception stop.
- Validate coroutine error context, nested/reentrant errors, handler failure, observer panic/failure containment, disconnect during a stop, and state close/reload. Exclude unsupported OOM handling explicitly.
- Removing the observer leaves no callbacks into an unloaded/closed module and no changes to ordinary protected-call behavior.
- Record results and relevant GMod/VM builds for all eight platform/realm combinations; a Linux64-only success is not a four-platform claim.

## Non-goals / fallback

No requirement to implement a full debugger or intercept every caught error. No request to replace the engine's script dispatcher. Do not add a broad generic detouring framework merely for this issue.

Post-unwind errors can be diagnostics, not live exception stops. The negative feasibility result led to removing this feature from v1; no observer, error-forwarding feature, or further issue 3 proof is required for release.

## Sources

- [Lua 5.1 lua_pcall: error handler before stack unwinding](https://www.lua.org/manual/5.1/manual.html#lua_pcall)
- [Lua 5.1 hook event model](https://www.lua.org/manual/5.1/manual.html#lua_sethook)
- [Facepunch: GM:OnLuaError](https://wiki.facepunch.com/gmod/GM:OnLuaError)
- [Inspected ILuaInterface slots](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-sys/src/vtables.rs)
- [Inspected protected-operation implementation](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-core/src/protected.rs)
