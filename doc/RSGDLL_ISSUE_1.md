# Native Lua debug API bindings and per-target ABI probes

**Status: resolved upstream.** Revision `0a71f1bb2d6f267927135b04be6992465e4ea1ad` provides the separate, lazily resolved `LuaDebugApi`, native debug types/constants, and per-pointer-width layout assertions. See the [updated bindings](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/crates/rsgdll-sys/src/lua.rs). This was confirmed by source inspection, not a new runtime test. Full gdebug target/realm qualification remains G1.

The original request below is retained for traceability, not an open missing-API blocker.

**Type:** feature request / ABI qualification  
**Inspected revision:** `e7edbecdf798acf566748e3a443f2d227bd703f3`  
**Affected areas:** `rsgdll-sys`, symbol probes, native integration tests  
**Original priority:** prerequisite for a supported native debugger bridge

## Use case

A downstream GMod Lua attach debugger needs native line/call/return/count hooks, stack frames, locals, upvalues, and hook save/restore. It targets Linux/Windows × x86/x86-64, server and client realms. A Lua-level `debug.sethook` wrapper is not the intended ABI integration and does not solve JIT/watchdog constraints.

## Baseline evidence

- `crates/rsgdll-sys/src/lua.rs` exposes a runtime-resolved `LuaApi`, but has no `lua_Debug`/`lua_Hook` types, hook constants, or raw hook/stack/local/upvalue API bindings.
- `crates/rsgdll-sys/src/vtables.rs` already includes ILuaInterface `get_stack`, `get_info`, `get_local`, and `get_upvalue` slots, with opaque debug pointers. This request does not claim those slots are missing.
- `docs/probe-linux64.txt` records exported native debug symbols in Linux64 `lua_shared.so`. It does not establish the callback ABI or four-platform behavior.
- `LuaApi::load` currently requires every listed symbol. Adding debugger-only requirements there unconditionally could break otherwise-working non-debugger consumers.

## Requested minimum

1. Expose ABI-correct debug types/constants and dynamically resolved functions from the **already loaded host VM**, without linking a second Lua VM:
   - `lua_sethook`, `lua_gethook`, `lua_gethookmask`, `lua_gethookcount`;
   - `lua_getstack`, `lua_getinfo`;
   - `lua_getlocal`, `lua_setlocal`, `lua_getupvalue`, `lua_setupvalue`;
   - `lua_Debug`, nullable `lua_Hook`, call/return/line/count/tail-return events and masks as actually supported by the shipped LuaJIT ABI.
2. Derive layouts/signatures from the applicable GMod/LuaJIT headers and runtime probes. Do not import Lua 5.4 layouts or assume x86-64 offsets on x86. Document string/pointer lifetimes and private activation fields.
3. Follow the framework's verified FFI/unwind conventions, including Windows x86 calling convention. Binding a callback does not itself make Rust hook code safe.
4. Preserve ordinary module loading when debugger symbols are unavailable: use an opt-in/lazy extension table or an equally small capability-gated mechanism. Return actionable missing-symbol/ABI diagnostics rather than weakening existing guarantees.
5. Document hook-domain behavior: VM-wide versus per-thread state, which `lua_State*` a coroutine hook receives, and how hooks apply to existing/new coroutines. Save/restore pointer, mask, and configured count; identify any unobservable countdown state.

No public high-level debugger class, hook multiplexer, breakpoint engine, or DAP support is requested. Prefer extending the current loader/types/probe patterns.

## Acceptance / preservation proof

- Build all four targets and run a tiny native hook fixture on each, in server and client realms. Record GMod branch/build and LuaJIT version.
- Receive line/call/return/count events; validate source/currentline/function identity, stack frames, argument/local/upvalue values, local/upvalue writes, nil values, and nested calls.
- Test coroutines created before/after installation. Ensure inspection addresses the active coroutine, not the main state's stack.
- Install/remove repeatedly, save/restore a sentinel hook with its mask/count, and test a pre-existing engine hook with explicit takeover consent.
- Verify the function pointer/null representation and struct size/alignment against the appropriate native headers; use runtime behavior to supplement, not replace, ABI checks.
- Missing debugger symbols must fail the debugger capability cleanly while existing non-debugger module tests continue to pass.
- Stock LuaJIT tests are useful but do not replace GMod runtime qualification.

## Boundaries / fallback

Downstream can resolve symbols through existing raw-loader access, so this is a request to centralize supported ABI definitions and avoid duplicate unsafe bindings. Protected hook-entry semantics are tracked separately in issue 2. JIT policy, consent to replace engine hooks, and network/pause-loop logic remain downstream responsibilities.

## Sources

- [Inspected raw Lua bindings](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-sys/src/lua.rs)
- [Existing ILuaInterface slots](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-sys/src/vtables.rs)
- [Recorded Linux64 probe](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/docs/probe-linux64.txt)
- [Lua 5.1 debug interface](https://www.lua.org/manual/5.1/manual.html#3.8)
- [LuaJIT lua.h reference, not proof of GMod's exact version](https://raw.githubusercontent.com/LuaJIT/LuaJIT/v2.1/src/lua.h)
- [Facepunch hook/JIT/watchdog warning](https://wiki.facepunch.com/gmod/debug.sethook)
