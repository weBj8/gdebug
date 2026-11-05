//! Owner-thread debugger core: session state machine, hook, breakpoints, stepping, inspection
//! and evaluation (spec §4–§9). Everything here runs on the Lua owner thread; the network side
//! only queues work (see `net`).
#![allow(unsafe_op_in_unsafe_fn, clippy::missing_safety_doc)]

use crate::net::{self, Job, Shared};
use gdebug_proto::{
    self as proto,
    bp::{Hit, Part},
    descriptor::{self, Descriptor},
    err, path,
};
use rsgdll::__rt::sys::{self, lua::*};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, c_char, c_int};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

type L = *mut lua_State;
type Er = (&'static str, String);
type R<T> = Result<T, Er>;

fn fail<T>(code: &'static str, m: impl Into<String>) -> R<T> {
    Err((code, m.into()))
}

const COUNT: c_int = 1000;
const HOOK: lua_Hook = Some(hook);

fn ours(h: lua_Hook) -> bool {
    h.is_some_and(|f| std::ptr::eq(f as *const (), hook as *const ()))
}

thread_local! {
    static DBG: RefCell<Option<Dbg>> = const { RefCell::new(None) };
}

fn a() -> &'static LuaApi {
    sys::api()
}
fn d() -> &'static LuaDebugApi {
    sys::debug_api().expect("debug API validated at open")
}

// ---------- small Lua helpers ----------

unsafe fn bytes_at(l: L, idx: c_int) -> Vec<u8> {
    let mut n = 0usize;
    let p = (a().lua_tolstring)(l, idx, &mut n);
    match p.is_null() {
        true => vec![],
        false => std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec(),
    }
}

unsafe fn cs(p: *const c_char) -> String {
    match p.is_null() {
        true => String::new(),
        false => CStr::from_ptr(p).to_string_lossy().into_owned(),
    }
}

unsafe fn type_name(l: L, idx: c_int) -> &'static str {
    match (a().lua_type)(l, idx) {
        LUA_TNIL => "nil",
        LUA_TBOOLEAN => "boolean",
        LUA_TLIGHTUSERDATA => "lightuserdata",
        LUA_TNUMBER => "number",
        LUA_TSTRING => "string",
        LUA_TTABLE => "table",
        LUA_TFUNCTION => "function",
        LUA_TUSERDATA => "userdata",
        LUA_TTHREAD => "thread",
        _ => "none",
    }
}

/// Pops the error object and renders it without running metamethods.
unsafe fn pop_err(l: L) -> String {
    let s = match (a().lua_type)(l, -1) {
        LUA_TSTRING => String::from_utf8_lossy(&bytes_at(l, -1)).into_owned(),
        _ => format!("(error object is a {} value)", type_name(l, -1)),
    };
    (a().lua_settop)(l, -2);
    s
}

unsafe fn load(l: L, code: &str, chunk: &CStr) -> Result<(), String> {
    match (a().luaL_loadbuffer)(l, code.as_ptr().cast(), code.len(), chunk.as_ptr()) {
        LUA_OK => Ok(()),
        _ => Err(pop_err(l)),
    }
}

unsafe fn pcall(l: L, nargs: c_int, nres: c_int) -> Result<(), String> {
    match (a().lua_pcall)(l, nargs, nres, 0) {
        LUA_OK => Ok(()),
        _ => Err(pop_err(l)),
    }
}

unsafe fn absidx(l: L, idx: c_int) -> c_int {
    if idx > 0 || idx <= LUA_REGISTRYINDEX {
        idx
    } else {
        (a().lua_gettop)(l) + idx + 1
    }
}

unsafe fn push_ref(l: L, r: c_int) {
    (a().lua_rawgeti)(l, LUA_REGISTRYINDEX, r);
}

unsafe fn make_ref(l: L) -> c_int {
    (a().luaL_ref)(l, LUA_REGISTRYINDEX)
}

unsafe fn unref(l: L, r: &mut c_int) {
    if *r > 0 {
        (a().luaL_unref)(l, LUA_REGISTRYINDEX, *r);
    }
    *r = LUA_NOREF;
}

unsafe fn rawget_str(l: L, t: c_int, k: &str) {
    let t = absidx(l, t);
    (a().lua_pushlstring)(l, k.as_ptr().cast(), k.len());
    (a().lua_rawget)(l, t);
}

/// Pushes `globals[a][b]...` via raw gets; nil if any step is not a table.
unsafe fn push_path(l: L, segs: &[&str]) {
    (a().lua_pushvalue)(l, LUA_GLOBALSINDEX);
    for s in segs {
        if (a().lua_type)(l, -1) != LUA_TTABLE {
            (a().lua_settop)(l, -2);
            (a().lua_pushnil)(l);
            return;
        }
        rawget_str(l, -1, s);
        (a().lua_remove)(l, -2);
    }
}

/// Value preview: (type, text, truncated byte length if cut).
unsafe fn preview(l: L, idx: c_int) -> (&'static str, String, Option<usize>) {
    let t = type_name(l, idx);
    let text = match (a().lua_type)(l, idx) {
        LUA_TNIL => "nil".into(),
        LUA_TBOOLEAN => ((a().lua_toboolean)(l, idx) != 0).to_string(),
        LUA_TNUMBER => proto::lua_number((a().lua_tonumber)(l, idx)),
        LUA_TSTRING => {
            let b = bytes_at(l, idx);
            let s = proto::lua_quote(&b, proto::MAX_PREVIEW);
            return (t, s, (b.len() > proto::MAX_PREVIEW).then_some(b.len()));
        }
        _ => format!("{t}: {:p}", (a().lua_topointer)(l, idx)),
    };
    (t, text, None)
}

/// Text for logpoint interpolation: strings verbatim (bounded), others as previews.
unsafe fn display(l: L, idx: c_int) -> String {
    match (a().lua_type)(l, idx) {
        LUA_TSTRING => {
            let b = bytes_at(l, idx);
            String::from_utf8_lossy(&b[..b.len().min(proto::MAX_PREVIEW)]).into_owned()
        }
        _ => preview(l, idx).1,
    }
}

unsafe fn depth(l: L) -> i32 {
    let mut ar = lua_Debug::default();
    let mut n = 0;
    while n < 100_000 && (d().lua_getstack)(l, n, &mut ar) != 0 {
        n += 1;
    }
    n
}

unsafe fn thread_done(l: L) -> bool {
    let mut ar = lua_Debug::default();
    (a().lua_status)(l) == LUA_OK && (d().lua_getstack)(l, 0, &mut ar) == 0
}

/// Normalized runtime path of a frame/function source label, if it names a file.
unsafe fn runtime_path(ar: &lua_Debug) -> Option<String> {
    match ar.source.is_null() {
        true => None,
        false => path::label_path(CStr::from_ptr(ar.source).to_bytes()),
    }
}

fn int(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}
fn string(v: &Value, k: &str) -> R<Option<String>> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => fail(err::INVALID_ARGUMENT, format!("{k} must be a string")),
    }
}

fn identifier_key(k: &str) -> bool {
    proto::is_identifier(k.as_bytes())
}

// ---------- session state ----------

#[derive(Clone, PartialEq)]
enum Loc {
    Line { source: String, line: i64 },
    FnName(String),
    FnDef { source: String, line: i64 },
}

#[derive(Clone)]
struct Bp {
    id: u64,
    loc: Loc,
    enabled: bool,
    cond: Option<String>,
    hit: Option<(Hit, String)>,
    log: Option<(Vec<Part>, String)>,
    count: u64,
    verified: bool,
    message: Option<String>,
    /// Resolved function identity for name selectors (pointer + registry ref keeping it alive).
    fptr: usize,
    fref: c_int,
    /// Owner of the breakpoint: `break` op, DAP setBreakpoints or setFunctionBreakpoints.
    /// Replace operations only touch their own group (REPL `:break` additions survive).
    group: Group,
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    Direct,
    Source,
    Function,
}

impl Bp {
    fn json(&self) -> Value {
        let mut v = json!({
            "id": self.id, "enabled": self.enabled, "verified": self.verified,
            "hitCount": self.count,
        });
        match &self.loc {
            Loc::Line { source, line } => {
                v["source"] = json!(source);
                v["line"] = json!(line);
            }
            Loc::FnName(n) => v["function"] = json!({"name": n}),
            Loc::FnDef { source, line } => {
                v["function"] = json!({"source": source, "linedefined": line})
            }
        }
        if let Some(m) = &self.message {
            v["message"] = json!(m);
        }
        if let Some(c) = &self.cond {
            v["condition"] = json!(c);
        }
        if let Some((_, h)) = &self.hit {
            v["hitCondition"] = json!(h);
        }
        if let Some((_, m)) = &self.log {
            v["logMessage"] = json!(m);
        }
        v
    }
    fn is_line(&self) -> bool {
        matches!(self.loc, Loc::Line { .. })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum StepKind {
    In,
    Over,
    Out,
}

struct Step {
    kind: StepKind,
    l: L,
    tref: c_int,
    depth: i32,
    returned: bool,
}

enum Handle {
    Locals(i32),
    Varargs(i32),
    Upvalues(i32),
    /// Value kept in the stop holder at `slot`; tables get a key snapshot on first expansion.
    Val {
        slot: c_int,
        eval: Option<String>,
        snap: Option<(c_int, usize, bool)>,
    },
}

struct Stop {
    id: u64,
    l: L,
    tref: c_int,
    tid: i64,
    reason: String,
    holder: c_int,
    next_slot: c_int,
    rev: u64,
    handles: Vec<Handle>,
}

struct Console {
    id: u64,
    name: String,
    args: Vec<String>,
}

struct Sess {
    conn: u64,
    configured: bool,
    stop_on_attach: bool,
    attach_stop: bool,
    prior: (lua_Hook, c_int, c_int),
    hook_owned: bool,
    jit_prior: bool,
    proxy_ref: c_int,
    threads_ref: c_int,
    next_tid: i64,
    bps: BTreeMap<u64, Bp>,
    next_bp: u64,
    step: Option<Step>,
    func_entry: Option<(L, Vec<u64>)>,
    stop_seq: u64,
    value_rev: u64,
    stop: Option<Stop>,
    console: Vec<Console>,
    next_cmd: u64,
    warnings: Vec<String>,
    think_n: u64,
}

pub struct Dbg {
    sh: Arc<Shared>,
    path: PathBuf,
    sess: Option<Sess>,
}

enum Flow {
    Stay,
    Resume,
}

// ---------- entry points ----------

/// Starts the listener for `realm` if its endpoint file is configured. Returns a message for
/// the console (never containing the token).
/// # Safety
/// Owner thread only; `_l` must be the live realm state.
pub unsafe fn open(_l: L, realm: &'static str, generation: u64) -> Result<String, String> {
    let var = match realm {
        "server" => "GDEBUG_SERVER_ENDPOINT_FILE",
        "client" => "GDEBUG_CLIENT_ENDPOINT_FILE",
        r => return Err(format!("gdebug: unsupported realm {r}")),
    };
    let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) else {
        return Ok(format!(
            "gdebug: inert; set {var} to an owner-controlled file path before starting GMod to enable attach"
        ));
    };
    let path = PathBuf::from(path);
    sys::debug_api().map_err(|e| format!("gdebug: {e}"))?;
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| format!("gdebug: OS randomness unavailable: {e}"))?;
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let sh = Shared::new(token.clone(), realm, generation);
    let (port, _h) = net::listen(sh.clone()).map_err(|e| format!("gdebug: listen failed: {e}"))?;
    let desc = Descriptor {
        protocol: proto::PROTOCOL.into(),
        host: "127.0.0.1".into(),
        port,
        realm: realm.into(),
        pid: std::process::id(),
        generation,
        token,
    };
    if let Err(e) = descriptor::write_private(&path, &desc.to_json().to_string()) {
        sh.closing.store(true, Ordering::Release);
        return Err(format!("gdebug: refusing insecure endpoint file: {e}"));
    }
    let dbg = Dbg {
        sh,
        path: path.clone(),
        sess: None,
    };
    dbg.publish();
    DBG.with(|c| *c.borrow_mut() = Some(dbg));
    Ok(format!(
        "gdebug: {realm} endpoint 127.0.0.1:{port} descriptor {}",
        path.display()
    ))
}

/// Owner-thread pump (GMod `Think`).
/// # Safety
/// Owner thread only, with a live state.
pub unsafe fn think(l: L) {
    with(|d| unsafe { d.on_think(l) });
}

/// Module close: clean up the session, stop listening, remove our descriptor.
/// # Safety
/// Owner thread only, before the state is destroyed.
pub unsafe fn close(l: L) {
    let dbg = DBG.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.take()));
    if let Some(mut dbg) = dbg {
        if dbg.sess.is_some() {
            unsafe { dbg.cleanup(l, "target closed") };
        }
        dbg.sh.closing.store(true, Ordering::Release);
        if let Some(c) = dbg.sh.controller_conn() {
            dbg.sh.release(c);
        }
        descriptor::remove_owned(&dbg.path, dbg.sh.generation, &dbg.sh.token);
    }
}

fn with(f: impl FnOnce(&mut Dbg)) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        DBG.with(|c| {
            // Busy borrow = debugger-internal execution (evaluation, command): suppress.
            if let Ok(mut g) = c.try_borrow_mut()
                && let Some(d) = g.as_mut()
            {
                f(d)
            }
        })
    }));
}

unsafe extern "C-unwind" fn hook(l: L, ar: *mut lua_Debug) {
    with(|d| unsafe { d.on_hook(l, ar) });
}

// ---------- the debugger ----------

impl Dbg {
    fn publish(&self) {
        let mut v = json!({
            "protocol": proto::PROTOCOL, "version": env!("CARGO_PKG_VERSION"),
            "realm": self.sh.realm, "generation": self.sh.generation, "mode": "reliable",
            "attached": self.sess.is_some(), "limits": proto::limits(),
        });
        match &self.sess {
            None => v["state"] = json!("idle"),
            Some(s) => {
                v["state"] = json!(match (&s.stop, s.configured) {
                    (Some(_), _) => "stopped",
                    (None, false) => "configuring",
                    (None, true) => "running",
                });
                v["configured"] = json!(s.configured);
                v["valueRevision"] = json!(s.value_rev);
                v["breakpoints"] = json!({
                    "total": s.bps.len(),
                    "verified": s.bps.values().filter(|b| b.verified).count(),
                    "enabled": s.bps.values().filter(|b| b.enabled).count(),
                });
                v["hook"] = json!({"owned": s.hook_owned, "replacedPrior": s.prior.0.is_some()});
                v["jit"] = json!({"priorEnabled": s.jit_prior, "disabledByDebugger": true});
                v["pendingConsole"] = json!(s.console.len());
                v["warnings"] = json!(s.warnings);
                v["stepping"] = json!(s.step.is_some());
                if let Some(st) = &s.stop {
                    v["stopId"] = json!(format!("stop-{}", st.id));
                    v["reason"] = json!(st.reason);
                    v["threadId"] = json!(st.tid);
                }
            }
        }
        self.sh.set_status(v);
    }

    fn event(&self, name: &str, data: Value) {
        if let Some(s) = &self.sess {
            self.sh.event(s.conn, name, data);
        }
    }

    unsafe fn on_think(&mut self, l: L) {
        (a().lua_checkstack)(l, 40);
        self.service(l, false);
        let Some(s) = &mut self.sess else { return };
        s.think_n += 1;
        if s.hook_owned && !ours((d().lua_gethook)(l)) {
            s.hook_owned = false;
            let w = "hookOwnershipLost: another component replaced the debugger hook; breakpoints and pause no longer work and detach will not restore the prior hook";
            s.warnings.push(w.into());
            self.event(
                "warning",
                json!({"code": "hookOwnershipLost", "message": w}),
            );
            self.publish();
        }
        if self.sess.as_ref().is_some_and(|s| s.think_n % 30 == 0) {
            self.resolve_fns(l);
        }
        self.flush_console(l);
        if self.sh.pause.load(Ordering::Acquire) {
            self.rearm(l);
        }
    }

    unsafe fn on_hook(&mut self, l: L, ar: *mut lua_Debug) {
        (a().lua_checkstack)(l, 40);
        if self.sh.has_work() {
            self.service(l, false);
        }
        let Some(s) = &mut self.sess else { return };
        if !s.configured || s.stop.is_some() {
            return;
        }
        let ev = (*ar).event;
        if ev == LUA_HOOKCALL {
            return self.on_call(l, ar);
        }
        if ev == LUA_HOOKRET {
            if let Some(st) = &mut s.step
                && st.l == l
                && depth(l) <= st.depth
            {
                st.returned = true;
            }
            return;
        }
        // Line or count checkpoint. Order: pause, function entry, breakpoint, step.
        if self.sh.pause.swap(false, Ordering::AcqRel) || std::mem::take(&mut s.attach_stop) {
            return self.stop(l, "pause", None, vec![]);
        }
        if ev != LUA_HOOKLINE {
            return;
        }
        if let Some((fl, ids)) = s.func_entry.take() {
            if fl == l {
                let hit = self.hits(l, ids);
                if !hit.is_empty() {
                    return self.stop(l, "function breakpoint", None, hit);
                }
            } else {
                self.sess.as_mut().unwrap().func_entry = Some((fl, ids));
            }
        }
        let line = (*ar).currentline as i64;
        let s = self.sess.as_ref().unwrap();
        if s.bps
            .values()
            .any(|b| b.enabled && matches!(&b.loc, Loc::Line { line: x, .. } if *x == line))
        {
            (d().lua_getinfo)(l, c"S".as_ptr(), ar);
            if let Some(rt) = runtime_path(&*ar) {
                let ids: Vec<u64> = s
                    .bps
                    .values()
                    .filter(|b| b.enabled && matches!(&b.loc, Loc::Line { source, line: x } if *x == line && path::runtime_matches(source, &rt)))
                    .map(|b| b.id)
                    .collect();
                if !ids.is_empty() {
                    let hit = self.hits(l, ids);
                    if !hit.is_empty() {
                        return self.stop(l, "breakpoint", None, hit);
                    }
                }
            }
        }
        let s = self.sess.as_mut().unwrap();
        if let Some(st) = &s.step {
            let (go, why) = if st.returned {
                (
                    true,
                    Some("stepped out of the original activation".to_owned()),
                )
            } else if st.l == l {
                let dd = depth(l);
                (
                    match st.kind {
                        StepKind::In => true,
                        StepKind::Over => dd <= st.depth,
                        StepKind::Out => dd < st.depth,
                    },
                    None,
                )
            } else if thread_done(st.l) {
                (
                    true,
                    Some(
                        "the stepped coroutine completed; stopped at the next available checkpoint"
                            .to_owned(),
                    ),
                )
            } else {
                (false, None)
            };
            if go {
                self.stop(l, "step", why, vec![])
            }
        }
    }

    unsafe fn on_call(&mut self, l: L, ar: *mut lua_Debug) {
        let s = self.sess.as_ref().unwrap();
        if !s.bps.values().any(|b| b.enabled && !b.is_line()) {
            return;
        }
        (d().lua_getinfo)(l, c"Sf".as_ptr(), ar);
        let ptr = (a().lua_topointer)(l, -1) as usize;
        (a().lua_settop)(l, -2);
        let rt = runtime_path(&*ar);
        let ld = (*ar).linedefined as i64;
        let ids: Vec<u64> = s
            .bps
            .values()
            .filter(|b| b.enabled)
            .filter(|b| match &b.loc {
                Loc::FnName(_) => b.fptr != 0 && b.fptr == ptr,
                Loc::FnDef { source, line } => {
                    *line == ld
                        && rt
                            .as_deref()
                            .is_some_and(|r| path::runtime_matches(source, r))
                }
                Loc::Line { .. } => false,
            })
            .map(|b| b.id)
            .collect();
        if !ids.is_empty() && cs((*ar).what) != "C" {
            let s = self.sess.as_mut().unwrap();
            s.func_entry = Some((l, ids));
            self.rearm(l);
        }
    }

    /// Counts hits and runs hit rules/conditions/logpoints; returns IDs that request a stop.
    unsafe fn hits(&mut self, l: L, ids: Vec<u64>) -> Vec<u64> {
        let mut stop = vec![];
        for id in ids {
            let Some(b) = self.sess.as_mut().unwrap().bps.get_mut(&id) else {
                continue;
            };
            b.count += 1;
            if !b.verified {
                b.verified = true;
                b.message = None;
                let j = b.json();
                self.event(
                    "breakpointChanged",
                    json!({"reason": "verified", "breakpoint": j}),
                );
            }
            let b = self.sess.as_ref().unwrap().bps[&id].clone();
            if let Some((h, _)) = b.hit
                && !h.matches(b.count)
            {
                continue;
            }
            if let Some(c) = &b.cond {
                match self.eval_values(
                    l,
                    0,
                    &format!("return ({c})"),
                    c"=(gdebug condition)",
                    |l, base, n| n > 0 && (a().lua_toboolean)(l, base) != 0,
                ) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err((_, m)) => {
                        self.disable_bp(id, format!("condition error: {m}"));
                        continue;
                    }
                }
            }
            if let Some((parts, _)) = &b.log {
                let mut out = String::new();
                let mut bad = None;
                for p in parts {
                    match p {
                        Part::Text(t) => out.push_str(t),
                        Part::Expr(e) => match self.eval_values(
                            l,
                            0,
                            &format!("return ({e})"),
                            c"=(gdebug logpoint)",
                            |l, base, n| match n {
                                0 => "nil".to_owned(),
                                _ => display(l, base),
                            },
                        ) {
                            Ok(s) => out.push_str(&s),
                            Err((_, m)) => {
                                bad = Some(m);
                                break;
                            }
                        },
                    }
                }
                match bad {
                    Some(m) => self.disable_bp(id, format!("logpoint error: {m}")),
                    None => self.event(
                        "output",
                        json!({"category": "console", "output": out + "\n", "breakpointId": id}),
                    ),
                }
                continue;
            }
            stop.push(id);
        }
        stop
    }

    fn disable_bp(&mut self, id: u64, msg: String) {
        let s = self.sess.as_mut().unwrap();
        if let Some(b) = s.bps.get_mut(&id) {
            b.enabled = false;
            b.message = Some(msg);
            let j = b.json();
            self.event(
                "breakpointChanged",
                json!({"reason": "disabled", "breakpoint": j}),
            );
        }
    }

    unsafe fn rearm(&mut self, l: L) {
        let Some(s) = &self.sess else { return };
        if !s.hook_owned || !ours((d().lua_gethook)(l)) {
            return;
        }
        let mut m = LUA_MASKCOUNT;
        if s.configured {
            let line = s.bps.values().any(|b| b.enabled && b.is_line())
                || s.step.is_some()
                || s.func_entry.is_some()
                || s.attach_stop
                || self.sh.pause.load(Ordering::Acquire);
            if line {
                m |= LUA_MASKLINE;
            }
            if s.bps.values().any(|b| b.enabled && !b.is_line()) {
                m |= LUA_MASKCALL;
            }
            if s.step.is_some() {
                m |= LUA_MASKRET;
            }
        }
        (d().lua_sethook)(l, HOOK, m, COUNT);
    }

    // ----- servicing requests -----

    /// Processes lost connections, queued jobs, detach and pause requests.
    unsafe fn service(&mut self, l: L, stopped: bool) -> Flow {
        let sh = self.sh.clone();
        let lost = sh.lost.swap(0, Ordering::AcqRel);
        if lost != 0 {
            sh.jobs.lock().unwrap().retain(|j| j.conn != lost);
            if self.sess.as_ref().is_some_and(|s| s.conn == lost) {
                self.cleanup(l, "connection lost or lease expired");
            }
            sh.release(lost);
        }
        let jobs = sh.take_jobs();
        let mut flow = Flow::Stay;
        let mut rest = jobs.into_iter();
        for j in rest.by_ref() {
            if let Flow::Resume = self.handle(l, &j, stopped) {
                flow = Flow::Resume;
                break;
            }
        }
        // Requests after a resume are handled in the new state.
        let rest: Vec<Job> = rest.collect();
        if !rest.is_empty() {
            let mut q = sh.jobs.lock().unwrap();
            for j in rest.into_iter().rev() {
                q.push_front(j);
            }
            sh.pending.fetch_add(1, Ordering::AcqRel);
        }
        let detaches: Vec<_> = sh.detach_ids.lock().unwrap().drain(..).collect();
        for (conn, id) in detaches {
            let r = match self.sess.as_ref().map(|s| s.conn) {
                Some(c) if c == conn => self.cleanup(l, "detached"),
                Some(_) => {
                    sh.reply(
                        conn,
                        id,
                        fail(err::BUSY, "session owned by another controller"),
                    );
                    continue;
                }
                None => json!({"detached": false, "message": "no active session"}),
            };
            sh.reply(conn, id, Ok(r));
        }
        let pauses: Vec<_> = sh.pause_ids.lock().unwrap().drain(..).collect();
        for (conn, id) in pauses {
            let r = match &self.sess {
                None => {
                    sh.pause.store(false, Ordering::Release);
                    fail(err::NOT_ATTACHED, "no active session")
                }
                Some(s) if s.conn != conn => fail(err::BUSY, "session owned by another controller"),
                Some(s) => match &s.stop {
                    Some(st) => {
                        sh.pause.store(false, Ordering::Release);
                        Ok(json!({"state": "stopped", "stopId": format!("stop-{}", st.id)}))
                    }
                    None => Ok(json!({"state": "pending"})),
                },
            };
            sh.reply(conn, id, r);
        }
        if stopped && self.sess.as_ref().is_none_or(|s| s.stop.is_none()) {
            return Flow::Resume;
        }
        flow
    }

    unsafe fn handle(&mut self, l: L, j: &Job, stopped: bool) -> Flow {
        let sh = self.sh.clone();
        if j.op == "attach" {
            let r = self.attach(l, j);
            sh.reply(j.conn, j.id, r);
            return Flow::Stay;
        }
        match &self.sess {
            None => {
                sh.reply(j.conn, j.id, fail(err::NOT_ATTACHED, "attach first"));
                return Flow::Stay;
            }
            Some(s) if s.conn != j.conn => {
                sh.reply(
                    j.conn,
                    j.id,
                    fail(err::BUSY, "session owned by another controller"),
                );
                return Flow::Stay;
            }
            _ => {}
        }
        let needs_stop = matches!(
            j.op.as_str(),
            "stack" | "scopes" | "variables" | "evaluate" | "continue" | "step"
        );
        if needs_stop && !stopped {
            sh.reply(
                j.conn,
                j.id,
                fail(
                    err::NOT_STOPPED,
                    "target is running; pause or wait for a breakpoint",
                ),
            );
            return Flow::Stay;
        }
        let r = match j.op.as_str() {
            "configurationDone" => self.configuration_done(l),
            "break" => self.op_break(l, &j.args),
            "clear" => self.op_clear(l, &j.args),
            "setBreakpoints" => self.op_set_bps(l, &j.args),
            "setFunctionBreakpoints" => self.op_set_fn_bps(l, &j.args),
            "breakpoints" => Ok(
                json!({"breakpoints": self.sess.as_ref().unwrap().bps.values().map(Bp::json).collect::<Vec<_>>()}),
            ),
            "threads" => self.op_threads(l),
            "stack" => self.op_stack(l, &j.args),
            "scopes" => self.op_scopes(l, &j.args),
            "variables" => self.op_variables(l, &j.args),
            "evaluate" => self.op_evaluate(l, &j.args),
            "command" => self.op_command(l, &j.args, stopped),
            "source" => self.op_source(l, &j.args),
            "continue" | "step" => {
                let r = self.op_resume(l, &j.op, &j.args);
                let ok = r.is_ok();
                sh.reply(j.conn, j.id, r);
                if ok {
                    self.event("continued", json!({"allThreadsContinued": true}));
                    return Flow::Resume;
                }
                return Flow::Stay;
            }
            op => fail(err::INVALID_ARGUMENT, format!("unknown operation {op}")),
        };
        sh.reply(j.conn, j.id, r);
        self.publish();
        Flow::Stay
    }

    // ----- attach / detach -----

    unsafe fn attach(&mut self, l: L, j: &Job) -> R<Value> {
        let args = &j.args;
        if self.sess.is_some() {
            return fail(err::BUSY, "a session is already attached");
        }
        if args.get("allowHookReplacement") != Some(&Value::Bool(true)) {
            return fail(
                err::INVALID_ARGUMENT,
                "attach requires explicit allowHookReplacement: true (replaces the Lua debug hook; GMod's infinite-loop watchdog and profilers are disabled while attached)",
            );
        }
        if let Some(r) = string(args, "realm")?
            && r != self.sh.realm
        {
            return fail(
                err::UNSUPPORTED_TARGET,
                format!("endpoint is the {} realm, not {r}", self.sh.realm),
            );
        }
        if let Some(m) = string(args, "mode")?
            && m != "reliable"
        {
            return fail(
                err::UNSUPPORTED_TARGET,
                format!("mode {m} is not supported; v1 supports reliable"),
            );
        }
        let configured = args
            .get("configure")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let stop_on_attach = args
            .get("stopOnAttach")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let top = (a().lua_gettop)(l);
        let prior = (
            (d().lua_gethook)(l),
            (d().lua_gethookmask)(l),
            (d().lua_gethookcount)(l),
        );
        if ours(prior.0) {
            return fail(
                err::HOOK_CONFLICT,
                "debugger hook already installed by a stale session",
            );
        }
        // Protected setup: env proxy factory, weak thread table, JIT policy.
        const SETUP: &str = r#"
            local setmetatable = setmetatable
            local function proxy(vals, present, env)
              local dirty = {}
              return setmetatable({}, {
                __index = function(_, k) if present[k] then return vals[k] end return env[k] end,
                __newindex = function(_, k, v)
                  if present[k] then vals[k] = v; dirty[k] = true else env[k] = v end
                end,
              }), dirty
            end
            local j = jit
            if type(j) ~= "table" or type(j.status) ~= "function" or type(j.off) ~= "function"
              or type(j.flush) ~= "function" then
              error("LuaJIT control (jit.status/off/flush) unavailable", 0)
            end
            local was = j.status()
            j.off(); j.flush()
            return proxy, setmetatable({}, {__mode = "k"}), was and true or false
        "#;
        let r = load(l, SETUP, c"=(gdebug attach)").and_then(|_| pcall(l, 0, 3));
        if let Err(m) = r {
            (a().lua_settop)(l, top);
            return fail(err::UNSUPPORTED_TARGET, format!("attach setup failed: {m}"));
        }
        let jit_prior = (a().lua_toboolean)(l, -1) != 0;
        (a().lua_settop)(l, -2);
        let threads_ref = make_ref(l);
        let proxy_ref = make_ref(l);
        (d().lua_sethook)(l, HOOK, LUA_MASKCOUNT, COUNT);
        if !ours((d().lua_gethook)(l)) {
            // Roll back this attempt only.
            (d().lua_sethook)(l, prior.0, prior.1, prior.2);
            self.restore_jit(l, jit_prior);
            let (mut p, mut t) = (proxy_ref, threads_ref);
            unref(l, &mut p);
            unref(l, &mut t);
            return fail(err::HOOK_CONFLICT, "could not install the debug hook");
        }
        let mut warnings = vec![];
        if prior.0.is_some() {
            warnings.push("replaced an existing Lua debug hook (likely GMod's infinite-loop watchdog or a profiler); it is restored on detach".to_owned());
        }
        self.sess = Some(Sess {
            conn: j.conn,
            configured,
            stop_on_attach,
            attach_stop: configured && stop_on_attach,
            prior,
            hook_owned: true,
            jit_prior,
            proxy_ref,
            threads_ref,
            next_tid: 1,
            bps: BTreeMap::new(),
            next_bp: 1,
            step: None,
            func_entry: None,
            stop_seq: 0,
            value_rev: 0,
            stop: None,
            console: vec![],
            next_cmd: 1,
            warnings: warnings.clone(),
            think_n: 0,
        });
        self.thread_id(l, l); // main/first-seen thread gets ID 1
        self.rearm(l);
        self.publish();
        for w in &warnings {
            self.event("warning", json!({"code": "hookReplaced", "message": w}));
        }
        Ok(json!({
            "state": if configured { "running" } else { "configuring" },
            "configured": configured, "realm": self.sh.realm, "generation": self.sh.generation,
            "mode": "reliable", "replacedHook": prior.0.is_some(), "jitPriorEnabled": jit_prior,
            "warnings": warnings,
        }))
    }

    unsafe fn restore_jit(&self, l: L, prior: bool) -> bool {
        if !prior {
            return true;
        }
        let top = (a().lua_gettop)(l);
        let ok = load(l, "jit.on()", c"=(gdebug detach)")
            .and_then(|_| pcall(l, 0, 0))
            .is_ok();
        (a().lua_settop)(l, top);
        ok
    }

    unsafe fn configuration_done(&mut self, l: L) -> R<Value> {
        let s = self.sess.as_mut().unwrap();
        let first = !s.configured;
        s.configured = true;
        if first && s.stop_on_attach {
            s.attach_stop = true;
        }
        self.rearm(l);
        Ok(json!({"configured": true}))
    }

    /// Idempotent session cleanup on the owner thread. Returns the restoration report.
    unsafe fn cleanup(&mut self, l: L, why: &str) -> Value {
        let Some(mut s) = self.sess.take() else {
            return json!({"detached": false});
        };
        for c in s.console.drain(..) {
            self.sh.event(s.conn, "commandResult", json!({"commandId": c.id, "status": "canceled", "message": "session ended before submission"}));
        }
        let hook_restored = s.hook_owned && ours((d().lua_gethook)(l));
        if hook_restored {
            (d().lua_sethook)(l, s.prior.0, s.prior.1, s.prior.2);
        }
        let jit_restored = self.restore_jit(l, s.jit_prior);
        for b in s.bps.values_mut() {
            unref(l, &mut b.fref);
        }
        if let Some(mut st) = s.step.take() {
            unref(l, &mut st.tref);
        }
        if let Some(mut st) = s.stop.take() {
            unref(l, &mut st.holder);
            unref(l, &mut st.tref);
        }
        unref(l, &mut s.proxy_ref);
        unref(l, &mut s.threads_ref);
        self.sh.pause.store(false, Ordering::Release);
        let mut warnings = vec![];
        if !hook_restored {
            warnings.push("hook ownership was lost; the prior hook was not restored".to_owned());
        }
        if !jit_restored {
            warnings.push("failed to re-enable JIT".to_owned());
        }
        self.sh.event(s.conn, "terminated", json!({"reason": why}));
        self.publish();
        json!({"detached": true, "hookRestored": hook_restored, "jitRestored": jit_restored, "warnings": warnings})
    }

    // ----- breakpoints -----

    unsafe fn compile_check(&self, l: L, code: &str) -> Result<(), String> {
        let r = load(l, code, c"=(gdebug)");
        if r.is_ok() {
            (a().lua_settop)(l, -2);
        }
        r
    }

    unsafe fn parse_bp(&mut self, l: L, v: &Value, loc: Loc) -> R<Bp> {
        let cond = string(v, "condition")?.filter(|c| !c.trim().is_empty());
        if let Some(c) = &cond {
            if c.len() > proto::MAX_EXPRESSION {
                return fail(err::LIMIT, "condition too long");
            }
            self.compile_check(l, &format!("return ({c})"))
                .map_err(|m| (err::COMPILE, format!("condition: {m}")))?;
        }
        let hit = match string(v, "hitCondition")?.filter(|c| !c.trim().is_empty()) {
            Some(h) => Some((Hit::parse(&h).map_err(|m| (err::INVALID_ARGUMENT, m))?, h)),
            None => None,
        };
        let log = match string(v, "logMessage")? {
            Some(m) => {
                let parts = proto::bp::parse_log(&m)
                    .map_err(|e| (err::INVALID_ARGUMENT, format!("logMessage: {e}")))?;
                for p in &parts {
                    if let Part::Expr(e) = p {
                        self.compile_check(l, &format!("return ({e})"))
                            .map_err(|m| (err::COMPILE, format!("logMessage {{{e}}}: {m}")))?;
                    }
                }
                Some((parts, m))
            }
            None => None,
        };
        let enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let message = match &loc {
            Loc::Line { .. } | Loc::FnDef { .. } => {
                Some("pending: verified when the target executes this location".into())
            }
            Loc::FnName(_) => None,
        };
        Ok(Bp {
            id: 0,
            loc,
            enabled,
            cond,
            hit,
            log,
            count: 0,
            verified: false,
            message,
            fptr: 0,
            fref: LUA_NOREF,
            group: Group::Direct,
        })
    }

    fn parse_loc(v: &Value) -> R<Loc> {
        let f = v.get("function").filter(|f| !f.is_null());
        let src = string(v, "source")?;
        let line = v.get("line").filter(|x| !x.is_null());
        match (f, src, line) {
            (Some(f), None, None) => match (
                string(f, "name")?,
                string(f, "source")?,
                int(f, "linedefined"),
            ) {
                (Some(n), None, None) => {
                    if n.split(['.', ':']).any(|s| !identifier_key(s)) {
                        return fail(
                            err::INVALID_ARGUMENT,
                            format!("invalid function name {n:?}"),
                        );
                    }
                    Ok(Loc::FnName(n))
                }
                (None, Some(s), Some(ld)) if ld >= 0 => Ok(Loc::FnDef {
                    source: path::normalize(&s),
                    line: ld,
                }),
                _ => fail(
                    err::INVALID_ARGUMENT,
                    "function selector needs exactly name, or source + linedefined",
                ),
            },
            (None, Some(s), Some(l)) => match l.as_i64() {
                Some(n) if n > 0 => Ok(Loc::Line {
                    source: path::normalize(&s),
                    line: n,
                }),
                _ => fail(err::INVALID_ARGUMENT, "line must be a positive integer"),
            },
            _ => fail(
                err::INVALID_ARGUMENT,
                "exactly one selector required: source + line, or function",
            ),
        }
    }

    unsafe fn add_bp(&mut self, l: L, mut b: Bp, id: Option<u64>) -> R<Value> {
        let s = self.sess.as_mut().unwrap();
        let id = match id {
            Some(id) => {
                let Some(old) = s.bps.get_mut(&id) else {
                    return fail(err::INVALID_ARGUMENT, format!("unknown breakpoint {id}"));
                };
                // Enabled-only changes preserve counts and verification.
                if old.loc == b.loc
                    && old.cond == b.cond
                    && old.hit.as_ref().map(|h| &h.1) == b.hit.as_ref().map(|h| &h.1)
                {
                    b.count = old.count;
                }
                if old.loc == b.loc && old.verified {
                    b.verified = true;
                    b.message = None;
                }
                unref(l, &mut old.fref);
                b.group = old.group;
                id
            }
            None => {
                if s.bps.len() >= proto::MAX_BREAKPOINTS {
                    return fail(
                        err::LIMIT,
                        format!("at most {} breakpoints", proto::MAX_BREAKPOINTS),
                    );
                }
                s.next_bp += 1;
                s.next_bp - 1
            }
        };
        b.id = id;
        s.bps.insert(id, b);
        self.resolve_fns(l);
        self.rearm(l);
        Ok(self.sess.as_ref().unwrap().bps[&id].json())
    }

    unsafe fn op_break(&mut self, l: L, v: &Value) -> R<Value> {
        let loc = Self::parse_loc(v)?;
        let b = self.parse_bp(l, v, loc)?;
        let id = match v.get("id") {
            None | Some(Value::Null) => None,
            Some(x) => Some(x.as_u64().ok_or((
                err::INVALID_ARGUMENT,
                "id must be a positive integer".to_owned(),
            ))?),
        };
        self.add_bp(l, b, id)
    }

    unsafe fn op_clear(&mut self, l: L, v: &Value) -> R<Value> {
        let id = v.get("id").and_then(Value::as_u64);
        let src = string(v, "source")?;
        let all = v.get("all") == Some(&Value::Bool(true));
        let s = self.sess.as_mut().unwrap();
        let keep: Box<dyn Fn(&Bp) -> bool> = match (id, src, all) {
            (Some(id), None, false) => Box::new(move |b| b.id != id),
            (None, Some(src), false) => {
                let src = path::normalize(&src);
                Box::new(
                    move |b| !matches!(&b.loc, Loc::Line { source, .. } | Loc::FnDef { source, .. } if *source == src),
                )
            }
            (None, None, true) => Box::new(|_| false),
            _ => {
                return fail(
                    err::INVALID_ARGUMENT,
                    "exactly one of id, source, all: true required",
                );
            }
        };
        let removed: Vec<u64> = s.bps.values().filter(|b| !keep(b)).map(|b| b.id).collect();
        for id in &removed {
            let mut b = s.bps.remove(id).unwrap();
            unref(l, &mut b.fref);
        }
        self.rearm(l);
        Ok(json!({"removed": removed}))
    }

    /// Atomic per-source replacement (DAP setBreakpoints).
    unsafe fn op_set_bps(&mut self, l: L, v: &Value) -> R<Value> {
        let Some(src) = string(v, "source")? else {
            return fail(err::INVALID_ARGUMENT, "source required");
        };
        let src = path::normalize(&src);
        let list = v
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let s = self.sess.as_ref().unwrap();
        let mine = |b: &Bp| {
            b.group == Group::Source && matches!(&b.loc, Loc::Line { source, .. } if *source == src)
        };
        let others = s.bps.values().filter(|b| !mine(b)).count();
        if others + list.len() > proto::MAX_BREAKPOINTS {
            return fail(
                err::LIMIT,
                format!("at most {} breakpoints", proto::MAX_BREAKPOINTS),
            );
        }
        // Old per-source line breakpoints keep counts when an identical spec is re-sent.
        let old: Vec<Bp> = s.bps.values().filter(|b| mine(b)).cloned().collect();
        let mut staged = vec![];
        for item in &list {
            let line = int(item, "line").filter(|n| *n > 0).ok_or((
                err::INVALID_ARGUMENT,
                "line must be a positive integer".to_owned(),
            ))?;
            let loc = Loc::Line {
                source: src.clone(),
                line,
            };
            staged.push(match self.parse_bp(l, item, loc.clone()) {
                Ok(mut b) => {
                    if let Some(o) = old.iter().find(|o| {
                        o.loc == b.loc
                            && o.cond == b.cond
                            && o.hit.as_ref().map(|h| &h.1) == b.hit.as_ref().map(|h| &h.1)
                    }) {
                        b.count = o.count;
                        b.verified = o.verified;
                        b.message = o.message.clone();
                    }
                    b
                }
                // Invalid entries are reported unverified and never armed.
                Err((_, m)) => Bp {
                    id: 0,
                    loc,
                    enabled: false,
                    cond: None,
                    hit: None,
                    log: None,
                    count: 0,
                    verified: false,
                    message: Some(m),
                    fptr: 0,
                    fref: LUA_NOREF,
                    group: Group::Direct,
                },
            });
        }
        let s = self.sess.as_mut().unwrap();
        s.bps.retain(|_, b| {
            !(b.group == Group::Source
                && matches!(&b.loc, Loc::Line { source, .. } if *source == src))
        });
        let mut out = vec![];
        for mut b in staged {
            b.group = Group::Source;
            b.id = s.next_bp;
            s.next_bp += 1;
            out.push(b.json());
            s.bps.insert(b.id, b);
        }
        self.rearm(l);
        Ok(json!({"breakpoints": out}))
    }

    /// Atomic replacement of all function breakpoints.
    unsafe fn op_set_fn_bps(&mut self, l: L, v: &Value) -> R<Value> {
        let list = v
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let s = self.sess.as_ref().unwrap();
        if s.bps
            .values()
            .filter(|b| b.group != Group::Function)
            .count()
            + list.len()
            > proto::MAX_BREAKPOINTS
        {
            return fail(
                err::LIMIT,
                format!("at most {} breakpoints", proto::MAX_BREAKPOINTS),
            );
        }
        let mut staged = vec![];
        for item in &list {
            // DAP shape {name} is accepted as well as {function: {...}}.
            let sel = match item.get("function") {
                Some(_) => item.clone(),
                None => json!({"function": {"name": item.get("name")}}),
            };
            staged.push(
                match Self::parse_loc(&sel).and_then(|loc| self.parse_bp(l, item, loc)) {
                    Ok(b) => b,
                    Err((_, m)) => Bp {
                        id: 0,
                        loc: Loc::FnName(item["name"].as_str().unwrap_or("?").into()),
                        enabled: false,
                        cond: None,
                        hit: None,
                        log: None,
                        count: 0,
                        verified: false,
                        message: Some(m),
                        fptr: 0,
                        fref: LUA_NOREF,
                        group: Group::Direct,
                    },
                },
            );
        }
        let s = self.sess.as_mut().unwrap();
        let old: Vec<u64> = s
            .bps
            .values()
            .filter(|b| b.group == Group::Function)
            .map(|b| b.id)
            .collect();
        for id in old {
            let mut b = s.bps.remove(&id).unwrap();
            unref(l, &mut b.fref);
        }
        let mut ids = vec![];
        for mut b in staged {
            b.group = Group::Function;
            b.id = s.next_bp;
            s.next_bp += 1;
            ids.push(b.id);
            s.bps.insert(b.id, b);
        }
        self.resolve_fns(l);
        self.rearm(l);
        let s = self.sess.as_ref().unwrap();
        Ok(json!({"breakpoints": ids.iter().map(|i| s.bps[i].json()).collect::<Vec<_>>()}))
    }

    /// (Re)binds name-selected function breakpoints to their current function values.
    unsafe fn resolve_fns(&mut self, l: L) {
        let top = (a().lua_gettop)(l);
        let mut changed = vec![];
        let s = self.sess.as_mut().unwrap();
        for b in s.bps.values_mut() {
            let Loc::FnName(n) = &b.loc else { continue };
            let segs: Vec<&str> = n.split(['.', ':']).collect();
            push_path(l, &segs);
            let is_lua_fn =
                (a().lua_type)(l, -1) == LUA_TFUNCTION && (a().lua_iscfunction)(l, -1) == 0;
            let ptr = if is_lua_fn {
                (a().lua_topointer)(l, -1) as usize
            } else {
                0
            };
            if ptr != b.fptr {
                unref(l, &mut b.fref);
                b.fptr = ptr;
                if ptr != 0 {
                    (a().lua_pushvalue)(l, -1);
                    b.fref = make_ref(l);
                }
                let was = b.verified;
                b.verified = ptr != 0;
                b.message = match ((a().lua_type)(l, -1), ptr) {
                    (_, p) if p != 0 => None,
                    (LUA_TFUNCTION, _) => Some("native (C) function; cannot break on entry".into()),
                    _ => Some("function not found; rebinds when it is defined".into()),
                };
                if was != b.verified || !was {
                    changed.push(b.json());
                }
            }
            (a().lua_settop)(l, top);
        }
        for j in changed {
            self.event(
                "breakpointChanged",
                json!({"reason": "rebound", "breakpoint": j}),
            );
        }
    }

    // ----- stopping -----

    /// Stable coroutine ID via a weak-keyed registry table.
    unsafe fn thread_id(&mut self, l: L, th: L) -> i64 {
        let s = self.sess.as_mut().unwrap();
        let top = (a().lua_gettop)(l);
        push_ref(l, s.threads_ref);
        if th == l {
            (a().lua_pushthread)(l);
        } else {
            (a().lua_pushthread)(th);
            (a().lua_xmove)(th, l, 1);
        }
        (a().lua_pushvalue)(l, -1);
        (a().lua_rawget)(l, -3);
        let id = match (a().lua_type)(l, -1) {
            LUA_TNUMBER => (a().lua_tonumber)(l, -1) as i64,
            _ => {
                (a().lua_settop)(l, -2);
                let id = s.next_tid;
                s.next_tid += 1;
                (a().lua_pushnumber)(l, id as f64);
                (a().lua_rawset)(l, -3);
                id
            }
        };
        (a().lua_settop)(l, top);
        id
    }

    unsafe fn stop(&mut self, l: L, reason: &str, desc: Option<String>, hit: Vec<u64>) {
        let tid = self.thread_id(l, l);
        let s = self.sess.as_mut().unwrap();
        if let Some(mut st) = s.step.take() {
            unref(l, &mut st.tref);
        }
        s.func_entry = None;
        s.attach_stop = false;
        self.sh.pause.store(false, Ordering::Release);
        s.stop_seq += 1;
        (a().lua_pushthread)(l);
        let tref = make_ref(l);
        (a().lua_createtable)(l, 0, 0);
        let holder = make_ref(l);
        let id = s.stop_seq;
        s.stop = Some(Stop {
            id,
            l,
            tref,
            tid,
            reason: reason.into(),
            holder,
            next_slot: 1,
            rev: s.value_rev,
            handles: vec![],
        });
        let mut ar = lua_Debug::default();
        let mut loc = json!(null);
        if (d().lua_getstack)(l, 0, &mut ar) != 0 {
            (d().lua_getinfo)(l, c"Sl".as_ptr(), &mut ar);
            loc = json!({"label": String::from_utf8_lossy(CStr::from_ptr(ar.source).to_bytes()), "path": runtime_path(&ar), "line": ar.currentline});
        }
        self.publish();
        self.event("stopped", json!({
            "stopId": format!("stop-{id}"), "reason": reason, "description": desc,
            "threadId": tid, "allThreadsStopped": true, "hitBreakpointIds": hit, "location": loc,
        }));
        // Pause loop: service the bounded command queue on this (owner) thread.
        loop {
            if let Flow::Resume = self.service(l, true) {
                break;
            }
            self.sh.wait(Duration::from_millis(100));
        }
        if let Some(s) = &mut self.sess
            && let Some(mut st) = s.stop.take()
        {
            unref(l, &mut st.holder);
            unref(l, &mut st.tref);
        }
        self.rearm(l);
        self.publish();
    }

    unsafe fn op_resume(&mut self, l: L, op: &str, v: &Value) -> R<Value> {
        let kind = match op {
            "continue" => None,
            _ => Some(match v.get("kind").and_then(Value::as_str) {
                Some("in") => StepKind::In,
                Some("over") => StepKind::Over,
                Some("out") => StepKind::Out,
                _ => return fail(err::INVALID_ARGUMENT, "step kind must be in, over or out"),
            }),
        };
        self.check_stop(v)?;
        let s = self.sess.as_mut().unwrap();
        let st = s.stop.as_ref().unwrap();
        if let Some(kind) = kind {
            let sl = st.l;
            (a().lua_pushthread)(sl);
            if sl != l {
                (a().lua_xmove)(sl, l, 1);
            }
            let tref = make_ref(l);
            s.step = Some(Step {
                kind,
                l: sl,
                tref,
                depth: depth(sl),
                returned: false,
            });
        }
        // The stop loop releases the stop handles once the response is sent.
        if let Some(st) = s.stop.as_mut() {
            st.handles.clear();
        }
        let mut st = s.stop.take().unwrap();
        unref(l, &mut st.holder);
        unref(l, &mut st.tref);
        Ok(json!({"state": "running"}))
    }

    // ----- inspection -----

    fn check_stop(&self, v: &Value) -> R<()> {
        let st = self
            .sess
            .as_ref()
            .and_then(|s| s.stop.as_ref())
            .ok_or((err::NOT_STOPPED, "not stopped".to_owned()))?;
        match v.get("stopId").and_then(Value::as_str) {
            None => Ok(()),
            Some(id) if id == format!("stop-{}", st.id) => Ok(()),
            Some(id) => fail(
                err::STALE_HANDLE,
                format!(
                    "{id} is no longer the current stop (current stop-{})",
                    st.id
                ),
            ),
        }
    }

    fn stop_ref(&self) -> &Stop {
        self.sess.as_ref().unwrap().stop.as_ref().unwrap()
    }
    fn stop_mut(&mut self) -> &mut Stop {
        self.sess.as_mut().unwrap().stop.as_mut().unwrap()
    }

    fn frame_level(&self, v: &Value) -> R<i32> {
        match v.get("frameId") {
            None | Some(Value::Null) => Ok(0),
            Some(Value::String(s)) => s
                .strip_prefix("frame-")
                .and_then(|n| n.parse().ok())
                .ok_or((err::STALE_HANDLE, format!("invalid frame id {s}"))),
            Some(Value::Number(n)) => n
                .as_i64()
                .map(|n| n as i32)
                .ok_or((err::INVALID_ARGUMENT, "bad frameId".into())),
            _ => fail(err::INVALID_ARGUMENT, "frameId must be a string"),
        }
    }

    fn page(v: &Value) -> R<(usize, usize)> {
        let start = v.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
        let count = match v.get("count").and_then(Value::as_u64).unwrap_or(0) as usize {
            0 => proto::MAX_PAGE,
            n if n > proto::MAX_PAGE => {
                return fail(err::LIMIT, format!("count exceeds {}", proto::MAX_PAGE));
            }
            n => n,
        };
        Ok((start, count))
    }

    unsafe fn op_threads(&mut self, l: L) -> R<Value> {
        let s = self.sess.as_ref().unwrap();
        let mut out = vec![json!({"id": 1, "name": "main"})];
        if let Some(st) = &s.stop
            && st.tid != 1
        {
            out.push(
                json!({"id": st.tid, "name": format!("coroutine {}", st.tid), "stopped": true}),
            );
        }
        let _ = l;
        Ok(
            json!({"threads": out, "complete": false, "note": "only the main state and the stopped coroutine are enumerable"}),
        )
    }

    unsafe fn op_stack(&mut self, _l: L, v: &Value) -> R<Value> {
        self.check_stop(v)?;
        let (start, count) = Self::page(v)?;
        let st = self.stop_ref();
        if let Some(t) = v.get("threadId").and_then(Value::as_i64)
            && t != st.tid
        {
            return fail(
                err::UNSUPPORTED_CONTEXT,
                "only the stopped coroutine's stack can be inspected",
            );
        }
        let sl = st.l;
        let total = depth(sl);
        let mut frames = vec![];
        let mut ar = lua_Debug::default();
        for lv in start as i32..(start + count).min(total as usize) as i32 {
            if (d().lua_getstack)(sl, lv, &mut ar) == 0 {
                break;
            }
            (d().lua_getinfo)(sl, c"nSl".as_ptr(), &mut ar);
            let what = cs(ar.what);
            let label = match ar.source.is_null() {
                true => String::new(),
                false => String::from_utf8_lossy(CStr::from_ptr(ar.source).to_bytes()).into_owned(),
            };
            let name = match (cs(ar.name), what.as_str()) {
                (n, _) if !n.is_empty() => n,
                (_, "main") => "main chunk".into(),
                (_, "C") => "[C function]".into(),
                _ => format!("<anonymous:{}>", ar.linedefined),
            };
            let mut f = json!({
                "id": format!("frame-{lv}"), "name": name, "kind": what.to_lowercase(),
                "namewhat": cs(ar.namewhat), "source": {"label": label, "path": runtime_path(&ar)},
                "line": ar.currentline, "linedefined": ar.linedefined,
            });
            if what == "C" {
                f["debugInfo"] = json!({"available": false, "reason": "native function: no Lua source or locals"});
            }
            frames.push(f);
        }
        Ok(json!({"frames": frames, "totalFrames": total, "threadId": st.tid}))
    }

    fn new_handle(&mut self, h: Handle) -> String {
        let st = self.stop_mut();
        st.handles.push(h);
        format!("v{}.{}.{}", st.id, st.rev, st.handles.len() - 1)
    }

    unsafe fn op_scopes(&mut self, l: L, v: &Value) -> R<Value> {
        self.check_stop(v)?;
        let lv = self.frame_level(v)?;
        let sl = self.stop_ref().l;
        let mut ar = lua_Debug::default();
        if (d().lua_getstack)(sl, lv, &mut ar) == 0 {
            return fail(
                err::STALE_HANDLE,
                format!("frame-{lv} does not exist in this stop"),
            );
        }
        (d().lua_getinfo)(sl, c"Su".as_ptr(), &mut ar);
        let mut out = vec![
            json!({"name": "Locals", "kind": "locals", "reference": self.new_handle(Handle::Locals(lv)), "expensive": false}),
        ];
        let top = (a().lua_gettop)(sl);
        let has_va = !(d().lua_getlocal)(sl, &ar, -1).is_null();
        (a().lua_settop)(sl, top);
        if has_va {
            out.push(json!({"name": "Varargs", "kind": "varargs", "reference": self.new_handle(Handle::Varargs(lv)), "expensive": false}));
        }
        if ar.nups > 0 {
            out.push(json!({"name": "Upvalues", "kind": "upvalues", "reference": self.new_handle(Handle::Upvalues(lv)), "expensive": false}));
        }
        (d().lua_getinfo)(sl, c"f".as_ptr(), &mut ar);
        (a().lua_getfenv)(sl, -1);
        if sl != l {
            (a().lua_xmove)(sl, l, 1);
        }
        let slot = self.keep(l, -1);
        (a().lua_settop)(l, -2);
        (a().lua_settop)(sl, top);
        out.push(json!({"name": "Environment", "kind": "environment", "reference": self.new_handle(Handle::Val { slot, eval: None, snap: None }), "expensive": true}));
        Ok(json!({"scopes": out}))
    }

    /// Stores the value at `idx` (on `l`) in the stop holder.
    unsafe fn keep(&mut self, l: L, idx: c_int) -> c_int {
        let idx = absidx(l, idx);
        let st = self.stop_mut();
        let slot = st.next_slot;
        st.next_slot += 1;
        push_ref(l, st.holder);
        (a().lua_pushvalue)(l, idx);
        (a().lua_rawseti)(l, -2, slot);
        (a().lua_settop)(l, -2);
        slot
    }

    /// Variable JSON for the value at `idx` (on `l`), with a child reference when expandable.
    unsafe fn describe(&mut self, l: L, idx: c_int, name: String, eval: Option<String>) -> Value {
        let idx = absidx(l, idx);
        let (t, text, trunc) = preview(l, idx);
        let mut v = json!({"name": name, "type": t, "value": text});
        if let Some(n) = trunc {
            v["truncated"] = json!(true);
            v["byteLength"] = json!(n);
        }
        if let Some(e) = &eval {
            v["evaluateName"] = json!(e);
        }
        let ty = (a().lua_type)(l, idx);
        let expandable = match ty {
            LUA_TTABLE => {
                v["length"] = json!((a().lua_objlen)(l, idx));
                true
            }
            LUA_TFUNCTION => {
                let top = (a().lua_gettop)(l);
                let has = !(d().lua_getupvalue)(l, idx, 1).is_null();
                (a().lua_settop)(l, top);
                has
            }
            LUA_TUSERDATA => {
                let has = (a().lua_getmetatable)(l, idx) != 0;
                if has {
                    (a().lua_settop)(l, -2);
                }
                has
            }
            _ => false,
        };
        if matches!(ty, LUA_TTABLE | LUA_TFUNCTION | LUA_TUSERDATA | LUA_TTHREAD) {
            v["id"] = json!(format!("{:p}", (a().lua_topointer)(l, idx)));
        }
        if expandable {
            let slot = self.keep(l, idx);
            v["reference"] = json!(self.new_handle(Handle::Val {
                slot,
                eval,
                snap: None
            }));
        }
        v
    }

    fn parse_ref(&self, r: &str) -> R<usize> {
        let st = self.stop_ref();
        let bad = || {
            (
                err::STALE_HANDLE,
                format!("reference {r} is stale or unknown; re-request scopes"),
            )
        };
        let mut it = r.strip_prefix('v').ok_or_else(bad)?.split('.');
        let (Some(sid), Some(rev), Some(i), None) = (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(bad());
        };
        if sid != st.id.to_string() || rev != st.rev.to_string() {
            return Err(bad());
        }
        i.parse::<usize>()
            .ok()
            .filter(|i| *i < st.handles.len())
            .ok_or_else(bad)
    }

    unsafe fn op_variables(&mut self, l: L, v: &Value) -> R<Value> {
        self.check_stop(v)?;
        let Some(r) = v.get("reference").and_then(Value::as_str) else {
            return fail(err::INVALID_ARGUMENT, "reference required");
        };
        let (start, count) = Self::page(v)?;
        let hi = self.parse_ref(r)?;
        let sl = self.stop_ref().l;
        let top = (a().lua_gettop)(l);
        let stop_top = (a().lua_gettop)(sl);
        let mut out = vec![];
        let mut total = None;
        let internal = v.get("internal") == Some(&Value::Bool(true));
        let frame_vars = |dbg: &mut Self, lv: i32, varargs: bool| -> R<(Vec<Value>, usize)> {
            let mut ar = lua_Debug::default();
            if (d().lua_getstack)(sl, lv, &mut ar) == 0 {
                return fail(err::STALE_HANDLE, "frame no longer exists");
            }
            let mut vars = vec![];
            let mut i = 1;
            loop {
                let n = if varargs { -i } else { i };
                let name = (d().lua_getlocal)(sl, &ar, n);
                if name.is_null() {
                    break;
                }
                let name = cs(name);
                // VM temporaries are not lexical locals; shown only on request.
                if !varargs && name.starts_with('(') && !internal {
                    (a().lua_settop)(sl, stop_top);
                    i += 1;
                    continue;
                }
                if sl != l {
                    (a().lua_xmove)(sl, l, 1);
                }
                if vars.len() >= start && vars.len() < start + count {
                    let shown = if varargs {
                        format!("...[{i}]")
                    } else {
                        name.clone()
                    };
                    let eval = (!varargs && identifier_key(&name)).then(|| name.clone());
                    let mut x = dbg.describe(l, -1, shown, eval);
                    x["slot"] = json!(n);
                    if name.starts_with('(') && !varargs {
                        x["internal"] = json!(true);
                    }
                    vars.push(x);
                } else {
                    vars.push(Value::Null);
                }
                (a().lua_settop)(l, top);
                (a().lua_settop)(sl, stop_top);
                i += 1;
            }
            let n = vars.len();
            Ok((vars.into_iter().filter(|x| !x.is_null()).collect(), n))
        };
        let h = match &self.stop_ref().handles[hi] {
            Handle::Locals(lv) => (0, *lv, None, None),
            Handle::Varargs(lv) => (1, *lv, None, None),
            Handle::Upvalues(lv) => (2, *lv, None, None),
            Handle::Val { slot, eval, snap } => (3, *slot, eval.clone(), *snap),
        };
        match h {
            (0, lv, ..) | (1, lv, ..) => {
                let (vars, n) = frame_vars(self, lv, h.0 == 1)?;
                out = vars;
                total = Some(n);
            }
            (2, lv, ..) => {
                let mut ar = lua_Debug::default();
                if (d().lua_getstack)(sl, lv, &mut ar) == 0 {
                    return fail(err::STALE_HANDLE, "frame no longer exists");
                }
                (d().lua_getinfo)(sl, c"f".as_ptr(), &mut ar);
                if sl != l {
                    (a().lua_xmove)(sl, l, 1);
                }
                let f = (a().lua_gettop)(l);
                out = self.upvalues(l, f, start, count, &mut total);
                (a().lua_settop)(l, top);
            }
            (_, slot, eval, snap) => {
                push_ref(l, self.stop_ref().holder);
                (a().lua_rawgeti)(l, -1, slot);
                let val = (a().lua_gettop)(l);
                match (a().lua_type)(l, val) {
                    LUA_TTABLE => {
                        let (kslot, n, truncated) = match snap {
                            Some(s) => s,
                            None => {
                                let s = self.snapshot(l, val);
                                if let Handle::Val { snap, .. } = &mut self.stop_mut().handles[hi] {
                                    *snap = Some(s);
                                }
                                s
                            }
                        };
                        push_ref(l, self.stop_ref().holder);
                        (a().lua_rawgeti)(l, -1, kslot);
                        let keys = (a().lua_gettop)(l);
                        for k in start..(start + count).min(n) {
                            (a().lua_rawgeti)(l, keys, k as c_int + 1);
                            let kidx = (a().lua_gettop)(l);
                            (a().lua_pushvalue)(l, kidx);
                            (a().lua_rawget)(l, val);
                            let (name, child_eval) = key_name(l, kidx, eval.as_deref());
                            let mut x = self.describe(l, -1, name, child_eval);
                            x["key"] = json!(preview(l, kidx).1);
                            x["keyType"] = json!(type_name(l, kidx));
                            out.push(x);
                            (a().lua_settop)(l, keys);
                        }
                        if start == 0 && (a().lua_getmetatable)(l, val) != 0 {
                            let mut x = self.describe(l, -1, "[metatable]".into(), None);
                            x["internal"] = json!(true);
                            out.push(x);
                        }
                        total = Some(n);
                        if truncated {
                            out.push(json!({"name": "[snapshot truncated]", "type": "notice", "value": format!("only the first {} entries are shown", proto::MAX_SNAPSHOT)}));
                        }
                    }
                    LUA_TFUNCTION => out = self.upvalues(l, val, start, count, &mut total),
                    LUA_TUSERDATA if (a().lua_getmetatable)(l, val) != 0 => {
                        let mut x = self.describe(l, -1, "[metatable]".into(), None);
                        x["internal"] = json!(true);
                        out.push(x);
                    }
                    _ => {}
                }
                (a().lua_settop)(l, top);
            }
        }
        (a().lua_settop)(l, top);
        Ok(json!({"variables": out, "total": total}))
    }

    unsafe fn upvalues(
        &mut self,
        l: L,
        f: c_int,
        start: usize,
        count: usize,
        total: &mut Option<usize>,
    ) -> Vec<Value> {
        let mut out = vec![];
        let mut i = 1;
        loop {
            let name = (d().lua_getupvalue)(l, f, i);
            if name.is_null() {
                break;
            }
            let idx = (i - 1) as usize;
            if idx >= start && idx < start + count {
                let mut name = cs(name);
                if name.is_empty() {
                    name = format!("[upvalue {i}]");
                }
                let mut x = self.describe(l, -1, name, None);
                x["index"] = json!(i);
                out.push(x);
            }
            (a().lua_settop)(l, -2);
            i += 1;
        }
        *total = Some((i - 1) as usize);
        out
    }

    /// Raw key snapshot of the table at `t`, stored in the holder. (slot, len, truncated)
    unsafe fn snapshot(&mut self, l: L, t: c_int) -> (c_int, usize, bool) {
        (a().lua_createtable)(l, 0, 0);
        let keys = (a().lua_gettop)(l);
        let mut n = 0usize;
        let mut truncated = false;
        (a().lua_pushnil)(l);
        while (a().lua_next)(l, t) != 0 {
            (a().lua_settop)(l, -2);
            if n >= proto::MAX_SNAPSHOT {
                truncated = true;
                (a().lua_settop)(l, -2);
                break;
            }
            n += 1;
            (a().lua_pushvalue)(l, -1);
            (a().lua_rawseti)(l, keys, n as c_int);
        }
        let slot = self.keep(l, keys);
        (a().lua_settop)(l, keys - 1);
        (slot, n, truncated)
    }

    // ----- evaluation -----

    /// Compiles and runs `code` with the frame at `level` (in the stopped/hooked thread) as
    /// lexical context: locals/upvalues are write-through, other names go to the function
    /// environment. Calls `f(l, base, nresults)` before the stack is restored. Writes back
    /// assigned bindings even when execution raises.
    unsafe fn eval_values<T>(
        &mut self,
        l: L,
        level: i32,
        code: &str,
        chunk: &CStr,
        f: impl FnOnce(L, c_int, c_int) -> T,
    ) -> R<T> {
        let top = (a().lua_gettop)(l);
        let out = self
            .eval_inner(l, level, code, chunk)
            .map(|(base, n)| f(l, base, n));
        (a().lua_settop)(l, top);
        out
    }

    unsafe fn eval_inner(
        &mut self,
        l: L,
        level: i32,
        code: &str,
        chunk: &CStr,
    ) -> R<(c_int, c_int)> {
        if code.len() > proto::MAX_EXPRESSION {
            return fail(err::LIMIT, "expression too long");
        }
        let mut ar = lua_Debug::default();
        if (d().lua_getstack)(l, level, &mut ar) == 0 {
            return fail(err::STALE_HANDLE, format!("frame-{level} does not exist"));
        }
        (a().lua_checkstack)(l, 40);
        (d().lua_getinfo)(l, c"f".as_ptr(), &mut ar);
        let func = (a().lua_gettop)(l);
        (a().lua_createtable)(l, 0, 8);
        let vals = (a().lua_gettop)(l);
        (a().lua_createtable)(l, 0, 8);
        let present = (a().lua_gettop)(l);
        let mut slots: HashMap<Vec<u8>, (bool, c_int)> = HashMap::new();
        let mut bind = |l: L, name: &[u8], local: bool, i: c_int| {
            // value on top
            (a().lua_pushlstring)(l, name.as_ptr().cast(), name.len());
            (a().lua_insert)(l, -2);
            (a().lua_rawset)(l, vals);
            (a().lua_pushlstring)(l, name.as_ptr().cast(), name.len());
            (a().lua_pushboolean)(l, 1);
            (a().lua_rawset)(l, present);
            slots.insert(name.to_vec(), (local, i));
        };
        let mut i = 1;
        loop {
            let n = (d().lua_getupvalue)(l, func, i);
            if n.is_null() {
                break;
            }
            let name = CStr::from_ptr(n).to_bytes().to_vec();
            if name.is_empty() || name[0] == b'(' {
                (a().lua_settop)(l, -2);
            } else {
                bind(l, &name, false, i);
            }
            i += 1;
        }
        // Later (inner) locals shadow earlier ones with the same name.
        let mut i = 1;
        loop {
            let n = (d().lua_getlocal)(l, &ar, i);
            if n.is_null() {
                break;
            }
            let name = CStr::from_ptr(n).to_bytes().to_vec();
            if name.is_empty() || name[0] == b'(' {
                (a().lua_settop)(l, -2);
            } else {
                bind(l, &name, true, i);
            }
            i += 1;
        }
        let proxy_ref = self.sess.as_ref().unwrap().proxy_ref;
        push_ref(l, proxy_ref);
        (a().lua_pushvalue)(l, vals);
        (a().lua_pushvalue)(l, present);
        (a().lua_getfenv)(l, func);
        pcall(l, 3, 2).map_err(|m| (err::INTERNAL, m))?;
        let proxy = (a().lua_gettop)(l) - 1;
        let dirty = proxy + 1;
        load(l, code, chunk).map_err(|m| (err::COMPILE, m))?;
        (a().lua_pushvalue)(l, proxy);
        (a().lua_setfenv)(l, -2);
        let base = (a().lua_gettop)(l);
        let r = pcall(l, 0, LUA_MULTRET);
        // Write back assigned locals/upvalues (even after an error).
        (a().lua_pushnil)(l);
        while (a().lua_next)(l, dirty) != 0 {
            (a().lua_settop)(l, -2);
            if (a().lua_type)(l, -1) == LUA_TSTRING
                && let Some(&(local, i)) = slots.get(&bytes_at(l, -1))
            {
                (a().lua_pushvalue)(l, -1);
                (a().lua_rawget)(l, vals);
                if local {
                    (d().lua_setlocal)(l, &ar, i);
                } else if (d().lua_setupvalue)(l, func, i).is_null() {
                    (a().lua_settop)(l, -2);
                }
            }
        }
        r.map_err(|m| (err::EVALUATION, m))?;
        Ok((base, (a().lua_gettop)(l) - base + 1))
    }

    fn bump_rev(&mut self) -> u64 {
        let s = self.sess.as_mut().unwrap();
        s.value_rev += 1;
        let rev = s.value_rev;
        if let Some(st) = &mut s.stop {
            st.rev = rev;
            st.handles.clear();
        }
        self.event(
            "invalidated",
            json!({"valueRevision": rev, "areas": ["variables"]}),
        );
        rev
    }

    unsafe fn results(
        &mut self,
        l: L,
        base: c_int,
        n: c_int,
        raw: bool,
        stopped: bool,
    ) -> Vec<Value> {
        (0..n)
            .map(|i| {
                let idx = base + i;
                let mut v = match stopped {
                    true => self.describe(l, idx, format!("[{}]", i + 1), None),
                    false => {
                        let (t, text, _) = preview(l, idx);
                        json!({"name": format!("[{}]", i + 1), "type": t, "value": text})
                    }
                };
                if raw && (a().lua_type)(l, idx) == LUA_TSTRING {
                    v["base64"] = json!(proto::base64(&bytes_at(l, idx)));
                }
                v
            })
            .collect()
    }

    unsafe fn op_evaluate(&mut self, _l: L, v: &Value) -> R<Value> {
        self.check_stop(v)?;
        let Some(expr) = string(v, "expression")? else {
            return fail(err::INVALID_ARGUMENT, "expression required");
        };
        let lv = self.frame_level(v)?;
        let raw = v.get("rawStrings") == Some(&Value::Bool(true));
        let sl = self.stop_ref().l;
        let code = format!("return {expr}");
        // Compile errors have no side effects and do not advance the value revision.
        self.compile_check(sl, &code)
            .map_err(|m| (err::COMPILE, m))?;
        let rev = self.bump_rev();
        let top = (a().lua_gettop)(sl);
        let r = self.eval_inner(sl, lv, &code, c"=(gdebug eval)");
        let out = r.map(|(base, n)| self.results(sl, base, n, raw, true));
        (a().lua_settop)(sl, top);
        Ok(json!({"results": out?, "valueRevision": rev}))
    }

    unsafe fn op_command(&mut self, l: L, v: &Value, stopped: bool) -> R<Value> {
        match v.get("kind").and_then(Value::as_str) {
            Some("glua") => {
                let Some(code) = string(v, "code")? else {
                    return fail(err::INVALID_ARGUMENT, "code required");
                };
                if code.len() > proto::MAX_EXPRESSION {
                    return fail(err::LIMIT, "chunk too long");
                }
                let raw = v.get("rawStrings") == Some(&Value::Bool(true));
                if stopped {
                    let lv = self.frame_level(v)?;
                    let sl = self.stop_ref().l;
                    self.compile_check(sl, &code)
                        .map_err(|m| (err::COMPILE, m))?;
                    let rev = self.bump_rev();
                    let top = (a().lua_gettop)(sl);
                    let r = self
                        .eval_inner(sl, lv, &code, c"=(gdebug command)")
                        .map(|(b, n)| self.results(sl, b, n, raw, true));
                    (a().lua_settop)(sl, top);
                    return Ok(
                        json!({"results": r?, "valueRevision": rev, "context": format!("frame-{lv}")}),
                    );
                }
                let top = (a().lua_gettop)(l);
                load(l, &code, c"=(gdebug command)").map_err(|m| (err::COMPILE, m))?;
                let rev = self.bump_rev();
                let r = pcall(l, 0, LUA_MULTRET)
                    .map(|_| self.results(l, top + 1, (a().lua_gettop)(l) - top, raw, false));
                (a().lua_settop)(l, top);
                Ok(
                    json!({"results": r.map_err(|m| (err::EVALUATION, m))?, "valueRevision": rev, "context": "global"}),
                )
            }
            Some("console") => {
                let Some(name) = string(v, "name")? else {
                    return fail(err::INVALID_ARGUMENT, "name required");
                };
                let args: Vec<String> = match v.get("args").or(v.get("arguments")) {
                    None | Some(Value::Null) => vec![],
                    Some(Value::Array(xs)) => xs
                        .iter()
                        .map(|x| {
                            x.as_str()
                                .map(str::to_owned)
                                .ok_or((err::INVALID_ARGUMENT, "args must be strings".to_owned()))
                        })
                        .collect::<R<_>>()?,
                    _ => return fail(err::INVALID_ARGUMENT, "args must be an array of strings"),
                };
                proto::validate_console(&name, &args).map_err(|m| (err::INVALID_ARGUMENT, m))?;
                let s = self.sess.as_mut().unwrap();
                let id = s.next_cmd;
                s.next_cmd += 1;
                s.console.push(Console { id, name, args });
                Ok(
                    json!({"status": "queued", "commandId": id, "note": "submitted to the engine command buffer at the next running safe point; console output is not captured"}),
                )
            }
            _ => fail(
                err::INVALID_ARGUMENT,
                "kind must be \"glua\" or \"console\"",
            ),
        }
    }

    unsafe fn flush_console(&mut self, l: L) {
        let Some(s) = &mut self.sess else { return };
        let cmds: Vec<Console> = std::mem::take(&mut s.console);
        for c in cmds {
            let top = (a().lua_gettop)(l);
            push_path(l, &["RunConsoleCommand"]);
            let r = match (a().lua_type)(l, -1) {
                LUA_TFUNCTION => {
                    for x in std::iter::once(&c.name).chain(&c.args) {
                        (a().lua_pushlstring)(l, x.as_ptr().cast(), x.len());
                    }
                    pcall(l, 1 + c.args.len() as c_int, 0)
                }
                _ => Err("RunConsoleCommand is unavailable in this realm".into()),
            };
            (a().lua_settop)(l, top);
            let (status, msg) = match r {
                // GMod may still refuse blocked commands; it logs that to the server console.
                Ok(()) => ("submitted", Some("handed to RunConsoleCommand; GMod may still block it (see the target console)".to_owned())),
                Err(m) => ("rejected", Some(m)),
            };
            self.event(
                "commandResult",
                json!({"commandId": c.id, "status": status, "message": msg}),
            );
        }
    }

    /// Source text through GMod's virtual filesystem, confined to `.lua` game paths.
    unsafe fn op_source(&mut self, l: L, v: &Value) -> R<Value> {
        let Some(p) = string(v, "path")? else {
            return fail(err::INVALID_ARGUMENT, "path required");
        };
        let p = p.strip_prefix('@').unwrap_or(&p);
        let n = path::normalize(p);
        if n.starts_with('/')
            || n.contains(':')
            || n.split('/').any(|s| s == "..")
            || !n.ends_with(".lua")
        {
            return fail(
                err::INVALID_ARGUMENT,
                "only relative game .lua paths without traversal are readable",
            );
        }
        let top = (a().lua_gettop)(l);
        push_path(l, &["file", "Read"]);
        if (a().lua_type)(l, -1) != LUA_TFUNCTION {
            (a().lua_settop)(l, top);
            return fail(
                err::UNSUPPORTED_CONTEXT,
                "source unavailable: GMod file.Read is not available",
            );
        }
        (a().lua_pushlstring)(l, n.as_ptr().cast(), n.len());
        (a().lua_pushstring)(l, c"GAME".as_ptr());
        let r = pcall(l, 2, 1);
        let out = match r {
            Ok(()) if (a().lua_type)(l, -1) == LUA_TSTRING => {
                let b = bytes_at(l, -1);
                match b.len() > proto::MAX_SOURCE {
                    true => fail(err::LIMIT, "source exceeds size limit"),
                    false => Ok(json!({"path": n, "content": String::from_utf8_lossy(&b)})),
                }
            }
            Ok(()) => fail(
                err::UNSUPPORTED_CONTEXT,
                format!("source unavailable for {n}"),
            ),
            Err(m) => fail(err::UNSUPPORTED_CONTEXT, format!("source unavailable: {m}")),
        };
        (a().lua_settop)(l, top);
        out
    }
}

/// Display name and child evaluate-name for a table key at `k`.
unsafe fn key_name(l: L, k: c_int, parent: Option<&str>) -> (String, Option<String>) {
    match (a().lua_type)(l, k) {
        LUA_TSTRING => {
            let b = bytes_at(l, k);
            match std::str::from_utf8(&b).ok().filter(|s| identifier_key(s)) {
                Some(s) => (s.to_owned(), parent.map(|p| format!("{p}.{s}"))),
                None => {
                    let q = proto::lua_quote(&b, proto::MAX_PREVIEW);
                    (
                        format!("[{q}]"),
                        parent
                            .filter(|_| b.len() <= proto::MAX_PREVIEW)
                            .map(|p| format!("{p}[{q}]")),
                    )
                }
            }
        }
        LUA_TNUMBER | LUA_TBOOLEAN => {
            let s = preview(l, k).1;
            (format!("[{s}]"), parent.map(|p| format!("{p}[{s}]")))
        }
        _ => (format!("[{}]", preview(l, k).1), None),
    }
}
