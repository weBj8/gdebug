//! Breakpoint pure rules: hit grammar (spec §7.2) and logpoint templates.

/// Hit predicate over the raw encounter count (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Eq(u64),
    Ge(u64),
    Every(u64),
}

impl Hit {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (ctor, num): (fn(u64) -> Hit, &str) = match s {
            s if s.starts_with("==") => (Hit::Eq, &s[2..]),
            s if s.starts_with(">=") => (Hit::Ge, &s[2..]),
            s if s.starts_with('%') => (Hit::Every, &s[1..]),
            s => (Hit::Eq, s),
        };
        let num = num.trim();
        let ok = !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit());
        let n: u64 = ok
            .then(|| num.parse().ok())
            .flatten()
            .ok_or_else(|| format!("invalid hit condition {s:?}: expected N, == N, >= N or % N"))?;
        (n > 0)
            .then(|| ctor(n))
            .ok_or_else(|| "hit condition count must be positive".into())
    }

    pub fn matches(self, count: u64) -> bool {
        match self {
            Hit::Eq(n) => count == n,
            Hit::Ge(n) => count >= n,
            Hit::Every(n) => count % n == 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Expr(String),
}

/// Splits a logpoint message into literal text and `{expr}` parts, honouring `{{`/`}}`,
/// nested braces, quoted strings and Lua long strings/comments inside expressions.
pub fn parse_log(msg: &str) -> Result<Vec<Part>, String> {
    let b = msg.as_bytes();
    let (mut parts, mut text, mut i) = (Vec::new(), Vec::<u8>::new(), 0);
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'{', Some(b'{')) | (b'}', Some(b'}')) => {
                text.push(b[i]);
                i += 2;
            }
            (b'}', _) => {
                return Err(format!(
                    "unmatched '}}' at byte {i}; use '}}}}' for a literal brace"
                ));
            }
            (b'{', _) => {
                let end = expr_end(b, i + 1)?;
                let e = msg[i + 1..end].trim();
                (!e.is_empty())
                    .then_some(())
                    .ok_or("empty logpoint expression")?;
                if !text.is_empty() {
                    parts.push(Part::Text(
                        String::from_utf8(std::mem::take(&mut text)).unwrap_or_default(),
                    ));
                }
                parts.push(Part::Expr(e.to_owned()));
                i = end + 1;
            }
            (c, _) => {
                text.push(c);
                i += 1;
            }
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(String::from_utf8(text).unwrap_or_default()));
    }
    Ok(parts)
}

/// Long bracket level at `i` (`[[` → 0, `[==[` → 2).
fn long_open(b: &[u8], i: usize) -> Option<usize> {
    (b.get(i) == Some(&b'[')).then_some(())?;
    let eq = b[i + 1..].iter().take_while(|c| **c == b'=').count();
    (b.get(i + 1 + eq) == Some(&b'[')).then_some(eq)
}

fn long_close(b: &[u8], from: usize, eq: usize) -> Result<usize, String> {
    let close: Vec<u8> = [b"]".as_slice(), &vec![b'='; eq], b"]"].concat();
    b[from..]
        .windows(close.len())
        .position(|w| w == close.as_slice())
        .map(|p| from + p + close.len())
        .ok_or_else(|| "unterminated long string in logpoint expression".into())
}

/// Index of the `}` closing an expression starting at `start`.
fn expr_end(b: &[u8], start: usize) -> Result<usize, String> {
    let (mut depth, mut i) = (0usize, start);
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                (i < b.len())
                    .then_some(())
                    .ok_or("unterminated string in logpoint expression")?;
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = match long_open(b, i + 2) {
                    Some(eq) => long_close(b, i + 4 + eq, eq)?,
                    None => {
                        return Err("line comments are not allowed in logpoint expressions".into());
                    }
                };
            }
            b'[' if long_open(b, i).is_some() => {
                let eq = long_open(b, i).unwrap_or(0);
                i = long_close(b, i + 2 + eq, eq)?;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' if depth == 0 => return Ok(i),
            b'}' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Err("unterminated '{' in logpoint message".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits() {
        assert_eq!(Hit::parse("3"), Ok(Hit::Eq(3)));
        assert_eq!(Hit::parse("== 3"), Ok(Hit::Eq(3)));
        assert_eq!(Hit::parse(">=2"), Ok(Hit::Ge(2)));
        assert_eq!(Hit::parse("% 4"), Ok(Hit::Every(4)));
        for bad in ["0", "-1", "> 3", "", "99999999999999999999999", "3x", "%"] {
            assert!(Hit::parse(bad).is_err(), "{bad}");
        }
        assert!(Hit::Every(2).matches(4) && !Hit::Every(2).matches(3));
        assert!(Hit::Ge(2).matches(2) && !Hit::Ge(2).matches(1));
    }

    #[test]
    fn logs() {
        use Part::*;
        assert_eq!(
            parse_log("x={x} {{lit}} t={ {a=1} }").unwrap(),
            vec![
                Text("x=".into()),
                Expr("x".into()),
                Text(" {lit} t=".into()),
                Expr("{a=1}".into())
            ]
        );
        assert_eq!(
            parse_log(r#"{ "}" .. '{' }"#).unwrap(),
            vec![Expr(r#""}" .. '{'"#.into())]
        );
        assert_eq!(
            parse_log("{ [==[ } ]] ]==] }").unwrap(),
            vec![Expr("[==[ } ]] ]==]".into())]
        );
        assert_eq!(
            parse_log(r#"{ "a\"}" }"#).unwrap(),
            vec![Expr(r#""a\"}""#.into())]
        );
        assert_eq!(parse_log("{t[1]}").unwrap(), vec![Expr("t[1]".into())]);
        for bad in ["{x", "x}", "{}", "{ 'a }", "{ [[ x }"] {
            assert!(parse_log(bad).is_err(), "{bad}");
        }
    }
}
