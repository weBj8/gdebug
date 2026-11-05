# Document/support protected native-hook entry and live-frame inspection

**Status: resolved upstream.** Revision `0a71f1bb2d6f267927135b04be6992465e4ea1ad` supplies [`rsgdll::debug` hook/frame APIs and their safety contract](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/crates/rsgdll-core/src/debug.rs), with pause-loop/error/panic/lifecycle fixtures. These were inspected, not rerun here. The installer refuses foreign hooks; consented takeover/restoration and full gdebug integration proof remain downstream G1/G2 work.

The original request below is retained for traceability, not an open missing-API blocker.

**Type:** integration/safety contract request  
**Inspected revision:** `e7edbecdf798acf566748e3a443f2d227bd703f3`  
**Affected areas:** `rsgdll-core` hook entry, protected operations, stack/refs/lifecycle  
**Original priority:** required safety proof before downstream Rust code pauses inside a Lua hook

## Use case

A native debugger must block in a `lua_Hook` callback and service inspection/evaluation commands on the Lua owner thread until continue/detach. Think cannot run while that thread is stopped, so scheduling work through `LuaHandle.call` cannot service this loop.

We need a supported way to use rsgdll's protected operations from that callback without inspecting helper frames by mistake or allowing a Lua nonlocal error/Rust panic to escape unsafely.

## Existing building blocks and exact gap

- `Lua::from_raw` is public and requires a live owner-thread state. A raw downstream bridge is possible.
- `Lua::protect`/`run` use a fixed interpreter under `lua_pcall`; `Op::Raw` already provides an unsafe callback extension point with strict no-Drop/no-panic rules.
- `trampoline::call` is for ordinary Lua C-function exports returning a result count. A debug hook has a different callback signature and cannot simply reuse its return/error behavior.
- `Lua::protect` performs setup pushes before entering `lua_pcall`. The existing reserved-stack guarantee must be demonstrated for hook entry and arbitrary stopped stack depth, not merely assumed.
- Protected helper calls add stack frames. Inspecting level 0/1 from inside those helpers can target the debugger rather than the originally stopped Lua activation.
- No documented native-hook example/API establishes these invariants at the inspected revision.

This is **not** an allegation that `Op::Raw` or `Lua::from_raw` is inherently broken. A documented, tested recipe may fully satisfy the request; add new API only where necessary.

## Requested minimum contract

1. Enter from the actual hook-provided `lua_State*`, validate live runtime/generation on its owner thread, and establish the stack-reserve guarantee before any unchecked setup pushes.
2. Provide a narrow hook-entry pattern/helper that contains Rust panics, keeps Lua-raising operations under protection, restores stack top on success/error, and reports debugger failure without raising an unrelated application error.
3. Explain how to capture and re-resolve the original stopped activation while protected callbacks add temporary frames. Cover `lua_getinfo` options that push results, local/upvalue reads/writes, function references, and frame invalidation.
4. Use fixed protected operations or a minimal documented `Op::Raw` recipe for allocating/pushing debug operations. Do not permit foreign longjmp through Rust-owned Drop state. `catch_unwind` is not a substitute for Lua error protection.
5. Define callback/reference lifetimes: copied owned metadata may leave the hook, but stack indices, borrowed strings, raw activation pointers, and `Lua` may not cross threads or survive resume/close.
6. Define lifecycle ordering so a downstream module can remove its hook and drain/drop debugger references before framework state teardown. No general-purpose pause manager is required.

The callback may run a bounded-work command loop and protected user evaluation. It must not reenter the normal Think/runtime pump, hold borrow guards across reentrant Lua calls, or claim evaluation is safely interruptible by another count hook.

## Acceptance fixture

A small example/test is enough; no full debugger is needed:

- Hook a nested Lua function with distinctive locals/upvalues; enter a test pause loop and accept read/continue requests through an ordinary Rust channel.
- Prove Think is not needed to service that loop and no worker invokes a Lua API.
- Add protected helper frames and still read/write the intended original activation, including shadowed/nil bindings.
- Repeat from main state and pre-existing/new coroutine contexts; validate registry identity/generation reuse.
- Verify identical stack top before/after successful calls, compile/runtime errors, debug-inspection failure, and contained Rust panic. Force stack pressure/allocation failures in a controlled fixture where supported.
- Ensure a failed inspection does not corrupt the user stack or change the application's error path.
- Continue/detach/close without stale hook callbacks or deferred unrefs into a destroyed state. Reattach to a replacement state and reject old handles.
- Run on Linux/Windows x86/x86-64, server and client; record which combinations are proven versus merely compiled.
- Existing function-export trampoline and protected-operation tests must remain green.

## Out of scope

No socket server, DAP, condition parser, breakpoint storage, engine tick pumping, or requirement to fix worker-thread `AddThreadedCall`. The downstream debugger owns its command loop. The existing Linux64 `AddThreadedCall` failure makes that worker-to-engine route unsuitable as a presumed workaround.

Raw debug symbol/layout support is issue 1 (resolved); observing engine-owned uncaught errors before unwind was issue 3 (closed, feature removed from v1).

## Sources

- [Lua owner-thread/protected entry code](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-core/src/lua.rs)
- [Protected operations and Op::Raw](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-core/src/protected.rs)
- [Ordinary exported-function trampoline](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-core/src/trampoline.rs)
- [Framework spec and runtime-pump verification](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/docs/SPEC.md)
- [Lua hook behavior and nested hook suppression](https://www.lua.org/manual/5.1/manual.html#lua_Hook)
