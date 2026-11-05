//! `gdebug dap`: Debug Adapter Protocol over stdin/stdout (Content-Length framing), translating to
//! one `gdebug/1` target connection. DAP input is read on its own thread and target round trips
//! complete on worker threads, so configuration and pause stay responsive while engine work is
//! pending (spec §10).

use crate::conn::{CLOSED, Target};
use gdebug_proto::path::{self, Mapping};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct St {
    target: Option<Arc<Target>>,
    lines1: bool,
    paging: bool,
    invalidated_ok: bool,
    maps: Vec<Mapping>,
    stop: Option<String>,
    /// DAP frameId - 1 -> (stopId, target frame id); valid for one stop.
    frames: Vec<(String, String)>,
    /// DAP variablesReference - 1 -> (stopId, target reference).
    vars: Vec<(String, String)>,
    /// DAP sourceReference - 1 -> runtime path.
    srcs: Vec<String>,
}

#[derive(Clone)]
struct Ctx {
    st: Arc<Mutex<St>>,
    out: Arc<Mutex<(std::io::Stdout, i64)>>,
}

enum Msg {
    Dap(Value),
    Target(Value),
    Eof,
}

impl Ctx {
    fn send(&self, mut v: Value) {
        let mut o = self.out.lock().unwrap();
        o.1 += 1;
        v["seq"] = json!(o.1);
        let body = v.to_string();
        let _ =
            write!(o.0, "Content-Length: {}\r\n\r\n{body}", body.len()).and_then(|_| o.0.flush());
    }
    fn respond(&self, req: &Value, body: Result<Value, String>) {
        let mut v = json!({"type": "response", "request_seq": req["seq"], "command": req["command"], "success": body.is_ok()});
        match body {
            Ok(b) => v["body"] = b,
            Err(m) => v["message"] = json!(m),
        }
        self.send(v);
    }
    fn event(&self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }
    fn line_out(&self, n: i64) -> i64 {
        if self.st.lock().unwrap().lines1 {
            n
        } else {
            n - 1
        }
    }
    fn line_in(&self, n: i64) -> i64 {
        if self.st.lock().unwrap().lines1 {
            n
        } else {
            n + 1
        }
    }
    fn target(&self) -> Result<Arc<Target>, String> {
        self.st
            .lock()
            .unwrap()
            .target
            .clone()
            .ok_or_else(|| "not attached".to_owned())
    }

    /// Sends a target request now (preserving order) and answers the DAP request from a worker.
    fn forward(
        &self,
        req: &Value,
        op: &str,
        args: Value,
        conv: impl FnOnce(&Ctx, Value) -> Result<Value, String> + Send + 'static,
    ) {
        let t = match self.target() {
            Ok(t) => t,
            Err(m) => return self.respond(req, Err(m)),
        };
        let rx = t.request(op, args);
        let (ctx, req) = (self.clone(), req.clone());
        std::thread::spawn(move || {
            let body = match rx.recv() {
                Ok(v) if v["ok"] == true => conv(&ctx, v["result"].clone()),
                Ok(v) => Err(format!(
                    "{}: {}",
                    v["error"]["code"].as_str().unwrap_or("error"),
                    v["error"]["message"].as_str().unwrap_or("")
                )),
                Err(_) => Err("target connection closed".into()),
            };
            ctx.respond(&req, body);
        });
    }

    /// Local editor path -> runtime source path (explicit mappings first, then the GMod
    /// `lua/` / `gamemodes/` virtual-root convention).
    fn to_remote(&self, local: &str) -> Result<String, String> {
        let maps = self.st.lock().unwrap().maps.clone();
        if let Some(r) = path::to_remote(&maps, local)? {
            return Ok(r);
        }
        let n = path::normalize(local);
        for root in ["lua/", "gamemodes/"] {
            if let Some(i) = n.rfind(&format!("/{root}")) {
                return Ok(n[i + 1..].to_owned());
            }
            if n.starts_with(root) {
                return Ok(n);
            }
        }
        Ok(n)
    }

    fn dap_source(&self, src: &Value) -> Value {
        let label = src["label"].as_str().unwrap_or("");
        let Some(rt) = src["path"].as_str() else {
            return json!({"name": label, "presentationHint": "deemphasize"});
        };
        let name = rt.rsplit('/').next().unwrap_or(rt);
        let maps = self.st.lock().unwrap().maps.clone();
        match path::to_local(&maps, rt) {
            Ok(Some(local)) => json!({"name": name, "path": local}),
            _ => {
                let mut st = self.st.lock().unwrap();
                let i = match st.srcs.iter().position(|s| s == rt) {
                    Some(i) => i,
                    None => {
                        st.srcs.push(rt.to_owned());
                        st.srcs.len() - 1
                    }
                };
                json!({"name": name, "sourceReference": i + 1, "origin": rt})
            }
        }
    }

    fn dap_bp(&self, b: &Value) -> Value {
        let mut v = json!({"id": b["id"], "verified": b["verified"]});
        if let Some(m) = b["message"].as_str() {
            v["message"] = json!(m);
        }
        if let Some(l) = b["line"].as_i64() {
            v["line"] = json!(self.line_out(l));
        }
        v
    }

    fn var_ref(&self, stop: &str, r: &Value) -> usize {
        let Some(r) = r.as_str() else { return 0 };
        let mut st = self.st.lock().unwrap();
        st.vars.push((stop.to_owned(), r.to_owned()));
        st.vars.len()
    }

    fn dap_var(&self, stop: &str, v: &Value) -> Value {
        let mut d = json!({
            "name": v["name"], "value": v["value"], "type": v["type"],
            "variablesReference": self.var_ref(stop, &v["reference"]),
        });
        if let Some(e) = v["evaluateName"].as_str() {
            d["evaluateName"] = json!(e);
        }
        if v["internal"] == true {
            d["presentationHint"] = json!({"kind": "virtual"});
        }
        d
    }

    fn clear_stop(&self) {
        let mut st = self.st.lock().unwrap();
        st.stop = None;
        st.frames.clear();
        st.vars.clear();
    }
}

fn read_dap(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let len = len.filter(|n| *n <= gdebug_proto::MAX_MESSAGE)?;
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

pub fn run() {
    let ctx = Ctx {
        st: Arc::default(),
        out: Arc::new(Mutex::new((std::io::stdout(), 0))),
    };
    let (tx, rx) = channel();
    let itx = tx.clone();
    std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(std::io::stdin());
        while let Some(v) = read_dap(&mut r) {
            if itx.send(Msg::Dap(v)).is_err() {
                return;
            }
        }
        let _ = itx.send(Msg::Eof);
    });
    for m in rx {
        match m {
            Msg::Dap(req) if req["type"] == "request" => request(&ctx, &req, &tx),
            Msg::Dap(_) => {}
            Msg::Target(ev) => target_event(&ctx, &ev),
            Msg::Eof => {
                if let Ok(t) = ctx.target() {
                    let _ = t.call("detach", json!({}), Duration::from_secs(5));
                }
                return;
            }
        }
    }
}

const NOT_SUPPORTED: &[&str] = &[
    "launch",
    "terminate",
    "restart",
    "exceptionInfo",
    "setVariable",
    "setExpression",
    "stepBack",
    "reverseContinue",
    "gotoTargets",
    "goto",
    "disassemble",
    "readMemory",
    "writeMemory",
    "dataBreakpointInfo",
    "setDataBreakpoints",
    "setInstructionBreakpoints",
    "restartFrame",
    "stepInTargets",
    "terminateThreads",
];

fn request(ctx: &Ctx, req: &Value, tx: &std::sync::mpsc::Sender<Msg>) {
    let a = &req["arguments"];
    let cmd = req["command"].as_str().unwrap_or("");
    match cmd {
        "initialize" => {
            {
                let mut st = ctx.st.lock().unwrap();
                st.lines1 = a["linesStartAt1"].as_bool().unwrap_or(true);
                st.paging = a["supportsVariablePaging"] == true;
                st.invalidated_ok = a["supportsInvalidatedEvent"] == true;
            }
            ctx.respond(req, Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsConditionalBreakpoints": true,
                "supportsHitConditionalBreakpoints": true,
                "supportsFunctionBreakpoints": true,
                "supportsLogPoints": true,
                "supportsEvaluateForHovers": false,
                "supportsExceptionInfoRequest": false,
                "exceptionBreakpointFilters": [],
                "supportsLoadedSourcesRequest": false,
                "supportsTerminateRequest": false,
                "supportsRestartRequest": false,
                "supportTerminateDebuggee": false,
                "supportsSetVariable": false,
            })));
        }
        "attach" => attach(ctx, req, tx),
        "configurationDone" => ctx.forward(req, "configurationDone", json!({}), |_, _| Ok(json!({}))),
        "setBreakpoints" => {
            let local = a["source"]["path"].as_str().unwrap_or("");
            let remote = match ctx.to_remote(local) {
                Ok(r) => r,
                Err(m) => return ctx.respond(req, Err(m)),
            };
            let bps: Vec<Value> = a["breakpoints"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|b| {
                    json!({"line": ctx.line_in(b["line"].as_i64().unwrap_or(0)), "condition": b["condition"],
                        "hitCondition": b["hitCondition"], "logMessage": b["logMessage"]})
                })
                .collect();
            ctx.forward(req, "setBreakpoints", json!({"source": remote, "breakpoints": bps}), |ctx, r| {
                Ok(json!({"breakpoints": r["breakpoints"].as_array().unwrap_or(&vec![]).iter().map(|b| ctx.dap_bp(b)).collect::<Vec<_>>()}))
            });
        }
        "setFunctionBreakpoints" => {
            let bps: Vec<Value> = a["breakpoints"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|b| json!({"name": b["name"], "condition": b["condition"], "hitCondition": b["hitCondition"]}))
                .collect();
            ctx.forward(req, "setFunctionBreakpoints", json!({"breakpoints": bps}), |ctx, r| {
                Ok(json!({"breakpoints": r["breakpoints"].as_array().unwrap_or(&vec![]).iter().map(|b| ctx.dap_bp(b)).collect::<Vec<_>>()}))
            });
        }
        "setExceptionBreakpoints" => {
            let empty = |k: &str| a[k].as_array().is_none_or(|x| x.is_empty());
            match empty("filters") && empty("filterOptions") && empty("exceptionOptions") {
                true => ctx.respond(req, Ok(json!({"breakpoints": []}))),
                false => ctx.respond(req, Err("exception breakpoints are not supported in gdebug v1".into())),
            }
        }
        "threads" => ctx.forward(req, "threads", json!({}), |_, r| {
            Ok(json!({"threads": r["threads"].as_array().unwrap_or(&vec![]).iter().map(|t| json!({"id": t["id"], "name": t["name"]})).collect::<Vec<_>>()}))
        }),
        "pause" => ctx.forward(req, "pause", json!({}), |_, _| Ok(json!({}))),
        "continue" => {
            let stop = ctx.st.lock().unwrap().stop.clone();
            ctx.forward(req, "continue", json!({"stopId": stop}), |ctx, _| {
                ctx.clear_stop();
                Ok(json!({"allThreadsContinued": true}))
            });
        }
        "next" | "stepIn" | "stepOut" => {
            let kind = match cmd {
                "next" => "over",
                "stepIn" => "in",
                _ => "out",
            };
            let stop = ctx.st.lock().unwrap().stop.clone();
            ctx.forward(req, "step", json!({"stopId": stop, "kind": kind}), |ctx, _| {
                ctx.clear_stop();
                Ok(json!({}))
            });
        }
        "stackTrace" => {
            let Some(stop) = ctx.st.lock().unwrap().stop.clone() else { return ctx.respond(req, Err("not stopped".into())) };
            let args = json!({"stopId": stop, "threadId": a["threadId"], "start": a["startFrame"].as_u64().unwrap_or(0), "count": a["levels"].as_u64().unwrap_or(0)});
            ctx.forward(req, "stack", args, move |ctx, r| {
                let frames: Vec<Value> = r["frames"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|f| {
                        let id = {
                            let mut st = ctx.st.lock().unwrap();
                            st.frames.push((stop.clone(), f["id"].as_str().unwrap_or("").to_owned()));
                            st.frames.len()
                        };
                        let mut d = json!({"id": id, "name": f["name"], "line": ctx.line_out(f["line"].as_i64().unwrap_or(0).max(0)), "column": 1, "source": ctx.dap_source(&f["source"])});
                        if f["kind"] == "c" {
                            d["presentationHint"] = json!("subtle");
                        }
                        d
                    })
                    .collect();
                Ok(json!({"stackFrames": frames, "totalFrames": r["totalFrames"]}))
            });
        }
        "scopes" => {
            let f = frame(ctx, &a["frameId"]);
            let Some((stop, fid)) = f else { return ctx.respond(req, Err("stale frame id".into())) };
            ctx.forward(req, "scopes", json!({"stopId": stop, "frameId": fid}), move |ctx, r| {
                let scopes: Vec<Value> = r["scopes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|s| {
                        let mut d = json!({"name": s["name"], "variablesReference": ctx.var_ref(&stop, &s["reference"]), "expensive": s["expensive"]});
                        if s["kind"] == "locals" {
                            d["presentationHint"] = json!("locals");
                        }
                        d
                    })
                    .collect();
                Ok(json!({"scopes": scopes}))
            });
        }
        "variables" => {
            let i = a["variablesReference"].as_u64().unwrap_or(0) as usize;
            let entry = ctx.st.lock().unwrap().vars.get(i.wrapping_sub(1)).cloned();
            let Some((stop, r)) = entry else { return ctx.respond(req, Err("stale variables reference; values changed since this view was opened".into())) };
            let mut args = json!({"stopId": stop, "reference": r});
            if ctx.st.lock().unwrap().paging {
                args["start"] = a["start"].clone();
                args["count"] = a["count"].clone();
            }
            ctx.forward(req, "variables", args, move |ctx, r| {
                Ok(json!({"variables": r["variables"].as_array().unwrap_or(&vec![]).iter().map(|v| ctx.dap_var(&stop, v)).collect::<Vec<_>>()}))
            });
        }
        "evaluate" => evaluate(ctx, req),
        "source" => {
            let i = a["sourceReference"].as_u64().or(a["source"]["sourceReference"].as_u64()).unwrap_or(0) as usize;
            let p = ctx.st.lock().unwrap().srcs.get(i.wrapping_sub(1)).cloned();
            match p {
                Some(p) => ctx.forward(req, "source", json!({"path": p}), |_, r| Ok(json!({"content": r["content"], "mimeType": "text/x-lua"}))),
                None => ctx.respond(req, Err("source unavailable".into())),
            }
        }
        "disconnect" => {
            let r = match ctx.target() {
                Ok(t) => t.call("detach", json!({}), Duration::from_secs(5)).map(|_| json!({})).map_err(|e| format!("detach unconfirmed: {}", e["error"]["message"])),
                Err(_) => Ok(json!({})),
            };
            ctx.respond(req, r);
            std::process::exit(0);
        }
        c if NOT_SUPPORTED.contains(&c) => ctx.respond(req, Err(format!("{c} is not supported by gdebug (attach-only debugger; it never launches or terminates GMod)"))),
        c => ctx.respond(req, Err(format!("unknown request {c}"))),
    }
}

fn frame(ctx: &Ctx, id: &Value) -> Option<(String, String)> {
    let st = ctx.st.lock().unwrap();
    let i = id.as_u64()? as usize;
    st.frames.get(i.checked_sub(1)?).cloned()
}

fn attach(ctx: &Ctx, req: &Value, tx: &std::sync::mpsc::Sender<Msg>) {
    let a = &req["arguments"];
    if a["allowHookReplacement"] != true {
        return ctx.respond(req, Err("set \"allowHookReplacement\": true in the attach configuration to consent to replacing the target's Lua debug hook (GMod's infinite-loop watchdog and profilers stop while attached)".into()));
    }
    let Some(file) = a["endpointFile"].as_str() else {
        return ctx.respond(req, Err("endpointFile is required".into()));
    };
    // `sourceMaps` is the documented key; `pathMappings` is accepted as an alias.
    let maps: Vec<Mapping> = a["sourceMaps"]
        .as_array()
        .or(a["pathMappings"].as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            Some(Mapping {
                local: m["localRoot"].as_str()?.to_owned(),
                remote: m["remoteRoot"].as_str()?.to_owned(),
            })
        })
        .collect();
    let (t, events, _) = match Target::connect(std::path::Path::new(file), a["endpoint"].as_str()) {
        Ok(x) => x,
        Err((_, m)) => return ctx.respond(req, Err(m)),
    };
    let mut args = json!({"allowHookReplacement": true, "configure": false, "stopOnAttach": a["stopOnAttach"] == true});
    if let Some(r) = a["realm"].as_str() {
        args["realm"] = json!(r);
    }
    match t.call("attach", args, Duration::from_secs(30)) {
        Ok(r) => {
            {
                let mut st = ctx.st.lock().unwrap();
                st.target = Some(t);
                st.maps = maps;
            }
            let etx = tx.clone();
            std::thread::spawn(move || {
                for ev in events {
                    if etx.send(Msg::Target(ev)).is_err() {
                        return;
                    }
                }
            });
            ctx.respond(req, Ok(json!({})));
            for w in r["warnings"].as_array().unwrap_or(&vec![]) {
                ctx.event("output", json!({"category": "important", "output": format!("gdebug: {}\n", w.as_str().unwrap_or(""))}));
            }
            ctx.event("initialized", json!({}));
        }
        Err(e) => ctx.respond(
            req,
            Err(e["error"]["message"]
                .as_str()
                .unwrap_or("attach failed")
                .to_owned()),
        ),
    }
}

fn evaluate(ctx: &Ctx, req: &Value) {
    let a = &req["arguments"];
    let expr = a["expression"].as_str().unwrap_or("").to_owned();
    let (stop, frame_id) = match a["frameId"].as_u64() {
        Some(_) => match frame(ctx, &a["frameId"]) {
            Some((s, f)) => (Some(s), Some(f)),
            None => return ctx.respond(req, Err("stale frame id".into())),
        },
        None => (ctx.st.lock().unwrap().stop.clone(), None),
    };
    let render = move |ctx: &Ctx, r: Value| -> Result<Value, String> {
        let stop = ctx.st.lock().unwrap().stop.clone().unwrap_or_default();
        let res = r["results"].as_array().cloned().unwrap_or_default();
        let text = res
            .iter()
            .map(|v| v["value"].as_str().unwrap_or("").to_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let reference = match res.as_slice() {
            [one] => ctx.var_ref(&stop, &one["reference"]),
            _ => 0,
        };
        Ok(json!({"result": text, "variablesReference": reference}))
    };
    let repl = a["context"] == "repl";
    let simple = |_: &Ctx, r: Value| Ok(json!({"result": r.to_string(), "variablesReference": 0}));
    if repl && let Some(rest) = expr.strip_prefix(':') {
        let (cmd, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let arg = arg.trim();
        return match cmd {
            "lua" => ctx.forward(req, "command", json!({"kind": "glua", "code": arg, "stopId": stop, "frameId": frame_id}), render),
            "console" => match serde_json::from_str::<Vec<String>>(arg) {
                Ok(v) if !v.is_empty() => ctx.forward(req, "command", json!({"kind": "console", "name": v[0], "args": v[1..]}), simple),
                _ => ctx.respond(req, Err(":console expects a JSON string array, e.g. :console [\"status\"]".into())),
            },
            "status" => ctx.forward(req, "status", json!({}), |_, r| Ok(json!({"result": serde_json::to_string_pretty(&r).unwrap_or_default(), "variablesReference": 0}))),
            "break" | "clear" => match serde_json::from_str::<Value>(arg) {
                Ok(v) if v.is_object() => {
                    let mut v = v;
                    if let Some(local) = v["source"].as_str().map(str::to_owned) {
                        match ctx.to_remote(&local) {
                            Ok(r) => v["source"] = json!(r),
                            Err(m) => return ctx.respond(req, Err(m)),
                        }
                    }
                    ctx.forward(req, cmd, v, simple)
                }
                _ => ctx.respond(req, Err(format!(":{cmd} expects a JSON object of {cmd} arguments"))),
            },
            _ => ctx.respond(req, Err("REPL commands: :lua <chunk>, :console [\"name\", ...], :status, :break {...}, :clear {...}".into())),
        };
    }
    match stop {
        Some(s) => ctx.forward(
            req,
            "evaluate",
            json!({"stopId": s, "frameId": frame_id, "expression": expr}),
            render,
        ),
        // Running: an explicit REPL expression is evaluated as a global GLua command.
        None if repl => ctx.forward(
            req,
            "command",
            json!({"kind": "glua", "code": format!("return {expr}")}),
            render,
        ),
        None => ctx.respond(req, Err("not stopped".into())),
    }
}

fn target_event(ctx: &Ctx, ev: &Value) {
    let d = &ev["data"];
    match ev["event"].as_str().unwrap_or("") {
        "stopped" => {
            {
                let mut st = ctx.st.lock().unwrap();
                st.stop = d["stopId"].as_str().map(str::to_owned);
                st.frames.clear();
                st.vars.clear();
            }
            let reason = match d["reason"].as_str().unwrap_or("") {
                "breakpoint" => "breakpoint",
                "function breakpoint" => "function breakpoint",
                "step" => "step",
                "pause" => "pause",
                _ => "pause",
            };
            let mut b = json!({"reason": reason, "threadId": d["threadId"], "allThreadsStopped": true, "hitBreakpointIds": d["hitBreakpointIds"]});
            if let Some(s) = d["description"].as_str() {
                b["description"] = json!(s);
            }
            ctx.event("stopped", b);
        }
        "continued" => {
            ctx.clear_stop();
            ctx.event("continued", json!({"threadId": 1, "allThreadsContinued": true}));
        }
        "breakpointChanged" => ctx.event("breakpoint", json!({"reason": "changed", "breakpoint": ctx.dap_bp(&d["breakpoint"])})),
        "output" => ctx.event("output", json!({"category": "console", "output": d["output"]})),
        "warning" => ctx.event("output", json!({"category": "important", "output": format!("gdebug warning: {}\n", d["message"].as_str().unwrap_or(""))})),
        "commandResult" => ctx.event("output", json!({"category": "console", "output": format!("console command {}: {}{}\n", d["commandId"], d["status"].as_str().unwrap_or(""), d["message"].as_str().map(|m| format!(" ({m})")).unwrap_or_default())})),
        "invalidated" => {
            ctx.st.lock().unwrap().vars.clear();
            match ctx.st.lock().unwrap().invalidated_ok {
                true => ctx.event("invalidated", json!({"areas": ["variables"]})),
                false => ctx.event("output", json!({"category": "console", "output": "gdebug: values changed; reopen variable views to refresh\n"})),
            }
        }
        "terminated" => ctx.event("terminated", json!({})),
        e if e == CLOSED => {
            ctx.event("output", json!({"category": "important", "output": "gdebug: target connection closed\n"}));
            ctx.event("terminated", json!({}));
        }
        _ => {}
    }
}
