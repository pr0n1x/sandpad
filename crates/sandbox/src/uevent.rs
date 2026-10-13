//! -p usb: forward udev events into the sandbox. Hot-plug events are
//! netlink multicasts in the host's network namespace; a sandbox netns gets
//! none, so libudev users (Chromium's WebHID and WebUSB, hidapi) never learn
//! of a device that appears or re-enumerates after the app started — a
//! firmware update reboots the device into its bootloader, as a new USB
//! device, and the app loses it halfway through. A forked helper subscribes
//! to udevd's "udev" group on the host, joins the sandbox userns (as the
//! sandbox's owner it holds every capability there, like pasta) and netns,
//! and re-sends each message, bytes unchanged, to the same group inside.
//! libudev trusts a sender whose uid is root's or the receiver's own, and
//! ours is the app's own from inside (plain and un-rooted -n) or root (the
//! fake-root modes).

use crate::util::warn;
use std::fs;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

/// udevd's multicast group: events after rule processing, with the libudev
/// header that libudev monitors expect ("kernel" is group 1).
const UDEV_GROUP: u32 = 2;

/// Start the forwarder for the sandbox whose init is `child_pid`; returns
/// the helper's pid, to be killed when the sandbox exits (it also dies with
/// us). None, with a warning, when the host side can't be set up.
pub fn forward(child_pid: i32) -> Option<i32> {
    // the host subscription first: receiving needs no privilege, and it has
    // to exist before we leave the host netns
    let host = match uevent_socket(UDEV_GROUP) {
        Ok(s) => s,
        Err(e) => {
            warn(format!("-p usb: no udev event forwarding: {e}"));
            return None;
        }
    };
    let user = fs::File::open(format!("/proc/{child_pid}/ns/user"));
    let net = fs::File::open(format!("/proc/{child_pid}/ns/net"));
    let (user, net) = match (user, net) {
        (Ok(u), Ok(n)) => (u, n),
        (Err(e), _) | (_, Err(e)) => {
            warn(format!("-p usb: no udev event forwarding: {e}"));
            return None;
        }
    };
    // SAFETY: the child only calls async-signal-safe functions and our own
    // code on its private copies, then _exit()s; the parent returns
    match unsafe { libc::fork() } {
        -1 => {
            warn(format!(
                "-p usb: no udev event forwarding: fork: {}",
                io::Error::last_os_error()
            ));
            None
        }
        0 => {
            // SAFETY: plain syscalls
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::_exit(relay(host, user, net));
            }
        }
        pid => Some(pid),
    }
}

/// The helper: join the sandbox's namespaces and copy events until the host
/// side closes. Returns the process exit code.
fn relay(host: OwnedFd, user: fs::File, net: fs::File) -> i32 {
    // joining a userns needs a single-threaded process: this fresh child is
    // one. Order matters: setns(net) wants CAP_SYS_ADMIN in the current
    // userns too, which we only have once inside the sandbox's
    for (ns, flag) in [(&user, libc::CLONE_NEWUSER), (&net, libc::CLONE_NEWNET)] {
        // SAFETY: a valid fd and flag
        if unsafe { libc::setns(ns.as_raw_fd(), flag) } < 0 {
            warn(format!(
                "-p usb: udev event forwarding stopped: setns: {}",
                io::Error::last_os_error()
            ));
            return 1;
        }
    }
    let inner = match uevent_socket(0) {
        Ok(s) => s,
        Err(e) => {
            warn(format!("-p usb: udev event forwarding stopped: {e}"));
            return 1;
        }
    };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: a valid socket and buffer
        let n = unsafe { libc::recv(host.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return 0;
        }
        if n == 0 {
            return 0;
        }
        // nobody listening inside is not an error
        let _ = send_group(&inner, &buf[..n as usize], UDEV_GROUP);
    }
}

/// A NETLINK_KOBJECT_UEVENT socket, subscribed to `group` (0: none).
fn uevent_socket(group: u32) -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh fd we own
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = nl_addr(group);
    // SAFETY: a valid socket and address
    if unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// Multicast `msg` to `group` (needs CAP_NET_ADMIN in the netns's userns).
fn send_group(sock: &OwnedFd, msg: &[u8], group: u32) -> io::Result<()> {
    let addr = nl_addr(group);
    // SAFETY: a valid socket, buffer and address
    let n = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            msg.as_ptr().cast(),
            msg.len(),
            0,
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn nl_addr(groups: u32) -> libc::sockaddr_nl {
    // SAFETY: all-zero is a valid sockaddr_nl
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = groups;
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_subscription_needs_no_privilege() {
        // NETLINK_KOBJECT_UEVENT allows unprivileged receivers, so the host
        // side of the relay works for any user
        let s = uevent_socket(UDEV_GROUP).expect("subscribes to udev events");
        assert!(s.as_raw_fd() >= 0);
    }

    #[test]
    fn multicast_send_is_refused_without_net_admin() {
        // ...while sending to a group is root's: the relay only works after
        // joining a userns the user owns
        let s = uevent_socket(0).unwrap();
        let e = send_group(&s, b"libudev\0", UDEV_GROUP).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
    }
}
