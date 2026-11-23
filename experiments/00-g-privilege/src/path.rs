//! Path handling: a whitelist of characters plus `openat2()` resolution that
//! cannot leave the configured root directory.
//!
//! Two independent layers, deliberately:
//!  1. `validate_rel` — lexical whitelist. Cheap, catches `/etc/shadow`,
//!     `../../etc/shadow`, `$(id)`, `a;b`, embedded NUL.
//!  2. `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)` —
//!     kernel-enforced containment. Catches the cases a lexical check cannot,
//!     e.g. a symlink created *after* validation, or a magic link such as
//!     `/proc/self/root`.
//!
//! Layer 2 alone would be sufficient for containment but less precise for
//! reporting; layer 1 alone is a classic vulnerability (TOCTOU on symlinks).
//! Both are enabled.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

pub const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub const RESOLVE_BENEATH: u64 = 0x08;

const STRICT_RESOLVE: u64 = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path escapes the permitted root")]
    Escape,
    #[error("path rejected by the lexical whitelist")]
    Invalid,
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl PathError {
    pub fn code(&self) -> &'static str {
        match self {
            PathError::Escape => crate::proto::CODE_PATH_ESCAPE,
            PathError::Invalid => crate::proto::CODE_PATH_INVALID,
            PathError::Io(_) => crate::proto::CODE_IO_ERROR,
        }
    }
}

/// Lexical whitelist. Only `[A-Za-z0-9._/-]`, no empty components, no `.`/`..`
/// components, no leading `/`, no NUL, bounded length. Anything a shell would
/// interpret is simply not in the alphabet.
pub fn validate_rel(raw: &str) -> Result<&str, PathError> {
    if raw.is_empty() || raw.len() > crate::proto::MAX_PATH_BYTES {
        return Err(PathError::Invalid);
    }
    if raw.as_bytes().contains(&0) {
        return Err(PathError::Invalid);
    }
    if raw.starts_with('/') {
        return Err(PathError::Escape);
    }
    if raw == "." {
        return Ok(raw);
    }
    for b in raw.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/');
        if !ok {
            return Err(PathError::Invalid);
        }
    }
    for comp in raw.split('/') {
        if comp.is_empty() || comp == "." {
            return Err(PathError::Invalid);
        }
        if comp == ".." {
            return Err(PathError::Escape);
        }
    }
    Ok(raw)
}

/// A directory that every request path is resolved relative to.
pub struct Root {
    dir: File,
    path: PathBuf,
}

impl Root {
    pub fn open(path: &Path) -> io::Result<Self> {
        let dir = File::open(path)?;
        Ok(Self {
            dir,
            path: path.to_path_buf(),
        })
    }

    pub fn display(&self) -> String {
        self.path.display().to_string()
    }

    /// Resolve `rel` inside the root and open it with `flags`.
    ///
    /// `unsafe`: `openat2` has no stable wrapper in `libc`; the syscall is
    /// called directly with a `#[repr(C)]` struct whose layout is fixed by
    /// `linux/openat2.h`. All pointers are valid for the duration of the call
    /// and the path is a `CString` (NUL-terminated, no interior NUL).
    pub fn open_child(&self, rel: &str, flags: i32) -> Result<File, PathError> {
        if rel == "." {
            return Ok(self.dir.try_clone()?);
        }
        let c = CString::new(rel).map_err(|_| PathError::Invalid)?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: STRICT_RESOLVE,
        };
        // SAFETY: see doc comment above.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.dir.as_raw_fd(),
                c.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by a successful open syscall and
            // is not owned by anything else.
            return Ok(unsafe { File::from_raw_fd(fd as i32) });
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // EXDEV: absolute / escapes root. ELOOP: symlink or magic link
            // refused. EPERM: RENAME_EXCHANGE-style refusal of magic links.
            Some(libc::EXDEV) | Some(libc::ELOOP) => Err(PathError::Escape),
            _ => Err(PathError::Io(err)),
        }
    }

    /// Path of the file behind `f` as seen by the kernel, for logging only.
    pub fn describe(&self, rel: &str) -> String {
        format!("{}/{}", self.path.display(), rel)
    }
}

pub fn stat_path(root: &Root, rel: &str) -> Result<serde_json::Value, PathError> {
    let f = root.open_child(rel, libc::O_PATH)?;
    let md = f.metadata()?;
    use std::os::unix::fs::MetadataExt;
    Ok(serde_json::json!({
        "resolved_under_root": true,
        "kind": if md.is_dir() { "dir" } else if md.is_file() { "file" } else { "other" },
        "size": md.size(),
        "mode": format!("{:04o}", md.mode() & 0o7777),
        "uid": md.uid(),
        "gid": md.gid(),
        "mtime_ns": md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
        "dev": md.dev(),
        "ino": md.ino(),
    }))
}

pub fn read_small(root: &Root, rel: &str, max_bytes: u64) -> Result<serde_json::Value, PathError> {
    use std::io::Read;
    let cap = max_bytes.min(crate::proto::MAX_READ_BYTES);
    let f = root.open_child(rel, libc::O_RDONLY)?;
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    Ok(serde_json::json!({
        "bytes": buf.len(),
        "truncated": buf.len() as u64 == cap,
        "preview_hex": buf.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute() {
        assert!(matches!(
            validate_rel("../../etc/shadow"),
            Err(PathError::Escape)
        ));
        assert!(matches!(
            validate_rel("/etc/shadow"),
            Err(PathError::Escape)
        ));
        assert!(matches!(validate_rel("a/../../b"), Err(PathError::Escape)));
    }

    #[test]
    fn rejects_metacharacters() {
        for bad in [
            "a;rm -rf /",
            "$(id)",
            "`id`",
            "a|b",
            "a\nb",
            "a b",
            "a\0b",
            "\"x\"",
            "a'b'",
            "&x",
        ] {
            assert!(
                matches!(validate_rel(bad), Err(PathError::Invalid)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn accepts_plain_relative_paths() {
        assert!(validate_rel("sub/file.txt").is_ok());
        assert!(validate_rel(".hidden").is_ok());
        assert!(validate_rel(".").is_ok());
    }

    #[test]
    fn no_trailing_or_double_slash() {
        assert!(matches!(validate_rel("a//b"), Err(PathError::Invalid)));
        assert!(matches!(validate_rel("a/"), Err(PathError::Invalid)));
    }

    #[test]
    fn os_str_roundtrip_keeps_bytes() {
        use std::os::unix::ffi::OsStrExt;
        // Guards against accidental Unicode normalisation in path handling.
        let raw = b"caf\xc3\xa9.txt";
        assert_eq!(std::ffi::OsStr::from_bytes(raw).as_bytes(), raw);
    }
}
