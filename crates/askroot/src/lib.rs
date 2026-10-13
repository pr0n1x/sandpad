//! `sandbox -p root`: a program inside a sandbox runs `sudo` or `pkexec`,
//! the user is asked on the host, and the command runs as root inside the
//! sandbox if they approve. Three parts:
//!
//! - the *shim* (`sandbox-askroot-shim`), bound over `/usr/bin/sudo`,
//!   `/usr/bin/pkexec` and `/usr/bin/su` in the sandbox: it parses the command line it was
//!   called with and sends a [`Request`] — the command, the working
//!   directory, the environment — together with its own stdin, stdout and
//!   stderr (SCM_RIGHTS) over a unix socket the host bind-mounted into the
//!   sandbox ([`request`]);
//! - the *server* ([`Server`]), a thread of the `sandbox` process on the
//!   host: for each request it runs the dialog and, if approved, a runner
//!   the caller provides, which executes the command on the shim's
//!   descriptors and returns its exit status; the shim gets a [`Reply`]
//!   and exits with that status;
//! - the *dialog* (`sandbox-askroot-dialog`), a window with Approve and
//!   Deny, a process of its own because a window's event loop wants a
//!   main thread of its own: it reads a [`Prompt`] on stdin and prints
//!   the answer.
//!
//! Wire formats. Shim to server: a 4-byte little-endian length carrying
//! the three descriptors, then that many bytes of `name\0value\0` fields.
//! Server to shim: one byte — 0 exited, 1 denied, 2 error — a 4-byte
//! little-endian status, then a message. Server to dialog: `key: value`
//! lines and an empty line; dialog to server: `approve` or `deny`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// The socket's name under `$XDG_RUNTIME_DIR` inside the sandbox.
pub const SOCKET_NAME: &str = "sandbox-askroot.sock";
/// The shim binary's name.
pub const SHIM: &str = "sandbox-askroot-shim";
/// The dialog binary's name.
pub const DIALOG: &str = "sandbox-askroot-dialog";
/// Set on the host, this runs instead of the dialog binary (tests).
pub const DIALOG_ENV: &str = "SANDBOX_ASKROOT_DIALOG";

// --- the command line of sudo and pkexec -----------------------------------

/// What a sudo/pkexec invocation asks for, once its options are read.
#[derive(Debug, Default, PartialEq)]
pub struct Invocation {
    /// the command and its arguments; empty for `sudo -v`
    pub argv: Vec<String>,
    /// `-E`: keep the caller's environment
    pub preserve_env: bool,
    /// `-v`: ask, run nothing
    pub validate: bool,
    /// `-l`: ask, then say what is allowed
    pub list: bool,
    /// `-u USER` / `--user USER`, if given
    pub user: Option<String>,
}

/// sudo's options: the ones that take a value are skipped with it, the
/// rest alone; `--` or the first non-option starts the command.
pub fn parse_sudo(args: &[String]) -> Result<Invocation, String> {
    let mut inv = Invocation::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match name {
                "preserve-env" => inv.preserve_env = true,
                "validate" => inv.validate = true,
                "list" => inv.list = true,
                "user" | "group" | "prompt" | "chdir" | "close-from" | "host" | "role" | "type"
                | "other-user" | "command-timeout" => {
                    if value.is_none() {
                        i += 1;
                    }
                    if name == "user" {
                        inv.user = value.or_else(|| args.get(i - 1).cloned());
                    }
                }
                "help" | "version" => {
                    return Err(format!("sudo: --{name} is not supported in the sandbox"))
                }
                _ => {} // -A, -n, -S, -H, -k, -K, -b, -i, -s, --askpass, --non-interactive...
            }
            continue;
        }
        if let Some(shorts) = a.strip_prefix('-').filter(|s| !s.is_empty()) {
            let chars: Vec<char> = shorts.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                j += 1;
                match c {
                    'E' => inv.preserve_env = true,
                    'v' => inv.validate = true,
                    'l' => inv.list = true,
                    'u' | 'g' | 'p' | 'C' | 'D' | 'h' | 'r' | 't' | 'T' | 'U' => {
                        let rest: String = chars[j..].iter().collect();
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i - 1).cloned()
                        } else {
                            Some(rest)
                        };
                        if c == 'u' {
                            inv.user = value;
                        }
                        j = chars.len();
                    }
                    'V' => return Err("sudo: -V is not supported in the sandbox".into()),
                    _ => {} // A n S H k K b i s e
                }
            }
            continue;
        }
        i -= 1;
        break;
    }
    inv.argv = args[i..].to_vec();
    Ok(inv)
}

/// pkexec's: `--user USER`, flags without values, then the command.
pub fn parse_pkexec(args: &[String]) -> Result<Invocation, String> {
    let mut inv = Invocation::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        match a.as_str() {
            "--user" => {
                inv.user = args.get(i).cloned();
                i += 1;
            }
            "--disable-internal-agent" | "--keep-cwd" => {}
            "--help" | "--version" => {
                return Err(format!("pkexec: {a} is not supported in the sandbox"))
            }
            s if s.starts_with("--user=") => inv.user = Some(s["--user=".len()..].to_string()),
            _ => {
                i -= 1;
                break;
            }
        }
    }
    inv.argv = args[i..].to_vec();
    Ok(inv)
}

/// su's: `su [options] [-] [user [shell args...]]`. The command is the
/// shell, `-c COMMAND` or the arguments after the user handed to it, or
/// nothing: the shell reads stdin, as it does when su is fed a heredoc.
/// The password is not an argument: su reads it from the terminal, not
/// stdin, so stdin is left to the shell. `-l`/`-` sets a login shell,
/// `-m`/`-p` keeps the environment, `-s SHELL` the shell; the user is
/// root unless named.
pub fn parse_su(args: &[String]) -> Result<Invocation, String> {
    let mut inv = Invocation::default();
    let mut command: Option<String> = None;
    let mut shell = "/bin/sh".to_string();
    let mut login = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "-" {
            login = true;
            break;
        }
        if a == "--" {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut take = || {
                value.clone().or_else(|| {
                    i += 1;
                    args.get(i - 1).cloned()
                })
            };
            match name {
                "login" => login = true,
                "preserve-environment" => inv.preserve_env = true,
                "command" | "session-command" => command = take(),
                "shell" => shell = take().unwrap_or(shell),
                "group" | "supp-group" | "whitelist-environment" => {
                    take();
                }
                "fast" | "pty" | "no-pty" => {}
                "help" | "version" => {
                    return Err(format!("su: --{name} is not supported in the sandbox"))
                }
                _ => return Err(format!("su: unknown option --{name}")),
            }
            continue;
        }
        if let Some(shorts) = a.strip_prefix('-').filter(|s| !s.is_empty()) {
            let chars: Vec<char> = shorts.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                j += 1;
                match c {
                    'l' => login = true,
                    'm' | 'p' => inv.preserve_env = true,
                    'c' | 's' | 'g' | 'G' | 'w' => {
                        let rest: String = chars[j..].iter().collect();
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i - 1).cloned()
                        } else {
                            Some(rest)
                        };
                        match c {
                            'c' => command = value,
                            's' => shell = value.unwrap_or(shell),
                            _ => {}
                        }
                        j = chars.len();
                    }
                    'f' | 'P' => {}
                    'h' | 'V' => return Err(format!("su: -{c} is not supported in the sandbox")),
                    _ => return Err(format!("su: unknown option -{c}")),
                }
            }
            continue;
        }
        i -= 1;
        break;
    }
    if let Some(user) = args.get(i) {
        inv.user = Some(user.clone());
        i += 1;
    }
    inv.argv.push(shell);
    if login {
        inv.argv.push("-l".into());
    }
    if let Some(c) = command {
        inv.argv.push("-c".into());
        inv.argv.push(c);
    }
    inv.argv.extend(args[i..].iter().cloned());
    Ok(inv)
}

/// Shell-style quoting, for display.
pub fn quote(a: &str) -> String {
    if !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

// --- the request: shim to server ---------------------------------------------

/// What the shim asks the server to run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// the command and its arguments; empty for `sudo -v` / `sudo -l`
    pub argv: Vec<String>,
    pub cwd: String,
    /// the shim's environment
    pub env: Vec<(String, String)>,
    /// `-E`: hand the command the whole environment
    pub preserve_env: bool,
}

impl Request {
    /// The caller's name, from its environment.
    pub fn user(&self) -> &str {
        self.env
            .iter()
            .find(|(k, _)| k == "USER")
            .map(|(_, v)| v.as_str())
            .unwrap_or("?")
    }

    /// The command line for display.
    pub fn command(&self) -> String {
        if self.argv.is_empty() {
            "(no command: validating root access)".to_string()
        } else {
            self.argv
                .iter()
                .map(|a| quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut fields: Vec<(&str, String)> = vec![
            ("cwd", self.cwd.clone()),
            ("preserve_env", u8::from(self.preserve_env).to_string()),
        ];
        fields.extend(self.argv.iter().map(|a| ("arg", a.clone())));
        fields.extend(self.env.iter().map(|(k, v)| ("env", format!("{k}={v}"))));
        let mut out = Vec::new();
        for (k, v) in fields {
            out.extend_from_slice(k.as_bytes());
            out.push(0);
            out.extend_from_slice(v.as_bytes());
            out.push(0);
        }
        out
    }

    fn decode(payload: &[u8]) -> Request {
        let mut req = Request::default();
        let mut parts = payload.split(|&b| b == 0);
        while let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            let v = String::from_utf8_lossy(v).into_owned();
            match k {
                b"cwd" => req.cwd = v,
                b"preserve_env" => req.preserve_env = v == "1",
                b"arg" => req.argv.push(v),
                b"env" => {
                    if let Some((k, v)) = v.split_once('=') {
                        req.env.push((k.to_string(), v.to_string()));
                    }
                }
                _ => {}
            }
        }
        req
    }
}

// --- the reply: server to shim -----------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// the command ran; its exit status
    Exited(i32),
    /// the user said no
    Denied,
    /// something failed before the command could run
    Error(String),
}

impl Reply {
    fn encode(&self) -> Vec<u8> {
        let (kind, status, msg) = match self {
            Reply::Exited(s) => (0u8, *s, ""),
            Reply::Denied => (1, 1, ""),
            Reply::Error(m) => (2, 1, m.as_str()),
        };
        let mut out = vec![kind];
        out.extend_from_slice(&status.to_le_bytes());
        out.extend_from_slice(msg.as_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Reply, String> {
        if bytes.len() < 5 {
            return Err("the server hung up without an answer".into());
        }
        let status = i32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let msg = String::from_utf8_lossy(&bytes[5..]).into_owned();
        match bytes[0] {
            0 => Ok(Reply::Exited(status)),
            1 => Ok(Reply::Denied),
            2 => Ok(Reply::Error(msg)),
            k => Err(format!("unknown reply kind {k}")),
        }
    }
}

// --- the client ----------------------------------------------------------------

/// Send the request, with this process's stdin, stdout and stderr for the
/// command to use, and wait for the reply.
pub fn request(socket: &Path, req: &Request) -> io::Result<Reply> {
    let (_dir, short) = short_path(socket)?;
    let mut sock = UnixStream::connect(short)?;
    let payload = req.encode();
    send_with_fds(&sock, &(payload.len() as u32).to_le_bytes(), &[0, 1, 2])?;
    sock.write_all(&payload)?;
    let mut reply = Vec::new();
    sock.read_to_end(&mut reply)?;
    Reply::decode(&reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// --- the server ----------------------------------------------------------------

/// The shim's descriptors, received with its request: what the command
/// reads and writes.
pub struct Stdio {
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

/// What the dialog shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    pub box_name: String,
    pub app: String,
    pub user: String,
    pub cwd: String,
    pub command: String,
}

impl Prompt {
    /// `key: value` lines and an empty line; newlines in values become spaces.
    pub fn to_text(&self) -> String {
        let one_line = |v: &str| v.replace(['\n', '\r'], " ");
        format!(
            "box: {}\napp: {}\nuser: {}\ncwd: {}\ncommand: {}\n\n",
            one_line(&self.box_name),
            one_line(&self.app),
            one_line(&self.user),
            one_line(&self.cwd),
            one_line(&self.command)
        )
    }

    /// The lines up to the first empty one; unknown keys are ignored.
    pub fn parse(text: &str) -> Prompt {
        let mut p = Prompt::default();
        for line in text.lines() {
            if line.is_empty() {
                break;
            }
            let Some((k, v)) = line.split_once(": ") else {
                continue;
            };
            let v = v.to_string();
            match k {
                "box" => p.box_name = v,
                "app" => p.app = v,
                "user" => p.user = v,
                "cwd" => p.cwd = v,
                "command" => p.command = v,
                _ => {}
            }
        }
        p
    }
}

/// Runs an approved command on the shim's descriptors; its exit status.
pub type Runner = dyn Fn(&Request, Stdio) -> Result<i32, String> + Send + Sync;

/// The host side of the socket.
pub struct Server {
    listener: UnixListener,
}

impl Server {
    /// Listen at `path`, replacing a stale socket there.
    pub fn bind(path: &Path) -> io::Result<Server> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let (_dir, short) = short_path(path)?;
        Ok(Server {
            listener: UnixListener::bind(short)?,
        })
    }

    /// Serve until the process ends, in a thread of its own: each request
    /// is shown by the dialog with `box_name` and `app`, and run by `run`
    /// if approved. Failures are reported to the shim and on stderr.
    pub fn serve(self, box_name: String, app: String, run: Box<Runner>) {
        let ctx = Arc::new(Context { box_name, app, run });
        std::thread::spawn(move || {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let ctx = ctx.clone();
                        std::thread::spawn(move || handle(&ctx, s));
                    }
                    Err(e) => eprintln!("sandbox: askroot: accept: {e}"),
                }
            }
        });
    }
}

struct Context {
    box_name: String,
    app: String,
    run: Box<Runner>,
}

/// One request: receive, ask, run, reply.
fn handle(ctx: &Context, stream: UnixStream) {
    let reply = match receive(&stream) {
        Ok((req, stdio)) => decide_and_run(ctx, &req, stdio),
        Err(e) => Reply::Error(format!("bad request: {e}")),
    };
    if let Reply::Error(e) = &reply {
        eprintln!("sandbox: askroot: {e}");
    }
    // best effort: the shim may be gone already
    let _ = (&stream).write_all(&reply.encode());
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn receive(stream: &UnixStream) -> io::Result<(Request, Stdio)> {
    let mut header = [0u8; 4];
    let (n, fds) = recv_with_fds(stream, &mut header, 3)?;
    // ours now, whatever happens next
    let mut files = fds
        .into_iter()
        // SAFETY: freshly received descriptors, owned by nobody else
        .map(|fd| unsafe { File::from_raw_fd(fd) });
    let (Some(stdin), Some(stdout), Some(stderr), None) =
        (files.next(), files.next(), files.next(), files.next())
    else {
        return Err(io::Error::other("expected three descriptors"));
    };
    if n != 4 {
        return Err(io::Error::other("expected a length"));
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > 1 << 20 {
        return Err(io::Error::other("too large"));
    }
    let mut payload = vec![0u8; len];
    (&*stream).read_exact(&mut payload)?;
    Ok((
        Request::decode(&payload),
        Stdio {
            stdin,
            stdout,
            stderr,
        },
    ))
}

fn decide_and_run(ctx: &Context, req: &Request, stdio: Stdio) -> Reply {
    let prompt = Prompt {
        box_name: ctx.box_name.clone(),
        app: ctx.app.clone(),
        user: req.user().to_string(),
        cwd: req.cwd.clone(),
        command: req.command(),
    };
    match ask(&prompt) {
        Err(e) => Reply::Error(e),
        Ok(false) => Reply::Denied,
        Ok(true) if req.argv.is_empty() => Reply::Exited(0),
        Ok(true) => match (ctx.run)(req, stdio) {
            Ok(status) => Reply::Exited(status),
            Err(e) => Reply::Error(e),
        },
    }
}

/// Show the dialog; true when approved.
pub fn ask(prompt: &Prompt) -> Result<bool, String> {
    let dialog = std::env::var_os(DIALOG_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| sibling_binary(DIALOG));
    let mut child = Command::new(&dialog)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", dialog.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.to_text().as_bytes())
            .map_err(|e| format!("{}: {e}", dialog.display()))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{}: {e}", dialog.display()))?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "approve" => Ok(true),
        "deny" => Ok(false),
        other => Err(format!(
            "{}: {} (answered {other:?})",
            dialog.display(),
            out.status
        )),
    }
}

/// `name` next to this binary (a build tree, a bin dir), else as PATH finds it.
pub fn sibling_binary(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// `path` as `/proc/self/fd/N/name`, with its directory held open as N:
/// a unix socket address is limited to 107 bytes, and a runtime dir
/// under a deep home (a test's scratch dir) can be longer than that.
fn short_path(path: &Path) -> io::Result<(File, PathBuf)> {
    let dir = File::open(path.parent().unwrap_or(Path::new(".")))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{}: no file name", path.display())))?;
    let short = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
    Ok((dir, short))
}

// --- descriptor passing ----------------------------------------------------------

/// Send `data` with `fds` attached (SCM_RIGHTS); `data` must be short
/// enough to go in one message.
fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE is a pure size computation
    let space = unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: msghdr is plain data; zeroed is a valid starting point
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is large enough for one SCM_RIGHTS header
    // with these descriptors, as CMSG_SPACE said
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(sock.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive into `buf`, collecting up to `max_fds` descriptors sent with it.
fn recv_with_fds(
    sock: &UnixStream,
    buf: &mut [u8],
    max_fds: usize,
) -> io::Result<(usize, Vec<RawFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: size computation
    let space = unsafe { libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as u32) };
    let mut cbuf = vec![0u8; space as usize];
    // SAFETY: as above
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: a valid msghdr over our buffers; the kernel fills them
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote, within msg_controllen
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = bytes / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    fds.push(*data.add(i));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, fds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sudo_plain_command() {
        let inv = parse_sudo(&s(&["apt-get", "install", "-y", "foo"])).unwrap();
        assert_eq!(inv.argv, s(&["apt-get", "install", "-y", "foo"]));
        assert!(!inv.preserve_env && !inv.validate && inv.user.is_none());
    }

    #[test]
    fn sudo_options_before_the_command() {
        let inv = parse_sudo(&s(&["-A", "-n", "-E", "-u", "root", "--", "ls", "-l"])).unwrap();
        assert_eq!(inv.argv, s(&["ls", "-l"]));
        assert!(inv.preserve_env);
        assert_eq!(inv.user.as_deref(), Some("root"));
    }

    #[test]
    fn sudo_attached_and_long_values() {
        let inv = parse_sudo(&s(&["-uroot", "--prompt=x", "-p", "y", "id"])).unwrap();
        assert_eq!(inv.argv, s(&["id"]));
        assert_eq!(inv.user.as_deref(), Some("root"));
        let inv = parse_sudo(&s(&["--user", "bob", "id"])).unwrap();
        assert_eq!(inv.user.as_deref(), Some("bob"));
    }

    #[test]
    fn sudo_validate_and_list_take_no_command() {
        assert!(parse_sudo(&s(&["-v"])).unwrap().validate);
        assert!(parse_sudo(&s(&["-nv"])).unwrap().validate);
        assert!(parse_sudo(&s(&["-l"])).unwrap().list);
    }

    #[test]
    fn su_command_login_and_stdin_shell() {
        let inv = parse_su(&s(&["-c", "id -u"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/sh", "-c", "id -u"]));
        assert!(inv.user.is_none());
        let inv = parse_su(&s(&["-l", "root"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/sh", "-l"]));
        assert_eq!(inv.user.as_deref(), Some("root"));
        let inv = parse_su(&s(&["-", "root", "-c", "x"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/sh", "-l", "-c", "x"]));
        let inv = parse_su(&s(&["-ps", "/bin/bash", "--command=ls", "bob"])).unwrap();
        assert_eq!(inv.argv, s(&["/bin/bash", "-c", "ls"]));
        assert!(inv.preserve_env);
        assert_eq!(inv.user.as_deref(), Some("bob"));
        assert!(parse_su(&s(&["-x"])).is_err());
    }

    #[test]
    fn pkexec_user_and_command() {
        let inv = parse_pkexec(&s(&[
            "--user",
            "root",
            "--disable-internal-agent",
            "sh",
            "-c",
            "x",
        ]))
        .unwrap();
        assert_eq!(inv.argv, s(&["sh", "-c", "x"]));
        assert_eq!(inv.user.as_deref(), Some("root"));
        let inv = parse_pkexec(&s(&["--user=root", "true"])).unwrap();
        assert_eq!(inv.argv, s(&["true"]));
    }

    #[test]
    fn request_round_trip() {
        let r = Request {
            argv: s(&["sh", "-c", "", "a b"]),
            cwd: "/a b".into(),
            env: vec![("K".into(), "v=w".into()), ("USER".into(), "me".into())],
            preserve_env: true,
        };
        assert_eq!(Request::decode(&r.encode()), r);
        assert_eq!(r.user(), "me");
        assert_eq!(r.command(), "sh -c '' 'a b'");
    }

    #[test]
    fn reply_round_trip() {
        for r in [Reply::Exited(7), Reply::Denied, Reply::Error("x y".into())] {
            assert_eq!(Reply::decode(&r.encode()).unwrap(), r);
        }
        assert!(Reply::decode(b"").is_err());
    }

    #[test]
    fn prompt_round_trip_stops_at_the_blank_line() {
        let p = Prompt {
            box_name: "rfgov".into(),
            app: "/opt/x".into(),
            user: "me".into(),
            cwd: "/".into(),
            command: "sh -c 'a\nb'".into(),
        };
        let text = p.to_text() + "cwd: ignored\n";
        let back = Prompt::parse(&text);
        assert_eq!(back.command, "sh -c 'a b'");
        assert_eq!(back.cwd, "/");
        assert_eq!(back.box_name, "rfgov");
    }

    #[test]
    fn quoting_for_display() {
        assert_eq!(quote("ls"), "ls");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn descriptors_travel_with_the_header() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, w) = {
            let mut fds = [0; 2];
            // SAFETY: a plain pipe
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            (fds[0], fds[1])
        };
        send_with_fds(&a, &7u32.to_le_bytes(), &[r, w]).unwrap();
        let mut hdr = [0u8; 4];
        let (n, fds) = recv_with_fds(&b, &mut hdr, 3).unwrap();
        assert_eq!((n, u32::from_le_bytes(hdr), fds.len()), (4, 7, 2));
        // the received write end feeds the received read end
        // SAFETY: descriptors we just received
        let (mut rr, mut ww) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        ww.write_all(b"hi").unwrap();
        drop(ww);
        // a fixed read: the original write end is still open, so EOF
        // would never come
        let mut got = [0u8; 2];
        rr.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        // SAFETY: closing our original ends
        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

    #[test]
    fn a_request_over_the_socket_gets_the_runner_s_status() {
        let dir = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
            .join("tmp/sandbox-tests")
            .join(format!("askroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dialog = dir.join("dialog");
        std::fs::write(&dialog, "#!/bin/sh\ncat > \"$0.prompt\"; echo approve\n").unwrap();
        // SAFETY: plain chmod of our own file
        unsafe {
            libc::chmod(
                std::ffi::CString::new(dialog.to_str().unwrap())
                    .unwrap()
                    .as_ptr(),
                0o755,
            )
        };
        std::env::set_var(DIALOG_ENV, &dialog);
        let sock = dir.join("sock");
        let server = Server::bind(&sock).unwrap();
        server.serve(
            "box".into(),
            "/bin/app".into(),
            Box::new(|req: &Request, mut stdio: Stdio| {
                writeln!(stdio.stdout, "ran {}", req.command()).unwrap();
                Ok(3)
            }),
        );
        let req = Request {
            argv: s(&["id", "-u"]),
            cwd: "/".into(),
            env: vec![("USER".into(), "me".into())],
            preserve_env: false,
        };
        assert_eq!(request(&sock, &req).unwrap(), Reply::Exited(3));
        let prompt = Prompt::parse(&std::fs::read_to_string(dir.join("dialog.prompt")).unwrap());
        assert_eq!(prompt.command, "id -u");
        assert_eq!(prompt.user, "me");
        assert_eq!(prompt.box_name, "box");
        // nothing to run: approved without the runner
        let none = Request {
            argv: vec![],
            ..req.clone()
        };
        assert_eq!(request(&sock, &none).unwrap(), Reply::Exited(0));
        std::fs::write(&dialog, "#!/bin/sh\necho deny\n").unwrap();
        assert_eq!(request(&sock, &req).unwrap(), Reply::Denied);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
