//! End-to-end scenario on stock LuaJIT (not GMod qualification): a client thread drives the
//! debugger over real loopback TCP while the Lua owner thread runs a loop with a `pump`.
use gdebug_proto::descriptor::Descriptor;
use rsgdll::__rt::sys::{self, lua::*};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIT: AtomicBool = AtomicBool::new(false);

fn api() -> &'static LuaApi {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let names: Vec<String> = std::env::var("RSGDLL_LUAJIT")
            .into_iter()
            .chain(["libluajit-5.1.so.2", "libluajit-5.1.so", "lua51.dll"].map(String::from))
            .collect();
        let lib = names
            .iter()
            .find_map(|n| unsafe { libloading::Library::new(n) }.ok())
            .expect("stock LuaJIT not found; set RSGDLL_LUAJIT");
        unsafe { sys::init_with(lib) }.expect("load LuaJIT");
    });
    sys::api()
}

unsafe extern "C-unwind" fn pump(l: *mut lua_State) -> i32 {
    unsafe { gdebug::dbg::think(l) };
    unsafe { (api().lua_pushboolean)(l, QUIT.load(Ordering::Acquire) as i32) };
    1
}

fn exec(l: *mut lua_State, src: &str, chunk: &std::ffi::CStr) -> Result<String, String> {
    let a = api();
    unsafe {
        let top = (a.lua_gettop)(l);
        let st = match (a.luaL_loadbuffer)(l, src.as_ptr().cast(), src.len(), chunk.as_ptr()) {
            LUA_OK => (a.lua_pcall)(l, 0, 1, 0),
            s => s,
        };
        let mut n = 0;
        let p = (a.lua_tolstring)(l, -1, &mut n);
        let s = if p.is_null() {
            "nil".into()
        } else {
            String::from_utf8_lossy(std::slice::from_raw_parts(p.cast(), n)).into_owned()
        };
        (a.lua_settop)(l, top);
        if st == LUA_OK { Ok(s) } else { Err(s) }
    }
}

const SCRIPT: &str = "local function work(n)
  local a = n * 2
  local t = {x = a, s = \"hi\", [1] = true}
  return a + 1
end
function gfn(x)
  return x
end
local total = 0
while not pump() do
  total = gfn(total + work(1))
end
return tostring(total)
";

struct Client {
    r: BufReader<TcpStream>,
    w: TcpStream,
    id: i64,
    events: Vec<Value>,
}

impl Client {
    fn connect(d: &Descriptor) -> Self {
        let w = TcpStream::connect((d.host.as_str(), d.port)).unwrap();
        w.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        Client {
            r: BufReader::new(w.try_clone().unwrap()),
            w,
            id: 0,
            events: vec![],
        }
    }
    fn read(&mut self) -> Value {
        let mut s = String::new();
        assert!(self.r.read_line(&mut s).unwrap() > 0, "connection closed");
        serde_json::from_str(&s).unwrap()
    }
    fn send(&mut self, op: &str, args: Value) -> Value {
        self.id += 1;
        let id = self.id;
        writeln!(self.w, "{}", json!({"id": id, "op": op, "args": args})).unwrap();
        loop {
            let v = self.read();
            if v.get("event").is_some() {
                self.events.push(v);
            } else if v["id"] == id {
                return v;
            }
        }
    }
    fn ok(&mut self, op: &str, args: Value) -> Value {
        let v = self.send(op, args);
        assert_eq!(v["ok"], true, "{op}: {v}");
        v["result"].clone()
    }
    fn err(&mut self, op: &str, args: Value) -> String {
        let v = self.send(op, args);
        assert_eq!(v["ok"], false, "{op} should fail: {v}");
        v["error"]["code"].as_str().unwrap().to_owned()
    }
    fn wait(&mut self, name: &str) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return self.events.remove(i)["data"].clone();
        }
        loop {
            let v = self.read();
            if v["event"] == name {
                return v["data"].clone();
            }
            self.events.push(v);
        }
    }
}

fn drive(d: Descriptor) {
    // Authentication.
    let mut bad = Client::connect(&d);
    assert_eq!(
        bad.err("hello", json!({"protocol": "gdebug/1", "token": "00"})),
        "ERR_AUTH"
    );
    let mut pre = Client::connect(&d);
    assert_eq!(pre.err("status", json!({})), "ERR_AUTH");
    let mut c = Client::connect(&d);
    let h = c.ok("hello", json!({"protocol": "gdebug/1", "token": d.token}));
    assert_eq!(h["protocol"], "gdebug/1");
    // Second controller is busy.
    let mut other = Client::connect(&d);
    assert_eq!(
        other.err("hello", json!({"protocol": "gdebug/1", "token": d.token})),
        "ERR_BUSY"
    );
    drop(other);

    assert_eq!(c.err("attach", json!({})), "ERR_INVALID_ARGUMENT");
    assert_eq!(
        c.err(
            "attach",
            json!({"allowHookReplacement": true, "realm": "client"})
        ),
        "ERR_UNSUPPORTED_TARGET"
    );
    let at = c.ok(
        "attach",
        json!({"allowHookReplacement": true, "realm": "server"}),
    );
    assert_eq!(at["state"], "running");
    assert_eq!(c.err("stack", json!({})), "ERR_NOT_STOPPED");
    assert_eq!(
        c.err(
            "break",
            json!({"source": "lua/test.lua", "line": 4, "condition": "n =="})
        ),
        "ERR_COMPILE"
    );

    // Conditional line breakpoint, inspection, write-through evaluation.
    let bp = c.ok(
        "break",
        json!({"source": "lua/test.lua", "line": 4, "condition": "n == 1"}),
    );
    assert_eq!(bp["verified"], false);
    let st = c.wait("stopped");
    assert_eq!(st["reason"], "breakpoint");
    assert_eq!(st["hitBreakpointIds"], json!([bp["id"]]));
    assert_eq!(st["location"]["line"], 4);
    let stop = st["stopId"].clone();
    assert_eq!(c.wait("breakpointChanged")["breakpoint"]["verified"], true);
    let stack = c.ok("stack", json!({"stopId": stop}));
    assert_eq!(stack["frames"][0]["name"], "work");
    assert_eq!(stack["frames"][0]["source"]["path"], "lua/test.lua");
    assert_eq!(stack["frames"][1]["line"], 11);
    let scopes = c.ok("scopes", json!({"stopId": stop, "frameId": "frame-0"}));
    let locals_ref = scopes["scopes"][0]["reference"].clone();
    let vars = c.ok(
        "variables",
        json!({"stopId": stop, "reference": locals_ref}),
    );
    let names: Vec<_> = vars["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["n", "a", "t"]);
    assert_eq!(vars["variables"][1]["value"], "2");
    let tref = vars["variables"][2]["reference"].clone();
    let tv = c.ok("variables", json!({"stopId": stop, "reference": tref}));
    let mut tn: Vec<_> = tv["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect();
    tn.sort();
    assert_eq!(tn, ["[1]", "s", "x"]);
    let ev = c.ok(
        "evaluate",
        json!({"stopId": stop, "expression": "a + n, t.s"}),
    );
    assert_eq!(ev["results"][0]["value"], "3");
    assert_eq!(ev["results"][1]["value"], "\"hi\"");
    assert_eq!(c.wait("invalidated")["valueRevision"], ev["valueRevision"]);
    assert_eq!(
        c.err("variables", json!({"stopId": stop, "reference": tref})),
        "ERR_STALE_HANDLE"
    );
    assert_eq!(
        c.err(
            "evaluate",
            json!({"stopId": stop, "expression": "error('boom')"})
        ),
        "ERR_EVALUATION"
    );
    c.ok(
        "command",
        json!({"kind": "glua", "code": "a = 40; SEEN = n"}),
    );
    assert_eq!(
        c.ok("evaluate", json!({"stopId": stop, "expression": "a"}))["results"][0]["value"],
        "40"
    );

    // Step over from the return line lands back in the caller.
    c.ok("step", json!({"kind": "over"}));
    let st = c.wait("stopped");
    assert_eq!(st["reason"], "step");
    assert_eq!(c.err("stack", json!({"stopId": stop})), "ERR_STALE_HANDLE");
    let stack = c.ok("stack", json!({"stopId": st["stopId"]}));
    assert_eq!(stack["frames"][0]["source"]["path"], "lua/test.lua");
    assert_ne!(stack["frames"][0]["name"], "work");
    c.ok("clear", json!({"all": true}));
    c.ok("continue", json!({}));

    // Hit condition: stops only on the 3rd hit.
    let bp = c.ok(
        "break",
        json!({"source": "lua/test.lua", "line": 2, "hitCondition": "== 3"}),
    );
    c.wait("stopped");
    let all = c.ok("breakpoints", json!({}));
    assert_eq!(all["breakpoints"][0]["hitCount"], 3);
    c.ok("clear", json!({"id": bp["id"]}));
    c.ok("continue", json!({}));

    // Logpoint.
    c.ok(
        "break",
        json!({"source": "lua/test.lua", "line": 2, "logMessage": "n={n} {{x}}"}),
    );
    let out = c.wait("output");
    assert_eq!(out["output"], "n=1 {x}\n");
    c.ok("clear", json!({"all": true}));

    // Function breakpoint by dotted name.
    let fb = c.ok(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "gfn"}]}),
    );
    assert_eq!(fb["breakpoints"][0]["verified"], true);
    let st = c.wait("stopped");
    assert_eq!(st["reason"], "function breakpoint");
    let stack = c.ok("stack", json!({"stopId": st["stopId"]}));
    assert_eq!(stack["frames"][0]["name"], "gfn");
    c.ok("setFunctionBreakpoints", json!({"breakpoints": []}));
    // Step out returns to the caller.
    c.ok("step", json!({"kind": "out"}));
    let st = c.wait("stopped");
    let stack = c.ok("stack", json!({"stopId": st["stopId"]}));
    assert_ne!(stack["frames"][0]["name"], "gfn");
    c.ok("continue", json!({}));

    // Pause and running GLua command.
    let p = c.ok("pause", json!({}));
    assert!(p["state"] == "pending" || p["state"] == "stopped");
    assert_eq!(c.wait("stopped")["reason"], "pause");
    c.ok("continue", json!({}));
    let r = c.ok(
        "command",
        json!({"kind": "glua", "code": "return SEEN, 1 + 1"}),
    );
    assert_eq!(r["results"][0]["value"], "1");
    assert_eq!(r["results"][1]["value"], "2");
    let st = c.ok("status", json!({}));
    assert_eq!(st["attached"], true);

    // Detach restores; the loop then quits.
    let det = c.ok("detach", json!({}));
    assert_eq!(det["detached"], true);
    assert_eq!(det["hookRestored"], true);
    assert_eq!(det["jitRestored"], true);
    QUIT.store(true, Ordering::Release);
}

#[test]
fn stock_luajit_session() {
    let a = api();
    let dir = std::env::temp_dir().join(format!("gdebug-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.json");
    unsafe { std::env::set_var("GDEBUG_SERVER_ENDPOINT_FILE", &path) };
    unsafe {
        let l = (a.luaL_newstate)();
        (a.luaL_openlibs)(l);
        (a.lua_pushcclosure)(l, pump, 0);
        (a.lua_setfield)(l, LUA_GLOBALSINDEX, c"pump".as_ptr());
        let msg = gdebug::dbg::open(l, "server", 7).unwrap();
        assert!(
            !msg.contains(&Descriptor::read(&path).unwrap().token),
            "token leaked: {msg}"
        );
        let d = Descriptor::read(&path).unwrap();
        assert_eq!((d.realm.as_str(), d.generation), ("server", 7));
        let t = std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive(d)));
            QUIT.store(true, Ordering::Release);
            if let Err(e) = r {
                std::panic::resume_unwind(e)
            }
        });
        let r = exec(l, SCRIPT, c"@lua/test.lua");
        let joined = t.join();
        if joined.is_err() {
            QUIT.store(true, Ordering::Release);
        }
        joined.unwrap();
        r.unwrap();
        assert_eq!(
            exec(l, "return tostring(debug.gethook())", c"=t").unwrap(),
            "nil"
        );
        assert_eq!(
            exec(l, "return tostring(jit.status())", c"=t").unwrap(),
            "true"
        );
        gdebug::dbg::close(l);
        assert!(!path.exists(), "descriptor removed on close");
    }
}
