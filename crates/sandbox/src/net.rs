//! -n: bwrap creates the namespaces (its AppArmor profile allows userns);
//! pasta only *attaches* to the sandbox netns via setns(), which the Ubuntu
//! userns restriction doesn't gate — so pasta needs no profile of its own.
//! The app is held on bwrap's --block-fd until pasta has configured the
//! network.

use crate::util::{die, output, run as run_tool};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// A pipe, both ends close-on-exec as Rust makes everything it opens.
pub struct Pipe {
    pub read: OwnedFd,
    pub write: OwnedFd,
}

impl Pipe {
    fn new() -> Pipe {
        let mut fds = [0; 2];
        // SAFETY: valid array
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            die(1, format!("pipe: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: fresh fds we own
        unsafe {
            Pipe {
                read: OwnedFd::from_raw_fd(fds[0]),
                write: OwnedFd::from_raw_fd(fds[1]),
            }
        }
    }

    /// Let a child inherit `end` across exec (clears close-on-exec on it).
    fn inherit(end: &OwnedFd) {
        // SAFETY: a valid fd we own
        if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, 0) } < 0 {
            die(1, format!("fcntl: {}", std::io::Error::last_os_error()));
        }
    }

    /// A pipe the child reads from.
    fn to_child() -> Pipe {
        let p = Pipe::new();
        Pipe::inherit(&p.read);
        p
    }

    /// A pipe the child writes to.
    fn from_child() -> Pipe {
        let p = Pipe::new();
        Pipe::inherit(&p.write);
        p
    }
}

/// The pipes bwrap is handed, created before its argument list is built so
/// their numbers can go into it. Only the child-side ends are inheritable;
/// the parent drops them right after spawning bwrap, so later children
/// (pasta, nsenter) don't get them.
pub struct Pipes {
    /// `--json-status-fd`: bwrap reports the sandbox's pid here once the
    /// namespaces exist
    status: Pipe,
    /// `--block-fd`: bwrap reads this before starting the app — the read
    /// blocks until we write a byte, once pasta has set the network up
    block: Pipe,
    /// `--ro-bind-data`: the resolv.conf contents
    pub resolv: Pipe,
}

impl Pipes {
    pub fn new() -> Pipes {
        Pipes {
            status: Pipe::from_child(),
            block: Pipe::to_child(),
            resolv: Pipe::to_child(),
        }
    }
}

/// What `run` sets up around bwrap.
pub struct Helpers<'a> {
    /// -n: pasta and the sysctls
    pub net: Option<&'a Net>,
    /// a snap app: its userns ban goes in with the sysctls
    pub snap: bool,
    /// -p usb: the udev event relay
    pub usb: bool,
    /// -p root: the server for the sudo/pkexec shims inside
    pub askroot: Option<crate::askroot::Host>,
    /// Whether the command prefix creates a nested user namespace to map.
    pub unroot: bool,
}

pub struct Net {
    /// -4, or nothing with -6 (pasta's defaults are both)
    pasta_ip: Vec<String>,
    /// -nIFACE: mirror IFACE inside and pin pasta's sockets to it
    out_args: Vec<String>,
    /// DNS forwarding: pasta's resolver address inside, and how it forwards
    fwd_args: Vec<String>,
    dns_fwd: String,
}

pub fn configure(out_if: Option<&str>, ipv6: bool) -> Net {
    let mut net = Net {
        pasta_ip: if ipv6 { vec![] } else { vec!["-4".into()] },
        out_args: Vec::new(),
        fwd_args: vec!["--dns-forward".into(), "169.254.1.1".into()],
        dns_fwd: "169.254.1.1".into(),
    };
    let Some(iface) = out_if else { return net };
    if !Path::new("/sys/class/net").join(iface).exists() {
        die(1, format!("no such interface: {iface}"));
    }
    // SO_BINDTODEVICE pins pasta's host sockets to IFACE, so its traffic
    // bypasses e.g. a WireGuard fwmark default route and leaves through IFACE
    net.out_args = vec![
        "-i".into(),
        iface.into(),
        "--outbound-if4".into(),
        iface.into(),
    ];
    if ipv6 {
        net.out_args.extend(["--outbound-if6".into(), iface.into()]);
    }
    // pinned sockets can't reach a loopback resolver (systemd-resolved's
    // 127.0.0.53 stub, pasta's default --dns-host from /etc/resolv.conf), so
    // forward DNS to IFACE's own upstream servers instead — not the global
    // list, which may hold servers only reachable through the tunnel being
    // bypassed. --dns-host takes one server per IP version
    if let Some(upstream) = output("resolvectl", &["dns", iface]) {
        let servers = upstream.split_once(':').map(|(_, s)| s).unwrap_or("");
        let ns4 = servers.split_whitespace().find(|s| !s.contains(':'));
        let ns6 = servers.split_whitespace().find(|s| s.contains(':'));
        let pasta_has_dns_host = output("pasta", &["--help"])
            .or_else(|| {
                Command::new("pasta")
                    .arg("--help")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            })
            .map(|h| h.contains("--dns-host"))
            .unwrap_or(false);
        if pasta_has_dns_host {
            if let Some(s) = ns4 {
                net.out_args.extend(["--dns-host".into(), s.into()]);
            }
            if let (Some(s), true) = (ns6, ipv6) {
                net.out_args.extend(["--dns-host".into(), s.into()]);
            }
        } else if let Some(s) = ns4 {
            // old pasta (< 2024_10_30, e.g. Ubuntu 24.04) can't retarget the
            // forwarder: point the sandbox resolv.conf straight at the upstream
            // server; --no-map-gw so a gateway-hosted DNS reaches the real
            // gateway instead of being remapped to the host; --dns none: no
            // "Couldn't get any nameserver" noise
            net.dns_fwd = s.into();
            net.fwd_args = vec!["--no-map-gw".into(), "--dns".into(), "none".into()];
        }
    }
    net
}

/// The pid of the running bwrap, for the signal handler.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: plain syscall
        unsafe { libc::kill(pid, sig) };
    }
}

/// Make SIGINT/SIGTERM stop the sandbox (bwrap is in its own session with
/// --new-session, so a terminal's Ctrl-C only reaches us), and let the
/// normal exit path clean up.
pub fn forward_signals_to(child: &Child) {
    forward_signals_to_pid(child.id() as i32);
}

/// The same for a child known by pid (a forked one).
pub fn forward_signals_to_pid(pid: i32) {
    CHILD.store(pid, Ordering::SeqCst);
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: installing an async-signal-safe handler
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = forward_signal as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Exit status of a finished child as a shell would report it.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Run bwrap with helpers that need its namespaces: spawn it held on the
/// block fd, attach pasta (-n), start the udev event relay (-p usb) and
/// the root server (-p root), set sysctls, release it, write the app's
/// nested uid map, wait.
pub fn run(
    bwrap_args: &[OsString],
    app_wrap: &[String],
    app: &Path,
    app_args: &[String],
    helpers: Helpers,
    pipes: Pipes,
) -> i32 {
    let Helpers {
        net,
        snap,
        usb,
        askroot,
        unroot,
    } = helpers;
    let Pipes {
        status,
        block,
        resolv,
    } = pipes;
    if let Some(net) = net {
        let mut w = fs::File::from(resolv.write);
        let _ = writeln!(w, "nameserver {}", net.dns_fwd);
    } // closed: bwrap reads the data up to EOF

    let mut child = Command::new("bwrap")
        .arg("--json-status-fd")
        .arg(status.write.as_raw_fd().to_string())
        .arg("--block-fd")
        .arg(block.read.as_raw_fd().to_string())
        .args(bwrap_args)
        .args(app_wrap)
        .arg(app)
        .args(app_args)
        .spawn()
        .unwrap_or_else(|e| die(1, format!("cannot run bwrap: {e}")));
    forward_signals_to(&child);
    // the child's ends: it has them, nobody spawned later should
    drop(status.write);
    drop(block.read);
    drop(resolv.read);
    let kill_all = |pids: &[i32]| {
        for p in pids {
            if *p > 0 {
                unsafe { libc::kill(*p, libc::SIGKILL) };
            }
        }
    };

    // the first status message arrives once the namespaces exist; it also
    // carries namespace inode numbers, so pick the child-pid field specifically.
    // The pipe stays open until bwrap has exited: bwrap writes the app's exit
    // status there too, and a closed read end would kill it with SIGPIPE
    let status_read = fs::File::from(status.read);
    let line = read_line_timeout(&status_read, Duration::from_secs(10)).unwrap_or_default();
    let child_pid = parse_child_pid(&line).unwrap_or_else(|| {
        kill_all(&[child.id() as i32]);
        die(1, "bwrap did not report a child pid")
    });

    if let Some(net) = net {
        let mut pasta = vec!["--config-net".to_string(), "--quiet".into()];
        pasta.extend(net.pasta_ip.iter().cloned());
        pasta.extend(net.out_args.iter().cloned());
        pasta.extend(net.fwd_args.iter().cloned());
        pasta.extend([
            "--userns".into(),
            format!("/proc/{child_pid}/ns/user"),
            "--netns".into(),
            format!("/proc/{child_pid}/ns/net"),
        ]);
        let pasta_ref: Vec<&str> = pasta.iter().map(String::as_str).collect();
        if !run_tool("pasta", &pasta_ref) {
            kill_all(&[child_pid, child.id() as i32]);
            die(1, "pasta failed (is the passt package installed?)");
        }

        // a fresh netns has net.ipv4.ping_group_range empty, and modern ping
        // drops its file caps and only tries unprivileged ICMP datagram
        // sockets, gated by that sysctl; /proc/sys/net follows the writer's
        // netns. Only gid 0 is mapped in the sandbox userns, so "0 0" is the
        // widest legal range. For snaps, zero max_user_namespaces: the app
        // holds no caps and can't gain any under no_new_privs, so it cannot
        // raise the limit back
        let mut sysctls = String::from("echo 0 0 > /proc/sys/net/ipv4/ping_group_range");
        if snap {
            sysctls += "; echo 0 > /proc/sys/user/max_user_namespaces";
        }
        let _ = Command::new("nsenter")
            .args([
                "--preserve-credentials",
                "-U",
                "-n",
                "-t",
                &child_pid.to_string(),
                "sh",
                "-c",
                &sysctls,
            ])
            .stderr(std::process::Stdio::null())
            .status();
    }
    let relay = if usb {
        crate::uevent::forward(child_pid)
    } else {
        None
    };
    if let Some(host) = askroot {
        host.start(child_pid);
    }

    let sandbox_ns = crate::unroot::namespace_of(child_pid);
    // network is up; release the app
    let _ = fs::File::from(block.write).write_all(b"\n");
    // the released app now creates its nested userns and waits for its map
    if unroot {
        crate::unroot::map_or_warn(child_pid, sandbox_ns.as_deref());
    }
    let status = child
        .wait()
        .unwrap_or_else(|e| die(1, format!("waiting for bwrap: {e}")));
    drop(status_read);
    if let Some(pid) = relay {
        kill_all(&[pid]);
        // SAFETY: reaping our own child
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    }
    exit_code(status)
}

/// The "child-pid" field of bwrap's JSON status line.
fn parse_child_pid(line: &str) -> Option<i32> {
    line.split("\"child-pid\":")
        .nth(1)
        .and_then(|s| s.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse().ok())
}

/// Read one line from a pipe, giving up after `timeout`.
fn read_line_timeout(mut f: &fs::File, timeout: Duration) -> Option<String> {
    let mut line = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: f.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if n <= 0 {
            return None;
        }
        let mut b = [0u8; 1];
        match f.read(&mut b) {
            Ok(0) => {
                return (!line.is_empty()).then(|| String::from_utf8_lossy(&line).into_owned())
            }
            Ok(_) if b[0] == b'\n' => return Some(String::from_utf8_lossy(&line).into_owned()),
            Ok(_) => line.push(b[0]),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn defaults_without_an_interface() {
        let n = configure(None, false);
        assert_eq!(n.pasta_ip, ["-4"]);
        assert!(n.out_args.is_empty());
        assert_eq!(n.fwd_args, ["--dns-forward", "169.254.1.1"]);
        assert_eq!(n.dns_fwd, "169.254.1.1");
        assert!(configure(None, true).pasta_ip.is_empty());
    }

    #[test]
    fn child_pid_from_status_line() {
        assert_eq!(
            parse_child_pid(r#"{ "child-pid": 4242, "ns": 1 }"#),
            Some(4242)
        );
        assert_eq!(parse_child_pid(r#"{"child-pid":7}"#), Some(7));
        assert_eq!(parse_child_pid(r#"{ "exit-code": 0 }"#), None);
        assert_eq!(parse_child_pid(""), None);
    }

    #[test]
    fn exit_codes_like_a_shell() {
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(
            exit_code(std::process::ExitStatus::from_raw(libc::SIGTERM)),
            143
        );
    }

    #[test]
    fn pipes_are_inheritable_on_the_child_side() {
        let p = Pipes::new();
        let flags = |fd: &OwnedFd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        for child_end in [&p.status.write, &p.block.read, &p.resolv.read] {
            assert_eq!(flags(child_end) & libc::FD_CLOEXEC, 0);
        }
        for our_end in [&p.status.read, &p.block.write, &p.resolv.write] {
            assert_ne!(flags(our_end) & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn line_with_and_without_data() {
        let p = Pipe::new();
        fs::File::from(p.write).write_all(b"hello\nrest").unwrap();
        let r = fs::File::from(p.read);
        assert_eq!(
            read_line_timeout(&r, Duration::from_secs(1)).as_deref(),
            Some("hello")
        );
        assert_eq!(
            read_line_timeout(&r, Duration::from_millis(50)).as_deref(),
            Some("rest") // the pipe stays usable: EOF without a newline still yields the data
        );
        let p = Pipe::new(); // nothing ever written: times out
        assert_eq!(
            read_line_timeout(&fs::File::from(p.read), Duration::from_millis(50)),
            None
        );
    }
}
