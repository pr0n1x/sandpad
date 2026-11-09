//! Command line: getopt-compatible (`+`: options stop at the first
//! non-option, so the app's own flags pass through), with the same help text
//! the script had.

use crate::util::{absolute, realpath_e, realpath_m};
use std::path::PathBuf;

/// Why parsing stopped short of a runnable command line.
#[derive(Debug, PartialEq)]
pub enum CliError {
    /// -h/--help
    Help,
    /// the command line itself is wrong: an unknown option, a missing value,
    /// no app; `message` is empty when the usage line says it all
    Usage { message: String },
    /// a -w/-W/-r directory that doesn't exist
    BindNotFound { dir: String },
}

impl CliError {
    fn usage(message: impl Into<String>) -> CliError {
        CliError::Usage {
            message: message.into(),
        }
    }

    /// What the process exits with: getopt's 2 for a usage error, 1 for a
    /// bad path, 0 for help.
    pub fn exit_status_code(&self) -> i32 {
        match self {
            CliError::Help => 0,
            CliError::Usage { .. } => 2,
            CliError::BindNotFound { .. } => 1,
        }
    }

    /// What to print: the help on stdout, everything else on stderr.
    pub fn report(&self) {
        match self {
            CliError::Help => print!("{}", help()),
            CliError::Usage { message } => {
                if !message.is_empty() {
                    eprintln!("sandpad: {message}");
                }
                eprintln!("{USAGE}");
            }
            CliError::BindNotFound { dir } => eprintln!("sandpad: bind dir not found: {dir}"),
        }
    }
}

const USAGE: &str = "usage: sandpad [-i|--interactive] [-s|--separate] [-z|--zygote ID] [-w|--bind DIR]... [-W|--workdir DIR]... [-r|--ro-bind DIR]... [-d|--chdir DIR] [-b|--box NAME|DIR] [-a|--app-box] [-n|--net[IFACE]] [-6|--ipv6] [-x|--x11] [-p|--permissions LIST]... [--root] /usr/bin/someapp [args...]
       sandpad [-b NAME|DIR] --reset-system";

fn help() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    format!(
        "usage: sandpad [options] /usr/bin/someapp [args...]
       sandpad [-b NAME|DIR] --reset-system

Run an app inside strict bubblewrap isolation: own namespaces, no network,
read-only system, a private home, Wayland/GPU/sound passed through.
Everything the app keeps lives in its box, ~/sandpad/<name>/, laid out
like the root filesystem:
  home/{user:<18}    bound as $HOME
  usr/ etc/ opt/ var/     the app's changes to the system, once a --root
                          run created them (fuse-overlayfs layers over
                          the host's dirs; a box without them sees the
                          host's system read-only)
See README.md for details.

options:
  -i, --interactive   keep the terminal session for job control, like
                      'docker run -i' (drops bwrap's --new-session)
  -s, --separate      a sandbox of its own, never joined. By default a run
                      whose box and options match a sandbox already
                      running joins it, like 'docker exec': the command
                      shares that sandbox's processes, /tmp, IPC and
                      network, and ends with it
  -z, --zygote ID     the sandbox named ID: joined if it is running,
                      started under that name otherwise. Later runs with
                      the same ID join it whatever their other options
  -w, --bind DIR      rw-bind DIR at its real path; repeatable
  -W, --workdir DIR   rw-bind DIR at its real path and start the app there
                      (in the last one if repeated)
  -r, --ro-bind DIR   ro-bind DIR at its real path; repeatable
  -d, --chdir DIR     start the app in DIR (overrides -W's chdir)
  -b, --box NAME|DIR  use box ~/sandpad/NAME, or the directory DIR
                      (default: the shared box ~/sandpad/default)
  -a, --app-box       per-app box instead, named after the binary's path:
                      /usr/bin/foo -> ~/sandpad/usr-bin-foo
  -n, --net[IFACE]    outbound networking via pasta (rootless NAT); snap
                      apps run as fake root inside. With an attached IFACE
                      (-nenp39s0 / --net=enp39s0) traffic is pinned to that
                      interface, bypassing e.g. a WireGuard default route
  -6, --ipv6          with -n: also enable IPv6 (default is IPv4-only)
  -x, --x11           pass the X11 socket and auth cookie through, for
                      X11-only apps (weakens isolation: X clients can snoop
                      each other)
  -p, --permissions LIST
                      grant extra access to host resources, comma-separated;
                      repeatable. Known permissions:
                        camera   the webcam(s): /dev/video* and /dev/media*
                        usb      USB and HID devices: the whole host /dev,
                                 live, and udev hot-plug events relayed in
                        x11      same as -x
                        root     sudo, pkexec and su inside ask you, on the host,
                                 whether the command may run as root in the
                                 sandbox (a dialog per request); root means
                                 the box's own system layer, never the host
  --root              run as root inside: uid 0 in the sandbox is your own
                      uid outside, so it grants no host privileges; files it
                      creates are yours. Gives the box a system layer (/usr,
                      /etc, /opt, /var writable, changes kept in the box,
                      never reaching the host) that shows every system file
                      as root's, so installers can change anything readable;
                      e.g. 'sandpad --root -b mybox dpkg -i x.deb'
  --reset-system      reset the box's system to the host's, keeping its
                      home: removes the system layer (usr/ etc/ opt/ var/
                      and the overlay's scratch dirs). Refused while a
                      sandbox runs on the box
  -h, --help          show this help
"
    )
}

/// A -w/-W/-r bind: DIR's real path, bound at itself; if DIR was given
/// through a symlink, that symlink is recreated inside so the path as
/// given keeps working.
#[derive(Debug, Clone)]
pub struct Bind {
    pub ro: bool,
    pub real: PathBuf,
    pub symlink_from: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Opts {
    pub interactive: bool,
    pub separate: bool,
    /// -z ID: the name under which this sandbox is joined
    pub zygote: Option<String>,
    pub binds: Vec<Bind>,
    pub workdir: Option<PathBuf>,
    pub chdir: Option<PathBuf>,
    pub box_: Option<String>,
    pub app_box: bool,
    pub net: bool,
    pub out_if: Option<String>,
    pub ipv6: bool,
    pub x11: bool,
    pub camera: bool,
    pub usb: bool,
    /// -p root: sudo/pkexec shims and the root service, asking the host
    pub askroot: bool,
    pub root: bool,
    pub reset: bool,
    /// the app and its arguments, verbatim
    pub cmd: Vec<String>,
}

fn bind(ro: bool, dir: &str) -> Result<Bind, CliError> {
    let given = PathBuf::from(dir);
    let real = realpath_e(&given).map_err(|_| CliError::BindNotFound { dir: dir.into() })?;
    let orig = absolute(&given);
    Ok(Bind {
        ro,
        real: real.clone(),
        symlink_from: (orig != real).then_some(orig),
    })
}

pub fn parse(args: Vec<String>) -> Result<Opts, CliError> {
    let mut o = Opts::default();
    let mut it = args.into_iter().peekable();
    // value of an option that takes one: attached ("-bNAME", "--box=NAME") or the next arg
    fn value(
        attached: Option<String>,
        it: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
        name: &str,
    ) -> Result<String, CliError> {
        attached
            .or_else(|| it.next())
            .ok_or_else(|| CliError::usage(format!("option requires an argument -- '{name}'")))
    }
    while let Some(arg) = it.peek().cloned() {
        if arg == "--" {
            it.next();
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break; // the app: stop here, its flags are its own
        }
        it.next();
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match name {
                "help" => return Err(CliError::Help),
                "interactive" => o.interactive = true,
                "bind" => o.binds.push(bind(false, &value(attached, &mut it, name)?)?),
                "workdir" => {
                    let b = bind(false, &value(attached, &mut it, name)?)?;
                    o.workdir = Some(b.real.clone());
                    o.binds.push(b)
                }
                "ro-bind" => o.binds.push(bind(true, &value(attached, &mut it, name)?)?),
                "chdir" => {
                    o.chdir = Some(realpath_m(&PathBuf::from(value(attached, &mut it, name)?)))
                }
                "box" => o.box_ = Some(value(attached, &mut it, name)?),
                "app-box" => o.app_box = true,
                "separate" => o.separate = true,
                "zygote" => o.zygote = Some(value(attached, &mut it, name)?),
                "net" => {
                    o.net = true;
                    o.out_if = attached.filter(|s| !s.is_empty())
                } // IFACE only attached: --net=IFACE
                "ipv6" => o.ipv6 = true,
                "x11" => o.x11 = true,
                "permissions" => permissions(&mut o, &value(attached, &mut it, name)?)?,
                "root" => o.root = true,
                "reset-system" => o.reset = true,
                _ => return Err(CliError::usage(format!("unrecognized option '--{name}'"))),
            }
            continue;
        }
        // short options, possibly clustered ("-ix"); a value option takes the
        // rest of the cluster as its value ("-bNAME") or the next arg
        let shorts: Vec<char> = arg[1..].chars().collect();
        let mut i = 0;
        while i < shorts.len() {
            let c = shorts[i];
            let rest: String = shorts[i + 1..].iter().collect();
            let attached = (!rest.is_empty()).then_some(rest);
            let takes_value = matches!(c, 'w' | 'W' | 'r' | 'd' | 'b' | 'p' | 'z');
            match c {
                'h' => return Err(CliError::Help),
                'i' => o.interactive = true,
                's' => o.separate = true,
                'z' => o.zygote = Some(value(attached, &mut it, "z")?),
                'a' => o.app_box = true,
                '6' => o.ipv6 = true,
                'x' => o.x11 = true,
                'n' => {
                    o.net = true;
                    o.out_if = attached;
                    i = shorts.len();
                    continue;
                } // -nIFACE: the rest is the interface
                'w' => o.binds.push(bind(false, &value(attached, &mut it, "w")?)?),
                'W' => {
                    let b = bind(false, &value(attached, &mut it, "W")?)?;
                    o.workdir = Some(b.real.clone());
                    o.binds.push(b)
                }
                'r' => o.binds.push(bind(true, &value(attached, &mut it, "r")?)?),
                'd' => o.chdir = Some(realpath_m(&PathBuf::from(value(attached, &mut it, "d")?))),
                'b' => o.box_ = Some(value(attached, &mut it, "b")?),
                'p' => permissions(&mut o, &value(attached, &mut it, "p")?)?,
                _ => return Err(CliError::usage(format!("invalid option -- '{c}'"))),
            }
            if takes_value {
                break;
            } // the value consumed the rest of the cluster
            i += 1;
        }
    }
    o.cmd = it.collect();
    if o.cmd.is_empty() && !o.reset {
        return Err(CliError::usage(""));
    }
    Ok(o)
}

/// -p NAME[,NAME...]: named grants of host resources, so new ones don't each
/// need an option letter
fn permissions(o: &mut Opts, list: &str) -> Result<(), CliError> {
    for perm in list.split(',') {
        match perm {
            "camera" => o.camera = true,
            "usb" => o.usb = true,
            "x11" => o.x11 = true,
            "root" => o.askroot = true,
            "" => {}
            other => {
                return Err(CliError::usage(format!(
                    "unknown permission: {other} (known: camera, usb, x11, root)"
                )))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse_ok(args: &[&str]) -> Opts {
        parse(args.iter().map(|s| s.to_string()).collect()).expect("parses")
    }

    fn parse_err(args: &[&str]) -> CliError {
        parse(args.iter().map(|s| s.to_string()).collect()).expect_err("fails")
    }

    /// a scratch dir under ~/tmp/sandpad-tests, on a real filesystem
    fn scratch(name: &str) -> PathBuf {
        let d = crate::util::home()
            .join("tmp/sandpad-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn options_and_app() {
        let o = parse_ok(&[
            "-b",
            "mybox",
            "-nenp39s0",
            "-x",
            "--root",
            "app",
            "--flag",
            "-x",
        ]);
        assert_eq!(o.box_.as_deref(), Some("mybox"));
        assert!(o.net);
        assert_eq!(o.out_if.as_deref(), Some("enp39s0"));
        assert!(o.x11 && o.root);
        assert_eq!(o.cmd, ["app", "--flag", "-x"]); // the app's flags are its own
        assert!(!o.separate && o.zygote.is_none());
        assert!(parse_ok(&["-s", "app"]).separate);
        assert!(parse_ok(&["--separate", "app"]).separate);
        assert_eq!(parse_ok(&["-zff", "app"]).zygote.as_deref(), Some("ff"));
        assert_eq!(parse_ok(&["-z", "ff", "app"]).zygote.as_deref(), Some("ff"));
        let o = parse_ok(&["-isz", "ff", "app"]);
        assert!(o.interactive && o.separate && o.zygote.as_deref() == Some("ff"));
        let o = parse_ok(&["--zygote=ff", "app"]);
        assert_eq!(o.zygote.as_deref(), Some("ff"));
        assert_eq!(
            parse_ok(&["--zygote", "ff", "app"]).zygote.as_deref(),
            Some("ff")
        );
    }

    #[test]
    fn net_interface_forms() {
        assert_eq!(parse_ok(&["-n", "app"]).out_if, None);
        assert_eq!(parse_ok(&["--net", "app"]).out_if, None);
        assert_eq!(
            parse_ok(&["--net=eth0", "app"]).out_if.as_deref(),
            Some("eth0")
        );
        // -n takes only an attached interface: "-n eth0" means the app is eth0
        assert_eq!(parse_ok(&["-n", "eth0"]).cmd, ["eth0"]);
    }

    #[test]
    fn clustered_shorts() {
        let o = parse_ok(&["-ix", "app"]);
        assert!(o.interactive && o.x11);
        let o = parse_ok(&["-ibmybox", "app"]); // a value option eats the rest
        assert!(o.interactive);
        assert_eq!(o.box_.as_deref(), Some("mybox"));
        let o = parse_ok(&["-i6", "-b", "x", "app"]);
        assert!(o.interactive && o.ipv6);
    }

    #[test]
    fn permissions_list() {
        let o = parse_ok(&["-p", "camera,x11", "app"]);
        assert!(o.camera && o.x11);
        let o = parse_ok(&["-p", "camera", "--permissions", "x11", "app"]);
        assert!(o.camera && o.x11);
        let o = parse_ok(&["-p", "usb", "app"]);
        assert!(o.usb && !o.camera);
        match parse_err(&["-p", "nfc", "app"]) {
            CliError::Usage { message } => assert!(message.contains("unknown permission: nfc")),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn errors() {
        assert_eq!(parse_err(&[]), CliError::usage(""));
        assert_eq!(parse_err(&["-x"]), CliError::usage(""));
        assert_eq!(parse_err(&["-h"]), CliError::Help);
        assert_eq!(parse_err(&["--help"]), CliError::Help);
        assert!(
            matches!(parse_err(&["--bogus", "app"]), CliError::Usage { message } if message.contains("--bogus"))
        );
        assert!(
            matches!(parse_err(&["-q", "app"]), CliError::Usage { message } if message.contains("'q'"))
        );
        assert!(
            matches!(parse_err(&["-b"]), CliError::Usage { message } if message.contains("requires an argument"))
        );
        assert!(
            parse_err(&["-w", "/nonexistent/dir", "app"])
                == CliError::BindNotFound {
                    dir: "/nonexistent/dir".into()
                }
        );
    }

    #[test]
    fn exit_status_codes() {
        assert_eq!(parse_err(&["-h"]).exit_status_code(), 0);
        assert_eq!(parse_err(&["--bogus"]).exit_status_code(), 2);
        assert_eq!(parse_err(&[]).exit_status_code(), 2);
        assert_eq!(
            parse_err(&["-w", "/nonexistent", "app"]).exit_status_code(),
            1
        );
    }

    #[test]
    fn reset_needs_no_app() {
        let o = parse_ok(&["-b", "x", "--reset-system"]);
        assert!(o.reset && o.cmd.is_empty());
        let o = parse_ok(&["--reset-system", "app"]);
        assert!(o.reset);
        assert_eq!(o.cmd, ["app"]);
    }

    #[test]
    fn double_dash_ends_options() {
        let o = parse_ok(&["-x", "--", "-b"]);
        assert!(o.x11);
        assert_eq!(o.cmd, ["-b"]);
    }

    #[test]
    fn binds_resolve_and_keep_symlinks() {
        let d = scratch("cli-binds");
        let real = d.join("real");
        fs::create_dir(&real).unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let o = parse_ok(&[
            "-w",
            real.to_str().unwrap(),
            "-r",
            link.to_str().unwrap(),
            "app",
        ]);
        assert_eq!(o.binds.len(), 2);
        assert!(!o.binds[0].ro && o.binds[0].symlink_from.is_none());
        assert_eq!(o.binds[0].real, real.canonicalize().unwrap());
        assert!(o.binds[1].ro);
        assert_eq!(o.binds[1].real, real.canonicalize().unwrap());
        assert_eq!(o.binds[1].symlink_from.as_deref(), Some(link.as_path()));
        // -W binds and remembers the dir; -d wins over it and may not exist
        let o = parse_ok(&["-W", real.to_str().unwrap(), "app"]);
        assert_eq!(o.workdir, Some(real.canonicalize().unwrap()));
        let o = parse_ok(&["-d", &format!("{}/missing/../gone", d.display()), "app"]);
        assert_eq!(o.chdir, Some(d.canonicalize().unwrap().join("gone")));
        let _ = fs::remove_dir_all(&d);
    }
}
