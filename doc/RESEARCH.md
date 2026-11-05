# Research and evidence

Research date: 2026-11-05. No GMod runtime probes, builds, or debugger implementation were performed for this research. Existing framework probe logs are evidence from that project, not new test results.

## 1. Inspected repositories

- gdebug: `/mnt/ssd/data/git/gdebug`; initial Cargo package `gdebug` 0.1.0, edition 2024, no dependencies, default example library.
- Initial rsgdll research: `/mnt/ssd/data/git/rsgdll`, commit **`e7edbecdf798acf566748e3a443f2d227bd703f3`** [R1].
- Follow-up local inspection: commit **`0a71f1bb2d6f267927135b04be6992465e4ea1ad`**, containing the issue 1/2 APIs and issue 3 feasibility report [R17, R18].
- Local rsgdll origin uses SSH. The requested Cargo dependency must instead use **`https://github.com/weBj8/rsgdll.git`**, with a pinned revision. Cargo finds the `rsgdll` workspace package; do not put `/crates/rsgdll` in the Git URL.
- SPEC.md now pins the follow-up revision. The missing debugger APIs are present; gdebug integration and runtime qualification remain implementation work.

## 2. Framework findings

The table records the **initial baseline** at [R1], retained as the rationale for the issue drafts. Its missing-API findings are superseded by the follow-up below; they are not claims about the current pin.

| Evidence | Finding | Consequence |
| --- | --- | --- |
| `crates/rsgdll/Cargo.toml`, `src/lib.rs` | Facade exports lifecycle macros, Lua types, hooks, timers, and async support; server/client/menu features exist | Reuse module/state lifecycle and protected Lua operations |
| `crates/rsgdll-core/src/state.rs`, `refs.rs`, `lua.rs` | Registry identity/generations, deferred unrefs, and owner-thread-only Lua wrappers | Scope debugger objects to a state generation; never send Lua handles to socket workers |
| `crates/rsgdll-sys/src/lua.rs` | No `lua_Debug`, `lua_Hook`, `lua_sethook`, `lua_gethook*`, `lua_getstack`, `lua_getinfo`, or `lua_getlocal` bindings | A native debugger needs an additional raw debug API; issue 1 |
| `crates/rsgdll-sys/src/vtables.rs` | Some ILuaInterface stack/info/local/upvalue slots already exist, with opaque pointers; `ErrorFromLua` is padding | Not equivalent to a typed Lua debug ABI or an error observer; do not say all introspection is absent |
| `docs/probe-linux64.txt` | Recorded Linux64 `lua_shared.so` exports debug hook and inspection symbols | Binding work looks feasible there; symbol presence is not ABI/callback safety proof for every target |
| `crates/rsgdll-core/src/protected.rs`, `lua.rs`, `trampoline.rs` | Protected-op interpreter, stack reserve, panic containment, and ordinary Lua C-function trampoline | Useful substrate, but no documented native `lua_Hook` adapter with live-frame access; issue 2 |
| `crates/rsgdll-core/src/rt/`, `docs/SPEC.md` §14 | Server/client runtime is pumped by Think; worker-thread `AddThreadedCall` probe hung Linux64 srcds | Paused debugger must run its own owner-thread command loop, not wait on `LuaHandle.call` |
| `crates/rsgdll-core/src/module.rs` | Open/close hooks and state teardown exist | Stop accepting work and remove debugger callbacks before framework state teardown |
| `docs/SPEC.md` header and §14; `.github/workflows/ci.yml` | Linux64 srcds recorded 50 PASS / 0 FAIL; other targets build-only. Actual srcds CI job is Linux64 | Four target builds are not four target runtime qualification; older aspirational matrix text is not evidence |
| `xtask/src/build.rs`, `docs/SPEC.md` §11 | GMod module suffixes: `linux`, `linux64`, `win32`, `win64`; extension always `.dll` | Build separate `gmsv_` and `gmcl_` artifacts for each target |

### Follow-up: upstream outcomes

At `0a71f1bb2d6f267927135b04be6992465e4ea1ad` [R17]:

- **Issue 1 resolved:** `rsgdll-sys/src/lua.rs` now defines the native debug types/constants and separate `LuaDebugApi`; `debug_api()` resolves it lazily without making non-debugger loading depend on those symbols. GMod's `LUA_IDSIZE` is recorded as 128, with per-pointer-width layout assertions; do not substitute stock LuaJIT's 60-byte layout.
- **Issue 2 resolved:** `rsgdll-core/src/debug.rs`, re-exported as `rsgdll::debug`, provides protected hook entry, panic containment, `Lua::frame` and live-frame inspection. Unit fixtures cover coroutine pause-loop commands, stack balance, Lua errors, panic containment, foreign-hook refusal, and state replacement; the showcase includes an in-engine live-frame probe. These fixtures were inspected, not rerun here.
- The safe hook installer refuses foreign hooks and does not restore a displaced engine hook. gdebug still owns the explicitly consented save/clear/restore transaction, watchdog/JIT policy, and independent owner-thread pause loop. Inspect frames through the supported hook contract, not by adding helper frames that change the stopped activation.
- **Issue 3 closed/out of scope:** `docs/uncaught-errors.md` [R18] reports no stable supported pre-unwind integration. Its Linux64 server probe found `OnLuaError` post-unwind and unable to preserve arbitrary error objects; private/version-specific engine detours would be required. The user accepted removing exception breakpoints rather than introducing those risks. This negative result is not a runtime test of the other seven combinations.

APIs being available removes the missing-framework blockers; it does not satisfy gdebug's G1/G2 integration and full runtime-matrix gates.

### Reuse boundary

Use rsgdll for loading, realm checks, live-state identity, references, protected operations, and lifecycle integration. Use its ordinary Think hook only for running-state work/bootstrap.

Do **not** ask rsgdll to implement DAP, JSONL, SSH, breakpoint policy, source maps, a network listener, or an agent CLI. Those belong to gdebug. Do not rely on its repository-local `cargo xtask` alias being installed by a Cargo Git dependency.

Reuse the now-available issue 1/2 APIs instead of duplicating their ABI definitions or hook panic/error boundary. The original issue drafts remain historical evidence. There is no pre-unwind observer to integrate, and v1 no longer requires one.

## 3. Lua/GMod constraints

### Hooks and LuaJIT

Facepunch [R2] explicitly discourages relying on `debug.sethook`, says it is used for infinite-loop detection, and warns that JIT-compiled code may skip hooks. Native `lua_sethook` uses the same underlying hook facility; it does not magically solve JIT visibility or watchdog coexistence.

Reliable mode should disable the JIT engine and flush compiled traces for the attached realm. Upstream LuaJIT documents separate `jit.off` and `jit.flush` behavior [R6]. Restore the previous enabled state on detach, but flushed traces cannot be restored: the VM must warm up again. Preserve function-level disable decisions; do not recursively force JIT on for all functions.

The wiki reports LuaJIT 2.0.4 for x32 and 2.1.0-beta3 for x64 [R3]. Treat these as documented observations, not a universal ABI promise. Record the actual GMod branch/build, `jit.version`, `jit.arch`, and `jit.os` during qualification. Current upstream LuaJIT source [R7] is an implementation reference, not the source of every shipped GMod VM. In particular, hook ownership/state sharing between coroutines must be probed; do not assume PUC-Lua's per-thread model or current LuaJIT behavior universally.

### Hook entry and evaluation

Lua 5.1 [R4] says hooks are disabled while a hook executes, including Lua called from inside the hook. Therefore putting a count hook around evaluation from a stopped hook does **not** supply a dependable timeout. Protected calls catch Lua errors; they do not interrupt infinite loops, blocking native calls, or unsafe native addons.

Lua uses nonlocal error propagation. A hook bridge must contain allocating/raising Lua operations within the framework's protected boundary and contain Rust panics separately. `catch_unwind` alone is not protection against all Lua error mechanisms. Stack capacity, original frame lookup, and borrowed pointer lifetime need explicit verification on x86 as well as x86-64.

### Exceptions

Lua debug events are call, return/tail-return, line, and instruction count—not an error event [R4]. A `lua_pcall` message handler runs while useful error-stack information is available; after `lua_pcall` returns, the stack has unwound. Memory errors do not invoke that normal handler.

A debugger only installing handlers around its **own** calls cannot catch arbitrary GMod timer/net/hook errors. Replacing global `error`/`assert` misses VM-generated errors. Replacing Lua `pcall`/`xpcall` changes script semantics and does not cover every engine call. `lua_atpanic` is not an ordinary runtime-error hook. Detouring exported `lua_error` does not prove interception of internal VM errors.

Facepunch's `GM:OnLuaError` provides an error, realm, and stack-trace table [R5]. That is useful for diagnostics, but its documentation does not promise a pre-unwind callback or inspectable live locals. The upstream follow-up [R18] confirms a post-unwind callback on Linux64 server and concludes that a stable rsgdll observer is not supportable. Exception breakpoints are therefore excluded by agreement; ordinary GMod reporting and protected debugger-operation errors remain required.

### GLua compilation and inspection

The framework records GLua syntax acceptance through `luaL_loadbuffer` on Linux64, not all targets. GMod `CompileString(code, identifier, false)` explicitly returns either a function or an error string [R8]. Use a verified GLua compilation path, capture compiler functions before user code can accidentally replace them, and preserve compilation errors rather than treating an error string as callable.

Lua debug info can be missing, stripped, tail-call-elided, or attached to dynamic chunks instead of files. Source text is not recoverable from a chunk label alone. Lua strings are arbitrary bytes, and table keys need not be strings. The protocol must not conflate nil with absent bindings or blindly call `tostring`/metamethods during variable display.

## 4. Existing debugger reference

`danielga/gm_rdb` [R9] is an existing GMod integration of LRDB. Its `source/basic_server.hpp` installs a pause handler that services socket messages in a loop while paused, and unpauses on connection close. This supports the proposed dedicated pause-loop architecture; it is not verification of a Rust implementation.

Its command set already covers line breakpoints, stack/locals/upvalues, evaluation, stepping, and console commands. The referenced LRDB revision's `include/lrdb/debugger.hpp` installs a call/return/line hook and has commented-out `error_break` state [R10]. A returned reason string named `exception` is not proof of functioning exception breakpoints.

Do not claim drop-in compatibility with its JSON protocol or copy its lifecycle/hook-replacement assumptions. This specification uses it as behavioral precedent, not as a code dependency.

## 5. Frontends

DAP is the common editor boundary [R11]. A single external adapter can translate CLI/core operations for both VS Code and Zed. DAP has framed messages, request/response correlation, events, explicit capabilities, per-source breakpoint replacement, function/exception breakpoints, stack/scopes/variables, and REPL evaluation.

VS Code still needs debugger registration, configuration schema, and launching of the adapter [R12]. Zed needs an adapter registered through a debugger extension with a configuration schema, `get_dap_binary`, and `dap_request_kind` returning attach [R13]. A random executable path in `.zed/debug.json` is not sufficient registration. Zed documents native conditional/hit-count/logpoint and exception-breakpoint UI support [R14]; gdebug must advertise only its own supported features, with no exception filters. Exact frontend behavior must still be smoke-tested against the chosen releases.

The Rust `dap` crate is an existing implementation worth evaluating for the external adapter [R15]; library selection/version is deferred until implementation. Do not invent a second editor-specific debugger core.

## 6. What remains unproven

1. gdebug integration of the available native debug ABI and protected hook entry on every target/realm.
2. Correct active-coroutine selection and frame identity across protected helper calls in gdebug.
3. Save/restore of the engine hook after a long pause, including possible watchdog timing debt.
4. JIT disable/flush coverage and restoration on GMod's shipped VMs.
5. Safe close/reload/map-change while attached or stopped, especially both realms sharing one engine thread.
6. Actual debugger overhead and editor end-to-end compatibility.

These are implementation/qualification gates, not unresolved product-scope choices. Failure of an active mandatory gate blocks a complete v1 claim; it does not silently remove a confirmed requirement. Pre-unwind interception/G3 is no longer a gate.

## Sources

- **R1:** [rsgdll inspected revision](https://github.com/weBj8/rsgdll/tree/e7edbecdf798acf566748e3a443f2d227bd703f3), including [framework spec](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/docs/SPEC.md), [raw Lua API](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/crates/rsgdll-sys/src/lua.rs), and [CI](https://github.com/weBj8/rsgdll/blob/e7edbecdf798acf566748e3a443f2d227bd703f3/.github/workflows/ci.yml).
- **R2:** [Facepunch: debug.sethook](https://wiki.facepunch.com/gmod/debug.sethook).
- **R3:** [Facepunch: jit.version](https://wiki.facepunch.com/gmod/jit.version).
- **R4:** [Lua 5.1 reference manual: debug interface](https://www.lua.org/manual/5.1/manual.html#3.8), [lua_pcall](https://www.lua.org/manual/5.1/manual.html#lua_pcall), [lua_Hook](https://www.lua.org/manual/5.1/manual.html#lua_Hook).
- **R5:** [Facepunch: GM:OnLuaError](https://wiki.facepunch.com/gmod/GM:OnLuaError).
- **R6:** [LuaJIT: jit module](https://luajit.org/ext_jit.html).
- **R7:** [LuaJIT v2.1 dispatch source](https://raw.githubusercontent.com/LuaJIT/LuaJIT/v2.1/src/lj_dispatch.c), [LuaJIT v2.1 lua.h](https://raw.githubusercontent.com/LuaJIT/LuaJIT/v2.1/src/lua.h).
- **R8:** [Facepunch: CompileString](https://wiki.facepunch.com/gmod/Global.CompileString), [debug.getinfo](https://wiki.facepunch.com/gmod/debug.getinfo), [debug.getlocal](https://wiki.facepunch.com/gmod/debug.getlocal), [RunConsoleCommand](https://wiki.facepunch.com/gmod/Global.RunConsoleCommand).
- **R9:** [gm_rdb](https://github.com/danielga/gm_rdb), [pause loop and commands](https://raw.githubusercontent.com/danielga/gm_rdb/master/source/basic_server.hpp).
- **R10:** [LRDB debugger used by gm_rdb](https://raw.githubusercontent.com/danielga/LRDB/7f4e0a2b8f5f456e943589afeba3b0e968817a98/include/lrdb/debugger.hpp).
- **R11:** [Debug Adapter Protocol specification](https://microsoft.github.io/debug-adapter-protocol/specification).
- **R12:** [VS Code debugger-extension guide](https://code.visualstudio.com/api/extension-guides/debugger-extension).
- **R13:** [Zed debugger extensions](https://zed.dev/docs/extensions/debugger-extensions).
- **R14:** [Zed debugger configuration and UI](https://zed.dev/docs/debugger).
- **R15:** [Rust dap crate documentation](https://docs.rs/dap/latest/dap/).
- **R16:** [Cargo: specifying Git dependencies](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#specifying-dependencies-from-git-repositories).
- **R17:** [rsgdll follow-up revision](https://github.com/weBj8/rsgdll/tree/0a71f1bb2d6f267927135b04be6992465e4ea1ad), [native debug bindings](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/crates/rsgdll-sys/src/lua.rs), [protected hook/frame contract](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/crates/rsgdll-core/src/debug.rs), and [hook integration fixtures](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/crates/rsgdll-core/tests/debug.rs).
- **R18:** [rsgdll uncaught-error feasibility result](https://github.com/weBj8/rsgdll/blob/0a71f1bb2d6f267927135b04be6992465e4ea1ad/docs/uncaught-errors.md).
