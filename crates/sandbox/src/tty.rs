//! The host side of `-i`'s terminal handoff (see the `sandbox-tty-exec`
//! crate): the terminal's foreground group before the command took it,
//! given back when the command is done.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use sandbox_tty_exec::without_sigttou;

/// Where the helper binary is bound inside, under the runtime dir.
pub const HELPER_NAME: &str = "sandbox-tty-exec";

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
