//! `-p root`, the host side: the socket the shims inside reach the
//! `sandbox` process by, the bwrap arguments that put the socket and the
//! shim binary into the sandbox, and the runner that executes an approved
//! command as root in the sandbox. The protocol, the server and the
//! dialog are the `sandbox_askroot` crate's.
//!
//! "Root" is uid 0 of the sandbox's first user namespace — the one bwrap
//! makes with `--uid 0`, where the real uid is root and the app, un-rooted
//! in a nested namespace of its own, is not (unroot.rs). The runner gets
//! there the way a joined command does (join.rs): a fork, `setns` on the
//! sandbox's pidfd, and a second fork so the command is in the sandbox's
//! pid namespace too. It then runs capability-less like everything in the
//! sandbox, with the shim's descriptors, sudo's environment rules and
//! `rootshim.so` preloaded as `--root` runs have it, over the box's
//! `--root` view, which `-p root` mounts for the whole sandbox since the
//! plain view can't be written even by uid 0.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sandbox_askroot::{sibling_binary, Request, Runner, Server, Stdio, SHIM, SOCKET_NAME};

use crate::boxdir::SHIM_INSIDE;
use crate::util::{die, gid, uid};

/// What sudo would put in PATH: its `secure_path`.
const SECURE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What sudo keeps of the caller's environment when not asked to keep it all.
const KEPT_ENV: &[&str] = &[
    "TERM",
    "COLORTERM",
    "LANG",
    "LANGUAGE",
    "HOME",
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The socket on the host: one per sandbox process, under the runtime dir.
pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime
        .join("sandbox")
        .join(format!("askroot-{}.sock", std::process::id()))
}

/// The bwrap arguments: the socket bound (writable: connecting needs that)
/// at its name under the sandbox's runtime dir, and the shim over sudo and
/// pkexec — where each name resolves on the host, since /usr/bin/sudo may
/// be an alternatives symlink and bwrap can't mount over a symlink; the
/// chain inside then ends at the shim, and argv[0] still names it sudo.
pub fn bwrap_args(socket: &Path, runtime_inside: &Path) -> Vec<OsString> {
    let shim = sibling_binary(SHIM);
    if !shim.is_file() {
        die(
            1,
            format!("-p root: {SHIM} not found next to sandbox or on PATH"),
        );
    }
    let mut args: Vec<OsString> = vec![
        "--bind".into(),
        socket.into(),
        runtime_inside.join(SOCKET_NAME).into(),
    ];
    for name in ["/usr/bin/sudo", "/usr/bin/pkexec"] {
        let dest = crate::util::realpath_e(Path::new(name)).unwrap_or_else(|_| PathBuf::from(name));
        args.extend([
            "--ro-bind".into(),
            shim.clone().into_os_string(),
            dest.into_os_string(),
        ]);
    }
    args
}

/// The server, bound before bwrap starts (the socket must exist to be
/// bound in), serving once the sandbox's pid is known.
pub struct Host {
    server: Server,
    box_name: String,
    app: String,
}

impl Host {
    pub fn bind(socket: &Path, box_name: String, app: String) -> Host {
        let server = Server::bind(socket).unwrap_or_else(|e| {
            die(
                1,
                format!("-p root: cannot listen on {}: {e}", socket.display()),
            )
        });
        Host {
            server,
            box_name,
            app,
        }
    }

    /// Serve, running approved commands in the sandbox whose init is
    /// `sandbox_pid` (bwrap's child, in the first user namespace).
    pub fn start(self, sandbox_pid: i32) {
        let run: Box<Runner> = Box::new(move |req, stdio| run_as_root(sandbox_pid, req, stdio));
        self.server.serve(self.box_name, self.app, run);
    }
}

/// Run the command as root in the sandbox; its exit status.
fn run_as_root(sandbox_pid: i32, req: &Request, stdio: Stdio) -> Result<i32, String> {
    // SAFETY: the child only sets up its own process state, forks and execs
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: plain syscalls on our own process
        unsafe {
            // what goes wrong from here is the shim's business to show
            libc::dup2(stdio.stderr.as_raw_fd(), 2);
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        }
        if let Err(e) = crate::join::enter(sandbox_pid) {
            eprintln!("sandbox: cannot enter the sandbox: {e}");
            // SAFETY: ending the forked child without the parent's exit code
            unsafe { libc::_exit(125) }
        }
        // in the sandbox's namespaces now, pid namespace included for
        // children: the command is one
        let mut cmd = command(req);
        cmd.stdin(stdio.stdin)
            .stdout(stdio.stdout)
            .stderr(stdio.stderr)
            .process_group(0);
        let status = match cmd.status() {
            Ok(s) => crate::net::exit_code(s),
            Err(e) => {
                eprintln!("sandbox: {}: {e}", req.argv[0]);
                127
            }
        };
        // SAFETY: as above
        unsafe { libc::_exit(status) }
    }
    drop(stdio); // the child has them
    let mut status = 0;
    // SAFETY: waiting for our own child
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(format!("waitpid: {e}"));
        }
    }
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    })
}

/// The command as sudo would run it: a filtered environment unless `-E`,
/// `secure_path`, the `SUDO_*` variables, root's identity; plus the
/// rootshim. Capabilities are dropped the way bwrap does before the exec.
fn command(req: &Request) -> Command {
    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..]);
    cmd.env_clear();
    for (k, v) in &req.env {
        if req.preserve_env || KEPT_ENV.contains(&k.as_str()) || k.starts_with("LC_") {
            cmd.env(k, v);
        }
    }
    cmd.env("PATH", SECURE_PATH)
        .env("USER", "root")
        .env("LOGNAME", "root")
        .env("SUDO_USER", req.user())
        .env("SUDO_UID", uid().to_string())
        .env("SUDO_GID", gid().to_string())
        .env("SUDO_COMMAND", req.command())
        .env("LD_PRELOAD", SHIM_INSIDE);
    if fs::metadata(&req.cwd).map(|m| m.is_dir()).unwrap_or(false) {
        cmd.current_dir(&req.cwd);
    }
    // SAFETY: only prctl/capset syscalls between fork and exec
    unsafe {
        cmd.pre_exec(|| {
            crate::join::drop_caps(&[]);
            Ok(())
        });
    }
    cmd
}
