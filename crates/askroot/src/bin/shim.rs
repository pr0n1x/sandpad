//! The shim: what runs as `/usr/bin/sudo`, `/usr/bin/pkexec` and
//! `/usr/bin/su` inside a `sandbox -p root` sandbox. Parses the command line as the program it
//! stands in for would, sends the request to the host over the socket in
//! `$XDG_RUNTIME_DIR`, and exits with the command's status — or fails the
//! way sudo does without a password.

use std::path::{Path, PathBuf};
use std::process::exit;

use sandbox_askroot::{parse_pkexec, parse_su, parse_sudo, request, Reply, Request, SOCKET_NAME};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let name = Path::new(&argv[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (name, parsed) = match name.as_str() {
        "pkexec" => ("pkexec", parse_pkexec(&argv[1..])),
        "su" => ("su", parse_su(&argv[1..])),
        _ => ("sudo", parse_sudo(&argv[1..])),
    };
    let inv = parsed.unwrap_or_else(|e| {
        eprintln!("{e}");
        exit(1)
    });
    if let Some(u) = &inv.user {
        if u != "root" && u != "0" && u != "#0" {
            eprintln!("{name}: only root is available inside the sandbox, not {u}");
            exit(1);
        }
    }
    if inv.argv.is_empty() && !inv.validate && !inv.list {
        eprintln!("usage: {name} [options] command [args...]");
        exit(1);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| {
        eprintln!("{name}: XDG_RUNTIME_DIR is not set");
        exit(1)
    });
    // a login shell starts in root's home, as su -l would have it; the
    // sandbox may not have one, then / like a shell with no home
    let login = name == "su" && inv.argv.get(1).map(String::as_str) == Some("-l");
    let cwd = if login {
        if Path::new("/root").is_dir() {
            "/root"
        } else {
            "/"
        }
        .to_string()
    } else {
        std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into())
    };
    let req = Request {
        argv: inv.argv,
        cwd,
        env: std::env::vars().collect(),
        preserve_env: inv.preserve_env,
    };
    match request(&PathBuf::from(runtime).join(SOCKET_NAME), &req) {
        Ok(Reply::Exited(status)) => {
            if inv.list {
                println!(
                    "User {} may run any command as root inside this sandbox",
                    req.user()
                );
            }
            exit(status)
        }
        Ok(Reply::Denied) => {
            eprintln!("{name}: root access denied by the sandbox owner");
            exit(1)
        }
        Ok(Reply::Error(e)) => {
            eprintln!("{name}: {e}");
            exit(1)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{name}: no root service in this sandbox (it needs `sandbox -p root`)");
            exit(1)
        }
        Err(e) => {
            eprintln!("{name}: cannot reach the sandbox's root service: {e}");
            exit(1)
        }
    }
}
