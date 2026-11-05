//! Source identity (spec §6): normalisation, runtime matching and explicit path mappings.

/// Normalises separators and `.`/`..` segments without changing case.
pub fn normalize(p: &str) -> String {
    let p = p.replace('\\', "/");
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let j = out.join("/");
    if abs { format!("/{j}") } else { j }
}

/// File path of a raw Lua chunk label (`@path`); other labels are not files.
pub fn label_path(label: &[u8]) -> Option<String> {
    label
        .strip_prefix(b"@")
        .map(|p| normalize(&String::from_utf8_lossy(p)))
}

/// True if `prefix` is `path` or a whole-segment prefix of it.
fn has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path == prefix
        || (path.starts_with(prefix) && path[prefix.len()..].starts_with('/'))
        || prefix.is_empty()
}

/// Does a breakpoint's target path select a runtime source path? Segment-boundary suffix match,
/// never basename-only unless the user gave only a basename.
pub fn runtime_matches(bp: &str, runtime: &str) -> bool {
    let bp = normalize(bp);
    runtime == bp || (runtime.ends_with(&bp) && runtime[..runtime.len() - bp.len()].ends_with('/'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub local: String,
    pub remote: String,
}

fn map(
    maps: &[Mapping],
    path: &str,
    from: fn(&Mapping) -> &str,
    to: fn(&Mapping) -> &str,
) -> Result<Option<String>, String> {
    let path = normalize(path);
    let hits: Vec<&Mapping> = maps
        .iter()
        .filter(|m| has_prefix(&path, &normalize(from(m))))
        .collect();
    let best = hits.iter().map(|m| normalize(from(m)).len()).max();
    let top: Vec<&&Mapping> = hits
        .iter()
        .filter(|m| Some(normalize(from(m)).len()) == best)
        .collect();
    match top.as_slice() {
        [] => Ok(None),
        [m] => {
            let base = normalize(from(m));
            let rest = path[base.len()..].trim_start_matches('/');
            let target = normalize(to(m));
            Ok(Some(match (target.is_empty(), rest.is_empty()) {
                (true, _) => rest.to_owned(),
                (_, true) => target,
                _ => format!("{target}/{rest}"),
            }))
        }
        _ => Err(format!("ambiguous path mapping for {path}")),
    }
}

/// Local editor path → target virtual path via the longest explicit mapping.
pub fn to_remote(maps: &[Mapping], local: &str) -> Result<Option<String>, String> {
    map(maps, local, |m| &m.local, |m| &m.remote)
}

/// Target virtual path → local editor path via the longest explicit mapping.
pub fn to_local(maps: &[Mapping], remote: &str) -> Result<Option<String>, String> {
    map(maps, remote, |m| &m.remote, |m| &m.local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise() {
        assert_eq!(normalize(r"lua\autorun\.\x\..\a.lua"), "lua/autorun/a.lua");
        assert_eq!(normalize("/a//b/"), "/a/b");
        assert_eq!(normalize("Lua/A.lua"), "Lua/A.lua");
        assert_eq!(
            label_path(b"@addons/x/lua/a.lua").as_deref(),
            Some("addons/x/lua/a.lua")
        );
        assert_eq!(label_path(b"=stdin"), None);
    }

    #[test]
    fn matching() {
        assert!(runtime_matches("lua/a/b.lua", "addons/x/lua/a/b.lua"));
        assert!(runtime_matches("lua/a/b.lua", "lua/a/b.lua"));
        assert!(!runtime_matches("a/b.lua", "lua/xa/b.lua"));
        assert!(!runtime_matches("lua/a/b.lua", "lua/a/B.lua"));
    }

    #[test]
    fn mappings() {
        let m = |l: &str, r: &str| Mapping {
            local: l.into(),
            remote: r.into(),
        };
        let maps = [m("/w/addon", "addons/addon"), m("/w/addon/lua", "lua")];
        assert_eq!(
            to_remote(&maps, "/w/addon/lua/x.lua").unwrap().as_deref(),
            Some("lua/x.lua")
        );
        assert_eq!(
            to_remote(&maps, "/w/addon/materials/a").unwrap().as_deref(),
            Some("addons/addon/materials/a")
        );
        assert_eq!(to_remote(&maps, "/w/addonx/lua/x.lua").unwrap(), None);
        assert_eq!(
            to_local(&maps, "lua/x.lua").unwrap().as_deref(),
            Some("/w/addon/lua/x.lua")
        );
        let amb = [m("/a", "lua"), m("/b", "lua")];
        assert!(to_local(&amb, "lua/x.lua").is_err());
    }
}
