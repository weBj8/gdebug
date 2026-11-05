//! CLI and DAP end-to-end against the stock-LuaJIT host example (not GMod qualification).
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

struct Host(Child, std::path::PathBuf);
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn host(name: &str) -> Host {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let st = Command::new(env!("CARGO"))
        .args([
            "build",
            "-q",
            "-p",
            "gdebug-module",
            "--example",
            "stock_host",
        ])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(st.success());
    let dir = std::env::temp_dir().join(format!("gdebug-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ep = dir.join("server.json");
    let _ = std::fs::remove_file(&ep);
    let child = Command::new(root.join("target/debug/examples/stock_host"))
        .env("GDEBUG_SERVER_ENDPOINT_FILE", &ep)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if ep.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Host(child, ep)
}

fn lines(r: impl Read + Send + 'static) -> Receiver<Value> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(r).lines() {
            let Ok(l) = l else { return };
            let v: Value = serde_json::from_str(&l)
                .unwrap_or_else(|e| panic!("stdout must be pure JSONL ({e}): {l}"));
            if tx.send(v).is_err() {
                return;
            }
        }
    });
    rx
}

fn until(rx: &Receiver<Value>, f: impl Fn(&Value) -> bool) -> Value {
    loop {
        let v = rx.recv_timeout(T).expect("timed out waiting for message");
        if f(&v) {
            return v;
        }
    }
}

#[test]
fn jsonl_cli_session() {
    let h = host("cli");
    let bin = env!("CARGO_BIN_EXE_gdebug");
    // Consent is mandatory.
    let st = Command::new(bin)
        .args(["attach", "--endpoint-file"])
        .arg(&h.1)
        .arg("--jsonl")
        .output()
        .unwrap();
    assert_eq!(st.status.code(), Some(2));
    assert!(st.stdout.is_empty());

    let mut c = Command::new(bin)
        .args([
            "attach",
            "--jsonl",
            "--allow-hook-replacement",
            "--endpoint-file",
        ])
        .arg(&h.1)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let rx = lines(c.stdout.take().unwrap());
    let mut w = c.stdin.take().unwrap();
    let a = until(&rx, |_| true);
    assert_eq!(
        (a["id"].clone(), a["ok"].clone()),
        (json!(0), json!(true)),
        "{a}"
    );
    let mut send = |v: Value| writeln!(w, "{v}").unwrap();
    send(json!({"id": 1, "op": "break", "args": {"source": "lua/host.lua", "line": 2}}));
    until(&rx, |v| v["id"] == 1);
    let st = until(&rx, |v| v["event"] == "stopped");
    let stop = st["data"]["stopId"].clone();
    send(json!({"id": 2, "op": "evaluate", "args": {"stopId": stop, "expression": "n * 10"}}));
    let ev = until(&rx, |v| v["id"] == 2);
    assert_eq!(ev["result"]["results"][0]["value"], "10", "{ev}");
    // Malformed input gets a structured error and the session survives.
    send(json!("not an object"));
    assert_eq!(
        until(&rx, |v| v["type"] == "response" && v["ok"] == false)["error"]["code"],
        "ERR_INVALID_ARGUMENT"
    );
    send(json!({"id": 3, "op": "clear", "args": {"all": true}}));
    send(json!({"id": 4, "op": "continue", "args": {"stopId": stop}}));
    assert_eq!(until(&rx, |v| v["id"] == 4)["ok"], true);
    send(json!({"id": 5, "op": "status", "args": {}}));
    assert_eq!(until(&rx, |v| v["id"] == 5)["result"]["state"], "running");
    drop(send);
    drop(w); // EOF => detach
    assert_eq!(c.wait().unwrap().code(), Some(0));
    // Target is reusable after detach.
    let mut c = Command::new(bin)
        .args([
            "attach",
            "--jsonl",
            "--allow-hook-replacement",
            "--endpoint-file",
        ])
        .arg(&h.1)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let rx = lines(c.stdout.take().unwrap());
    assert_eq!(until(&rx, |_| true)["ok"], true);
    writeln!(
        c.stdin.as_mut().unwrap(),
        "{}",
        json!({"id": 1, "op": "detach"})
    )
    .unwrap();
    assert_eq!(until(&rx, |v| v["id"] == 1)["result"]["hookRestored"], true);
    assert_eq!(c.wait().unwrap().code(), Some(0));
}

struct Dap {
    w: std::process::ChildStdin,
    rx: Receiver<Value>,
    seq: i64,
}

impl Dap {
    fn req(&mut self, cmd: &str, args: Value) -> Value {
        self.seq += 1;
        let b = json!({"seq": self.seq, "type": "request", "command": cmd, "arguments": args})
            .to_string();
        write!(self.w, "Content-Length: {}\r\n\r\n{b}", b.len()).unwrap();
        self.w.flush().unwrap();
        let s = self.seq;
        until(&self.rx, |v| {
            v["type"] == "response" && v["request_seq"] == s
        })
    }
    fn ok(&mut self, cmd: &str, args: Value) -> Value {
        let r = self.req(cmd, args);
        assert_eq!(r["success"], true, "{cmd}: {r}");
        r["body"].clone()
    }
}

fn dap_reader(r: impl Read + Send + 'static) -> Receiver<Value> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        loop {
            let mut len = 0;
            loop {
                let mut l = String::new();
                if r.read_line(&mut l).unwrap_or(0) == 0 {
                    return;
                }
                let l = l.trim_end();
                if l.is_empty() {
                    break;
                }
                len = l
                    .strip_prefix("Content-Length: ")
                    .expect("DAP header")
                    .parse()
                    .unwrap();
            }
            let mut b = vec![0; len];
            r.read_exact(&mut b).unwrap();
            if tx.send(serde_json::from_slice(&b).unwrap()).is_err() {
                return;
            }
        }
    });
    rx
}

#[test]
fn dap_session() {
    let h = host("dap");
    let mut c = Command::new(env!("CARGO_BIN_EXE_gdebug"))
        .arg("dap")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let rx = dap_reader(c.stdout.take().unwrap());
    let mut d = Dap {
        w: c.stdin.take().unwrap(),
        rx,
        seq: 0,
    };
    let caps = d.ok(
        "initialize",
        json!({"adapterID": "gdebug", "linesStartAt1": true, "supportsInvalidatedEvent": true}),
    );
    assert_eq!(caps["supportsLogPoints"], true);
    assert_eq!(caps["exceptionBreakpointFilters"], json!([]));
    assert_eq!(d.req("launch", json!({}))["success"], false);
    assert_eq!(
        d.req("attach", json!({"endpointFile": h.1}))["success"],
        false,
        "consent required"
    );
    d.ok(
        "attach",
        json!({"endpointFile": h.1, "allowHookReplacement": true, "realm": "server",
        "sourceMaps": [{"localRoot": "/work/addon", "remoteRoot": "lua"}]}),
    );
    until(&d.rx, |v| v["event"] == "initialized");
    let bps = d.ok(
        "setBreakpoints",
        json!({"source": {"path": "/work/addon/host.lua"}, "breakpoints": [{"line": 2}]}),
    );
    assert_eq!(bps["breakpoints"][0]["line"], 2);
    assert_eq!(
        d.req("setExceptionBreakpoints", json!({"filters": ["all"]}))["success"],
        false
    );
    d.ok("setExceptionBreakpoints", json!({"filters": []}));
    d.ok("configurationDone", json!({}));
    let st = until(&d.rx, |v| v["event"] == "stopped");
    assert_eq!(st["body"]["reason"], "breakpoint");
    let tid = st["body"]["threadId"].clone();
    let threads = d.ok("threads", json!({}));
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == tid)
    );
    let stack = d.ok("stackTrace", json!({"threadId": tid}));
    let f0 = &stack["stackFrames"][0];
    assert_eq!(f0["line"], 2);
    assert_eq!(f0["source"]["path"], "/work/addon/host.lua");
    let scopes = d.ok("scopes", json!({"frameId": f0["id"]}));
    let vars = d.ok(
        "variables",
        json!({"variablesReference": scopes["scopes"][0]["variablesReference"]}),
    );
    assert_eq!(vars["variables"][0]["name"], "n");
    let ev = d.ok(
        "evaluate",
        json!({"expression": "n + 1", "frameId": f0["id"], "context": "repl"}),
    );
    assert_eq!(ev["result"], "2");
    let s = d.ok(
        "evaluate",
        json!({"expression": ":status", "context": "repl"}),
    );
    assert!(s["result"].as_str().unwrap().contains("stopped"));
    d.ok(
        "evaluate",
        json!({"expression": ":break {\"function\":{\"name\":\"print\"}}", "context": "repl"}),
    );
    d.ok(
        "setBreakpoints",
        json!({"source": {"path": "/work/addon/host.lua"}, "breakpoints": []}),
    );
    d.ok("next", json!({"threadId": tid}));
    assert_eq!(
        until(&d.rx, |v| v["event"] == "stopped")["body"]["reason"],
        "step"
    );
    d.ok("continue", json!({"threadId": tid}));
    d.ok("disconnect", json!({}));
    assert_eq!(c.wait().unwrap().code(), Some(0));
}
