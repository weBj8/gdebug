//! Endpoint descriptor files (spec §5.1): private, atomic, generation-owned.

use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub realm: String,
    pub pid: u32,
    pub generation: u64,
    pub token: String,
}

impl Descriptor {
    pub fn to_json(&self) -> Value {
        json!({"protocol": self.protocol, "endpoint": format!("{}:{}", self.host, self.port),
            "realm": self.realm, "pid": self.pid, "generation": self.generation, "token": self.token})
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value =
            serde_json::from_str(text).map_err(|e| format!("descriptor is not JSON: {e}"))?;
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(format!("descriptor lacks {k}"))
        };
        let n = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .ok_or(format!("descriptor lacks {k}"))
        };
        let (host, port) = split_endpoint(&s("endpoint")?)?;
        Ok(Self {
            protocol: s("protocol")?,
            host,
            port,
            realm: s("realm")?,
            pid: n("pid")? as u32,
            generation: n("generation")?,
            token: s("token")?,
        })
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        check_private(path)?;
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read endpoint file {}: {e}", path.display()))?;
        Self::parse(&text)
    }
}

/// Parses `host:port` and requires a loopback host (spec: no non-loopback endpoints in v1).
pub fn split_endpoint(ep: &str) -> Result<(String, u16), String> {
    let addr: std::net::SocketAddr = ep
        .parse()
        .or_else(|_| ep.replacen("localhost:", "127.0.0.1:", 1).parse())
        .map_err(|_| format!("endpoint {ep:?} must be <loopback-ip>:<port>"))?;
    addr.ip()
        .is_loopback()
        .then(|| (addr.ip().to_string(), addr.port()))
        .ok_or_else(|| format!("endpoint {ep:?} is not loopback; use an SSH local forward"))
}

#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat endpoint file {}: {e}", path.display()))?;
    (m.permissions().mode() & 0o077 == 0)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "endpoint file {} is readable by other users; chmod 600 it",
                path.display()
            )
        })
}

#[cfg(not(unix))]
fn check_private(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Atomically writes `content` as an owner-only file. The parent directory must already exist
/// and be trusted (owned by us or root, not writable by others unless sticky).
pub fn write_private(path: &Path, content: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    check_dir(dir)?;
    let name = path
        .file_name()
        .ok_or("endpoint file path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    create_owner_only(&tmp, content.as_bytes())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot publish endpoint file {}: {e}", path.display())
    })
}

/// Removes `path` only if it still holds a descriptor for `generation` with `token`.
pub fn remove_owned(path: &Path, generation: u64, token: &str) {
    let owned = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| Descriptor::parse(&t).ok())
        .is_some_and(|d| d.generation == generation && crate::token_eq(&d.token, token));
    if owned {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
fn check_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m =
        std::fs::metadata(dir).map_err(|e| format!("endpoint directory {}: {e}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let owner_ok = m.uid() == me || m.uid() == 0;
    let mode = m.mode();
    let writable_by_others = mode & 0o022 != 0 && mode & 0o1000 == 0;
    (m.is_dir() && owner_ok && !writable_by_others).then_some(()).ok_or_else(|| {
        format!("endpoint directory {} is not trusted (must be owned by you and not writable by others)", dir.display())
    })
}

#[cfg(unix)]
fn create_owner_only(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // create_new = O_EXCL: never follows a pre-planted symlink.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    f.write_all(data)
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(windows)]
fn check_dir(dir: &Path) -> Result<(), String> {
    let m = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("endpoint directory {}: {e}", dir.display()))?;
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    (m.is_dir() && m.file_attributes() & REPARSE == 0)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "endpoint directory {} must be a real directory, not a reparse point",
                dir.display()
            )
        })
}

#[cfg(windows)]
fn create_owner_only(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    // Protected DACL granting full access to the owner only.
    let sddl = wide("D:P(A;;FA;;;OW)".as_ref());
    let mut sd = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated inputs; out pointer freed with LocalFree below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    (ok != 0)
        .then_some(())
        .ok_or("cannot build owner-only security descriptor")?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let p = wide(path.as_os_str());
    // SAFETY: CREATE_NEW never opens an existing file or reparse point target.
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            0x4000_0000,
            0,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: sd came from the conversion above.
    unsafe { LocalFree(sd as _) };
    (h != INVALID_HANDLE_VALUE)
        .then_some(())
        .ok_or_else(|| format!("cannot create {}", path.display()))?;
    // SAFETY: h is a valid, exclusively owned handle; File closes it exactly once.
    let mut f = unsafe { std::fs::File::from_raw_handle(h as _) };
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            split_endpoint("127.0.0.1:47000").unwrap(),
            ("127.0.0.1".into(), 47000)
        );
        assert_eq!(split_endpoint("localhost:1").unwrap().1, 1);
        assert!(split_endpoint("[::1]:5").is_ok());
        assert!(split_endpoint("0.0.0.0:5").is_err());
        assert!(split_endpoint("10.0.0.1:5").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_roundtrip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gdebug-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("server.json");
        let d = Descriptor {
            protocol: crate::PROTOCOL.into(),
            host: "127.0.0.1".into(),
            port: 4,
            realm: "server".into(),
            pid: 1,
            generation: 7,
            token: "t".into(),
        };
        write_private(&path, &d.to_json().to_string()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Descriptor::read(&path).unwrap(), d);
        remove_owned(&path, 8, "t");
        assert!(path.exists());
        remove_owned(&path, 7, "t");
        assert!(!path.exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(write_private(&path, "{}").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
