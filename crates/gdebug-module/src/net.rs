//! Loopback listener, authentication and per-connection I/O (spec §5). Never touches Lua:
//! VM work is queued for the owner thread; only `hello`, heartbeat and cached `status` are
//! answered here.

use gdebug_proto::{self as proto, err};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A VM request waiting for the owner thread.
pub struct Job {
    pub conn: u64,
    pub id: i64,
    pub op: String,
    pub args: Value,
}

pub struct Controller {
    pub conn: u64,
    out: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    seq: AtomicU64,
    stream: TcpStream,
}

pub struct Shared {
    pub token: String,
    pub realm: &'static str,
    pub generation: u64,
    pub jobs: Mutex<VecDeque<Job>>,
    pub wake: Condvar,
    /// Priority control flags, never trapped behind a full queue.
    pub pause: AtomicBool,
    pub pause_ids: Mutex<Vec<(u64, i64)>>,
    pub detach_ids: Mutex<Vec<(u64, i64)>>,
    /// Connection id whose session must be cleaned up (lost/lease expired); 0 = none.
    pub lost: AtomicU64,
    pub controller: Mutex<Option<Controller>>,
    pub last_seen: Mutex<Instant>,
    pub status: Mutex<(Value, Instant)>,
    pub closing: AtomicBool,
    pub pending: AtomicUsize,
    next_conn: AtomicU64,
}

impl Shared {
    pub fn new(token: String, realm: &'static str, generation: u64) -> Arc<Self> {
        Arc::new(Self {
            token,
            realm,
            generation,
            jobs: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            pause: AtomicBool::new(false),
            pause_ids: Mutex::new(vec![]),
            detach_ids: Mutex::new(vec![]),
            lost: AtomicU64::new(0),
            controller: Mutex::new(None),
            last_seen: Mutex::new(Instant::now()),
            status: Mutex::new((json!({"state": "idle"}), Instant::now())),
            closing: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
        })
    }

    /// Anything for the owner thread to look at?
    pub fn has_work(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0 || self.lost.load(Ordering::Acquire) != 0
    }

    fn poke(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_all();
    }

    pub fn take_jobs(&self) -> Vec<Job> {
        let jobs: Vec<Job> = self.jobs.lock().unwrap().drain(..).collect();
        self.pending.store(0, Ordering::Release);
        jobs
    }

    /// Owner waits (bounded) for new work while stopped.
    pub fn wait(&self, timeout: Duration) {
        let g = self.jobs.lock().unwrap();
        if g.is_empty() && !self.has_flags() {
            let _ = self.wake.wait_timeout(g, timeout);
        }
    }

    fn has_flags(&self) -> bool {
        self.lost.load(Ordering::Acquire) != 0
            || !self.pause_ids.lock().unwrap().is_empty()
            || !self.detach_ids.lock().unwrap().is_empty()
    }

    pub fn lease_expired(&self) -> bool {
        self.last_seen.lock().unwrap().elapsed() > Duration::from_millis(proto::LEASE_MS)
    }

    pub fn controller_conn(&self) -> Option<u64> {
        self.controller.lock().unwrap().as_ref().map(|c| c.conn)
    }

    /// Queue one line to the current controller (if it is `conn`). Overflow drops the
    /// connection rather than blocking the game (spec §5.3).
    pub fn send(&self, conn: u64, msg: &Value) {
        let g = self.controller.lock().unwrap();
        let Some(c) = g.as_ref().filter(|c| c.conn == conn) else {
            return;
        };
        let mut line = msg.to_string().into_bytes();
        line.push(b'\n');
        if c.queued.fetch_add(line.len(), Ordering::AcqRel) + line.len() > proto::MAX_OUTBOUND {
            let _ = c.stream.shutdown(Shutdown::Both);
            return;
        }
        let _ = c.out.send(line);
    }

    pub fn reply(&self, conn: u64, id: i64, r: Result<Value, (&'static str, String)>) {
        let msg = match r {
            Ok(v) => proto::ok(id, v),
            Err((code, m)) => proto::error(Some(id), code, m),
        };
        self.send(conn, &msg);
    }

    pub fn event(&self, conn: u64, name: &str, data: Value) {
        let seq = {
            let g = self.controller.lock().unwrap();
            match g.as_ref().filter(|c| c.conn == conn) {
                Some(c) => c.seq.fetch_add(1, Ordering::AcqRel) + 1,
                None => return,
            }
        };
        self.send(conn, &proto::event(seq, name, data));
    }

    /// Release the controller slot (after owner cleanup); closes its socket.
    pub fn release(&self, conn: u64) {
        let mut g = self.controller.lock().unwrap();
        if g.as_ref().is_some_and(|c| c.conn == conn) {
            // Writer thread flushes what is queued, then exits when the sender drops.
            let c = g.take().unwrap();
            drop(c.out);
            let s = c.stream;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                let _ = s.shutdown(Shutdown::Both);
            });
        }
    }

    pub fn set_status(&self, v: Value) {
        *self.status.lock().unwrap() = (v, Instant::now());
    }

    fn cached_status(&self) -> Value {
        let (mut v, at) = self.status.lock().unwrap().clone();
        v["cacheAgeMs"] = json!(at.elapsed().as_millis() as u64);
        v["pendingJobs"] = json!(self.jobs.lock().unwrap().len());
        v["pauseRequested"] = json!(self.pause.load(Ordering::Acquire));
        v
    }
}

/// Binds `127.0.0.1:0` and serves connections until `closing`.
pub fn listen(shared: Arc<Shared>) -> std::io::Result<(u16, std::thread::JoinHandle<()>)> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    l.set_nonblocking(true)?;
    let port = l.local_addr()?.port();
    let h = std::thread::Builder::new()
        .name("gdebug-listen".into())
        .spawn(move || {
            while !shared.closing.load(Ordering::Acquire) {
                match l.accept() {
                    Ok((s, _)) => {
                        let sh = shared.clone();
                        let _ = std::thread::Builder::new()
                            .name("gdebug-conn".into())
                            .spawn(move || serve(sh, s));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?;
    Ok((port, h))
}

/// Bounded line read: `Ok(None)` on EOF; errors on oversize/timeout/IO.
fn read_line(r: &mut BufReader<TcpStream>) -> Result<Option<String>, String> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(proto::MAX_MESSAGE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(|e| e.to_string())?;
    match n {
        0 => Ok(None),
        _ if buf.last() != Some(&b'\n') => Err("message too large or truncated".into()),
        _ => String::from_utf8(buf)
            .map(Some)
            .map_err(|_| "invalid UTF-8".into()),
    }
}

fn write_now(s: &mut TcpStream, v: &Value) {
    let _ = s.write_all(format!("{v}\n").as_bytes());
}

fn serve(shared: Arc<Shared>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(mut w) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    // Authentication: only `hello` is accepted, within 5 s.
    let line = match read_line(&mut r) {
        Ok(Some(l)) => l,
        _ => return,
    };
    let req = match proto::parse_request(&line) {
        Ok(q) if q.op == "hello" => q,
        Ok(q) => {
            return write_now(
                &mut w,
                &proto::error(Some(q.id), err::AUTH, "hello required first"),
            );
        }
        Err((id, m)) => return write_now(&mut w, &proto::error(id, err::INVALID_ARGUMENT, m)),
    };
    let version = req.args["protocol"].as_str().unwrap_or("");
    if !version.is_empty() && version.split('.').next() != Some(proto::PROTOCOL) {
        return write_now(
            &mut w,
            &proto::error(
                Some(req.id),
                err::VERSION,
                format!(
                    "unsupported protocol {version}; target speaks {}",
                    proto::PROTOCOL
                ),
            ),
        );
    }
    if !proto::token_eq(req.args["token"].as_str().unwrap_or(""), &shared.token) {
        std::thread::sleep(Duration::from_millis(200));
        return write_now(
            &mut w,
            &proto::error(Some(req.id), err::AUTH, "authentication failed"),
        );
    }
    let conn = shared.next_conn.fetch_add(1, Ordering::AcqRel);
    let (tx, rx) = channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    {
        let mut g = shared.controller.lock().unwrap();
        if g.is_some() {
            return write_now(
                &mut w,
                &proto::error(
                    Some(req.id),
                    err::BUSY,
                    "another controller owns this realm",
                ),
            );
        }
        *g = Some(Controller {
            conn,
            out: tx,
            queued: queued.clone(),
            seq: AtomicU64::new(0),
            stream: w.try_clone().unwrap(),
        });
    }
    *shared.last_seen.lock().unwrap() = Instant::now();
    {
        let q = queued.clone();
        let mut w = w.try_clone().unwrap();
        std::thread::spawn(move || {
            for line in rx {
                q.fetch_sub(line.len(), Ordering::AcqRel);
                if w.write_all(&line).is_err() {
                    break;
                }
            }
        });
    }
    shared.reply(
        conn,
        req.id,
        Ok(json!({
            "protocol": proto::PROTOCOL,
            "realm": shared.realm,
            "generation": shared.generation,
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "limits": proto::limits(),
            "capabilities": {
                "conditionalBreakpoints": true, "hitConditionalBreakpoints": true,
                "functionBreakpoints": true, "logPoints": true, "exceptionBreakpoints": false,
                "steppingGranularity": "line", "modes": ["reliable"], "console": true,
            },
        })),
    );
    let _ = stream.set_read_timeout(Some(Duration::from_millis(proto::LEASE_MS)));
    loop {
        let line = match read_line(&mut r) {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break,
        };
        *shared.last_seen.lock().unwrap() = Instant::now();
        if line.trim().is_empty() {
            continue;
        }
        let req = match proto::parse_request(&line) {
            Ok(q) => q,
            Err((id, m)) => {
                shared.send(conn, &proto::error(id, err::INVALID_ARGUMENT, m));
                continue;
            }
        };
        match req.op.as_str() {
            "heartbeat" => shared.reply(conn, req.id, Ok(json!({}))),
            "status" => shared.reply(conn, req.id, Ok(shared.cached_status())),
            "hello" => shared.reply(
                conn,
                req.id,
                Err((err::INVALID_ARGUMENT, "already authenticated".into())),
            ),
            "pause" => {
                shared.pause.store(true, Ordering::Release);
                shared.pause_ids.lock().unwrap().push((conn, req.id));
                shared.poke();
            }
            "detach" => {
                shared.detach_ids.lock().unwrap().push((conn, req.id));
                shared.poke();
            }
            _ => {
                let mut q = shared.jobs.lock().unwrap();
                if q.len() >= proto::MAX_QUEUE {
                    drop(q);
                    shared.reply(
                        conn,
                        req.id,
                        Err((err::BUSY, "VM request queue full".into())),
                    );
                    continue;
                }
                q.push_back(Job {
                    conn,
                    id: req.id,
                    op: req.op,
                    args: req.args,
                });
                drop(q);
                shared.poke();
            }
        }
    }
    // Connection lost / lease expired: owner cleans up at the next safe opportunity.
    shared.lost.store(conn, Ordering::Release);
    shared.poke();
}
