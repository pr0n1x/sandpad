//! An interactive command under `sandbox -i` shares the user's terminal,
//! whose foreground process group is the `sandbox` process's — a group
//! outside the sandbox's pid namespace, which reads as 0 from inside. A
//! shell saves that at startup and fails to restore it at exit ("Cannot
//! set tty process group"), and its exit status is lost. So the command
//! gets a group of its own, made inside its pid namespace, and that group
//! becomes the terminal's foreground one just before the exec: `foreground`,
//! called from a pre-exec hook where the sandbox forks the command itself
//! (a joined command, an approved root command) and by the
//! `sandbox-tty-exec` binary where bwrap does the exec and offers no hook.
//! The host side saves the previous foreground group and hands the
//! terminal back when the command exits.

use std::io;

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
pub fn without_sigttou(f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
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
