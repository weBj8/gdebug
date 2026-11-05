//! `gdebug` — agent CLI (persistent JSONL session) and DAP adapter for the gdebug native module.

mod conn;
mod dap;

use conn::{CLOSED, EXIT_ARGS, EXIT_CONN, EXIT_FATAL, Target};
use gdebug_proto::{self as proto, err};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "usage:
  gdebug attach --endpoint-file PATH --jsonl --allow-hook-replacement [--endpoint 127.0.0.1:PORT]
                [--realm server|client] [--stop-on-attach]
  gdebug dap      (Debug Adapter Protocol on stdin/stdout)";

fn die(code: i32, msg: &str) -> ! {
    eprintln!("gdebug: {msg}");
    std::process::exit(code)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("attach") => attach(&args[1..]),
        Some("dap") => dap::run(),
        Some("-h" | "--help" | "help") => println!("{USAGE}"),
        _ => die(EXIT_ARGS, USAGE),
    }
}

struct Opts {
    endpoint_file: PathBuf,
    endpoint: Option<String>,
    realm: Option<String>,
    stop_on_attach: bool,
}

fn parse_opts(args: &[String]) -> Opts {
    let (mut file, mut ep, mut realm) = (None, None, None);
    let (mut jsonl, mut consent, mut soa) = (false, false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .unwrap_or_else(|| die(EXIT_ARGS, &format!("{a} needs a value")))
        };
        match a.as_str() {
            "--endpoint-file" => file = Some(PathBuf::from(val())),
            "--endpoint" => ep = Some(val()),
            "--realm" => realm = Some(val()),
            "--jsonl" => jsonl = true,
            "--allow-hook-replacement" => consent = true,
            "--stop-on-attach" => soa = true,
            _ => die(EXIT_ARGS, &format!("unknown argument {a}\n{USAGE}")),
        }
    }
    if !jsonl {
        die(EXIT_ARGS, "--jsonl is required (the only v1 CLI mode)");
    }
    if !consent {
        die(
            EXIT_ARGS,
            "--allow-hook-replacement is required: attaching replaces the target's Lua debug hook (GMod's infinite-loop watchdog and profilers stop working while attached)",
        );
    }
    if realm
        .as_deref()
        .is_some_and(|r| r != "server" && r != "client")
    {
        die(EXIT_ARGS, "--realm must be server or client");
    }
    Opts {
        endpoint_file: file.unwrap_or_else(|| die(EXIT_ARGS, "--endpoint-file is required")),
        endpoint: ep,
        realm,
        stop_on_attach: soa,
    }
}

type Out = Arc<Mutex<std::io::Stdout>>;

fn emit(out: &Out, v: &Value) {
    let mut o = out.lock().unwrap();
    // A closed stdout means the agent went away; detach happens via the EOF path.
    let _ = writeln!(o, "{v}").and_then(|_| o.flush());
}

fn attach(args: &[String]) {
    let o = parse_opts(args);
    let (t, events, _hello) = Target::connect(&o.endpoint_file, o.endpoint.as_deref())
        .unwrap_or_else(|(c, m)| die(c, &m));
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut aargs =
        json!({"allowHookReplacement": true, "configure": true, "stopOnAttach": o.stop_on_attach});
    if let Some(r) = &o.realm {
        aargs["realm"] = json!(r);
    }
    match t
        .request("attach", aargs)
        .recv_timeout(Duration::from_secs(30))
    {
        Ok(mut v) => {
            v["id"] = json!(0);
            emit(&out, &v);
            if v["ok"] != true {
                die(
                    EXIT_FATAL,
                    &format!("attach failed: {}", v["error"]["message"]),
                );
            }
        }
        Err(_) => die(
            EXIT_CONN,
            "attach: no response (owner thread busy or connection lost)",
        ),
    }
    let eo = out.clone();
    std::thread::spawn(move || {
        for ev in events {
            if ev["event"] == CLOSED {
                die(EXIT_FATAL, "target connection closed");
            }
            emit(&eo, &ev);
        }
    });
    let active: Arc<Mutex<HashSet<i64>>> = Arc::default();
    install_sigint(t.clone());
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bounded read: framing beyond the message limit terminates safely.
        let n = (&mut lock)
            .take(proto::MAX_MESSAGE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        if buf.len() > proto::MAX_MESSAGE {
            emit(
                &out,
                &proto::error(
                    None,
                    err::LIMIT,
                    "input line exceeds the 1 MiB message limit; closing",
                ),
            );
            shutdown(&t, EXIT_ARGS);
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            emit(
                &out,
                &proto::error(None, err::INVALID_ARGUMENT, "input is not UTF-8"),
            );
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let req = match proto::parse_request(line) {
            Ok(r) => r,
            Err((id, m)) => {
                emit(&out, &proto::error(id, err::INVALID_ARGUMENT, m));
                continue;
            }
        };
        if req.id == 0 {
            emit(
                &out,
                &proto::error(
                    Some(0),
                    err::INVALID_ARGUMENT,
                    "id 0 is reserved for the attach response",
                ),
            );
            continue;
        }
        if !active.lock().unwrap().insert(req.id) {
            emit(
                &out,
                &proto::error(
                    Some(req.id),
                    err::INVALID_ARGUMENT,
                    "id is already in flight",
                ),
            );
            continue;
        }
        let rx = t.request(&req.op, req.args);
        let detach = req.op == "detach";
        let (out2, active2, uid) = (out.clone(), active.clone(), req.id);
        let h = std::thread::spawn(move || {
            let mut v = rx.recv().unwrap_or_else(|_| {
                proto::error(None, err::TARGET_CLOSED, "target connection closed")
            });
            v["id"] = json!(uid);
            emit(&out2, &v);
            active2.lock().unwrap().remove(&uid);
            v["ok"] == true
        });
        if detach {
            let ok = h.join().unwrap_or(false);
            std::process::exit(if ok { 0 } else { EXIT_FATAL });
        }
    }
    shutdown(&t, 0);
}

/// EOF/SIGINT: detach with a bounded wait, then exit.
fn shutdown(t: &Target, code: i32) -> ! {
    match t.call("detach", json!({}), Duration::from_secs(5)) {
        Ok(r) if r["hookRestored"] == false => die(
            EXIT_FATAL,
            &format!("detached, but restoration unconfirmed: {}", r["warnings"]),
        ),
        Ok(_) => std::process::exit(code),
        Err(e) => die(
            EXIT_FATAL,
            &format!("detach unconfirmed: {}", e["error"]["message"]),
        ),
    }
}

#[cfg(unix)]
fn install_sigint(t: Arc<Target>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HIT: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_sigint(_: libc::c_int) {
        HIT.store(true, Ordering::SeqCst);
    }
    // SAFETY: the handler only stores an atomic.
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
    std::thread::spawn(move || {
        while !HIT.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("gdebug: interrupted; detaching");
        shutdown(&t, 0);
    });
}

// shortcut: on Windows Ctrl+C kills the CLI; the target's disconnect cleanup still detaches, but
// restoration is not reported. Add SetConsoleCtrlHandler if agents need the report there.
#[cfg(not(unix))]
fn install_sigint(_: Arc<Target>) {}
