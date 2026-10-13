//! Interactive commands need a foreground group whose leader lives in their
//! PID namespace. A host group is reported as 0 inside, which dash cannot
//! restore on exit. Set up the group just before exec, and keep the previous
//! foreground group in the waiting parent so it can hand the terminal back.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

pub const EXEC_OPTION: &str = "--internal-tty-exec";

pub struct Terminal {
    fd: File,
    foreground: libc::pid_t,
}

impl Terminal {
    /// Only a controlling terminal can be handed to another group in this
    /// session. In particular, piped stdin and a root request from another
    /// terminal session must not change our own terminal's foreground group.
    pub fn capture(fd: RawFd) -> Option<Self> {
        // SAFETY: terminal queries and duplication of a borrowed descriptor.
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
            // SAFETY: an owned terminal descriptor and the saved host group.
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

/// Call in the command child, after entering its PID namespace. Blocking
/// SIGTTOU allows the handoff from a background group; restore the old mask
/// before exec so the application retains normal job-control signals.
pub fn foreground() -> io::Result<()> {
    // SAFETY: queries on stdin and this process's session.
    if unsafe { libc::tcgetpgrp(0) } < 0
        || unsafe { libc::tcgetsid(0) } != unsafe { libc::getsid(0) }
    {
        return Ok(());
    }
    without_sigttou(|| {
        // SAFETY: only this process's group and controlling terminal change.
        if unsafe { libc::setpgid(0, 0) } < 0 || unsafe { libc::tcsetpgrp(0, libc::getpgrp()) } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

fn without_sigttou(f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    // SAFETY: valid sigsets; the calling thread's mask is restored, without
    // changing signal dispositions in the root server's other threads.
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

/// bwrap has no pre-exec hook inside the new PID namespace. Bind this same
/// executable into its private runtime directory and exec through this mode;
/// no extra installed binary or shell-specific workaround is needed.
pub fn exec(args: &[String]) -> ! {
    if let Err(e) = foreground() {
        crate::util::die(1, format!("taking the terminal foreground group: {e}"));
    }
    let Some((app, args)) = args.split_first() else {
        crate::util::die(2, "missing command for terminal setup");
    };
    let e = Command::new(app).args(args).exec();
    crate::util::die(127, format!("{app}: {e}"));
}
