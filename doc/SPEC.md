# gdebug v1 specification

**Status:** finalized v1 implementation specification; runtime qualification pending. **Date:** 2026-11-05.

This document specifies future work only. It does not authorize implementation in this task. Confirmed choices are summarized in [README.md](README.md); evidence and qualifications are in [RESEARCH.md](RESEARCH.md).

“MUST” defines a v1 acceptance requirement. An unproven requirement remains a release blocker, not an advertised capability.

## 1. Product and scope

Build an attach debugger for Garry's Mod Lua using rsgdll. One native debugger core serves a standalone agent-oriented CLI and one external Debug Adapter Protocol (DAP) adapter shared by VS Code and Zed.

Required operations: **attach, break, clear, pause, stack, scopes, variables, continue, step in, step over, step out, evaluate, command, status, detach**.

Required advanced features: conditional breakpoints, hit-count breakpoints, logpoints, and function breakpoints. Exception breakpoints (automatic stops on uncaught runtime Lua errors) are explicitly out of scope.

### 1.1 Target matrix

| GMod platform | Rust native-module target | Server artifact | Client artifact |
| --- | --- | --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | `gmsv_gdebug_linux64.dll` | `gmcl_gdebug_linux64.dll` |
| Linux x86 | `i686-unknown-linux-gnu` | `gmsv_gdebug_linux.dll` | `gmcl_gdebug_linux.dll` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `gmsv_gdebug_win64.dll` | `gmcl_gdebug_win64.dll` |
| Windows x86 | `i686-pc-windows-msvc` | `gmsv_gdebug_win32.dll` | `gmcl_gdebug_win32.dll` |

All eight module/realm combinations require runtime qualification. Test dedicated servers and clients connected to a server, plus a listen server where both realms share a process. Process bitness determines the native artifact, not OS bitness. The CLI/adapter talks over a portable wire protocol and need not match target bitness. An editor's own supported host architectures are separate from these target requirements; a 64-bit editor can debug a 32-bit target.

Menu realm, macOS native modules, process injection, native/C++ debugging, launch/restart of GMod, reverse execution, data watchpoints, disassembly, and simultaneous controllers of one realm are out of scope. No daemon/broker, automatic SSH manager, or alternative restricted-expression interpreter is required.

### 1.2 Deployment and trust

- Install the appropriate binary under `garrysmod/lua/bin/` and load it with `require("gdebug")` before attaching. Loading in one realm does not load the other. Loading client native modules requires explicit local installation/permission; server-delivered Lua is not an installation mechanism.
- A server debugger controls that server's Lua; a client debugger controls only that client's Lua. No player identity confers debugger access, and no net-message bridge is provided.
- Attach can only stop future execution. To debug early startup, load the module before the relevant code and arrange a later execution; no retrospective capture is promised.
- Debugger authorization grants code-execution power within the target account. Use trusted development targets. Lua evaluation can mutate the game, access whatever the target can access, call native addons, or hang/crash the process.
- A breakpoint stops the owning engine thread. Rendering, networking, Think, and timers can stall; another realm in the same listen-server process may stall too. Separate clients may disconnect and engine timeouts may fire. No attempt is made to keep gameplay alive by recursively pumping the engine.

## 2. Framework dependency and build contract

The future native-module crate MUST use an HTTPS Git dependency, not the neighboring checkout or an SSH URL. Example manifest fragment, **not a change to this repository's Cargo.toml**:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
rsgdll = { git = "https://github.com/weBj8/rsgdll.git", rev = "0a71f1bb2d6f267927135b04be6992465e4ea1ad", default-features = false }

[features]
server = ["rsgdll/server"]
client = ["rsgdll/client"]
```

This inspected revision provides the separate `LuaDebugApi` bindings and `rsgdll::debug` protected hook/frame API addressing issues 1 and 2. Use it as the implementation baseline, not as proof that gdebug passes its runtime gates. Qualify the pinned revision before release and commit Cargo.lock; any later pin change must be retested. Keep the Git source and revision consistent for any direct rsgdll-family dependencies. Do not combine different revisions of the facade/core/sys crates.

Build server and client variants separately, matching rsgdll's realm feature conventions; do not enable both to produce a universal module. Follow its `panic = "unwind"` requirement and validated toolchain/ABI. Cargo's Git dependency does not install rsgdll's repository-local xtask. gdebug must supply its own minimal build/rename/install instructions when implemented.

Keep editor/CLI code outside the loaded game module. Share protocol types and pure breakpoint/path rules rather than implementing those rules twice. Exact package layout is an implementation choice; no plugin framework is needed.

## 3. Architecture and ownership

```text
agent -- JSONL stdin/stdout --> gdebug attach ----+
                                                |
VS Code -- DAP stdio --> gdebug dap --------------+-- loopback TCP / SSH forward
Zed ----- DAP stdio --> gdebug dap --------------+          |
                                                           v
                                                 native realm endpoint
                                                 socket worker / queues
                                                           |
                                                 engine-owner-thread core
                                                 rsgdll + debug-hook bridge
```

### 3.1 Responsibilities

| Component | Responsibility |
| --- | --- |
| Native module | State lifecycle, hooks/JIT policy, pause loop, actual stack/variables/evaluation, breakpoint matching, authenticated endpoint |
| Socket worker | Framing, authentication, bounded queues, heartbeats, cached status; no Lua or engine API access |
| CLI | Persistent session, JSONL validation, request correlation, automatic heartbeats, stderr diagnostics |
| DAP adapter | DAP framing/lifecycle/capabilities, ID translation, source maps, operation translation; same core semantics as CLI |
| Editor extensions | Register and start `gdebug dap`, validate attach configuration, expose installation/path selection; no debugger logic |

One realm endpoint has at most one attached controller. A second authenticated attach receives `ERR_BUSY`; it does not steal the session. Use separate sessions for server and client, even in one process. Non-controlling connections may authenticate and read cached status, but cannot inspect live Lua or mutate the attached session.

### 3.2 Threading and pause loop

1. Network threads receive/validate owned bytes and enqueue typed requests. They never hold `Lua`, `LuaRef`, a stack index, `lua_Debug`, or borrowed Lua memory.
2. While running, bootstrap/ordinary engine-owner-thread callbacks service control work. Hook checkpoints additionally observe pause, detach, and breakpoint updates so pure Lua loops remain interruptible once instrumentation is installed.
3. At a stop, the native hook retains the actual executing state/coroutine on the owner thread and enters a debugger-only command loop. It services inspection, evaluation, breakpoint edits, continuation, and detach directly.
4. That loop waits on a channel/condition variable with a deadline; it does not spin. Socket I/O continues independently. The loop does **not** call `LuaHandle.call` and wait for Think, tick the general async executor, invoke arbitrary Think hooks, or pump the game loop.
5. Runtime status snapshots are updated by the owner. Cached network status includes a freshness timestamp; no worker invents a live stack snapshot.
6. No mutex/RefCell borrow needed by another thread or a reentrant Lua callback is held while entering Lua. Contain debugger panics and Lua errors separately. Reuse `rsgdll::debug`'s documented hook-entry/frame contract; allocating/raising operations must remain within its supported protected-call paths.

Attaching before instrumentation is installed still needs an engine safe point. If the game is already stuck in native code or an uninstrumented loop, attach/pause cannot promise to interrupt it. An attached count hook can catch future interpreted tight loops; it cannot preempt a blocked C/engine call.

### 3.3 State and handle lifetime

Logical states: `idle`, `attaching`, `running`, `pause_pending`, `stopped`, `detaching`, `closed`. A separate `configured` flag controls whether newly attached breakpoints are armed.

- `idle` means module/listener loaded without a controller. `closed` means the Lua state is gone.
- A successful attach identifies `sessionId`, `targetGeneration`, realm, process ID, architecture, GMod/VM versions, and negotiated capabilities.
- Every stop allocates a monotonically changing `stopId`. Stack, frame, scope, variable, and function references belong to that stop and state generation.
- Mutating evaluation/GLua commands invalidate cached variable views using a separate `valueRevision`; they do not silently retarget frame handles. Even an expression may mutate, so every executed evaluation invalidates prior variable snapshots.
- Continue, step, detach, state close, and state replacement invalidate stop-scoped handles. Never identify a state/coroutine only by a reusable raw pointer.
- Return `ERR_STALE_HANDLE` for expired handles. Adapters must not map an old editor integer ID onto a new frame by accident.
- Release Lua registry references on the owner thread before state teardown. Delayed network requests from an old generation fail instead of touching the new state.

## 4. Hook and execution policy

### 4.1 Hook ownership

Attachment requires explicit `allowHookReplacement: true` (CLI `--allow-hook-replacement`). Consent is never inferred from possession of a token or silently supplied by an editor extension.

At attach, inspect and save the prior native hook pointer, mask, and configured count, then install the debugger hook. `rsgdll::debug::set_hook` deliberately refuses a foreign hook: after explicit consent, gdebug must save/clear that slot through `LuaDebugApi` before installing the supported callback, with rollback on failure. Hook takeover/restoration belongs to gdebug, not an implicit framework guarantee. Warn prominently that this may disable GMod's infinite-loop watchdog and other profilers. Do not try to chain an unknown hook in v1: its frame assumptions, error propagation, count cadence, and pause-time accounting are not known.

Maintain call, return, line, and count events as needed for breakpoints, stepping, and control responsiveness. A proposed initial count interval is 1,000 VM instructions, subject to measured tuning; it is not a hard time bound.

On detach or attach rollback, restore the prior hook only if gdebug still owns the current hook. If another component replaced it, do not overwrite that component's hook: report `hookOwnershipLost`, cancel stepping, and detach at an owner-thread opportunity. Periodic running-state checks detect replacement, but detection is not immediate inside code that has removed the debugger hook.

Probe whether hooks are global to the VM or per coroutine on each supported GMod VM. Cover coroutines existing before attach and created afterward. Restore each modified hook domain exactly once. Configured mask/count can be saved; an in-progress instruction countdown may not be recoverable. Safe resumption of GMod's watchdog after long pauses is a mandatory gate, not an assumed consequence of restoring the pointer.

### 4.2 JIT

Reliable mode is the only v1 execution mode: record JIT enabled state, disable the JIT engine, and flush existing traces for the attached realm. Apply this on the owner thread before arming breakpoints. Return clear failure/rollback if the verified operation is unavailable.

On detach, restore the prior enabled/disabled engine state without recursively enabling individually disabled functions. Flushed traces are not recoverable and will recompile normally. No claim of zero performance impact or exact restoration of trace caches. Script code that changes hooks/JIT settings while debugging can invalidate the session; detect and report this where possible.

### 4.3 Stops, coroutines, and stepping

- Stops occur at hook-safe execution points, not arbitrary machine instructions. A line stop is before that line's next execution, subject to the VM's line table.
- Identify the active coroutine from the hook argument, not from a cached main-state pointer. Keep registry-backed coroutine identity while exposed; avoid keeping every historical coroutine alive forever.
- Required inspection is the current stopped coroutine's stack. DAP `threads` reports the known execution contexts and marks the current one; inspecting an inactive/suspended coroutine may return `ERR_UNSUPPORTED_CONTEXT` in v1. Do not fabricate complete enumeration of all pre-existing coroutines.
- The whole realm's execution is stopped; `allThreadsStopped` is true within that debug session. Cross-realm stalling in a shared process is a side effect, not coordinated multi-target suspension.
- `step in`: next executable line event in the selected coroutine, including a callee.
- `step over`: next executable line event at the same logical activation or after it returns; ignore deeper calls in that coroutine unless an explicit breakpoint or pause wins.
- `step out`: next executable line in the surviving caller after the selected activation returns. At a top-level return/coroutine completion, stop at the next available owner-thread Lua checkpoint and explain that the original context completed.
- Track activation identity and actual stack relationships; a naive increment/decrement depth counter is insufficient for recursion, tail calls, errors, and coroutine switches. A new execution of the same source line can satisfy a step; do not wait exclusively for a different line number.
- A step follows its selected coroutine across yield/resume, but does not resume a suspended coroutine itself. Other coroutines may run. Their explicit breakpoints can interrupt the step, which is then canceled. No single-thread execution guarantee is advertised.
- C/native frames may be displayed but are not stepped at machine-instruction granularity. Missing/tail-elided frames are identified, never reconstructed with invented locals.
- At one checkpoint, process cancellation/detach first, then explicit pause, breakpoint, and step reasons in that order. Report all matching breakpoint IDs when several match. Cancel any pending step on a stop.

## 5. Endpoint and wire protocol

### 5.1 Listener and discovery

Proposed minimal provisioning: before starting GMod, set `GDEBUG_SERVER_ENDPOINT_FILE` and/or `GDEBUG_CLIENT_ENDPOINT_FILE` to an owner-controlled file path. When the corresponding realm loads the module, it binds **`127.0.0.1:0`**, generates a fresh 256-bit OS-random token, and atomically writes a descriptor containing protocol version, endpoint, realm, PID, generation, and token. With no configured path, the module remains inert and reports how to enable it. No wildcard/non-loopback binding is supported in v1.

Descriptor files must be private (POSIX 0600, equivalent Windows owner-only DACL), created without following an attacker-controlled symlink/reparse point, in a trusted directory. Reject insecure configuration rather than falling back to an unauthenticated listener. On close, remove only the descriptor still owned by that generation. Never print its token in logs, status, DAP traces, or error messages.

The client reads `endpointFile`; an optional `endpoint` override replaces only host/port for an SSH tunnel. The override must also resolve to loopback. For remote use, the user creates an SSH local forward to the target's loopback port and securely provides the descriptor/token locally. SSH provides transport protection; gdebug does not implement SSH, TLS, or public-network authentication. Treat endpoint files as secrets, not checked-in project files.

Use a fresh token when the listener/state is recreated. Authenticate before returning target details or accepting debugger work. Bound failed-auth attempts, connection count, and handshake duration. Token comparison must avoid content-dependent early exit.

### 5.2 Framing and messages

Internal protocol `gdebug/1`: one UTF-8 JSON object per newline over TCP; JSON strings escape embedded newlines. It is not DAP and is not gm_rdb's protocol. The CLI uses the same request vocabulary/envelopes over stdin/stdout.

Request shape:

```json
{"id":1,"op":"stack","args":{"stopId":"stop-7","start":0,"count":20}}
```

Successful response shape:

```json
{"type":"response","id":1,"ok":true,"result":{"frames":[],"totalFrames":0}}
```

Failure shape:

```json
{"type":"response","id":1,"ok":false,"error":{"code":"ERR_NOT_STOPPED","message":"stack requires a stopped target","retryable":false}}
```

Event shape:

```json
{"type":"event","seq":12,"event":"stopped","sessionId":"session-a","targetGeneration":"generation-a","data":{"stopId":"stop-7","reason":"breakpoint","threadId":"thread-1","breakpointIds":[3]}}
```

IDs are unique positive request integers within a connection; CLI-generated attachment uses reserved ID 0. Responses correlate only by ID, not arrival order. Events have monotonically increasing connection-local sequence numbers. Stop-scoped requests carry `stopId`; session/generation are also bound to the authenticated connection. Opaque IDs are strings internally; numeric IDs exposed to DAP must fit its bounds.

Initial `hello` negotiates protocol version and authenticates using the token; unsupported major versions fail before state changes. It returns metadata/capabilities/limits. Only `hello` is accepted before authentication. The CLI/adapter performs it privately and never echoes the token. Unknown fields may be ignored for forward-compatible additions; unknown operations, bad types, ambiguous selectors, or invalid ranges are errors.

Events include `stopped`, `continued`, `breakpointChanged`, `output`, `invalidated`, `commandResult`, `warning`, and `terminated`. Ordinary command failures are responses, not process termination. Critical control events must not be silently dropped.

### 5.3 Ordering, bounds, and liveness

- Serialize VM-affecting work per session; workers may answer cached `status` and heartbeat independently. Apply request effects once and emit their responses before a causally resulting stopped/continued event for that request.
- Complete every accepted request once, or explicitly fail it on close. No automatic replay of evaluate/command after connection loss: effects may already have occurred. A timed-out caller must not infer rollback.
- Proposed fixed initial bounds: 1 MiB JSON message, 64 KiB expression/chunk, 64 queued VM requests, 4 MiB outbound queue, 1,024 breakpoints, 500 variables per page, and 4 KiB inline string previews. Return negotiated limits in `hello`; reject overflow with `ERR_LIMIT`/`ERR_BUSY`.
- Set a 5-second authentication deadline and bound incomplete-frame reads. CLI/adapter workers send a heartbeat every 2 seconds; after 10 seconds without valid owner traffic, the target schedules detach/resume. Live paused sessions have no automatic pause-duration limit.
- Disconnect, stdin EOF, cancellation of attach, or expired lease must cancel queued work, remove debugger breakpoints, and resume/detach at the next safe owner-thread opportunity. Priority detach/pause flags must not be trapped behind a full normal queue.
- Keep enough queue capacity for control messages. Optional diagnostic/logpoint output may be dropped/coalesced under pressure with an explicit dropped-count notice. If control output cannot be delivered, terminate the session and schedule cleanup rather than blocking the game indefinitely on a slow reader.
- A worker cannot force a Lua evaluation or native call to return. Lease expiry, request deadlines, and Ctrl-C are best-effort recovery only while the owner can service control. No thread kill, unsafe stack unwind, or forced process termination is performed.

Stable error codes include `ERR_AUTH`, `ERR_VERSION`, `ERR_BUSY`, `ERR_INVALID_ARGUMENT`, `ERR_NOT_ATTACHED`, `ERR_NOT_STOPPED`, `ERR_STALE_HANDLE`, `ERR_UNSUPPORTED_TARGET`, `ERR_UNSUPPORTED_CONTEXT`, `ERR_LIMIT`, `ERR_COMPILE`, `ERR_EVALUATION`, `ERR_HOOK_CONFLICT`, `ERR_TARGET_CLOSED`, and `ERR_TIMEOUT`. Include actionable text and retryability without leaking secrets.

## 6. Operation contract

`A` below means authenticated connection; `R` attached/running; `S` attached/stopped. `pause_pending` behaves like running until a stop actually occurs. All operations return structured success/error results.

| Operation | Valid state | Contract |
| --- | --- | --- |
| `attach` | A, no controller | Claim realm; validate target and explicit hook consent; install instrumentation/JIT policy transactionally; return session/capabilities. Optional `stopOnAttach` requests a subsequent safe stop, not instant suspension |
| `break` | R/S | Add or update one source/function breakpoint, condition, hit rule, or logpoint; return stable ID, verification state, location, message |
| `clear` | R/S | Remove breakpoint by ID, all for an exact source, or all session breakpoints; exactly one selector required; unknown ID is an idempotent no-op |
| `pause` | R/S | In R, set pause request and return `pending`; send `stopped` only on an actual checkpoint. In S, return current stop without another event |
| `stack` | S | Return current coroutine frames with pagination, IDs, source and line, names, frame kind, and availability flags |
| `scopes` | S | Return a frame's locals/arguments, upvalues, environment/globals, and applicable varargs; named expandable references and expensive flags |
| `variables` | S | Expand a scope/value reference with pagination and bounded previews; preserve keys/types/nil and cycle identity |
| `continue` | S | Invalidate stop handles, cancel step, leave pause loop; response then `continued` event |
| `step` | S | `kind` is `in`, `over`, or `out`; arm the defined stepping state and continue |
| `evaluate` | S | Compile an expression/expression list in selected frame context, return typed results/references or Lua error; no implicit statement fallback |
| `command` | R/S | Explicit `kind: "glua"` or `kind: "console"`; semantics below |
| `status` | A | Cached target/session state, freshness, pending action, versions, hook/JIT ownership, limits, breakpoint summary, and warnings; never run Lua to answer |
| `detach` | A/R/S | Idempotent session cleanup, clear breakpoints, cancel queued commands/step, release references, restore owned instrumentation, resume if stopped; leave module loaded |

Supporting operations are required where needed: `threads`, `source`, atomic `setBreakpoints` (replace source list), atomic `setFunctionBreakpoints`, `configurationDone`, and transport heartbeat. `break`/`clear` cover ordinary CLI use; the replace operations avoid incorrect DAP clear-then-add windows. No core exception-breakpoint or exception-info operation is required.

`break.args` has exactly one selector: `source` plus `line`, or `function: {"name": "MyAddon.Tick"}`, or `function: {"source": "lua/myaddon/test.lua", "linedefined": 12}`. Optional `id` updates an existing breakpoint; `enabled` defaults true. `clear.args` selects `id`, `source`, or `all: true`.

Detach is not unload, process termination, map restart, or destruction of another realm. If cleanup is pending because the owner is blocked, return/report that fact rather than falsely claiming restoration.

### 6.1 Attachment transaction

Authenticate and reserve ownership first. On the owner thread: validate live generation/realm/ABI, record hook/JIT state, establish protected debugger context, install instrumentation, and commit the session. On any failure, roll back only this attempt's changes and release ownership. Never leave a partly installed hook or permanently disabled JIT after a failed attach.

For CLI use, attach auto-configures the session before returning ID 0. For DAP use, attach establishes a configuring session; breakpoints are staged and atomically armed by `configurationDone`. GMod may continue executing during initial configuration; this is an attach debugger and makes no promise to capture lines already executed. `stopOnAttach` takes effect after configuration is complete. Continue reading DAP input while attach/configuration is pending—do not deadlock waiting for `configurationDone` in the request-reading loop.

## 7. Breakpoints

### 7.1 Source identity and line binding

Represent a source by a runtime source ID and raw Lua source label, plus normalized virtual path when available. Strip the leading `@` only for file-label interpretation; retain the raw label for diagnostics. Normalize separators/dot segments without silently collapsing distinct mounts or changing case according to the editor host OS. Apply explicit longest-prefix, segment-boundary source mappings between local paths and target virtual paths.

Example mapping: local `/work/myaddon/lua/` to runtime `lua/`. Runtime paths may also name addons, mounted content, or generated chunks. Do not assume all code is a real OS file or use basename-only matching. Ambiguous matches stay unverified with candidates/reason. Windows drive paths and Linux case sensitivity must be tested independently.

Lines are 1-based internally. Use available line tables (`lua_getinfo`'s valid-lines information where supported) and observed hook lines to verify executable locations. A not-yet-seen function/source can remain pending. Do not claim a line is nonexistent merely because one observed function in that file does not contain it. Do not silently move a breakpoint to an unrelated line; unsupported/non-executable positions remain unverified with a reason.

A breakpoint has `id`, `enabled`, selector, optional `condition`, optional `hitCondition`, optional `logMessage`, current hit count, verification/message, and source generation. Reloaded code invalidates prior line verification and updates breakpoint events; source-path breakpoints remain pending for the new code. Unchanged entries in an atomic replace retain identity/count; changed selectors/predicates reset their count. Clearing/re-adding starts at zero.

### 7.2 Conditions, counts, and logs

At each matching location: increment its raw encounter count, apply hit predicate, evaluate condition if present, then log or stop. Lua truthiness applies: only nil/false are false. Evaluations run in the stopped/candidate frame context and can have side effects. Do not execute them on a network thread.

Hit grammar: a positive integer `N` or `== N` means exactly the Nth encounter; `>= N` means N and later; `% N` means every Nth encounter. Reject zero, negative values, overflow, and unsupported syntax. Counts are per breakpoint/session across coroutines in that realm, including encounters failing the condition. Changing enabled state alone does not reset counts.

A logpoint has `logMessage` and does not stop on a successful match. Support literal text plus `{GLua expression}` interpolation, escaped literal braces `{{`/`}}`, and correct quoted-string/long-string/nested-brace handling. Do not use a naive regex that terminates an expression at a brace inside a string. Render values with bounded safe previews and emit `output` tagged with source/line/breakpoint ID. Do not replace global `print` solely to implement logpoints.

Compile invalid conditions/log expressions at installation when possible and report an unverified breakpoint. A runtime condition/interpolation error disables that breakpoint for the session, emits one actionable diagnostic and `breakpointChanged`, and does not propagate into gameplay. Explicit user editing/re-enabling retries it. A hung condition is not recoverable through a promised timeout.

### 7.3 Function breakpoints

Required selectors:

1. A dotted global/table symbol such as `MyAddon.Tick` or `GAMEMODE.Think`, resolved by raw table lookup without invoking `__index`. A colon spelling may normalize to a dot; it does not call a method or synthesize `self`.
2. A source-definition selector `(source, linedefined)` for local/anonymous Lua functions, matching closures from that definition. Multiple closures can match; document this instead of treating a displayed function name as unique.

DAP `FunctionBreakpoint.name` accepts the dotted form or `@<runtime-source-path>:<definition-line>`. CLI uses a typed selector. Name text from `debug.getinfo` alone is not a stable function identity.

Use call events and function identity/definition metadata to detect entry, then stop at the callee's first inspectable executable line so arguments/locals have meaningful state. Rebinding a global during reload must not permanently bind the old closure: recheck relevant raw symbol resolution at call checkpoints. Unknown symbols remain pending. C/native functions without a Lua entry line are unverified/unsupported, not native-code breakpoints. Conditions and counts have the same semantics as line breakpoints.

### 7.4 Error handling — no exception breakpoints

V1 does not stop automatically on caught or uncaught Lua errors. The user removed this feature because rsgdll cannot guarantee a stable, safe pre-unwind engine interface; [issue 3](RSGDLL_ISSUE_3.md) records the negative feasibility outcome. Gate G3 is retired, not deferred as a release blocker.

Preserve ordinary GMod error handling and reporting. Do not detour private engine error handlers or replace `error`, `assert`, `pcall`, or `xpcall` to simulate exception stops. Any available error messages/tracebacks are diagnostics, not inspectable live fault frames; forwarding them is not a new v1 requirement.

Protected compile/evaluate/GLua-command failures still return structured errors. Runtime condition/logpoint errors still disable the failing breakpoint and report diagnostics as specified in §7.2, without propagating debugger errors into gameplay. Reproduce a failure with an ordinary breakpoint before the failing statement to inspect its live state.

## 8. Inspection and execution

### 8.1 Stack/scopes/variables

Stack frames contain a stop-scoped ID, coroutine ID, function name or explicit anonymous label, source identity, current/definition line where available, Lua/C kind, and unavailable-debug-info reason. Resolve frames against the original execution context even while protected helper calls add temporary frames.

Locals must preserve lexical slots and shadowed duplicate names; upvalues preserve identity/order; environment scope uses the selected function's environment, not always `_G`. Nil is a present value. Enumerate arguments/varargs where the shipped VM exposes them, otherwise mark unavailable rather than inventing values. Internal VM temporaries are distinguished from user bindings.

Variable entries include display name/key, explicit type, bounded display value, optional evaluate name, child reference, and known named/indexed counts. Primitive nil/boolean/number/string and table/function/thread/userdata must be distinguishable. Encode NaN/infinity and arbitrary-byte strings without invalid JSON or silent byte loss (escaped preview; optional base64 bytes when explicitly requested). Non-string keys need typed previews and identity, not collisions in a JSON object.

Traverse raw tables without calling `pairs`, `__index`, `__len`, `__tostring`, or userdata methods. Preserve cycle/shared-object identity. Present GMod entities/vectors/angles/other userdata as opaque typed objects unless a side-effect-free type inspector has been specifically verified. Users may explicitly evaluate methods when they want them.

Expansion is shallow and paginated, not recursive serialization of the world. Use bounded, stop/revision-scoped key snapshots/cursors; cap retained keys/objects and total bytes. If a table exceeds the inspection budget, return explicit truncation/continuation information and known counts only. DAP may expose a labeled truncation child if it cannot represent a continuation. Do not scan an unbounded table just to produce a total. Evaluation invalidates snapshots because it can mutate tables during a stop.

### 8.2 Evaluation and GLua commands

`evaluate` requires a selected stopped frame (default top Lua frame). Compile as expression(s) with a verified GLua compiler; report syntax/runtime errors as protocol errors. Do not silently reinterpret a failed expression as a statement chunk. Returned tuples preserve nil positions and typed expandable values.

A frame environment resolves the innermost active local binding, then upvalues, then the original function environment. Use an explicit binding-presence map: a nil local must shadow an upvalue/global. Ordinary assignments in `command(kind="glua")` write back to the selected local/upvalue slot or function environment; declarations introduced by the new chunk remain local to that chunk. Table/object mutation and called functions have real effects. No rollback is promised, including when a later statement errors.

Compilation/setup/call/result handling must run under the protected integration boundary. Hook/breakpoint recursion is suppressed for debugger-internal execution. Do not assume this suppression can simultaneously supply a count-hook deadline. Code cannot yield across the debugger's protected hook/engine boundary; return the VM's error instead of pretending to suspend evaluation.

A running `command(kind="glua")` executes a global-environment statement chunk at an owner-thread safe point, with no selected-frame locals. A stopped command can specify a frame and use write-through bindings. Every executed evaluation/GLua command returns a new `valueRevision` and an invalidation notification even if it errors after side effects. No `setVariable`/`setExpression` DAP capability is needed for v1; explicit commands provide mutation.

### 8.3 Source console commands

`command(kind="console")` takes a command name and an array of argument strings, not a shell command. Validate argument count/length and reject NUL, newline, carriage return, or console-command separator injection. Call the verified realm-appropriate `RunConsoleCommand`/engine API, never an OS shell. Preserve engine permission/blocked-command restrictions; do not reinterpret a client command as an elevated server command.

While stopped, enqueue console submission until after an explicit continue/step returns to ordinary engine processing. Reply `queued` with a command ID; later emit `commandResult` indicating `submitted`, rejected, or canceled. While running, submit at a safe callback. Submission is not proof that an asynchronous engine command completed successfully. Detach/connection loss cancels not-yet-submitted commands, even though detach resumes the game. Do not run queued console commands as an unintended side effect of cleanup.

## 9. Agent CLI

One process owns one session:

```sh
gdebug attach --endpoint-file /private/gdebug/server.json --jsonl --allow-hook-replacement
```

For an existing SSH forward, add `--endpoint 127.0.0.1:47000`. The CLI manages heartbeat independently of stdin so an agent can pause between commands without losing a healthy session.

After private authentication/attach, stdout contains an ID-0 attach response. Thereafter send requests with positive unique IDs. Responses and events can interleave; consume by `type` and `id`, not one-input/one-output-line assumptions. Flush each output line. Stdout contains protocol JSON only; diagnostics go to stderr. No prompts, ANSI text, banners, or secrets contaminate the stream.

Example request vocabulary (replace illustrative stop/frame IDs with those returned by the target):

```jsonl
{"id":1,"op":"break","args":{"source":"lua/myaddon/test.lua","line":24,"condition":"self:IsValid()","hitCondition":">= 3"}}
{"id":2,"op":"pause","args":{}}
{"id":3,"op":"stack","args":{"stopId":"stop-7","start":0,"count":20}}
{"id":4,"op":"scopes","args":{"stopId":"stop-7","frameId":"frame-1"}}
{"id":5,"op":"variables","args":{"stopId":"stop-7","reference":"locals-1","start":0,"count":100}}
{"id":6,"op":"evaluate","args":{"stopId":"stop-7","frameId":"frame-1","expression":"self:GetClass()"}}
{"id":7,"op":"command","args":{"kind":"glua","stopId":"stop-7","frameId":"frame-1","code":"counter = counter + 1"}}
{"id":8,"op":"step","args":{"stopId":"stop-7","kind":"over"}}
{"id":9,"op":"status","args":{}}
{"id":10,"op":"clear","args":{"all":true}}
{"id":11,"op":"command","args":{"kind":"console","name":"status","arguments":[]}}
{"id":12,"op":"detach","args":{}}
```

The example is vocabulary, not a blindly replayable transcript: wait for `stopped`, use its `stopId`, and fetch fresh variable references after evaluation. `step` also accepts `in`/`out`; `continue` takes the current `stopId`.

EOF/SIGINT requests detach and exits after a bounded cleanup wait, reporting if restoration is unconfirmed. Exit codes: 0 normal detach, 1 fatal target/internal failure, 2 argument/input error, 3 connection/authentication failure, 4 protocol/version incompatibility. An individual valid request receiving an operation error does not terminate the session. Malformed complete JSONL receives a structured input error when correlatable; oversized/broken framing terminates safely. Independent `gdebug break` processes and a persistent broker are intentionally absent.

## 10. DAP and editor integration

### 10.1 Adapter

`gdebug dap` reads/writes standard DAP `Content-Length`-framed messages on stdin/stdout, never JSONL on that interface. Diagnostic logs go to stderr. It owns one target connection and shares request/response types and behavior with the CLI. DAP request parsing and target event handling must remain concurrent with pending engine work.

Handle `initialize` first, reply before emitting any event, then attach/authenticate, advertise negotiated capabilities, emit `initialized`, accept configuration requests, and arm on `configurationDone`. Static initialize capabilities must be conservative; update target-dependent capabilities before configuration as necessary. Reject `launch`, process termination, and unsupported execution modes explicitly. Disconnect always detaches/resumes; it does not kill GMod even if a client asks to terminate the debuggee.

| DAP request/event | Core mapping |
| --- | --- |
| `attach` | Endpoint/realm validation, consent, source maps, configuring attach |
| `setBreakpoints` | Atomic replacement for one source; empty list clears it |
| `setFunctionBreakpoints` | Atomic replacement of function breakpoints |
| `setExceptionBreakpoints` | Adapter-only no-op for empty configuration; reject any non-empty `filters`, `filterOptions`, or `exceptionOptions` as unsupported |
| `configurationDone` | Arm staged configuration; optional stop-on-attach |
| `threads` | Known coroutine contexts, current context identified |
| `pause`, `continue`, `stepIn`, `next`, `stepOut` | Pause/continue/step operations |
| `stackTrace`, `scopes`, `variables` | Stop-scoped ID translation and pagination |
| `evaluate` | Expression evaluation; explicit REPL command prefixes below |
| `exceptionInfo` | Unsupported; return a clear DAP error, never fabricated fault details |
| `source`, `loadedSources` | Known source registry/text availability |
| `disconnect` | Detach; cancel queued work; emit `terminated` for the session, not a fake process-exit event |
| `stopped`, `continued`, `breakpoint`, `output` | Corresponding core events with correct DAP reason/IDs |

Full qualified v1 advertises configurationDone, conditional/hit-conditional/function breakpoints, and logpoints. Set `exceptionBreakpointFilters: []` and `supportsExceptionInfoRequest: false`; do not advertise exception options/filter-options support. Advertise `supportsEvaluateForHovers: false`: watch/REPL evaluation is explicit trusted code, and silently executing method calls on hover is not required. Advertise loaded-sources only when provided. Do not advertise reverse stepping, memory/disassembly/data breakpoints, restart/terminate, instruction stepping, single-thread execution, or setVariable/setExpression. Respect client `supportsVariablePaging` and `supportsInvalidatedEvent`; these are client capabilities, not invented adapter capability fields.

DAP IDs remain stable for their defined lifetime and fit positive 31-bit bounds. Translate line/column bases and path/URI conventions negotiated in `initialize`. On continued/stopped and value mutation, invalidate adapter caches; emit `invalidated` only when supported, otherwise refresh on subsequent requests and tell the REPL user that previous variable views may need reopening. Never serve an obsolete value silently as fresh.

Source requests may return captured/verified target-known Lua text through the authenticated endpoint. Do not expose an arbitrary file-read RPC. Bound size, confine file reads to configured/verified source roots or GMod virtual sources, and reject traversal. For generated chunks with unknown text, return a clear unavailable-source error. Do not invent source text from a chunk label.

### 10.2 Debug console commands

In DAP `evaluate` with `context: "repl"` only:

- Ordinary input: GLua expression evaluation.
- `:lua <chunk>`: GLua statement command in the selected frame, or global command if running.
- `:console <JSON string array>`: console command; first element is name, remainder arguments, e.g. `:console ["status"]`.
- `:status`: cached debugger status.
- `:break <JSON args>` / `:clear <JSON args>`: the corresponding core operation, providing a portable fallback when an editor has no function-breakpoint UI. Example: `:break {"function":{"name":"GAMEMODE.Think"}}`.

REPL-created breakpoints are adapter-owned additions, kept separate from editor-owned breakpoint sets so a later DAP replace request does not accidentally erase them. An explicit REPL `clear` may remove either; notify the editor when its displayed breakpoint changes.

These prefixes are not interpreted in watches/conditions and do not require parsing shell quoting. Structured/custom DAP requests may expose the same operations, but standard debugging must not depend on editor-specific APIs.

### 10.3 VS Code

Provide a thin extension registering debug type `gdebug`, breakpoint support for the installed Lua/GLua language IDs, an attach-only configuration schema, and the native adapter executable. A user-specified adapter path is sufficient initially; automatic downloading is not required. Check compatible extension host/editor releases during implementation.

Illustrative `.vscode/launch.json` after the extension is installed:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "GMod server",
      "type": "gdebug",
      "request": "attach",
      "endpointFile": "/private/gdebug/server.json",
      "realm": "server",
      "allowHookReplacement": true,
      "sourceMaps": [
        { "localRoot": "${workspaceFolder}/lua", "remoteRoot": "lua" }
      ]
    }
  ]
}
```

The explicit true setting is user consent, not an extension-injected default. Config files contain paths, not tokens. `realm` is an assertion against the descriptor/target, not authority to switch realms.

### 10.4 Zed

Provide a debugger extension registering adapter `GDebug` in `extension.toml`, its JSON schema, `get_dap_binary` returning `gdebug dap`, and `dap_request_kind` returning attach. Standard configuration-based workflows are required; automatic task locators and launch workflows are not. Reuse the same adapter binary/protocol as VS Code.

Illustrative `.zed/debug.json` after registration:

```json
[
  {
    "adapter": "GDebug",
    "label": "GMod server",
    "request": "attach",
    "endpointFile": "/private/gdebug/server.json",
    "realm": "server",
    "allowHookReplacement": true,
    "sourceMaps": [
      { "localRoot": "$ZED_WORKTREE_ROOT/lua", "remoteRoot": "lua" }
    ]
  }
]
```

Do not mistake Zed's `tcp_connection` DAP-server option for the native target endpoint: gdebug's target speaks `gdebug/1`, not DAP. Port forwarding is configured through the adapter's `endpoint` override. Verify debugger-console support, function-breakpoint configuration, absence of advertised exception filters, and stopped-variable UX in actual supported Zed builds rather than assuming VS Code parity from the protocol alone.

## 11. Cleanup and failure behavior

- Network disconnect/lease expiry requests owner-thread detach. Cancel pending actions, release references, restore owned hook/JIT state, and leave the Lua hook so gameplay can resume.
- Normal detach should report successful restoration. If ownership changed or the state already closed, report the precise outcome; never call a saved callback into a dead state.
- Module close/map change cancels the session before rsgdll destroys the state. Remove callbacks, stop/join socket workers without waiting on workers that need the owner thread, and invalidate generation-bound work. Physical library unloading is separate from logical state teardown.
- Auto-refresh/reload updates source verification. Full state replacement ends the session; reconnect is explicit and gets a new session/generation. No invisible replay of conditions/commands.
- Debugger-internal recoverable errors disable the failing feature or detach safely, without turning a debugger diagnostic into an application Lua exception. Failure to guarantee memory/ABI safety is a target qualification failure, not a catch-and-continue policy.
- An infinite evaluation/native call can defeat all graceful recovery. Report this limitation; only the user can decide whether to terminate/restart the target externally.

## 12. Acceptance gates and implementation order

No implementation is included here. When authorized, proceed in this order; do not build polished editor integrations before the native feasibility gates pass.

| Gate | Required evidence | Current status |
| --- | --- | --- |
| G1: ABI/hook access | Resolve debug symbols from the existing VM; validate struct/callback ABI, line/call/return/count events, hook save/restore and JIT behavior on four platforms × two realms | Upstream API available (issue 1 resolved); gdebug runtime qualification pending |
| G2: protected paused inspection | Stop and service commands without Think; correct active-coroutine locals/upvalues across protected helpers; stack balance, allocation errors, panic containment, cleanup | Upstream API/fixtures available (issue 2 resolved); gdebug integration proof pending |
| G4: core operations | All operations and advanced semantics below, security/liveness, reload/detach, runtime matrix | Not implemented |
| G5: frontend parity | Persistent agent CLI plus VS Code and Zed scenarios through the same adapter; loopback and SSH-forwarded target | Not implemented |
| G6: release qualification | Recorded GMod/editor/framework versions, all target/realm checks, reproducible Git dependency/build artifacts, documented limits | Not implemented |

G3 (uncaught-error interception) is retired by the agreed scope change. Gate numbering is retained for traceability. Issues 1 and 2 are no longer missing-framework-API blockers; their downstream integration and runtime acceptance checks remain mandatory.

### 12.1 Focused future tests

**Pure logic/protocol:** path maps/case/ambiguous sources; hit grammar/count reset; logpoint lexer; frame binding presence/shadowing/nil; typed arbitrary-byte values; JSON framing split/coalesced/oversized input; request correlation; bounded queue overflow; stale IDs/generations; breakpoint replacement atomicity; capability and DAP ID translation. These do not need GMod.

**In-engine fixtures, each supported combination:**

1. Load module, reject absent consent/bad token/wrong realm, attach/detach repeatedly, restore JIT state and hook ownership. Fail every attach stage and verify rollback.
2. Break in nested functions/recursion; inspect locals/arguments/upvalues/global environment, nil shadowing, cycles, non-string keys, large strings/tables, and opaque GMod userdata.
3. Continue and step in/over/out across repeated same-line loops, nested calls, tail calls, errors, C calls, coroutine yield/resume, pre-existing/new coroutines, and top-level completion.
4. Conditional true/false/error/side effects, each hit predicate, logpoints with nested/quoted braces, disabled/re-enabled entries, function definition/symbol selectors, and function rebinding on reload.
5. Pause an interpreted tight loop; show pending status during an intentionally blocking native call; prove no VM calls occur on workers. Verify hook-count responsiveness without making a hard real-time guarantee.
6. Evaluate GLua-only syntax, multiple results including nil, selected-frame writes, syntax/runtime error recovery, mutation-before-error invalidation, and forbidden yield. Run intentionally infinite evaluations only in a disposable externally killable test process.
7. Submit console commands while running and queued while stopped; continue submits once, detach cancels, blocked/invalid commands do not gain privileges.
8. Uncaught nil-index/arithmetic errors, explicit `error`, timer/net/hook failures, and non-string error objects follow normal GMod error handling/reporting without exception stops. Script `pcall`/`xpcall` behavior remains unchanged. Debugger compile/evaluation/condition errors follow their structured error/diagnostic paths and never escape into gameplay.
9. Disconnect/kill the adapter while stopped; EOF/SIGINT; dropped SSH tunnel; stale heartbeat; output backpressure; another component replacing the hook; long pause followed by watchdog restoration; map change/client disconnect/state close; both realms loaded in a listen server.
10. Validate source mapping and line verification for normal Lua, addons/mounted files, dynamic chunks, unavailable source, reload, non-executable lines, and mismatched local versus target code.

**Frontend contract:** attach/configurationDone ordering, source/function breakpoints, logpoints, stop/step/variables, evaluate/REPL command/status, disconnect, and asynchronous events in CLI, VS Code, and Zed. Verify empty exception capabilities, harmless empty `setExceptionBreakpoints`, and explicit rejection of non-empty exception settings and `exceptionInfo`. Where an editor lacks a UI for a supported feature, supply and test an explicit configuration/REPL path; do not claim a nonexistent UI.

**Measurements:** report detached overhead, attached-interpreter overhead, hook cost with/without breakpoints, pause/control round-trip, and bounded memory under unread output/large object inspection. Initial engineering target: an idle stopped target answers cached status/ordinary control within 100 ms locally, excluding executed user code; measure and document hardware instead of treating this as a universal guarantee. Detached modules must install no debug hook and leave JIT untouched.

Release requires all active gates, not just a green Rust build. If a required platform or remaining feature cannot be made safe, return to the user with evidence and a scope decision; do not silently relabel a confirmed requirement optional.

## 13. Sources and upstream handoff

See [RESEARCH.md — Sources](RESEARCH.md#sources) for primary documentation and inspected source paths.

- [RSGDLL_ISSUE_1.md](RSGDLL_ISSUE_1.md): resolved upstream; native debug API available, downstream target qualification still required.
- [RSGDLL_ISSUE_2.md](RSGDLL_ISSUE_2.md): resolved upstream; protected hook/frame API and fixtures available, downstream integration still required.
- [RSGDLL_ISSUE_3.md](RSGDLL_ISSUE_3.md): closed with a negative feasibility result; exception breakpoints removed from v1.

These files retain the original requests and their outcomes for traceability. No issue was filed automatically.
