//! `sandbox-tty-exec COMMAND [ARGS...]`: take the terminal's foreground,
//! then become the command. Bound into a `sandbox -i` sandbox and put in
//! front of the app, since bwrap execs the app itself with no hook before
//! it (see the lib).

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((app, args)) = args.split_first() else {
        eprintln!("usage: sandbox-tty-exec COMMAND [ARGS...]");
        std::process::exit(2);
    };
    if let Err(e) = sandbox_tty_exec::foreground() {
        eprintln!("sandbox-tty-exec: taking the terminal foreground group: {e}");
        std::process::exit(1);
    }
    let e = Command::new(app).args(args).exec();
    eprintln!("sandbox-tty-exec: {app}: {e}");
    std::process::exit(127);
}
