//! Integration tests: the built binary runs real bwrap sandboxes on a
//! scratch box under `~/tmp/sandbox-tests`, with a throwaway `$HOME` and
//! runtime dir, so nothing touches the user's own boxes. Each test checks
//! what the app sees from inside (via `sh -c`) against what lands in the
//! box on disk. Tests whose host prerequisites are missing (user
//! namespaces, `/dev/dri`, pasta, fuse-overlayfs) skip themselves with a
//! note on stderr instead of failing.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SANDBOX: &str = env!("CARGO_BIN_EXE_sandbox");

/// Skip the test, with the reason, when a host prerequisite is missing.
macro_rules! require {
    ($cond:expr, $why:expr) => {
        if !$cond {
            eprintln!("skipped: {}", $why);
            return;
        }
    };
}

/// A throwaway environment for one test: `home/` is `$HOME` (so even the
/// default box lands here), `run/` is `XDG_RUNTIME_DIR` with a plain file
/// standing in for the Wayland socket (bwrap binds it like any file), and
/// `box/` is the box the test passes with `-b`. Removed afterwards unless
/// the test failed.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
            .join("tmp/sandbox-tests")
            .join(format!("it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(dir.join("run")).unwrap();
        fs::write(dir.join("run/wayland-0"), "").unwrap();
        // as /proc/self/mounts will show it: no `..` from the crate dir
        let dir = fs::canonicalize(dir).unwrap();
        Scratch { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn box_dir(&self) -> PathBuf {
        self.dir.join("box")
    }

    /// `<box>$HOME`: where the sandbox's home lives on disk.
    fn box_home(&self) -> PathBuf {
        self.box_dir().join(self.home().strip_prefix("/").unwrap())
    }

    /// A path under the scratch dir, as a string for the command line.
    fn path(&self, rel: &str) -> String {
        self.dir.join(rel).to_str().unwrap().to_string()
    }

    /// The binary in this environment; the box is not chosen yet.
    fn sandbox(&self) -> Command {
        let mut c = Command::new(SANDBOX);
        c.env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env_remove("LD_PRELOAD");
        c
    }

    /// `sandbox <opts> -b <box> sh -c <script>`, run to completion.
    fn sh(&self, opts: &[&str], script: &str) -> Output {
        self.sandbox()
            .args(opts)
            .arg("-b")
            .arg(self.box_dir())
            .args(["sh", "-c", script])
            .output()
            .expect("runs sandbox")
    }

    /// `sandbox -b <box> --reset-system`
    fn reset(&self) -> Output {
        self.sandbox()
            .arg("-b")
            .arg(self.box_dir())
            .arg("--reset-system")
            .output()
            .expect("runs sandbox")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // keep the evidence when the test failed
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The run succeeded; its stdout as lines.
fn lines(o: &Output) -> Vec<String> {
    assert!(
        o.status.success(),
        "sandbox failed ({}): {}",
        o.status,
        stderr(o)
    );
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Can bwrap build the sandbox here? Needs unprivileged user namespaces and
/// the GPU node the binary always binds.
fn bwrap_works() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("bwrap")
            .args([
                "--unshare-all",
                "--ro-bind",
                "/",
                "/",
                "--dev-bind",
                "/dev/dri",
                "/dev/dri",
                "/bin/true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// -n needs pasta and the namespace tools, and a host with a default route
/// for pasta to pick its interface from.
fn net_works() -> bool {
    let default_route = fs::read_to_string("/proc/net/route")
        .map(|t| {
            t.lines()
                .any(|l| l.split_whitespace().nth(1) == Some("00000000"))
        })
        .unwrap_or(false);
    have("pasta") && have("nsenter") && have("unshare") && default_route
}

/// How many mounts are under `dir` (from /proc/self/mounts).
fn mounts_under(dir: &Path) -> usize {
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(1))
        .filter(|m| Path::new(m).starts_with(dir))
        .count()
}

fn wait_for(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "sandbox did not exit in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn uid() -> String {
    // SAFETY: always succeeds
    unsafe { libc::getuid() }.to_string()
}

fn uid_name() -> String {
    std::env::var("USER").unwrap_or_default()
}

#[test]
fn home_is_the_box_and_the_real_one_is_hidden() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("home");
    fs::write(s.home().join("secret"), "").unwrap();
    let o = s.sh(
        &[],
        r#"echo "$HOME"; ls -A "$HOME" | wc -l; test -e "$HOME/secret" && echo visible || echo hidden; echo kept > "$HOME/kept"; id -u; id -g"#,
    );
    let out = lines(&o);
    assert_eq!(out[0], s.home().to_str().unwrap()); // $HOME is the same path...
    assert_eq!(out[1], "0"); // ...but empty: the box's, not the real one
    assert_eq!(out[2], "hidden");
    assert_eq!(out[3], uid());
    assert_eq!(out[4], unsafe { libc::getgid() }.to_string());
    // what the app wrote is in the box, laid out like /
    assert_eq!(
        fs::read_to_string(s.box_home().join("kept")).unwrap(),
        "kept\n"
    );
    assert!(!s.home().join("kept").exists());
}

#[test]
fn system_is_read_only_offline_and_renamed() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("system");
    let o = s.sh(
        &[],
        "cat /proc/sys/kernel/hostname; \
         tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '; \
         touch /usr/x 2>/dev/null && echo writable || echo read-only; \
         touch /etc/x 2>/dev/null && echo writable || echo read-only; \
         test -e /var && echo var || echo no-var; \
         ls -A /tmp | wc -l; \
         echo \"${DBUS_SESSION_BUS_ADDRESS-unset}\"; \
         echo \"$WAYLAND_DISPLAY\"; stat -c %a \"$XDG_RUNTIME_DIR\"; \
         test -e \"$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY\" && echo socket",
    );
    let bus = if have("dbus-daemon") {
        format!("unix:path={}/bus", s.dir.join("run").display())
    } else {
        "unset".into()
    };
    assert_eq!(
        lines(&o),
        [
            "sandbox",
            "lo", // the only interface: no network
            "read-only",
            "read-only",
            "no-var", // only a box with a system layer has /var
            "0",      // a fresh tmpfs
            &bus,     // the sandbox's own session bus, not the host's
            "wayland-0",
            "700",
            "socket",
        ]
    );
}

#[test]
fn the_session_bus_is_private_and_works() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(
        have("dbus-daemon") && have("dbus-send"),
        "dbus-daemon/dbus-send are not installed"
    );
    let s = Scratch::new("bus");
    let id = "dbus-send --session --print-reply --dest=org.freedesktop.DBus / \
              org.freedesktop.DBus.GetId | tail -n1";
    // reachable, with nothing but the bus itself on it; two sandboxes have
    // two buses, and --root gets one too
    let o = s.sh(
        &[],
        &format!(
            "{id} > /tmp/id && echo bus; \
             dbus-send --session --print-reply --dest=org.freedesktop.DBus / \
               org.freedesktop.DBus.ListNames | grep -c 'string \"org' "
        ),
    );
    assert_eq!(lines(&o), ["bus", "1"]);
    let mut a = hold(
        &s,
        &[],
        &format!("{id} > /tmp/id; echo ready; exec sleep 30"),
    );
    let o = s.sh(
        &[],
        &format!("[ \"$({id})\" = \"$(cat /tmp/id)\" ] && echo same || echo other"),
    );
    assert_eq!(lines(&o), ["same"], "a joined run is on the sandbox's bus");
    let o = s.sh(&["-s"], &format!("{id} > /dev/null && echo bus"));
    assert_eq!(lines(&o), ["bus"]);
    // SAFETY: our own child's pid
    unsafe { libc::kill(a.id() as i32, libc::SIGTERM) };
    wait_for(&mut a, Duration::from_secs(10));
}

#[test]
fn usb_permission_binds_the_whole_dev_with_a_private_shm() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(
        Path::new("/dev/bus/usb").is_dir(),
        "no /dev/bus/usb on this host"
    );
    let s = Scratch::new("usb");
    let script = "test -d /dev/bus/usb && echo usb || echo no-usb; \
                  ls /dev/hidraw* 2>/dev/null | wc -l; \
                  ls -A /dev/shm | wc -l; \
                  test -d /run/udev && echo udev || echo no-udev";
    // withheld by default: the minimal /dev
    assert_eq!(lines(&s.sh(&[], script))[0], "no-usb");
    // granted: the host's /dev as is, except a private /dev/shm
    let out = lines(&s.sh(&["-p", "usb"], script));
    let host_hidraws = fs::read_dir("/dev")
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("hidraw"))
        .count();
    assert_eq!(out[0], "usb");
    assert_eq!(out[1], host_hidraws.to_string());
    assert_eq!(out[2], "0");
    assert_eq!(
        out[3],
        if Path::new("/run/udev").is_dir() {
            "udev"
        } else {
            "no-udev"
        }
    );
}

#[test]
fn exit_status_stdin_and_stdout_pass_through() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("stdio");
    let o = s.sh(&[], "exit 7");
    assert_eq!(o.status.code(), Some(7));
    let mut child = s
        .sandbox()
        .arg("-b")
        .arg(s.box_dir())
        .args(["sh", "-c", "tr a-z A-Z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(lines(&o), ["HELLO"]);
}

#[test]
fn binds_at_their_real_paths_with_the_symlink_recreated() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("binds");
    fs::create_dir(s.dir.join("shared")).unwrap();
    fs::create_dir(s.dir.join("ro")).unwrap();
    fs::write(s.dir.join("ro/data"), "data").unwrap();
    fs::create_dir(s.dir.join("private")).unwrap();
    std::os::unix::fs::symlink(s.dir.join("shared"), s.dir.join("link")).unwrap();
    let real = fs::canonicalize(s.dir.join("shared")).unwrap();
    // -W through the symlink: the app starts in the real dir, and the link
    // works inside too; -r is readable but not writable
    let o = s.sh(
        &["-W", &s.path("link"), "-r", &s.path("ro")],
        &format!(
            "pwd; echo made > f; test -L {link} && echo link; echo via-link > {link}/g; \
             cat {ro}/data; echo; touch {ro}/x 2>/dev/null && echo writable || echo read-only; \
             test -e {other} && echo visible || echo hidden",
            link = s.path("link"),
            ro = s.path("ro"),
            other = s.path("private") // the rest of the scratch dir is not bound
        ),
    );
    assert_eq!(
        lines(&o),
        [
            real.to_str().unwrap(),
            "link",
            "data",
            "read-only",
            "hidden"
        ]
    );
    assert_eq!(
        fs::read_to_string(s.dir.join("shared/f")).unwrap(),
        "made\n"
    );
    assert_eq!(
        fs::read_to_string(s.dir.join("shared/g")).unwrap(),
        "via-link\n"
    );
    assert!(!s.dir.join("ro/x").exists());
    // -d wins over -W's start dir
    let o = s.sh(&["-W", &s.path("shared"), "-d", "/usr"], "pwd");
    assert_eq!(lines(&o), ["/usr"]);
}

#[test]
fn an_app_outside_every_bind_is_bound_read_only_by_itself() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("app-bind");
    // an executable in the scratch dir, which nothing binds; its sibling
    // must stay hidden and the file itself read-only
    fs::create_dir(s.dir.join("app")).unwrap();
    let exe = s.dir.join("app/run");
    fs::write(
        &exe,
        "#!/bin/sh\necho ran; echo x >> \"$0\" 2>/dev/null && echo writable || echo read-only; \
         test -e \"${0%/*}/sibling\" && echo visible || echo hidden\n",
    )
    .unwrap();
    fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    fs::write(s.dir.join("app/sibling"), "").unwrap();
    let run = |opts: &[&str]| {
        s.sandbox()
            .args(opts)
            .arg("-b")
            .arg(s.box_dir())
            .arg(&exe)
            .output()
            .expect("runs sandbox")
    };
    assert_eq!(lines(&run(&[])), ["ran", "read-only", "hidden"]);
    // under a -r bind the directory is already there, sibling included
    assert_eq!(
        lines(&run(&["-r", &s.path("app")])),
        ["ran", "read-only", "visible"]
    );
    // under the real home the mountpoint lands in the box home: bwrap's
    // empty file and the directories down to it are gone afterwards
    fs::create_dir_all(s.home().join("src/proj")).unwrap();
    let under_home = s.home().join("src/proj/run");
    fs::copy(&exe, &under_home).unwrap();
    let o = s
        .sandbox()
        .arg("-b")
        .arg(s.box_dir())
        .arg(&under_home)
        .output()
        .unwrap();
    assert_eq!(lines(&o), ["ran", "read-only", "hidden"]);
    assert!(!s.box_home().join("src").exists());
}

#[test]
fn default_and_per_app_boxes_are_created_under_home() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("boxes");
    let rel_home = s.home();
    let rel_home = rel_home.strip_prefix("/").unwrap();
    let sandboxes = s.home().join("sandboxes");
    let o = s.sandbox().args(["sh", "-c", "true"]).output().unwrap();
    lines(&o);
    assert!(sandboxes.join("default").join(rel_home).is_dir());
    // -a: named after the binary's path; -b NAME: under ~/sandboxes
    let o = s
        .sandbox()
        .args(["-a", "/usr/bin/sh", "-c", "true"])
        .output()
        .unwrap();
    lines(&o);
    assert!(sandboxes.join("usr-bin-sh").join(rel_home).is_dir());
    let o = s
        .sandbox()
        .args(["-b", "named", "sh", "-c", "true"])
        .output()
        .unwrap();
    lines(&o);
    assert!(sandboxes.join("named").join(rel_home).is_dir());
}

#[test]
fn usage_errors_and_help_through_the_binary() {
    let s = Scratch::new("usage");
    let o = s.sandbox().output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).starts_with("usage: sandbox"));
    let o = s.sandbox().arg("-h").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("--reset-system"));
    let o = s.sandbox().arg("--bogus").arg("sh").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unrecognized option '--bogus'"));
    let o = s.sandbox().arg("no-such-app-here").output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("app not found: no-such-app-here"));
    let o = s
        .sandbox()
        .args(["-w", "/nonexistent/dir", "sh"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("bind dir not found: /nonexistent/dir"));
    // resetting a box without a layer is a no-op that succeeds
    let o = s.reset();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(s.box_home().is_dir());
}

#[test]
fn system_layer_lifecycle() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    let s = Scratch::new("layer");
    let b = s.box_dir();

    // --root: fake root, the shim preloaded, the system writable. mkdir
    // mode 000 then chmod is what dpkg does for every directory; chown to
    // an id the sandbox doesn't have "succeeds"
    let o = s.sh(
        &["--root"],
        "id -u; echo \"$LD_PRELOAD\"; \
         mkdir -m 000 /opt/d && chmod 755 /opt/d && echo x > /opt/d/f && echo shim-ok; \
         chown 12345:12345 /opt/d/f && echo chown-ok; \
         echo marker > /usr/local/it-marker && echo installed; \
         test -d /var/lib && echo var",
    );
    let preload = format!("{}/sandbox-rootshim.so", s.path("run"));
    assert_eq!(
        lines(&o),
        [
            "0",
            preload.as_str(),
            "shim-ok",
            "chown-ok",
            "installed",
            "var"
        ]
    );
    // the changes are in the box, the host untouched, the mounts gone
    for d in ["usr", "etc", "opt", "var", ".work", ".mnt"] {
        assert!(b.join(d).is_dir(), "{d}");
    }
    assert_eq!(
        fs::read_to_string(b.join("usr/local/it-marker")).unwrap(),
        "marker\n"
    );
    assert_eq!(fs::read_to_string(b.join("opt/d/f")).unwrap(), "x\n");
    assert!(!Path::new("/usr/local/it-marker").exists());
    assert_eq!(mounts_under(&b.join(".mnt")), 0);

    // a plain run on the same box: the real uid again, the installed files
    // part of the read-only system (root's, not the app's), its own files its
    let o = s.sh(
        &[],
        "id -u; cat /usr/local/it-marker; stat -c %u /usr/local/it-marker; cat /opt/d/f; \
         echo y >> /usr/local/it-marker 2>/dev/null && echo writable || echo read-only; \
         touch /usr/local/new 2>/dev/null && echo writable || echo read-only; \
         echo own > \"$HOME/own\" && stat -c %u \"$HOME/own\"",
    );
    let out = lines(&o);
    assert_eq!(out[0], uid());
    assert_eq!(out[1], "marker");
    assert_ne!(out[2], uid(), "installed file must not be the app's");
    assert_eq!(out[3], "x");
    assert_eq!(out[4], "read-only");
    assert_eq!(out[5], "read-only");
    assert_eq!(out[6], uid());
    assert_eq!(mounts_under(&b.join(".mnt")), 0);
    assert_eq!(
        fs::read_to_string(b.join("usr/local/it-marker")).unwrap(),
        "marker\n"
    );

    // --reset-system: the layer goes, the home stays
    let o = s.reset();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    for d in ["usr", "etc", "opt", "var", ".work", ".mnt"] {
        assert!(!b.join(d).exists(), "{d} should be gone");
    }
    assert_eq!(
        fs::read_to_string(s.box_home().join("own")).unwrap(),
        "own\n"
    );
    // and the next plain run sees the host's system again
    let o = s.sh(
        &[],
        "test -e /usr/local/it-marker && echo still || echo gone",
    );
    assert_eq!(lines(&o), ["gone"]);
    assert!(!b.join("usr").exists());
}

#[test]
fn reset_is_refused_while_the_box_is_mounted() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    let s = Scratch::new("busy");
    let b = s.box_dir();
    let mut running = s
        .sandbox()
        .args(["--root", "-b"])
        .arg(&b)
        .args(["sh", "-c", "echo ready; exec sleep 30"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(running.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
    assert!(mounts_under(&b.join(".mnt")) > 0);

    let o = s.reset();
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("is in use"), "{}", stderr(&o));
    assert!(b.join("usr").is_dir());

    // SIGTERM to sandbox stops bwrap, and the layers are unmounted on the way out
    // SAFETY: our own child's pid
    unsafe { libc::kill(running.id() as i32, libc::SIGTERM) };
    let status = wait_for(&mut running, Duration::from_secs(10));
    assert!(!status.success());
    assert_eq!(mounts_under(&b.join(".mnt")), 0);
    let o = s.reset();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!b.join("usr").exists());
}

#[test]
fn the_last_sandbox_on_a_box_unmounts_its_layers() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    let s = Scratch::new("last-out");
    let b = s.box_dir();
    // two names: the second run doesn't join the first
    let mut a = hold(&s, &["--root", "-z", "a"], "echo ready; exec sleep 30");
    let mounted = mounts_under(&b.join(".mnt"));
    assert!(mounted > 0);
    let mut c = hold(&s, &["--root", "-z", "c"], "echo ready; exec sleep 30");
    assert_eq!(
        mounts_under(&b.join(".mnt")),
        mounted,
        "reused, not stacked"
    );

    // the one that mounted leaves first: the layers stay, for real — a
    // third run finds and reuses them, and the box can't be reset
    // SAFETY: our own child's pid
    unsafe { libc::kill(a.id() as i32, libc::SIGTERM) };
    wait_for(&mut a, Duration::from_secs(10));
    assert_eq!(mounts_under(&b.join(".mnt")), mounted);
    let o = s.sh(&["--root", "-s"], "test -d /var/lib/dpkg && echo ok");
    assert_eq!(lines(&o), ["ok"]);
    assert_eq!(mounts_under(&b.join(".mnt")), mounted);
    let o = s.reset();
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("is in use"), "{}", stderr(&o));

    // the last one out takes them down
    // SAFETY: our own child's pid
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    wait_for(&mut c, Duration::from_secs(10));
    assert_eq!(mounts_under(&b.join(".mnt")), 0);
    let o = s.reset();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn reset_takes_down_what_a_dead_sandbox_left_mounted() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    let s = Scratch::new("orphans");
    let b = s.box_dir();
    let mut a = hold(&s, &["--root"], "echo ready; exec sleep 30");
    assert!(mounts_under(&b.join(".mnt")) > 0);
    // SIGKILL: no cleanup; bwrap dies with its parent, the mounts stay
    // SAFETY: our own child's pid
    unsafe { libc::kill(a.id() as i32, libc::SIGKILL) };
    wait_for(&mut a, Duration::from_secs(10));
    assert!(mounts_under(&b.join(".mnt")) > 0);
    // nobody holds the box: reset unmounts the orphans and goes ahead
    // (bwrap may still be on its way out for a moment)
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut o = s.reset();
    while o.status.code() != Some(0) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        o = s.reset();
    }
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(mounts_under(&b.join(".mnt")), 0);
    assert!(!b.join("usr").exists());
}

/// A sandbox held open on `script` (which must print "ready" once it is
/// up), with its stdout consumed up to that line.
fn hold(s: &Scratch, opts: &[&str], script: &str) -> Child {
    let mut c = s
        .sandbox()
        .args(opts)
        .arg("-b")
        .arg(s.box_dir())
        .args(["sh", "-c", script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(c.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
    c
}

#[test]
fn a_run_like_a_running_sandbox_joins_it() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("join");
    let mut a = hold(&s, &[], "touch /tmp/from-a; echo ready; exec sleep 30");
    // same box, same options: inside that sandbox — its /tmp, processes and
    // hostname; as the real uid, with bwrap's environment edits, without
    // capabilities, in the -d directory
    let o = s
        .sandbox()
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nowhere")
        .env("SESSION_MANAGER", "local/host:/nowhere")
        .args(["-d", "/usr", "-b"])
        .arg(s.box_dir())
        .args([
            "sh",
            "-c",
            "pwd; test -e /tmp/from-a && echo shared || echo own; cat /proc/sys/kernel/hostname; \
             id -u; echo ${SESSION_MANAGER:-unset} $WAYLAND_DISPLAY; \
             test \"$DBUS_SESSION_BUS_ADDRESS\" = \"unix:path=$XDG_RUNTIME_DIR/bus\" && echo own-bus; \
             cat /proc/[0-9]*/comm | grep -c '^sleep$'; grep '^CapEff' /proc/self/status; \
             grep '^NoNewPrivs' /proc/self/status",
        ])
        .output()
        .unwrap();
    assert_eq!(
        lines(&o),
        [
            "/usr",
            "shared",
            "sandbox",
            &uid(),
            "unset wayland-0",
            "own-bus",
            "1",
            "CapEff:\t0000000000000000",
            "NoNewPrivs:\t1"
        ]
    );
    // the command's exit status comes back
    assert_eq!(s.sh(&[], "exit 7").status.code(), Some(7));
    // -s, or options that shape the sandbox differently: one of its own
    assert_eq!(
        lines(&s.sh(&["-s"], "test -e /tmp/from-a && echo shared || echo own")),
        ["own"]
    );
    fs::create_dir(s.dir.join("ro")).unwrap();
    assert_eq!(
        lines(&s.sh(
            &["-r", &s.path("ro")],
            "test -e /tmp/from-a && echo shared || echo own"
        )),
        ["own"]
    );
    // an app the sandbox doesn't have (one outside every bind, bound in
    // by itself) gets a sandbox of its own, and the running one stays
    // joinable next to it; a shell joins the app's sandbox too
    fs::create_dir(s.dir.join("app")).unwrap();
    let exe = s.dir.join("app/run");
    fs::write(
        &exe,
        "#!/bin/sh\ntest -e /tmp/from-a && echo shared || echo own; \
         [ \"$1\" = hold ] && { touch /tmp/from-run; echo ready; exec sleep 30; }; true\n",
    )
    .unwrap();
    fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let run_exe = |hold: bool| {
        let mut c = s.sandbox();
        c.arg("-b").arg(s.box_dir()).arg(&exe);
        if hold {
            c.arg("hold");
        }
        c
    };
    assert_eq!(lines(&run_exe(false).output().unwrap()), ["own"]);
    assert_eq!(
        lines(&s.sh(&[], "test -e /tmp/from-a && echo shared || echo own")),
        ["shared"]
    );
    let mut r = run_exe(true).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    let mut out = BufReader::new(r.stdout.take().unwrap());
    out.read_line(&mut line).unwrap(); // "own"
    line.clear();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    // two sandboxes fit it now; either way a shell lands in one of them
    assert_eq!(
        lines(&s.sh(
            &[],
            "test -e /tmp/from-run && echo in-run || { test -e /tmp/from-a && echo in-a; }"
        ))
        .len(),
        1
    );
    // SAFETY: our own child's pid
    unsafe { libc::kill(r.id() as i32, libc::SIGTERM) };
    wait_for(&mut r, Duration::from_secs(10));
    assert_eq!(
        lines(&s.sh(&[], "test -e /tmp/from-a && echo shared || echo own")),
        ["shared"]
    );
    // -z ID: the name alone decides, whatever the options
    let mut n = hold(
        &s,
        &["-z", "one"],
        "touch /tmp/from-one; echo ready; exec sleep 30",
    );
    let see = "test -e /tmp/from-one && echo one || { test -e /tmp/from-a && echo a || echo own; }";
    assert_eq!(lines(&s.sh(&["-zone", "-r", &s.path("ro")], see)), ["one"]);
    assert_eq!(lines(&s.sh(&["--zygote=two"], see)), ["own"]);
    assert_eq!(lines(&s.sh(&["-s"], see)), ["own"]);
    assert_eq!(lines(&s.sh(&[], see)), ["a"]);
    // SAFETY: our own child's pid
    unsafe { libc::kill(n.id() as i32, libc::SIGTERM) };
    wait_for(&mut n, Duration::from_secs(10));
    // a joined command ends with the sandbox
    let mut j = hold(&s, &[], "echo ready; exec sleep 30");
    // SAFETY: our own child's pid
    unsafe { libc::kill(a.id() as i32, libc::SIGTERM) };
    assert!(!wait_for(&mut a, Duration::from_secs(10)).success());
    assert!(!wait_for(&mut j, Duration::from_secs(10)).success());
    // with the sandbox gone its record is stale: a new run starts one
    assert_eq!(
        lines(&s.sh(&[], "test -e /tmp/from-a && echo shared || echo own")),
        ["own"]
    );
}

#[test]
fn joining_under_net_lands_in_the_app_s_nested_namespace() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(
        net_works(),
        "pasta/nsenter/unshare or a default route is missing"
    );
    let s = Scratch::new("join-net");
    let mut a = hold(&s, &["-n"], "echo ready; exec sleep 30");
    let o = s.sh(
        &["-n"],
        "id -u; tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | grep -vc '^lo$'; \
         cat /proc/[0-9]*/comm | grep -c '^sleep$'",
    );
    assert_eq!(lines(&o), [uid().as_str(), "1", "1"]);
    // SAFETY: our own child's pid
    unsafe { libc::kill(a.id() as i32, libc::SIGTERM) };
    wait_for(&mut a, Duration::from_secs(10));
}

#[test]
fn net_gives_an_interface_and_keeps_the_real_uid() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(
        net_works(),
        "pasta/nsenter/unshare or a default route is missing"
    );
    let s = Scratch::new("net");
    let o = s.sh(
        &["-n"],
        "id -u; cat /etc/resolv.conf; tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | grep -vc '^lo$'; \
         echo f > \"$HOME/f\" && stat -c %u \"$HOME/f\"; exit 7",
    );
    // the app's exit status comes through (bwrap keeps reporting on the
    // status pipe until it exits)
    assert_eq!(o.status.code(), Some(7), "{}", stderr(&o));
    let out: Vec<&str> = std::str::from_utf8(&o.stdout).unwrap().lines().collect();
    assert_eq!(out[0], uid(), "un-rooted after pasta's root mapping");
    assert_eq!(out[1], "nameserver 169.254.1.1");
    assert_eq!(out[2], "1", "one interface besides lo");
    assert_eq!(out[3], uid(), "files stay the user's");
    assert_eq!(fs::read_to_string(s.box_home().join("f")).unwrap(), "f\n");
}

#[test]
fn root_permission_asks_the_host_and_runs_the_command_as_root() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    require!(
        have("nsenter") && have("unshare"),
        "nsenter/unshare missing"
    );
    let s = Scratch::new("askroot");
    // a dialog that records the prompt and answers as told
    let dialog = s.dir.join("dialog");
    fs::write(
        &dialog,
        format!(
            "#!/bin/sh\ncat > {}; echo $(cat {})\n",
            s.path("prompt"),
            s.path("answer")
        ),
    )
    .unwrap();
    fs::set_permissions(&dialog, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(s.path("answer"), "approve").unwrap();
    let o = s
        .sandbox()
        .env("SANDBOX_ASKROOT_DIALOG", &dialog)
        .args(["-p", "root", "-b"])
        .arg(s.box_dir())
        .args([
            "sh",
            "-c",
            "id -u; sudo id -u; sudo sh -c 'echo hi > /usr/local/ok; echo $SUDO_USER'; \
             cat /usr/local/ok; pkexec --user root sh -c 'echo $$ > /dev/null && echo in'; \
             echo x | sudo cat; sudo -v && echo validated; sudo false; echo $?; \
             su -c 'id -u'; echo 'id -u; case $PWD in /|/root) echo home;; esac' | su -l root; \
             su bob -c id; echo $?",
        ])
        .output()
        .unwrap();
    assert_eq!(
        lines(&o),
        [
            uid(),
            "0".into(),
            uid_name(),
            "hi".into(),
            "in".into(),
            "x".into(),
            "validated".into(),
            "1".into(),
            "0".into(),
            "0".into(),
            "home".into(),
            "1".into()
        ]
    );
    let prompt = fs::read_to_string(s.path("prompt")).unwrap();
    assert!(prompt.contains("command: /bin/sh -l\n"), "{prompt}");
    assert!(
        prompt.contains("app: /bin/sh\n") || prompt.contains("app: /usr/bin/sh\n"),
        "{prompt}"
    );
    assert_eq!(
        fs::read_to_string(s.box_dir().join("usr/local/ok")).unwrap(),
        "hi\n"
    );
    assert_eq!(mounts_under(&s.box_dir().join(".mnt")), 0);

    fs::write(s.path("answer"), "deny").unwrap();
    let o = s
        .sandbox()
        .env("SANDBOX_ASKROOT_DIALOG", &dialog)
        .args(["-p", "root", "-b"])
        .arg(s.box_dir())
        .args(["sh", "-c", "sudo id -u; echo $?"])
        .output()
        .unwrap();
    assert_eq!(lines(&o), ["1"]);
    assert!(stderr(&o).contains("denied"), "{}", stderr(&o));
}

#[test]
fn net_with_root_keeps_root() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(
        net_works(),
        "pasta/nsenter/unshare or a default route is missing"
    );
    require!(have("fuse-overlayfs"), "fuse-overlayfs is not installed");
    let s = Scratch::new("net-root");
    let o = s.sh(&["-n", "--root"], "id -u; cat /etc/resolv.conf");
    assert_eq!(lines(&o), ["0", "nameserver 169.254.1.1"]);
    assert_eq!(mounts_under(&s.box_dir().join(".mnt")), 0);
}

/// A real controlling terminal, with a host shell in its initial foreground
/// group. The shell waits after the sandbox exits so the test can check the
/// hand-back while that group still exists.
struct Pty {
    master: fs::File,
    child: Child,
    output: String,
}

impl Pty {
    fn start(s: &Scratch, opts: &[&str]) -> Self {
        Self::command(s, opts, &["sh"])
    }

    fn command(s: &Scratch, opts: &[&str], command: &[&str]) -> Self {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid output pointers; default terminal settings and size.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: these freshly allocated descriptors are ours.
        let (master, slave) =
            unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) };
        // openpty doesn't set CLOEXEC; only the three standard descriptors
        // should reach the host shell, and neither end should leak into other
        // commands spawned by concurrent tests.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "\"$@\"; result=$?; printf '\\nPTY_RESULT=%s\\n' \"$result\"; read finish",
            "sh",
            SANDBOX,
            "-i",
            "-b",
        ])
        .arg(s.box_dir())
        .args(opts)
        .args(command)
        .env("HOME", s.home())
        .env("XDG_RUNTIME_DIR", s.dir.join("run"))
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("PS1", "PTY_PROMPT> ")
        .env_remove("LD_PRELOAD")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
        if s.dir.join("dialog").exists() {
            cmd.env("SANDBOX_ASKROOT_DIALOG", s.dir.join("dialog"));
        }
        // SAFETY: session/terminal syscalls only, in the new host shell.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        Pty {
            master,
            child,
            output: String::new(),
        }
    }

    fn send(&mut self, input: &str) {
        self.master.write_all(input.as_bytes()).unwrap();
    }

    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.output.contains(marker) {
            assert!(
                Instant::now() < deadline,
                "waiting for {marker:?}: {}",
                self.output
            );
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: valid descriptor and buffer, with a bounded wait.
            if unsafe { libc::poll(&mut poll, 1, 100) } > 0 {
                let mut buffer = [0u8; 4096];
                let n = unsafe {
                    libc::read(
                        self.master.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                assert!(
                    n > 0,
                    "PTY closed while waiting for {marker:?}: {}",
                    self.output
                );
                self.output
                    .push_str(&String::from_utf8_lossy(&buffer[..n as usize]));
            }
        }
    }

    fn finish(mut self, status: i32) {
        self.until(&format!("PTY_RESULT={status}\r\n"));
        assert!(
            !self.output.contains("Cannot set tty process group"),
            "{}",
            self.output
        );
        assert!(!self.output.contains("can't access tty"), "{}", self.output);
        // The master can query the foreground group without belonging to the
        // slave's session; querying the slave from this test would be ENOTTY.
        assert_eq!(
            unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) },
            self.child.id() as i32,
            "terminal not returned to the host shell: {}",
            self.output
        );
        self.send("finish\n");
        assert!(wait_for(&mut self.child, Duration::from_secs(5)).success());
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            // Stop the foreground job too if an assertion failed mid-session.
            let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
            if group > 0 && group != unsafe { libc::getpgrp() } {
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn interactive_shell_preserves_status_and_returns_the_terminal() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("tty");
    let mut pty = Pty::start(&s, &[]);
    pty.until("PTY_PROMPT> ");
    // Interrupt a foreground external command, then keep using the shell.
    pty.send("sh -c 'echo SLEEP_READY; exec sleep 30'\n");
    pty.until("SLEEP_READY\r\n");
    pty.output.clear();
    pty.send("\u{3}");
    pty.until("PTY_PROMPT> ");
    pty.send("printf 'INTERRUPTED=%s\\n' \"$?\"\n");
    pty.until("INTERRUPTED=130\r\n");
    pty.send("exit 5\n");
    pty.finish(5);
}

#[test]
fn interactive_shell_killed_by_a_signal_returns_the_terminal() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("tty-signal");
    let mut pty = Pty::start(&s, &[]);
    pty.until("PTY_PROMPT> ");
    pty.send("kill -KILL $$\n");
    pty.finish(137);
}

#[test]
fn interactive_join_preserves_status_and_returns_the_terminal() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    let s = Scratch::new("tty-join");
    let mut running = hold(&s, &[], "echo ready; exec sleep 30");
    let mut pty = Pty::start(&s, &[]);
    pty.until("PTY_PROMPT> ");
    // A file in the existing sandbox's /tmp proves this shell joined it.
    pty.send("echo joined > /tmp/tty-joined; exit 5\n");
    pty.finish(5);
    assert_eq!(lines(&s.sh(&[], "cat /tmp/tty-joined")), ["joined"]);
    assert!(running.try_wait().unwrap().is_none());
    running.kill().unwrap();
    running.wait().unwrap();
}

#[test]
fn interactive_root_shell_returns_to_the_requesting_shell() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs missing");
    require!(
        have("nsenter") && have("unshare"),
        "namespace tools missing"
    );
    let s = Scratch::new("tty-root");
    fs::write(
        s.dir.join("dialog"),
        "#!/bin/sh\ncat >/dev/null; echo approve\n",
    )
    .unwrap();
    fs::set_permissions(s.dir.join("dialog"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut pty = Pty::start(&s, &["-p", "root"]);
    pty.until("PTY_PROMPT> ");
    // Both nested shells read the PTY and must hand it back to the shim,
    // which exits and lets the requesting shell resume its own job control.
    for command in ["sudo sh", "sudo -s", "su root", "sudo su", "sudo su -"] {
        let original = unsafe { libc::tcgetpgrp(pty.master.as_raw_fd()) };
        pty.output.clear();
        pty.send(&format!("{command}\n"));
        // sudo filters PS1, so use a marker printed by the root shell instead
        // of relying on its prompt. It cannot run until the handoff finishes.
        pty.send("printf 'ROOT_READY=%s\\n' \"$(id -u)\"\n");
        pty.until("ROOT_READY=0\r\n");
        pty.output.clear();
        pty.send("exit 7\n");
        pty.until("PTY_PROMPT> ");
        assert_eq!(unsafe { libc::tcgetpgrp(pty.master.as_raw_fd()) }, original);
        pty.send("printf 'ROOT_STATUS=%s\\n' \"$?\"\n");
        pty.until("ROOT_STATUS=7\r\n");
        assert!(
            !pty.output.contains("Cannot set tty process group"),
            "{}",
            pty.output
        );
    }
    pty.send("exit 5\n");
    pty.finish(5);
}

#[test]
fn sudo_login_starts_an_interactive_root_shell() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(have("fuse-overlayfs"), "fuse-overlayfs missing");
    require!(
        have("nsenter") && have("unshare"),
        "namespace tools missing"
    );
    let s = Scratch::new("sudo-login");
    fs::write(
        s.dir.join("dialog"),
        "#!/bin/sh\ncat >\"$0.prompt\"; echo approve\n",
    )
    .unwrap();
    fs::set_permissions(s.dir.join("dialog"), fs::Permissions::from_mode(0o755)).unwrap();
    // Root's shell and home must come from the box, not the caller's SHELL or
    // HOME. A custom profile also verifies login startup without relying on
    // the host's shell configuration.
    // /root isn't a persistent system directory; bind a fixture home for the
    // login shell, with the profile in a custom passwd home path.
    let root_home = s.dir.join("root-home");
    fs::create_dir_all(&root_home).unwrap();
    fs::write(root_home.join(".profile"), "echo LOGIN_READY\n").unwrap();
    let setup = s.sh(
        &["--root"],
        &format!(
            "sed -i 's|^root:[^:]*:0:0:[^:]*:[^:]*:.*|root:x:0:0:root:{}:/bin/sh|' /etc/passwd",
            root_home.display()
        ),
    );
    assert!(setup.status.success(), "{}", stderr(&setup));
    let home = root_home.to_str().unwrap();
    let mut pty = Pty::command(&s, &["-p", "root", "-w", home], &["sudo", "-i"]);
    pty.until("LOGIN_READY\r\n");
    pty.send("printf 'ROOT_UID=%s\\nROOT_HOME=%s\\nROOT_SHELL=%s\\nROOT_CWD=%s\\n' \"$(id -u)\" \"$HOME\" \"$SHELL\" \"$PWD\"\n");
    pty.until("ROOT_UID=0\r\n");
    pty.until(&format!("ROOT_HOME={home}\r\n"));
    pty.until("ROOT_SHELL=/bin/sh\r\n");
    pty.until(&format!("ROOT_CWD={home}\r\n"));
    pty.send("exit 5\n");
    pty.finish(5);
    let prompt = fs::read_to_string(s.dir.join("dialog.prompt")).unwrap();
    assert!(prompt.contains("command: /bin/sh -l\n"), "{prompt}");
    assert!(prompt.contains(&format!("cwd: {home}\n")), "{prompt}");
    // Supplying a command must execute it in the login shell rather than
    // opening an interactive shell or dropping arguments containing spaces.
    let output = s
        .sandbox()
        .env("SANDBOX_ASKROOT_DIALOG", s.dir.join("dialog"))
        .args(["-p", "root", "-b"])
        .arg(s.box_dir())
        .args([
            "-w", home, "sudo", "--login", "printf", "%s\\n", "a b", "x'y", "",
        ])
        .output()
        .unwrap();
    assert_eq!(lines(&output), ["LOGIN_READY", "a b", "x'y", ""]);
}

#[test]
fn interactive_network_shell_preserves_status() {
    require!(bwrap_works(), "bwrap can't create a sandbox here");
    require!(net_works(), "network prerequisites missing");
    let s = Scratch::new("tty-net");
    let mut pty = Pty::start(&s, &["-n"]);
    pty.until("PTY_PROMPT> ");
    pty.send("exit 5\n");
    pty.finish(5);
}
