//! Un-rooting the app under -n: a handshake in two halves, kept together
//! here because neither makes sense without the other.
//!
//! Why: with -n, bwrap maps your uid to 0 inside the sandbox, because pasta
//! can only configure the sandbox's network if its uid is root there. That
//! would leave the app running as root, with every file you own shown as
//! root:root. So the app alone gets a second, nested user namespace that
//! maps 0 back to your uid: real 1000 -> sandbox 0 -> nested 1000, and the
//! app sees itself and its files as 1000 again (podman unshare, inverted).
//!
//! Why in two halves: a process may write its own uid_map only with
//! CAP_SETUID in the parent namespace, and Ubuntu's AppArmor profile for
//! bwrap leaves the sandbox's children no capabilities at all. Creating a
//! user namespace isn't gated, only populating it. So the app's wrapper
//! (`wrapper()`) creates the nested namespace and waits, and we write the
//! map from outside (`map()`), from a process that joined the sandbox
//! namespace — the nested one's parent, which we created and therefore own.
//!
//! The namespaces, and what your uid looks like in each:
//!
//! | namespace | created by                           | your uid |
//! |-----------|--------------------------------------|----------|
//! | host      | the system                           | 1000     |
//! | sandbox   | bwrap --uid 0: map `0 1000 1`        | 0        |
//! | nested    | the wrapper's unshare -U: `1000 0 1` | 1000     |
//!
//! A map line is `inside outside length`, with "outside" meaning the
//! parent namespace. Both lines point at the same kernel uid; the second
//! undoes the first for the app.
//!
//! Snaps keep the fake root: they run with --disable-userns, so the nested
//! namespace can't be created. --root keeps it on purpose.

use crate::util::{die, gid, uid, warn};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// The sandbox's user namespace identity, as `/proc/<pid>/ns/user` reads
/// (`user:[4026532345]`), unique per namespace.
pub fn namespace_of(pid: i32) -> Option<std::path::PathBuf> {
    fs::read_link(format!("/proc/{pid}/ns/user")).ok()
}

/// The command prefix the app is started with: `unshare -U` creates the
/// nested namespace, then the shell waits until its map is written. With an
/// empty map the kernel reports every uid as the overflow value 65534, so
/// "my uid is no longer 65534" means "someone wrote the map"; then it
/// becomes the app. Gives up after 5 s.
pub fn wrapper() -> Vec<String> {
    [
        "unshare",
        "-U",
        "sh",
        "-c",
        "n=0; while [ \"$(id -u)\" = 65534 ]; do \
           [ \"$((n+=1))\" -lt 100 ] || { echo 'sandbox: no uid map after 5s' >&2; exit 1; }; \
           sleep 0.05; done; exec \"$0\" \"$@\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The other half: find the wrapper, wait for its nested namespace to
/// appear, write its map. `init_pid` is bwrap's init in the sandbox
/// namespace, the wrapper is its child; `sandbox_ns` the sandbox
/// namespace's identity taken before the app was released.
///
/// The three writes, in this order because the kernel insists: `setgroups`
/// to `deny` (a gid map from a writer without CAP_SETGID is accepted only
/// then), then one line each into `gid_map` and `uid_map`: "inside id
/// 1000 is outside id 0", outside being the sandbox namespace. They are
/// done through `nsenter -U -t <init>`: only a process in the parent
/// namespace may write a child's map, and from the host we are two levels
/// up; joining a namespace we own grants the capabilities needed there.
/// `--preserve-credentials` keeps our uid, which the line maps.
///
/// Returns whether the map was written. If not, the app runs as nobody.
fn map(init_pid: i32, sandbox_ns: Option<&Path>) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if !Path::new(&format!("/proc/{init_pid}")).exists() {
            return false; // app already gone
        }
        // the wrapper is init's (only) child; its ns/user link changes once
        // it has unshared
        let app_pid = fs::read_to_string(format!("/proc/{init_pid}/task/{init_pid}/children"))
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string));
        if let Some(app_pid) = app_pid {
            let ns = fs::read_link(format!("/proc/{app_pid}/ns/user")).ok();
            if ns.is_some() && ns.as_deref() != sandbox_ns {
                let script = format!(
                    "echo deny > /proc/{app_pid}/setgroups && \
                     echo '{} 0 1' > /proc/{app_pid}/gid_map && \
                     echo '{} 0 1' > /proc/{app_pid}/uid_map",
                    gid(),
                    uid()
                );
                return Command::new("nsenter")
                    .args([
                        "--preserve-credentials",
                        "-U",
                        "-t",
                        &init_pid.to_string(),
                        "sh",
                        "-c",
                        &script,
                    ])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or_else(|e| die(1, format!("cannot run nsenter: {e}")));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// `map()`, with the warning on failure.
pub fn map_or_warn(init_pid: i32, sandbox_ns: Option<&Path>) {
    if !map(init_pid, sandbox_ns) {
        warn("setting the app's uid map failed; it runs as nobody");
    }
}
