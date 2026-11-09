//! Small host-side helpers: path resolution, PATH lookup, mounts, xattrs,
//! and running external tools.

use std::ffi::CString;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// Exit with a message on stderr, like the script's `echo ... >&2; exit N`.
pub fn die(code: i32, msg: impl AsRef<str>) -> ! {
    eprintln!("sandbox: {}", msg.as_ref());
    std::process::exit(code)
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!("sandbox: {}", msg.as_ref());
}

/// `realpath -e`: resolve everything; the path must exist.
pub fn realpath_e(p: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(p)
}

/// `realpath -m`: resolve as far as the path exists, then append the rest.
pub fn realpath_m(p: &Path) -> PathBuf {
    let abs = absolute(p);
    let mut existing = abs.clone();
    let mut rest = Vec::new();
    while fs::symlink_metadata(&existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_owned());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = fs::canonicalize(&existing).unwrap_or(existing);
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

/// `realpath -s`: absolute and lexically normalized, symlinks kept.
pub fn absolute(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

/// `which`: the first executable regular file named `name` on PATH; a name
/// with a slash is taken as a path.
pub fn which(name: &str) -> Option<PathBuf> {
    let is_exe = |p: &Path| {
        fs::metadata(p)
            .map(|m| {
                m.is_file()
                    && (std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111) != 0
            })
            .unwrap_or(false)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_exe(&p).then_some(p);
    }
    std::env::var_os("PATH")?
        .as_bytes()
        .split(|b| *b == b':')
        .map(|dir| Path::new(std::ffi::OsStr::from_bytes(dir)).join(name))
        .find(|p| is_exe(p))
}

/// Is `path` a mount point? (From /proc/self/mounts, like `mountpoint -q`.)
pub fn is_mountpoint(path: &Path) -> bool {
    mounts().iter().any(|m| m == path)
}

/// Is anything mounted under `dir`?
pub fn has_mounts_under(dir: &Path) -> bool {
    !mounts_under(dir).is_empty()
}

/// The mount points under `dir`, in mount order.
pub fn mounts_under(dir: &Path) -> Vec<PathBuf> {
    mounts()
        .into_iter()
        .filter(|m| m.starts_with(dir) && m != dir)
        .collect()
}

fn mounts() -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| l.split(' ').nth(1))
        .map(unescape_mount)
        .collect()
}

/// /proc/mounts escapes space, tab, newline and backslash as \ooo.
fn unescape_mount(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    PathBuf::from(std::ffi::OsStr::from_bytes(&out))
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path without NUL")
}

/// Does `path` carry the user xattr `name`? (Symlinks are not followed.)
pub fn has_xattr(path: &Path, name: &str) -> bool {
    let p = cpath(path);
    let n = CString::new(name).unwrap();
    // SAFETY: valid C strings; a NULL buffer with size 0 only queries the size
    unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) >= 0 }
}

pub fn set_xattr(path: &Path, name: &str, value: &str) -> io::Result<()> {
    let p = cpath(path);
    let n = CString::new(name).unwrap();
    // SAFETY: valid C strings and a valid buffer of the given length
    let rc = unsafe {
        libc::lsetxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Run a tool, inheriting stdio; true on exit status 0.
pub fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a tool quietly and return its stdout on success.
pub fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn exists(cmd: &str) -> bool {
    which(cmd).is_some()
}

pub fn uid() -> u32 {
    // SAFETY: always succeeds
    unsafe { libc::getuid() }
}

pub fn gid() -> u32 {
    // SAFETY: always succeeds
    unsafe { libc::getgid() }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| die(1, "HOME is not set")))
}

/// `bytes` as a file for bwrap to bind at `dest` with mode `perms`, from
/// memory: the arguments, and the memfd they name. bwrap reads it to EOF
/// at start (`--ro-bind-data`), so the descriptor must stay open, and
/// inheritable, until bwrap has started; nothing is written to disk.
pub fn memfd_bind_args(
    label: &str,
    bytes: &[u8],
    perms: &str,
    dest: &Path,
) -> (OwnedFd, Vec<OsString>) {
    let name = std::ffi::CString::new(label).unwrap_or_else(|_| c"sandbox".to_owned());
    // SAFETY: a fresh memfd, inheritable on purpose (no MFD_CLOEXEC)
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        die(
            1,
            format!("memfd_create: {}", std::io::Error::last_os_error()),
        );
    }
    // SAFETY: the descriptor is ours and nobody else's
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut f = fs::File::from(
        fd.try_clone()
            .unwrap_or_else(|e| die(1, format!("dup: {e}"))),
    );
    f.write_all(bytes)
        .and_then(|_| f.seek(SeekFrom::Start(0)).map(|_| ()))
        .unwrap_or_else(|e| die(1, format!("writing {label} to memory: {e}")));
    let args = vec![
        "--perms".into(),
        perms.into(),
        "--ro-bind-data".into(),
        fd.as_raw_fd().to_string().into(),
        dest.as_os_str().to_owned(),
    ];
    (fd, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = home()
            .join("tmp/sandbox-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn absolute_normalizes_lexically() {
        assert_eq!(absolute(Path::new("/a/../b/./c")), PathBuf::from("/b/c"));
        assert_eq!(absolute(Path::new("/../x")), PathBuf::from("/x"));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(Path::new("rel/../y")), cwd.join("y"));
    }

    #[test]
    fn realpath_m_resolves_the_existing_part() {
        let d = scratch("util-realpath");
        let real = d.canonicalize().unwrap();
        assert_eq!(
            realpath_m(&d.join("missing/deeper")),
            real.join("missing/deeper")
        );
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(realpath_m(&link.join("new")), real.join("new"));
        assert!(realpath_e(&d.join("missing")).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mount_escapes() {
        assert_eq!(unescape_mount("/a\\040b"), PathBuf::from("/a b"));
        assert_eq!(unescape_mount("/plain"), PathBuf::from("/plain"));
        assert_eq!(unescape_mount("/end\\"), PathBuf::from("/end\\"));
    }

    #[test]
    fn mountpoints() {
        assert!(is_mountpoint(Path::new("/proc")));
        assert!(!is_mountpoint(Path::new("/proc/self")));
        assert!(has_mounts_under(Path::new("/")));
        let d = scratch("util-mounts");
        assert!(!has_mounts_under(&d));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn which_finds_executables() {
        assert!(which("sh").is_some());
        assert_eq!(which("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert!(which("surely-no-such-program-here").is_none());
        assert!(which("/etc/hostname").is_none()); // exists but not executable
    }

    #[test]
    fn xattrs_roundtrip() {
        let d = scratch("util-xattr");
        let f = d.join("f");
        fs::write(&f, "x").unwrap();
        assert!(!has_xattr(&f, "user.test"));
        set_xattr(&f, "user.test", "0:0:100644").unwrap();
        assert!(has_xattr(&f, "user.test"));
        let _ = fs::remove_dir_all(&d);
    }
}
