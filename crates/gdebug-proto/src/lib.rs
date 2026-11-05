//! Shared `gdebug/1` protocol vocabulary and pure rules (spec §2, §5–§7).
//! Used by the native module, the JSONL CLI and the DAP adapter so the rules exist once.

pub mod bp;
pub mod descriptor;
pub mod path;

use serde_json::{Value, json};

pub const PROTOCOL: &str = "gdebug/1";
pub const MAX_MESSAGE: usize = 1 << 20;
pub const MAX_EXPRESSION: usize = 64 * 1024;
pub const MAX_QUEUE: usize = 64;
pub const MAX_OUTBOUND: usize = 4 << 20;
pub const MAX_BREAKPOINTS: usize = 1024;
pub const MAX_PAGE: usize = 500;
pub const MAX_PREVIEW: usize = 4096;
pub const MAX_SNAPSHOT: usize = 10_000;
pub const MAX_CONSOLE_ARGS: usize = 64;
pub const MAX_CONSOLE_ARG: usize = 1024;
pub const MAX_SOURCE: usize = 4 << 20;
pub const LEASE_MS: u64 = 10_000;
pub const HEARTBEAT_MS: u64 = 2_000;

pub fn limits() -> Value {
    json!({
        "maxMessageBytes": MAX_MESSAGE, "maxExpressionBytes": MAX_EXPRESSION,
        "maxQueuedRequests": MAX_QUEUE, "maxOutboundBytes": MAX_OUTBOUND,
        "maxBreakpoints": MAX_BREAKPOINTS, "maxPage": MAX_PAGE, "maxPreviewBytes": MAX_PREVIEW,
        "maxSnapshotEntries": MAX_SNAPSHOT, "leaseMs": LEASE_MS, "heartbeatMs": HEARTBEAT_MS,
    })
}

/// Stable machine-readable error codes.
pub mod err {
    pub const AUTH: &str = "ERR_AUTH";
    pub const VERSION: &str = "ERR_VERSION";
    pub const BUSY: &str = "ERR_BUSY";
    pub const NOT_ATTACHED: &str = "ERR_NOT_ATTACHED";
    pub const NOT_STOPPED: &str = "ERR_NOT_STOPPED";
    pub const STALE_HANDLE: &str = "ERR_STALE_HANDLE";
    pub const INVALID_ARGUMENT: &str = "ERR_INVALID_ARGUMENT";
    pub const UNSUPPORTED_TARGET: &str = "ERR_UNSUPPORTED_TARGET";
    pub const UNSUPPORTED_CONTEXT: &str = "ERR_UNSUPPORTED_CONTEXT";
    pub const LIMIT: &str = "ERR_LIMIT";
    pub const COMPILE: &str = "ERR_COMPILE";
    pub const EVALUATION: &str = "ERR_EVALUATION";
    pub const HOOK_CONFLICT: &str = "ERR_HOOK_CONFLICT";
    pub const TARGET_CLOSED: &str = "ERR_TARGET_CLOSED";
    pub const TIMEOUT: &str = "ERR_TIMEOUT";
    pub const INTERNAL: &str = "ERR_INTERNAL";

    pub fn retryable(code: &str) -> bool {
        matches!(code, BUSY | NOT_STOPPED | TIMEOUT)
    }
}

/// A parsed request envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: i64,
    pub op: String,
    pub args: Value,
}

/// Parses one complete JSON line. On error returns the correlatable id (if any) and a message.
pub fn parse_request(line: &str) -> Result<Request, (Option<i64>, String)> {
    let v: Value =
        serde_json::from_str(line).map_err(|e| (None, format!("malformed JSON: {e}")))?;
    let id = v.get("id").and_then(Value::as_i64);
    let fail = |m: &str| (id, m.to_owned());
    let obj = v
        .as_object()
        .ok_or_else(|| fail("request must be a JSON object"))?;
    let id = id
        .filter(|i| *i >= 0)
        .ok_or_else(|| fail("id must be a non-negative integer"))?;
    let op = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("op must be a string"))?;
    let args = obj.get("args").cloned().unwrap_or_else(|| json!({}));
    args.is_object()
        .then_some(())
        .ok_or_else(|| fail("args must be an object"))?;
    Ok(Request {
        id,
        op: op.to_owned(),
        args,
    })
}

pub fn ok(id: i64, result: Value) -> Value {
    json!({"type": "response", "id": id, "ok": true, "result": result})
}

pub fn error(id: Option<i64>, code: &str, message: impl Into<String>) -> Value {
    json!({"type": "response", "id": id, "ok": false,
        "error": {"code": code, "message": message.into(), "retryable": err::retryable(code)}})
}

pub fn event(seq: u64, name: &str, data: Value) -> Value {
    json!({"type": "event", "seq": seq, "event": name, "data": data})
}

/// Constant-time-ish token comparison (no content-dependent early exit).
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let diff = a
        .iter()
        .zip(b)
        .fold(a.len() ^ b.len(), |d, (x, y)| d | (x ^ y) as usize);
    diff == 0
}

/// Lua-style quoted, lossless preview of raw string bytes, truncated at `max` input bytes.
pub fn lua_quote(bytes: &[u8], max: usize) -> String {
    let mut s = String::with_capacity(bytes.len().min(max) + 2);
    s.push('"');
    let shown = &bytes[..bytes.len().min(max)];
    let mut rest = shown;
    while !rest.is_empty() {
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(v) => (v, &[][..]),
            Err(e) => (
                std::str::from_utf8(&rest[..e.valid_up_to()]).unwrap_or_default(),
                &rest[e.valid_up_to()..],
            ),
        };
        valid.chars().for_each(|c| match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => s.push_str(&format!("\\{}", c as u32)),
            c => s.push(c),
        });
        if let Some(b) = bad.first() {
            s.push_str(&format!("\\x{b:02X}"));
            rest = &bad[1..];
        } else {
            rest = &[];
        }
    }
    s.push('"');
    s
}

/// Number formatting that keeps NaN/infinities explicit and integers exact.
pub fn lua_number(n: f64) -> String {
    match n {
        n if n.is_nan() => "nan".into(),
        n if n == f64::INFINITY => "inf".into(),
        n if n == f64::NEG_INFINITY => "-inf".into(),
        n if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 => format!("{}", n as i64),
        n => format!("{n}"),
    }
}

pub fn is_identifier(s: &[u8]) -> bool {
    const KW: [&[u8]; 22] = [
        b"and",
        b"break",
        b"do",
        b"else",
        b"elseif",
        b"end",
        b"false",
        b"for",
        b"function",
        b"if",
        b"in",
        b"local",
        b"nil",
        b"not",
        b"or",
        b"repeat",
        b"return",
        b"then",
        b"true",
        b"until",
        b"while",
        b"continue",
    ];
    !s.is_empty()
        && !s[0].is_ascii_digit()
        && s.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
        && !KW.contains(&s)
}

/// Minimal standard base64 (for explicitly requested raw string bytes).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    data.chunks(3)
        .flat_map(|c| {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            (0..4).map(move |i| match i <= c.len() {
                true => T[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            })
        })
        .collect()
}

/// Console command validation (spec §8.3): name + argument strings, no injection.
pub fn validate_console(name: &str, args: &[String]) -> Result<(), String> {
    let bad = |s: &str| s.contains(['\0', '\n', '\r', ';']);
    (!name.is_empty() && name.len() <= 128 && !bad(name) && !name.contains([' ', '"']))
        .then_some(())
        .ok_or("invalid console command name")?;
    (args.len() <= MAX_CONSOLE_ARGS)
        .then_some(())
        .ok_or("too many console arguments")?;
    args.iter()
        .all(|a| a.len() <= MAX_CONSOLE_ARG && !bad(a) && !a.contains('"'))
        .then_some(())
        .ok_or_else(|| "console argument too long or contains a separator/control character".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        let r = parse_request(r#"{"id":3,"op":"stack","args":{"stopId":"stop-1"}}"#).unwrap();
        assert_eq!((r.id, r.op.as_str()), (3, "stack"));
        assert_eq!(
            parse_request(r#"{"id":4,"args":{}}"#).unwrap_err().0,
            Some(4)
        );
        assert_eq!(parse_request("{nope").unwrap_err().0, None);
        assert!(parse_request(r#"{"id":1,"op":"x","args":[]}"#).is_err());
    }

    #[test]
    fn quoting_and_numbers() {
        assert_eq!(lua_quote(b"a\"b\n\xff", 100), r#""a\"b\n\xFF""#);
        assert_eq!(lua_quote(b"abcdef", 3), r#""abc""#);
        assert_eq!(lua_number(3.0), "3");
        assert_eq!(lua_number(0.5), "0.5");
        assert_eq!(lua_number(f64::NAN), "nan");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert!(is_identifier(b"_x1") && !is_identifier(b"end") && !is_identifier(b"1a"));
        assert!(token_eq("abc", "abc") && !token_eq("abc", "abd") && !token_eq("ab", "abc"));
    }

    #[test]
    fn console() {
        assert!(validate_console("status", &[]).is_ok());
        assert!(validate_console("say", &["hi; quit".into()]).is_err());
        assert!(validate_console("say\n", &[]).is_err());
    }
}
