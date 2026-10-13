//! sandbox: run an app inside strict bubblewrap isolation — own namespaces,
//! no network unless asked, a per-box home, and an optional writable system
//! layer. See README.md.

mod askroot;
mod boxdir;
mod cli;
mod join;
mod net;
mod tty;
mod uevent;
mod unroot;
mod util;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use util::{die, home, output, realpath_e, realpath_m, warn, which};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some(tty::EXEC_OPTION) {
        tty::exec(&argv[2..]);
    }
    let opts = match cli::parse(argv.into_iter().skip(1).collect()) {
        Ok(o) => o,
        Err(e) => {
            e.report();
            std::process::exit(e.exit_status_code())
        }
    };
    let home = home();
    let mut args: Vec<OsString> = Vec::new();
    let push =
        |args: &mut Vec<OsString>, items: &[&str]| args.extend(items.iter().map(OsString::from));

    // -w/-W/-r at their real paths, the symlink as given recreated inside
    let mut bind_args: Vec<OsString> = Vec::new();
    for b in &opts.binds {
        bind_args.extend([
            if b.ro { "--ro-bind" } else { "--bind" }.into(),
            b.real.clone().into(),
            b.real.clone().into(),
        ]);
        if let Some(from) = &b.symlink_from {
            bind_args.extend([
                "--symlink".into(),
                b.real.clone().into(),
                from.clone().into(),
            ]);
        }
    }
    if let Some(cd) = opts.chdir.clone().or(opts.workdir.clone()) {
        bind_args.extend(["--chdir".into(), cd.into()]);
    }

    // --root, -n or -p root: map the real uid to 0 inside. With -n it's
    // required: pasta can only gain caps in the sandbox userns if its uid
    // maps to root there (its self-hardening blocks the join otherwise) —
    // like rootless podman/docker. With -p root it is what the approved
    // commands run as
    let mut net_args: Vec<OsString> = Vec::new();
    if opts.root || opts.net || opts.askroot {
        push(&mut net_args, &["--uid", "0", "--gid", "0"]);
    }
    // -n, -p usb and -p root: the pipes bwrap gets (its helpers need the
    // sandbox's pid), and with -n the sandbox resolv.conf — its contents
    // come through one of them — bound at the symlink target (e.g.
    // systemd-resolved's stub under /run), creating its directory; must
    // come after the /etc bind or it gets buried
    let pipes = (opts.net || opts.usb || opts.askroot).then(net::Pipes::new);
    let mut resolv_args: Vec<OsString> = Vec::new();
    if let (Some(p), true) = (&pipes, opts.net) {
        let resolv = realpath_m(Path::new("/etc/resolv.conf"));
        let dir = resolv.parent().unwrap_or(Path::new("/")).to_path_buf();
        resolv_args.extend([
            "--perms".into(),
            "0755".into(),
            "--dir".into(),
            dir.into(),
            "--ro-bind-data".into(),
            p.resolv.read.as_raw_fd().to_string().into(),
            resolv.into(),
        ]);
    }

    // -x: the X11 socket and auth cookie; DISPLAY ":1" or "host:1.0" -> "1"
    let mut x11_args: Vec<OsString> = Vec::new();
    if opts.x11 {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        let num = display.split_once(':').map(|(_, d)| d).unwrap_or(&display);
        let num = num.split('.').next().unwrap_or(num);
        let sock = PathBuf::from(format!("/tmp/.X11-unix/X{num}"));
        if !fs::metadata(&sock)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
        {
            die(1, format!("no X11 socket at {}", sock.display()));
        }
        x11_args.extend([
            "--ro-bind".into(),
            sock.clone().into(),
            sock.into(),
            "--setenv".into(),
            "DISPLAY".into(),
            format!(":{num}").into(),
        ]);
        if let Some(auth) = std::env::var_os("XAUTHORITY") {
            x11_args.extend([
                "--ro-bind".into(),
                auth.clone(),
                auth.clone(),
                "--setenv".into(),
                "XAUTHORITY".into(),
                auth,
            ]);
        }
    }

    // -p camera: --dev gives a minimal /dev with no video nodes, so bind each
    // V4L2 device (and the media controller nodes some drivers pair with them)
    let mut camera_args: Vec<OsString> = Vec::new();
    if opts.camera {
        if let Ok(rd) = fs::read_dir("/dev") {
            let mut devs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (n.starts_with("video") || n.starts_with("media"))
                        && fs::metadata(p)
                            .map(|m| m.file_type().is_char_device())
                            .unwrap_or(false)
                })
                .collect();
            devs.sort();
            for d in devs {
                camera_args.extend(["--dev-bind".into(), d.clone().into(), d.into()]);
            }
        }
        if camera_args.is_empty() {
            warn("-p camera: no /dev/video* devices found");
        }
    }

    // -p usb: the whole host /dev, so a device that re-enumerates while the
    // sandbox runs (a firmware update reboots the device into its bootloader,
    // with new USB and hidraw nodes) stays reachable; binding single nodes
    // at start can't follow that. /dev/shm is kept private, /run/udev read
    // only for libudev's property database. Replaces the minimal --dev
    let mut usb_args: Vec<OsString> = Vec::new();
    if opts.usb {
        push(
            &mut usb_args,
            &[
                "--dev-bind",
                "/dev",
                "/dev",
                "--tmpfs",
                "/dev/shm",
                "--ro-bind-try",
                "/run/udev",
                "/run/udev",
            ],
        );
    }

    let netconf = if opts.net {
        Some(net::configure(opts.out_if.as_deref(), opts.ipv6))
    } else {
        None
    };

    // the app: resolved like `which` does, so `sandbox ping` and
    // `sandbox /usr/bin/ping` run the same binary and (with -a) share a box
    let (app, app_args): (Option<PathBuf>, Vec<String>) = match opts.cmd.split_first() {
        Some((name, rest)) => {
            let p = which(name).unwrap_or_else(|| die(1, format!("app not found: {name}")));
            let p = if p.is_absolute() {
                p
            } else {
                realpath_e(&p).unwrap_or(p)
            };
            (Some(p), rest.to_vec())
        }
        None => (None, Vec::new()),
    };

    // a snap's binary run directly (realpath /snap/foo/current/...): expose
    // /snap and forbid nested user namespaces. Snap builds never run their own
    // userns sandbox under snapd (AppArmor denies it), and e.g. the firefox
    // snap segfaults in every content process when that path is reachable;
    // with --disable-userns the app sees clone() fail and falls back cleanly,
    // as under Flatpak. --disable-userns works by nesting a second userns,
    // whose parent owns the netns — pasta can't control that from inside the
    // nested ns, so with -n forbid userns creation via sysctl instead (net.rs)
    let mut snap = false;
    let mut snap_args: Vec<OsString> = Vec::new();
    if let Some(rest) = app
        .as_ref()
        .and_then(|a| a.to_str())
        .and_then(|a| a.strip_prefix("/snap/"))
    {
        snap = true;
        // launcher scripts exec "$SNAP/...", which snapd normally provides. The
        // name vars matter too: e.g. firefox keys per-install profile selection
        // on them — without SNAP_INSTANCE_NAME it hashes its versioned
        // /snap/<name>/<rev> path as the install identity and orphans the
        // profile on every snap refresh
        let mut parts = rest.splitn(3, '/');
        let name = parts.next().unwrap_or("");
        let rev = parts.next().unwrap_or("");
        push(
            &mut snap_args,
            &[
                "--ro-bind",
                "/snap",
                "/snap",
                "--setenv",
                "SNAP",
                &format!("/snap/{name}/{rev}"),
                "--setenv",
                "SNAP_NAME",
                name,
                "--setenv",
                "SNAP_INSTANCE_NAME",
                name,
            ],
        );
        if !opts.net {
            push(&mut snap_args, &["--unshare-user", "--disable-userns"]);
        }
    }

    // -n and -p root map the real uid to 0; un-root the app again unless
    // --root asks for root or it's a snap (see unroot.rs)
    let app_wrap: Vec<String> = if (opts.net || opts.askroot) && !snap && !opts.root {
        unroot::wrapper()
    } else {
        Vec::new()
    };

    // a session bus of the sandbox's own: dbus-daemon started inside before
    // the app, listening where a session bus usually does. Nothing serves on
    // it, so it opens no door to the host; it lets the apps of one sandbox
    // talk to each other, as a second run joining the first does — Firefox
    // asks its running instance for a new window over it, and without a bus
    // on Wayland it has no other way and reports the instance "not
    // responding" at the profile lock. The daemon dies with the sandbox.
    // It gets a config of its own rather than --session: the stock session
    // config lists the host's service directories, and a bus that can
    // activate services starts the host's desktop inside the sandbox on
    // demand — the first name Chrome asks for brings up kwalletd6 (with
    // no wallet in the box, so its new-wallet wizard pops up and Chrome's
    // basic-store cookies and passwords stop decrypting), the portals,
    // at-spi, gvfs. Without service directories a name nobody owns just
    // doesn't exist, as on a bus-less sandbox before
    let bus_wrap: Vec<String> = if util::exists("dbus-daemon") {
        [
            "sh",
            "-c",
            "printf '%s\\n' \
               '<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"' \
               ' \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">' \
               '<busconfig><type>session</type><keep_umask/><auth>EXTERNAL</auth>' \
               \"<listen>unix:path=$XDG_RUNTIME_DIR/bus</listen>\" \
               '<policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>' \
               '<allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>' \
               > \"$XDG_RUNTIME_DIR/bus.conf\"; \
             dbus-daemon --config-file=\"$XDG_RUNTIME_DIR/bus.conf\" --fork --nopidfile --nosyslog \
               >/dev/null 2>&1; \
             exec \"$0\" \"$@\"",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else {
        Vec::new()
    };
    // mirror the binary's file capabilities (e.g. ping's cap_net_raw=ep), which
    // no_new_privs would silently drop at exec, as ambient caps in the sandbox userns
    let mut cap_args: Vec<OsString> = Vec::new();
    if let Some(a) = &app {
        if let Some(out) = output("getcap", &[&a.to_string_lossy()]) {
            let caps = out.split_whitespace().last().unwrap_or("");
            let caps = caps.split(['=', '+']).next().unwrap_or("");
            if caps.starts_with("cap_") {
                for c in caps.split(',') {
                    cap_args.extend(["--cap-add".into(), c.to_uppercase().into()]);
                }
            }
        }
    }

    let bx = boxdir::resolve(opts.box_.as_deref(), opts.app_box, app.as_deref(), &home);
    if opts.reset {
        boxdir::reset_system(&bx);
        if app.is_none() {
            return;
        }
    }
    let app = app.expect("an app to run");

    // the shim's mountpoint file goes into the upper layer before the layer
    // is mounted: fuse-overlayfs must see it from the start (boxdir::shim_args)
    // -p root mounts the --root view too: the root service writes the
    // system layer as uid 0, which the plain view's bookkeeping would deny
    let root_view = opts.root || opts.askroot;
    let shim_args = if root_view {
        boxdir::shim_args(&bx)
    } else {
        Vec::new()
    };
    let layer = if boxdir::has_layer(&bx) || root_view {
        boxdir::mount_layer(&bx, root_view)
    } else {
        boxdir::plain()
    };
    let mut cleanup = Cleanup {
        mounts: layer.mounts.clone(),
        ro_file_mountpoint: Vec::new(),
        files: Vec::new(),
    };

    // an app outside everything the sandbox shows (the system dirs, /snap, the
    // -w/-W/-r binds): e.g. a binary built under the real home, which the box
    // home hides. Bind just the file, read-only, at its own path; what the
    // app needs next to it is the user's call (-r its directory). Under $HOME
    // the mountpoint lands in the box home on disk: note what bwrap will have
    // to create there, to remove it afterwards
    let mut app_bind_args: Vec<OsString> = Vec::new();
    let mut app_bound = false;
    {
        let mut shown: Vec<PathBuf> = bind_destinations(&layer.args);
        shown.extend(bind_destinations(&snap_args));
        shown.extend(opts.binds.iter().map(|b| b.real.clone()));
        shown.extend(opts.binds.iter().filter_map(|b| b.symlink_from.clone()));
        let real = realpath_e(&app).unwrap_or_else(|_| app.clone());
        if !shown
            .iter()
            .any(|d| app.starts_with(d) || real.starts_with(d))
        {
            app_bound = true;
            app_bind_args.extend([
                "--ro-bind".into(),
                app.clone().into_os_string(),
                app.clone().into_os_string(),
            ]);
            if let Ok(rel) = app.strip_prefix(&home) {
                let mut p = bx.home.join(rel);
                while p != bx.home && fs::symlink_metadata(&p).is_err() {
                    cleanup.ro_file_mountpoint.push(p.clone());
                    p.pop();
                }
            }
        }
    }

    let runtime = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| die(1, "XDG_RUNTIME_DIR is not set")),
    );
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let h = |rel: &str| home.join(rel).into_os_string();

    push(&mut args, &["--unshare-all"]);
    args.extend(net_args);
    args.extend(cap_args);
    push(&mut args, &["--die-with-parent"]);
    if !opts.interactive {
        push(&mut args, &["--new-session"]);
    }
    push(&mut args, &["--hostname", "sandbox"]);
    args.extend(layer.args.iter().cloned());
    push(
        &mut args,
        &[
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/sbin",
            "/sbin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
        ],
    );
    args.extend(snap_args);
    args.extend(resolv_args);
    push(&mut args, &["--proc", "/proc"]);
    if opts.usb {
        args.extend(usb_args);
    } else {
        push(
            &mut args,
            &["--dev", "/dev", "--dev-bind", "/dev/dri", "/dev/dri"],
        );
        args.extend(camera_args);
    }
    // securityfs hidden: Ubuntu's dbus-daemon mediates its clients through
    // AppArmor when it finds it, and the policy query writes to securityfs,
    // read-only here, so every connection would be refused. Without the
    // mount AppArmor reads as unavailable and the bus runs unmediated, as
    // the sandbox's own bus should (see bus_wrap)
    push(
        &mut args,
        &[
            "--ro-bind",
            "/sys",
            "/sys",
            "--tmpfs",
            "/sys/kernel/security",
            "--tmpfs",
            "/tmp",
        ],
    );
    args.extend(x11_args);
    args.extend([
        "--bind".into(),
        bx.home.clone().into_os_string(),
        home.clone().into_os_string(),
    ]);
    // system appearance (dark/light theme): host toolkit configs, read-only.
    // GTK on Wayland takes theme and titlebar-button layout from GSettings,
    // which override settings.ini; dconf reads its db by mmap, so no D-Bus
    for rel in [
        ".config/kdeglobals",
        ".config/gtk-3.0/settings.ini",
        ".config/gtk-4.0/settings.ini",
        ".gtkrc-2.0",
        ".config/qt5ct",
        ".config/qt6ct",
        ".themes",
        ".icons",
        ".config/dconf/user",
    ] {
        args.extend(["--ro-bind-try".into(), h(rel), h(rel)]);
    }
    args.extend(bind_args);
    args.extend(app_bind_args);
    args.extend(shim_args);
    args.extend([
        "--perms".into(),
        "0700".into(),
        "--dir".into(),
        runtime.clone().into_os_string(),
    ]);
    let wl = runtime.join(&wayland);
    args.extend([
        "--ro-bind".into(),
        wl.clone().into_os_string(),
        wl.into_os_string(),
        "--setenv".into(),
        "WAYLAND_DISPLAY".into(),
        wayland.into(),
    ]);
    for s in ["pipewire-0", "pulse"] {
        let p = runtime.join(s);
        args.extend([
            "--ro-bind-try".into(),
            p.clone().into_os_string(),
            p.into_os_string(),
        ]);
    }
    let terminal = opts
        .interactive
        .then(|| tty::Terminal::capture(0))
        .flatten();
    let tty_wrap = if terminal.is_some() {
        let inside = runtime.join("sandbox-tty-exec");
        let executable = std::env::current_exe()
            .unwrap_or_else(|e| die(1, format!("finding sandbox executable: {e}")));
        args.extend(["--ro-bind".into(), executable.into(), inside.clone().into()]);
        vec![
            inside.to_string_lossy().into_owned(),
            tty::EXEC_OPTION.into(),
        ]
    } else {
        Vec::new()
    };
    // -p root: the shim over sudo and pkexec, and the socket it reaches the
    // host by; the server behind it is bound once this run is known not to
    // join another sandbox (askroot_host below)
    let askroot_socket = opts.askroot.then(|| askroot::socket_path(&runtime));
    if let Some(sock) = &askroot_socket {
        args.extend(askroot::bwrap_args(sock, &runtime));
    }
    // the host's session bus and session manager are out of reach: point
    // at the sandbox's own bus, drop the manager (its socket is in the host's
    // /tmp; ICE would otherwise complain about a non-local host at startup).
    // gtk3-nocsd forces server-side titlebars onto every GTK app via env; the
    // preload works inside too (/usr is bound) and silently overrides the
    // app's own decoration setting (e.g. firefox's titlebar checkbox): strip
    // it. --root prepends the shim
    if bus_wrap.is_empty() {
        push(&mut args, &["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]);
    } else {
        push(
            &mut args,
            &[
                "--setenv",
                "DBUS_SESSION_BUS_ADDRESS",
                &format!("unix:path={}/bus", runtime.display()),
            ],
        );
    }
    push(
        &mut args,
        &["--unsetenv", "SESSION_MANAGER", "--unsetenv", "GTK_CSD"],
    );
    let host_preload = std::env::var("LD_PRELOAD").ok();
    let mut preload: Vec<String> = host_preload
        .iter()
        .flat_map(|s| s.split(':'))
        .filter(|l| !l.is_empty() && !l.contains("nocsd"))
        .map(str::to_string)
        .collect();
    if opts.root {
        preload.insert(0, boxdir::SHIM_INSIDE.into());
    }
    if !preload.is_empty() {
        push(&mut args, &["--setenv", "LD_PRELOAD", &preload.join(":")]);
    } else if host_preload.is_some() {
        push(&mut args, &["--unsetenv", "LD_PRELOAD"]);
    }

    let app_prefix: Vec<String> = app_wrap
        .iter()
        .chain(&tty_wrap)
        .chain(&bus_wrap)
        .cloned()
        .collect();

    // a sandbox with the same box and the same shape already running gets
    // the command instead of a second sandbox (join.rs); -s opts out. What
    // shapes it: the options that decide what the app sees inside, not what
    // only concerns the command itself (chdir, the session, the environment).
    // What the app itself adds (/snap, its own bind) is matched by need.
    // -z ID names the sandbox instead: the name is all that has to match
    let provides = join::Provides {
        snap,
        app: app_bound.then(|| app.clone()),
    };
    let needs = if opts.zygote.is_some() {
        join::Provides::default()
    } else {
        join::Provides {
            snap: provides.snap,
            app: provides.app.clone(),
        }
    };
    let mut registry = (!opts.separate || opts.zygote.is_some()).then(|| {
        if let Some(name) = &opts.zygote {
            return join::Registry::open(&runtime, &join::key(&[format!("name={name}")]));
        }
        let mut parts = vec![
            format!("box={}", bx.path().display()),
            format!("root={}", opts.root),
            format!("askroot={}", opts.askroot),
            format!("net={}", opts.net),
            format!("out_if={}", opts.out_if.as_deref().unwrap_or("")),
            format!("ipv6={}", opts.ipv6),
            format!("x11={}", opts.x11),
            format!("camera={}", opts.camera),
            format!("usb={}", opts.usb),
        ];
        // one part per field: a path may contain anything but NUL, which
        // is what separates parts, so joining fields with a ':' could make
        // two different bind sets read the same
        let mut binds: Vec<(bool, &Path, &Path)> = opts
            .binds
            .iter()
            .map(|b| {
                (
                    b.ro,
                    b.real.as_path(),
                    b.symlink_from.as_deref().unwrap_or(Path::new("")),
                )
            })
            .collect();
        binds.sort();
        for (ro, real, from) in binds {
            parts.push(format!("bind.ro={ro}"));
            parts.push(format!("bind.real={}", real.display()));
            parts.push(format!("bind.from={}", from.display()));
        }
        join::Registry::open(&runtime, &join::key(&parts))
    });
    if let Some(target) = registry
        .as_ref()
        .and_then(|r| r.find(!app_wrap.is_empty(), &needs))
    {
        drop(registry); // the lock: others may look while we run
                        // what of the bwrap command line concerns the command itself
        let mut env: Vec<join::EnvEdit> = Vec::new();
        let mut caps: Vec<String> = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--setenv" {
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    env.push(join::EnvEdit::Set(k.clone(), v.clone()));
                }
            } else if a == "--unsetenv" {
                if let Some(k) = it.next() {
                    env.push(join::EnvEdit::Unset(k.clone()));
                }
            } else if a == "--cap-add" {
                if let Some(c) = it.next() {
                    caps.push(c.to_string_lossy().into_owned());
                }
            }
        }
        let code = join::run(
            &target,
            &app,
            &app_args,
            &env,
            opts.chdir.as_deref().or(opts.workdir.as_deref()),
            !opts.interactive,
            &caps,
        );
        drop(terminal);
        drop(cleanup);
        std::process::exit(code);
    }
    if let Some(r) = registry.as_mut() {
        r.register(&provides);
    }
    drop(registry);
    let askroot_host = askroot_socket.map(|sock| {
        cleanup.files.push(sock.clone());
        let box_name = bx
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        askroot::Host::bind(&sock, box_name, app.to_string_lossy().into_owned())
    });

    let code = match pipes {
        None => {
            if cleanup.is_empty() && terminal.is_none() {
                // nothing to clean up afterwards: become bwrap
                let err = Command::new("bwrap")
                    .args(&args)
                    .args(&bus_wrap)
                    .arg(&app)
                    .args(&app_args)
                    .exec();
                die(1, format!("cannot run bwrap: {err}"))
            }
            let mut child = Command::new("bwrap")
                .args(&args)
                .args(&tty_wrap)
                .args(&bus_wrap)
                .arg(&app)
                .args(&app_args)
                .spawn()
                .unwrap_or_else(|e| die(1, format!("cannot run bwrap: {e}")));
            net::forward_signals_to(&child);
            net::exit_code(
                child
                    .wait()
                    .unwrap_or_else(|e| die(1, format!("waiting for bwrap: {e}"))),
            )
        }
        Some(pipes) => net::run(
            &args,
            &app_prefix,
            &app,
            &app_args,
            net::Helpers {
                net: netconf.as_ref(),
                snap,
                usb: opts.usb,
                askroot: askroot_host,
                unroot: !app_wrap.is_empty(),
            },
            pipes,
        ),
    };
    drop(terminal);
    drop(cleanup);
    std::process::exit(code);
}

/// Where a list of bwrap bind options mounts things: the destination of
/// every `--bind`/`--ro-bind`/`--ro-bind-try` in it.
fn bind_destinations(args: &[OsString]) -> Vec<PathBuf> {
    args.windows(3)
        .filter(|w| ["--bind", "--ro-bind", "--ro-bind-try"].contains(&w[0].to_str().unwrap_or("")))
        .map(|w| PathBuf::from(&w[2]))
        .collect()
}

/// What this run leaves behind, undone whichever way main() ends: the
/// layers it mounted, the mountpoint of the app's read-only file bind when
/// it lies in the box home (an empty file bwrap creates there, and the
/// directories down to it that didn't exist), deepest first, and files of
/// its own (the -p root socket).
struct Cleanup {
    mounts: Vec<PathBuf>,
    ro_file_mountpoint: Vec<PathBuf>,
    files: Vec<PathBuf>,
}
impl Cleanup {
    fn is_empty(&self) -> bool {
        self.mounts.is_empty() && self.ro_file_mountpoint.is_empty() && self.files.is_empty()
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        boxdir::unmount(&self.mounts);
        for f in &self.files {
            let _ = fs::remove_file(f);
        }
        for p in &self.ro_file_mountpoint {
            let _ = match fs::symlink_metadata(p) {
                Ok(m) if m.is_dir() => fs::remove_dir(p),
                Ok(m) if m.is_file() && m.len() == 0 => fs::remove_file(p),
                _ => Ok(()),
            };
        }
    }
}
