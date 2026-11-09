//! Running a command in a sandbox that is already up, instead of a second
//! one next to it: processes of both then share namespaces — pids, /tmp,
//! IPC, the network — and can talk to each other as they would on the host
//! (a browser's second window, a shell next to an app for a look inside).
//!
//! Which sandbox is "the same": one started with the same box and the same
//! options that shape it (binds, network, X11, permissions, --root), hashed
//! into a key. What the sandbox has because of its app — /snap for a snap,
//! an app outside every bind bound by itself — is recorded with it instead,
//! and a command joins if the sandbox has what it needs: a shell can join a
//! snap's sandbox, a snap can't join a shell's. A starting sandbox registers
//! its own pid in `$XDG_RUNTIME_DIR/sandpad/<key>-<what it has>`; the
//! process either becomes bwrap or stays its parent, so a later run finds
//! bwrap by descending from that pid, bwrap's init inside the sandbox below
//! it, and the app below that. The app's namespaces are the ones to join: with -n
//! it sits in a nested user namespace of its own (unroot.rs). Entries are
//! never removed; a dead or reused pid, told by the process start time
//! recorded with it, is simply a stale one.
//!
//! Joining is what nsenter does: one setns() on the app's pidfd naming
//! every namespace at once. One call, not one per namespace, because the
//! kernel then checks each of them against our host credentials, which own
//! the sandbox's user namespace (we created it) and so hold CAP_SYS_ADMIN
//! over all of them — the nested one of -n included. Joining the user
//! namespace first and the others after it is refused on Ubuntu
//! (`kernel.apparmor_restrict_unprivileged_unconfined`: once inside, an
//! unconfined process is treated as unprivileged and the later checks get
//! EPERM); nsenter falls back to the one-call form for the same reason.
//! The joined process then holds every capability the namespace grants;
//! the child that becomes the command gives them up the way bwrap's does
//! (bounding set and inheritable cleared, no_new_privs), keeping only the
//! app binary's file capabilities as ambient ones, and takes the same
//! session, working directory and environment bwrap would have given it.
//! One difference remains: bwrap's children run under Ubuntu's bwrap
//! AppArmor profile, a joined command doesn't.

use crate::util::{die, warn};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The running sandbox: its app, whose namespaces are the ones to join.
pub struct Target {
    pub app: i32,
}

/// An environment change bwrap was told to make (`--setenv`/`--unsetenv`).
pub enum EnvEdit {
    Set(OsString, OsString),
    Unset(OsString),
}

/// A sandbox's key: FNV-1a over what shapes it, as a hex file name.
pub fn key(parts: &[String]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.as_bytes().iter().chain(b"\0") {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

/// What a sandbox has because of its app, beyond the key: /snap bound,
/// an app bound in by itself.
#[derive(Default, PartialEq)]
pub struct Provides {
    pub snap: bool,
    pub app: Option<PathBuf>,
}

impl Provides {
    fn covers(&self, needs: &Provides) -> bool {
        (self.snap || !needs.snap) && (needs.app.is_none() || needs.app == self.app)
    }
}

/// The registry entries of a key, locked while they are read or written:
/// one file per sandbox shape, `<key>-<what it has>`, holding the sandbox
/// process's pid and start time and what it has.
pub struct Registry {
    dir: PathBuf,
    key: String,
    _lock: fs::File,
}

impl Registry {
    pub fn open(runtime: &Path, key: &str) -> Registry {
        let dir = runtime.join("sandpad");
        let _ = fs::create_dir(&dir);
        let path = dir.join(format!("{key}.lock"));
        let lock = fs::File::create(&path)
            .unwrap_or_else(|e| die(1, format!("cannot open {}: {e}", path.display())));
        // SAFETY: a valid fd; the lock goes with the file (closed at exec)
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        Registry {
            dir,
            key: key.into(),
            _lock: lock,
        }
    }

    /// The key's entries: path, pid, start time, what the sandbox has.
    fn entries(&self) -> Vec<(PathBuf, i32, u64, Provides)> {
        let prefix = format!("{}-", self.key);
        let mut out = Vec::new();
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if !e.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            let parse = |s: String| {
                let mut it = s.split_whitespace();
                let pid = it.next()?.parse::<i32>().ok()?;
                let start = it.next()?.parse::<u64>().ok()?;
                let snap = it.next()? == "snap=1";
                let app = it
                    .next()?
                    .strip_prefix("app=")
                    .filter(|a| !a.is_empty())
                    .map(PathBuf::from);
                Some((pid, start, Provides { snap, app }))
            };
            if let Some((pid, start, has)) = fs::read_to_string(e.path()).ok().and_then(parse) {
                out.push((e.path(), pid, start, has));
            }
        }
        out
    }

    /// A sandbox of this key that is up and has what the command `needs`:
    /// its process must be the one recorded (same start time; a stale entry
    /// is removed), and bwrap, its init and the app below it must exist.
    /// One still starting is waited for, 5 s at most. With `nested`, the
    /// app is expected in a user namespace of its own (the -n un-rooting)
    /// and is waited for until it is.
    pub fn find(&self, nested: bool, needs: &Provides) -> Option<Target> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut starting = false;
            for (path, pid, start, has) in self.entries() {
                if !has.covers(needs) {
                    continue;
                }
                if starttime(pid) != Some(start) {
                    let _ = fs::remove_file(path); // gone, or the pid reused
                    continue;
                }
                match target(pid, nested) {
                    Some(t) => return Some(t),
                    None => starting = true,
                }
            }
            if !starting || Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Record this process as a sandbox of the key, with what it has.
    pub fn register(&mut self, has: &Provides) {
        let me = std::process::id() as i32;
        let start = starttime(me).unwrap_or(0);
        let app = has.app.as_deref().unwrap_or(Path::new("")).display();
        let name = format!(
            "{}-{}",
            self.key,
            key(&[format!("snap={}", has.snap), format!("app={app}")])
        );
        let _ = fs::write(
            self.dir.join(name),
            format!("{me} {start} snap={} app={app}", u8::from(has.snap)),
        );
    }
}

/// The app of the sandbox whose process is `pid`, once it is there: bwrap
/// (that process itself, or its child), bwrap's init below it, the app
/// below that.
fn target(pid: i32, nested: bool) -> Option<Target> {
    let is_bwrap = |p: &i32| comm(*p).as_deref() == Some("bwrap");
    let bwrap = if is_bwrap(&pid) {
        pid
    } else {
        children(pid).into_iter().find(is_bwrap)?
    };
    let init = children(bwrap).into_iter().find(is_bwrap)?;
    let app = *children(init).first()?;
    (!nested || userns(app) != userns(init)).then_some(Target { app })
}

fn comm(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn children(pid: i32) -> Vec<i32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .map(|s| {
            s.split_whitespace()
                .filter_map(|c| c.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn userns(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/ns/user")).ok()
}

/// The process's start time (clock ticks since boot), field 22 of
/// /proc/PID/stat, counted after the parenthesized command name.
fn starttime(pid: i32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &s[s.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

/// Enter the sandbox and run the command there; returns its exit code as a
/// shell would report it. `caps` are the binary's file capabilities to keep
/// (`CAP_NET_RAW`), `chdir` the directory asked for, else the current one
/// if the sandbox has it, else /.
pub fn run(
    t: &Target,
    app: &Path,
    app_args: &[String],
    env: &[EnvEdit],
    chdir: Option<&Path>,
    new_session: bool,
    caps: &[String],
) -> i32 {
    let cwd = std::env::current_dir().ok();
    if let Err(e) = enter(t.app) {
        die(1, format!("cannot join the running sandbox: {e}"));
    }

    // SAFETY: the child only sets up its own process state, then execs
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        die(1, format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: plain syscalls on our own process
        unsafe {
            if new_session {
                libc::setsid();
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        }
        if !new_session {
            if let Err(e) = crate::tty::foreground() {
                eprintln!("sandpad: taking the terminal foreground group: {e}");
                // SAFETY: ending only the forked command child.
                unsafe { libc::_exit(125) }
            }
        }
        let dir = match chdir {
            Some(d) => d.to_path_buf(),
            None => cwd
                .filter(|d| d.is_dir())
                .unwrap_or_else(|| PathBuf::from("/")),
        };
        drop_caps(caps);
        let mut cmd = Command::new(app);
        cmd.args(app_args).current_dir(&dir).env("PWD", &dir);
        for e in env {
            match e {
                EnvEdit::Set(k, v) => cmd.env(k, v),
                EnvEdit::Unset(k) => cmd.env_remove(k),
            };
        }
        let err = cmd.exec();
        eprintln!("sandpad: {}: {err}", app.display());
        // SAFETY: ending the forked child without running the parent's exit code
        unsafe { libc::_exit(127) }
    }
    crate::net::forward_signals_to_pid(pid);
    let mut status = 0;
    loop {
        // SAFETY: waiting for our own child
        if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0 {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 1;
        }
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    }
}

/// Enter the namespaces of `pid`, all at once (see above). The caller
/// must be single-threaded; its later children are in the pid namespace.
pub fn enter(pid: i32) -> Result<(), String> {
    // SAFETY: plain syscall
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    if pidfd < 0 {
        return Err(format!("pidfd_open: {}", io::Error::last_os_error()));
    }
    let mut flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWNS;
    // --unshare-all only tries cgroup; the host's own can't be "joined"
    let ns_of = |pid: &str| fs::read_link(format!("/proc/{pid}/ns/cgroup")).ok();
    if ns_of(&pid.to_string()) != ns_of("self") {
        flags |= libc::CLONE_NEWCGROUP;
    }
    // SAFETY: a valid pidfd and flags
    let r = unsafe { libc::setns(pidfd, flags) };
    let err = io::Error::last_os_error();
    // SAFETY: closing the fd we opened
    unsafe { libc::close(pidfd) };
    if r < 0 {
        return Err(format!("setns: {err}"));
    }
    Ok(())
}

/// Names and numbers from linux/capability.h.
const CAPS: &[(&str, u32)] = &[
    ("CAP_CHOWN", 0),
    ("CAP_DAC_OVERRIDE", 1),
    ("CAP_DAC_READ_SEARCH", 2),
    ("CAP_FOWNER", 3),
    ("CAP_FSETID", 4),
    ("CAP_KILL", 5),
    ("CAP_SETGID", 6),
    ("CAP_SETUID", 7),
    ("CAP_SETPCAP", 8),
    ("CAP_LINUX_IMMUTABLE", 9),
    ("CAP_NET_BIND_SERVICE", 10),
    ("CAP_NET_BROADCAST", 11),
    ("CAP_NET_ADMIN", 12),
    ("CAP_NET_RAW", 13),
    ("CAP_IPC_LOCK", 14),
    ("CAP_IPC_OWNER", 15),
    ("CAP_SYS_MODULE", 16),
    ("CAP_SYS_RAWIO", 17),
    ("CAP_SYS_CHROOT", 18),
    ("CAP_SYS_PTRACE", 19),
    ("CAP_SYS_PACCT", 20),
    ("CAP_SYS_ADMIN", 21),
    ("CAP_SYS_BOOT", 22),
    ("CAP_SYS_NICE", 23),
    ("CAP_SYS_RESOURCE", 24),
    ("CAP_SYS_TIME", 25),
    ("CAP_SYS_TTY_CONFIG", 26),
    ("CAP_MKNOD", 27),
    ("CAP_LEASE", 28),
    ("CAP_AUDIT_WRITE", 29),
    ("CAP_AUDIT_CONTROL", 30),
    ("CAP_SETFCAP", 31),
    ("CAP_MAC_OVERRIDE", 32),
    ("CAP_MAC_ADMIN", 33),
    ("CAP_SYSLOG", 34),
    ("CAP_WAKE_ALARM", 35),
    ("CAP_BLOCK_SUSPEND", 36),
    ("CAP_AUDIT_READ", 37),
    ("CAP_PERFMON", 38),
    ("CAP_BPF", 39),
    ("CAP_CHECKPOINT_RESTORE", 40),
];

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
const CAP_VERSION_3: u32 = 0x20080522;
const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;

/// What bwrap does for the app: no capabilities but the ones asked for,
/// which survive exec as ambient ones; no_new_privs. Joining the user
/// namespace gave this process all of them.
pub fn drop_caps(keep: &[String]) {
    let mut wanted: Vec<u32> = Vec::new();
    for name in keep {
        match CAPS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            Some((_, c)) => wanted.push(*c),
            None => warn(format!("unknown capability {name} not kept")),
        }
    }
    let mut hdr = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: the structs match the kernel ABI for version 3
    unsafe {
        if libc::syscall(libc::SYS_capget, &mut hdr, data.as_mut_ptr()) == 0 {
            data[0].inheritable = 0;
            data[1].inheritable = 0;
            for c in &wanted {
                data[(c / 32) as usize].inheritable |= 1 << (c % 32);
            }
            libc::syscall(libc::SYS_capset, &hdr, data.as_ptr());
        }
        for c in &wanted {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_RAISE,
                libc::c_ulong::from(*c),
                0,
                0,
            );
        }
        for c in 0..64u32 {
            if !wanted.contains(&c)
                && libc::prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(c)) < 0
            {
                break; // past the last capability
            }
        }
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_order_sensitive() {
        let a = key(&["box=/x".into(), "net=true".into()]);
        assert_eq!(a, key(&["box=/x".into(), "net=true".into()]));
        assert_ne!(a, key(&["net=true".into(), "box=/x".into()]));
        assert_ne!(a, key(&["box=/x".into()]));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn a_sandbox_covers_a_command_needing_no_more() {
        let none = Provides::default();
        let snap = Provides {
            snap: true,
            app: None,
        };
        let app = Provides {
            snap: false,
            app: Some("/home/u/bin/x".into()),
        };
        assert!(none.covers(&none) && snap.covers(&none) && app.covers(&none));
        assert!(snap.covers(&snap) && !none.covers(&snap) && !app.covers(&snap));
        assert!(app.covers(&app) && !none.covers(&app) && !snap.covers(&app));
        let other = Provides {
            snap: false,
            app: Some("/home/u/bin/y".into()),
        };
        assert!(!other.covers(&app));
    }

    #[test]
    fn own_process_is_found_in_proc() {
        let me = std::process::id() as i32;
        assert!(starttime(me).is_some());
        assert_eq!(starttime(me), starttime(me));
        assert!(comm(me).is_some());
        assert!(userns(me).is_some());
    }
}
