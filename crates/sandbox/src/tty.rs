//! `-i`'s terminal handoff. An interactive command shares the user's
//! terminal, whose foreground process group is the `sandbox` process's — a
//! group outside the sandbox's pid namespace, which reads as 0 from inside.
//! A shell saves that at startup and fails to restore it at exit ("Cannot
//! set tty process group"), and its exit status is lost. So the command
//! gets a group of its own, made inside its pid namespace, and that group
//! becomes the terminal's foreground one just before the exec:
//! `foreground`, from a pre-exec hook where the sandbox forks the command
//! itself (a joined command, an approved root command), and `ttyexec`, a
//! C program embedded here and put in front of the app where bwrap does
//! the exec and offers no hook. The host side (`Terminal`) saves the
//! previous foreground group and hands the terminal back when the command
//! exits.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

/// Compiled by build.rs from ttyexec.c.
static TTYEXEC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ttyexec"));

/// The launcher's name under the sandbox's runtime dir.
pub const TTYEXEC_NAME: &str = "sandbox-tty-exec";

/// The launcher, handed to bwrap from memory and bound inside under the
/// runtime dir; its path inside, to go in front of the app.
pub fn ttyexec_args(runtime: &Path) -> (OwnedFd, Vec<OsString>, PathBuf) {
    let inside = runtime.join(TTYEXEC_NAME);
    let (fd, args) = crate::util::memfd_bind_args(TTYEXEC_NAME, TTYEXEC, "0755", &inside);
    (fd, args, inside)
}

/// Make this process a process group of its own and the foreground group
/// of its controlling terminal, if stdin is one of this session's.
/// Otherwise (piped stdin, another session's terminal) do nothing.
pub fn foreground() -> io::Result<()> {
    // SAFETY: queries on stdin and this process's session
    if unsafe { libc::tcgetpgrp(0) } < 0
        || unsafe { libc::tcgetsid(0) } != unsafe { libc::getsid(0) }
    {
        return Ok(());
    }
    without_sigttou(|| {
        // SAFETY: only this process's group and its terminal's foreground change
        if unsafe { libc::setpgid(0, 0) } < 0 || unsafe { libc::tcsetpgrp(0, libc::getpgrp()) } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Run `f` with SIGTTOU blocked on this thread: a background group's
/// `tcsetpgrp` is stopped by that signal otherwise. The mask is restored
/// afterwards, so the command keeps normal job-control signals.
fn without_sigttou(f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    // SAFETY: valid sigsets; only this thread's mask changes, and is restored
    unsafe {
        let mut blocked = std::mem::zeroed();
        let mut saved = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut saved);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let result = f();
        let error = libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut());
        result.and(if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error))
        })
    }
}

/// The terminal's foreground group before a command took it, given back
/// when this is dropped.
pub struct Terminal {
    fd: File,
    foreground: libc::pid_t,
}

impl Terminal {
    /// The terminal on `fd`, if it is this session's controlling one; a
    /// piped stdin, or a root request from another terminal session, must
    /// not change our own terminal's foreground group.
    pub fn capture(fd: RawFd) -> Option<Self> {
        // SAFETY: terminal queries and duplication of a borrowed descriptor
        unsafe {
            let foreground = libc::tcgetpgrp(fd);
            if foreground <= 0 || libc::tcgetsid(fd) != libc::getsid(0) {
                return None;
            }
            let copy = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3);
            (copy >= 0).then(|| Self {
                fd: File::from_raw_fd(copy),
                foreground,
            })
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let result = without_sigttou(|| {
            // SAFETY: an owned terminal descriptor and the saved group
            if unsafe { libc::tcsetpgrp(self.fd.as_raw_fd(), self.foreground) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
        if let Err(e) = result {
            crate::util::warn(format!("restoring the terminal foreground group: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn ttyexec_is_an_executable_from_memory_under_the_runtime_dir() {
        let (fd, args, inside) = ttyexec_args(Path::new("/run/user/1000"));
        assert_eq!(inside, PathBuf::from("/run/user/1000/sandbox-tty-exec"));
        assert_eq!(args[0], "--perms");
        assert_eq!(args[1], "0755");
        assert_eq!(args[2], "--ro-bind-data");
        assert_eq!(args[4], inside.as_os_str());
        let mut got = Vec::new();
        File::from(fd).read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"\x7fELF"));
        assert_eq!(got, TTYEXEC);
    }
}
