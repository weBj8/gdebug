//! One authenticated `gdebug/1` target connection shared by the JSONL CLI and the DAP adapter.
//! A reader thread routes responses by internal ID and forwards events; a heartbeat thread keeps
//! the owner lease alive independently of user input.

use gdebug_proto::{self as proto, descriptor};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Exit codes (spec §9).
pub const EXIT_FATAL: i32 = 1;
pub const EXIT_ARGS: i32 = 2;
pub const EXIT_CONN: i32 = 3;
pub const EXIT_VERSION: i32 = 4;

pub struct Target {
    w: Mutex<TcpStream>,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, Sender<Value>>>,
    pub closed: AtomicBool,
}

/// Synthetic event emitted when the target connection ends.
pub const CLOSED: &str = "connectionClosed";

impl Target {
    /// Connects using the private descriptor (optionally overriding host:port for an SSH
    /// forward) and authenticates. Returns the target, its event stream and the hello result.
    pub fn connect(
        endpoint_file: &Path,
        endpoint: Option<&str>,
    ) -> Result<(Arc<Target>, Receiver<Value>, Value), (i32, String)> {
        let d = descriptor::Descriptor::read(endpoint_file).map_err(|e| (EXIT_CONN, e))?;
        let (host, port) = match endpoint {
            Some(ep) => descriptor::split_endpoint(ep).map_err(|e| (EXIT_ARGS, e))?,
            None => descriptor::split_endpoint(&format!("{}:{}", d.host, d.port))
                .map_err(|e| (EXIT_CONN, e))?,
        };
        let s = TcpStream::connect((host.as_str(), port))
            .map_err(|e| (EXIT_CONN, format!("connect {host}:{port}: {e}")))?;
        s.set_nodelay(true).ok();
        let r = s.try_clone().map_err(|e| (EXIT_CONN, e.to_string()))?;
        let t = Arc::new(Target {
            w: Mutex::new(s),
            next: AtomicI64::new(1),
            pending: Mutex::default(),
            closed: AtomicBool::new(false),
        });
        let (etx, erx) = channel();
        let rt = t.clone();
        std::thread::spawn(move || rt.read_loop(r, etx));
        let hello = t.call(
            "hello",
            json!({"protocol": proto::PROTOCOL, "token": d.token, "client": "gdebug-cli"}),
            Duration::from_secs(10),
        );
        match hello {
            Ok(h) => {
                let ht = t.clone();
                std::thread::spawn(move || {
                    while !ht.closed.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(proto::HEARTBEAT_MS));
                        drop(ht.request("heartbeat", json!({})));
                    }
                });
                Ok((t, erx, h))
            }
            Err(e) => {
                let code = match e["error"]["code"].as_str() {
                    Some(proto::err::VERSION) => EXIT_VERSION,
                    _ => EXIT_CONN,
                };
                Err((
                    code,
                    format!(
                        "hello failed: {}",
                        e["error"]["message"]
                            .as_str()
                            .unwrap_or("connection closed")
                    ),
                ))
            }
        }
    }

    fn read_loop(&self, r: TcpStream, events: Sender<Value>) {
        let mut r = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match v["type"].as_str() {
                Some("response") => {
                    if let Some(tx) = v["id"]
                        .as_i64()
                        .and_then(|id| self.pending.lock().unwrap().remove(&id))
                    {
                        let _ = tx.send(v);
                    }
                }
                Some("event") => {
                    let _ = events.send(v);
                }
                _ => {}
            }
        }
        self.closed.store(true, Ordering::Release);
        self.pending.lock().unwrap().clear();
        let _ = events.send(json!({"type": "event", "event": CLOSED, "data": {}}));
    }

    /// Sends a request; the receiver yields the full response object (or disconnects).
    pub fn request(&self, op: &str, args: Value) -> Receiver<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"id": id, "op": op, "args": args}).to_string() + "\n";
        if self.w.lock().unwrap().write_all(line.as_bytes()).is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        rx
    }

    /// Request + wait. Ok(result) or Err(full error response / synthetic closed error).
    pub fn call(&self, op: &str, args: Value, timeout: Duration) -> Result<Value, Value> {
        match self.request(op, args).recv_timeout(timeout) {
            Ok(v) if v["ok"] == true => Ok(v["result"].clone()),
            Ok(v) => Err(v),
            Err(_) => Err(proto::error(
                None,
                proto::err::TIMEOUT,
                format!("{op}: no response (target closed or blocked)"),
            )),
        }
    }
}
