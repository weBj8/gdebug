# gdebug usage

## Build

[GitHub Actions builds and releases](CI.md) provide ready-to-download native bundles,
a VS Code `.vsix`, and a Zed WASM archive. To build locally:

```sh
cargo build --release -p gdebug-cli                       # gdebug CLI + DAP adapter
cargo build --release -p gdebug-module --features server  # or --features client
# cross (clear user rustflags first):
RUSTFLAGS="" cargo zigbuild --release -p gdebug-module --features server --target i686-unknown-linux-gnu.2.17
RUSTFLAGS="" XWIN_ARCH=x86,x86_64 cargo xwin build --release -p gdebug-module --features client --target x86_64-pc-windows-msvc
```

Install the module as `garrysmod/lua/bin/gmsv_gdebug_<platform>.dll` (server) or
`gmcl_gdebug_<platform>.dll` (client), platform = `linux`, `linux64`, `win32`, `win64`, and
`require("gdebug")` from realm startup code.

## Enable the listener

The module is inert unless the realm's descriptor path is set before GMod starts:

```sh
mkdir -m 700 ~/gdebug
GDEBUG_SERVER_ENDPOINT_FILE=~/gdebug/server.json ./srcds_run_x64 ...
GDEBUG_CLIENT_ENDPOINT_FILE=~/gdebug/client.json  # client realm
```

It binds `127.0.0.1:0` and writes an owner-only descriptor holding the port and a fresh token.
Remote targets: forward the port with SSH and copy the descriptor, then pass `--endpoint`/`endpoint`.

## Agent CLI

```sh
gdebug attach --endpoint-file ~/gdebug/server.json --jsonl --allow-hook-replacement
```

Stdout is JSONL only (attach response has id 0); write requests to stdin:

```json
{"id":1,"op":"break","args":{"source":"lua/autorun/server/x.lua","line":12,"condition":"n > 3"}}
{"id":2,"op":"stack","args":{"stopId":"stop-1"}}
{"id":3,"op":"evaluate","args":{"stopId":"stop-1","expression":"ply:Nick()"}}
{"id":4,"op":"command","args":{"kind":"glua","code":"print(1)"}}
{"id":5,"op":"command","args":{"kind":"console","name":"status","args":[]}}
{"id":6,"op":"continue"}
```

EOF/Ctrl+C detaches. Exit codes: 0 ok, 1 fatal/unconfirmed detach, 2 arguments, 3 connection, 4 protocol version.

## Editors

- VS Code: `editors/vscode` (set `gdebug.adapterPath` or put `gdebug` on PATH).
- Zed: `editors/zed` (dev extension; finds `gdebug` on PATH).

Attach configs need `endpointFile`, `"allowHookReplacement": true` (consent: GMod's
infinite-loop watchdog and profilers stop while attached), optional `realm`, `endpoint`,
`stopOnAttach`, `sourceMaps: [{localRoot, remoteRoot}]`. REPL: plain expression, `:lua <chunk>`,
`:console ["cmd", "arg"]`, `:status`, `:break {...}`, `:clear {...}`.

## Limits

An evaluation or native call that never returns blocks the realm; only restarting GMod recovers.
No exception breakpoints in v1.
