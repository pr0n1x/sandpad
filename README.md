# sandpad

Run a GUI (or any) application inside strict boundaries using
[bubblewrap](https://github.com/containers/bubblewrap): separate namespaces,
no network, and a private home directory — without giving the app access to
the real `$HOME`. Useful for apps installed system-wide (e.g. from a `.deb`)
that you don't fully trust.

The project lives at <https://github.com/pr0n1x/sandpad>.

## Building and installing

```sh
git clone https://github.com/pr0n1x/sandpad.git && cd sandpad
cargo build --release          # needs cargo and a C compiler
ln -s "$PWD/target/release/sandpad" ~/bin/sandpad
```

The workspace builds three binaries, all into `target/release/`: `sandpad`
(`crates/sandpad/`), and for `-p root` (`crates/askroot/`) the
`sandpad-askroot-shim` that stands in for `sudo`/`pkexec`/`su` inside a
sandbox and the `sandpad-askroot-dialog` that asks you on the host. (Two
small C programs, `rootshim.c` and `ttyexec.c`, are compiled by `build.rs`
and embedded in `sandpad`.) `sandpad` looks for
the two helpers, in this order: next to its own file (which the symlink
above resolves to, so a build tree, a flat bin dir or `cargo install`
all work), then in `/usr/libexec/sandpad/` (or the dir given as
`SANDPAD_LIBEXEC=/some/libexec/sandpad` at build time — for a package that
puts `sandpad` alone in a bin dir) and `/usr/local/libexec/sandpad/`, then
on `PATH`. Nothing else needs them.

## What it does

`sandpad` launches the app with:

- **Own user / PID / IPC / UTS / network namespaces** (`--unshare-all`) —
  the network namespace contains only loopback, so by default the app has no
  network access and can't see host interfaces. `-n` grants outbound internet
  through [pasta](https://passt.top/) while keeping the separate netns.
- **A box per sandbox**: everything the app keeps lives in one directory,
  `~/sandpad/<name>/` — by default the shared box `~/sandpad/default`,
  or a per-app / named box via `-a` / `-b`. The box is laid out like the
  root filesystem:
  - `home/<user>` (the real home path under the box, e.g.
    `~/sandpad/default/home/user`) is bind-mounted as the app's `$HOME`;
    the real home directory is invisible.
  - `usr/`, `etc/`, `opt/`, `var/` hold the app's changes to the system,
    once a `--root` run created them (see below). (`.work/` is the overlay's scratch
    space, `.mnt/` where the layered views are mounted while a sandbox runs:
    `.mnt/user/` the plain view, `.mnt/root/` the `--root` one; `.mnt.lock`
    is how the sandboxes on a box count each other.)
- **Read-only system**: `/usr`, `/etc`, `/opt`, `/sys` are bound read-only;
  `/tmp` is a fresh tmpfs. A box that has a system layer (created by a
  `--root` run, see below) gets fuse-overlayfs layers over the first three
  and `/var` instead: installed software appears as part of the system,
  changes land in the box and the host stays untouched.
- **Wayland GUI, GPU and sound**: the Wayland socket, `/dev/dri` and
  PipeWire/PulseAudio sockets are passed through. The host's D-Bus session
  bus is deliberately not: the sandbox gets a session bus of its own, a
  `dbus-daemon` started inside it before the app (at `$XDG_RUNTIME_DIR/bus`
  as usual), with nothing on it but the apps of that sandbox. That is what
  lets a second run joining the first talk to it (see below): Firefox asks
  its running instance for a new window over the session bus.
  The webcam is not either, unless granted with `-p camera`, nor are USB
  and HID devices without `-p usb`.
- **System appearance**: the host's theme configs (`~/.config/kdeglobals`,
  GTK `settings.ini`, `qt5ct`/`qt6ct`, `~/.themes`, `~/.icons`, and the
  dconf database `~/.config/dconf/user`) are bound read-only into the box
  home, so Qt/GTK apps follow the host dark/light theme. The dconf db
  matters on Wayland: GTK takes the theme and the titlebar-button layout
  (KDE syncs both into GSettings) from there, overriding `settings.ini` —
  without it Firefox draws GNOME's `menu:close` buttons in Adwaita. Apps that only listen to the desktop portal (libadwaita, Electron)
  won't pick it up — there is no portal on the sandbox's bus.
- **No terminal control** by default (`--new-session`).
- **Snap binaries** (app path under `/snap`, e.g.
  `` sandbox "$(realpath /snap/firefox/current/usr/lib/firefox/firefox)" ``):
  the binary runs without snapd, with `/snap` bound read-only. For these
  `sandpad` passes bwrap `--disable-userns`, which forbids the app to create
  user namespaces, for this reason: Ubuntu strips every capability from a
  user namespace an unprivileged program creates, so such a namespace is
  useless; Firefox's snap build creates one anyway for each tab process,
  then fails inside it and every tab crashes. `--disable-userns` makes the
  creation fail instead, and Firefox handles that case: it prints
  `CanCreateUserNamespace() clone() failure` once at startup and runs its
  tabs without that layer, as it does under Flatpak. Under `-n` the bwrap
  option would get in pasta's way, so the same ban is applied with the
  sysctl `user.max_user_namespaces=0` once the network is up.

## Usage

```sh
sandpad [-i|--interactive] [-s|--separate] [-z|--zygote ID] [-w|--bind DIR]... [-W|--workdir DIR]... [-r|--ro-bind DIR]... [-d|--chdir DIR] [-b|--box NAME|DIR] [-a|--app-box] [-n|--net[IFACE]] [-6|--ipv6] [-x|--x11] [-p|--permissions LIST]... [--root] /usr/bin/someapp [args...]
sandpad [-b NAME|DIR] --reset-system
```

- `-i`, `--interactive` — drop `--new-session` so an interactive shell inside
  the sandbox gets job control (like `docker run -i`). Only safe when the
  kernel has `dev.tty.legacy_tiocsti = 0` (default on modern kernels), which
  blocks the TIOCSTI terminal-injection attack `--new-session` guards against.
  The app is started through a tiny launcher (`ttyexec.c`, embedded in
  `sandpad` and handed to bwrap from memory at
  `$XDG_RUNTIME_DIR/sandpad-tty-exec`), which makes it the terminal's
  foreground process group inside the sandbox's own pid namespace before
  becoming it (a group from outside is invisible in there, and a shell
  would fail to hand the terminal back at exit and lose its exit status);
  `sandpad` returns the terminal to its previous group when the app exits.
  Joined commands and approved `sudo`/`su` commands get the same handoff
  without the launcher, since `sandpad` forks those itself.
- `-s`, `--separate` — a sandbox of its own. By default a run joins a
  sandbox that is already running with the same box and the same options
  that shape it (see below); a `-s` run neither joins one nor is joined by
  later runs.
- `-z ID`, `--zygote ID` — the sandbox named ID: joined if it is running,
  started under that name otherwise. Later runs with the same ID join it
  whatever their other options: `sandpad -z ff -b work -n firefox`, then
  `sandpad -z ff sh` for a shell next to it.
- `-w DIR`, `--bind DIR` — bind DIR read-write at its real path inside the
  sandbox; repeatable. This is the way to hand the app a specific
  project/data directory while the rest of `$HOME` stays hidden.
  A symlink DIR is dereferenced: its target is bound at the target's path and
  the symlink itself is recreated inside the sandbox, so the path as given
  keeps working (same for `-W`/`-r`).
- `-W DIR`, `--workdir DIR` — like `-w`, and also start the app there (like
  `docker run -w`); if repeated, the app starts in the last one.
- `-r DIR`, `--ro-bind DIR` — like `-w`, but read-only; repeatable.
- `-d DIR`, `--chdir DIR` — start the app in DIR, overriding `-W`'s chdir.
- `-b NAME|DIR`, `--box NAME|DIR` — the box to use (created if missing): a
  bare NAME means `~/sandpad/NAME`, anything containing a slash is taken
  as a directory. Overrides `-a`.
- `-a`, `--app-box` — use a per-app box named after the binary's path, with
  dashes for slashes (`/usr/bin/foo` → `~/sandpad/usr-bin-foo`), instead
  of the default shared box `~/sandpad/default` that all apps see together.
- `-n[IFACE]`, `--net[=IFACE]` — outbound internet access, still in a separate network
  namespace: bwrap creates the namespaces as usual, then `pasta` (rootless
  user-mode NAT, as used by Podman) *attaches* to the sandbox netns and relays
  TCP/UDP through unprivileged host sockets; the app is held on bwrap's
  `--block-fd` until the network is configured. Because pasta only joins an
  existing namespace instead of creating one, it needs no AppArmor userns
  profile of its own. DNS goes through pasta's `--dns-forward` to the host's
  real resolver, so systemd-resolved and VPN/split-DNS setups keep working.
  With `-n` the sandbox userns maps the real uid to root — pasta's
  self-hardening only lets it gain the needed capabilities there when its uid
  maps to root. The app itself still runs under the real uid: it is nested
  into a second userns mapping root back, so files keep their normal
  ownership (without this everything the user owns shows as `root:root`).
  Ubuntu's userns restriction strips the sandboxed app of the capabilities
  needed to write its own uid map, so sandpad writes the map from outside,
  from the parent (sandpad) userns — joining one isn't gated, only creating.
  Exception: snap apps keep the fake root, since their nested userns is
  deliberately forbidden (see above).
  Requires the `passt` package. TCP/UDP (curl, browsers) work out
  of the box; `ping` replies additionally need unprivileged ping sockets
  enabled on the host — see the ICMP note in Notes below.
- `-6`, `--ipv6` — with `-n`, also enable IPv6 in the sandbox network; the
  default is IPv4-only (pasta `-4`). On hosts with IPv6 disabled, pasta then
  prints `No routable interface for IPv6` and falls back to IPv4.
- `-x`, `--x11` — pass the X11 socket for `$DISPLAY` (and the `$XAUTHORITY`
  cookie) through, for X11-only apps such as Qt builds without the Wayland
  plugin. Weakens isolation: X11 clients can snoop each other's windows and
  input.
  The optional IFACE (must be attached: `-nenp39s0` or `--net=enp39s0`)
  mirrors that interface inside the sandbox (pasta `-i`) and pins pasta's
  host sockets to it (`--outbound-if4`, via `SO_BINDTODEVICE`). Bound sockets
  bypass policy routing, so sandbox traffic goes straight out of IFACE even
  when a WireGuard tunnel with `AllowedIPs 0.0.0.0/0` owns the host's default
  route — the sandbox gets the physical uplink while the host stays on the
  VPN. Caveat: DNS is forwarded to the host resolver (systemd-resolved),
  whose own upstream queries still follow host routing.
- `-p LIST`, `--permissions LIST` — grant the app extra access to host
  resources that are withheld by default; comma-separated, repeatable
  (`-p camera` or `-p camera,foo`). Unknown names are rejected. Known
  permissions:
  - `camera` — the webcam(s): every `/dev/video*` (V4L2) and `/dev/media*`
    node is bound into the sandbox's otherwise minimal `/dev`. Without the
    D-Bus session bus there is no camera portal, so apps must use V4L2
    directly (Firefox and Chromium do).
  - `usb` — USB and HID devices (security keys, microcontroller boards,
    phones, anything an app talks to directly). Two things in the sandbox's
    design get in the way of such devices, and this permission undoes both:
    - The sandbox's `/dev` is a fresh minimal one, with nodes bound into it
      one by one. A bind mount attaches to the node that exists at that
      moment, not to its name, so a node the kernel creates later is simply
      not there inside — and a device re-enumerates whenever it reboots: a
      firmware update takes it through its bootloader, a mode switch gives it
      a new identity, each time with new `/dev/bus/usb/...` and
      `/dev/hidraw*` nodes. The app would lose the device halfway. So the
      host's whole `/dev` is bound in its place, live (`/dev/shm` stays
      private; `/run/udev` is bound read-only for libudev's property
      database). The cost: the app sees every device node your user can
      open on the host — sound cards, cameras, host ptys — so grant this only
      to apps that need a device. Access is still governed by the host's
      udev rules.
    - The sandbox has its own network namespace, and hot-plug events are
      netlink multicasts: udevd sends them in the host's namespace, and a
      netlink socket only receives from its own. libudev users (Chromium's
      WebHID and WebUSB, hidapi) enumerate once at start and learn of every
      later change from those events, so a device that appears or comes back
      after start would stay invisible to the app even though its node is
      now there. So `sandpad` keeps a helper that subscribes to udevd's
      events on the host, joins the sandbox's user and network namespaces
      (as their owner it holds every capability there, the way pasta does)
      and re-sends each event inside. libudev accepts them because the
      sender's uid is the app's own.
  - `x11` — same as `-x`.
  - `root` — `sudo`, `pkexec` and `su` inside the sandbox ask *you*, on
    the host, whether the command may run as root in the sandbox. Vendor
    tools do their privileged part through `sudo -A ...`, `pkexec ...` or
    `su -l root` (a browser's native-messaging helper may try all three
    in turn, installers the first), and all are dead ends in a sandbox: a
    setuid bit means
    nothing under `no_new_privs` in a user namespace, and pkexec needs
    polkit on a system bus the sandbox doesn't have. With this permission
    `sandpad-askroot-shim` is bound over `/usr/bin/sudo`,
    `/usr/bin/pkexec` and `/usr/bin/su` (at the path each name resolves
    to on the host: `/usr/bin/sudo` may be an alternatives symlink). The
    shim parses the command line as the program it stands in for would
    and sends the command, its working directory, its environment and its
    own stdin/stdout/stderr over a unix socket the `sandpad` process
    bind-mounted into the sandbox's `$XDG_RUNTIME_DIR`. For each request
    the `sandpad` process runs `sandpad-askroot-dialog` — a small window
    naming the app, the box, the command and the directory, with two
    buttons: *Approve sandboxed root access* and *Deny* (closing the
    window denies). *Deny* starts focused; arrow keys or Tab switch the
    highlighted button, Enter or Space activates it, and Escape denies.
    On approval it enters the sandbox the way a joining
    run does (one `setns` on the sandbox's pidfd, see below) and runs the
    command there as uid 0 on the shim's descriptors, with sudo's
    `secure_path` and environment filtering (`-E` keeps it all), the
    `SUDO_*` variables, no capabilities, and `rootshim.so` preloaded as
    `--root` runs have it; the shim exits with the command's status. A
    denial fails like sudo does without a password. `sudo -v` and
    `sudo -l` ask and run nothing; `-A`/`SUDO_ASKPASS`, `-n`, `-S`,
    `-u root` are accepted, any other user refused. `sudo -i` / `--login`
    starts root's configured login shell in its home (or `/` if that home
    is absent); `sudo -s` / `--shell` starts the caller's shell. With a
    command, both shell modes run it through `-c`. For `su` the command
    is the shell (`/bin/sh`, or `-s`): with `-c` running the command,
    without it reading stdin — which is what `su -l root <<EOF` feeds it;
    no password is read, since su takes it from the terminal, not stdin.
    "Root" is the box's own system layer, never the host. Like `-n`, the
    permission maps your uid to root in the sandbox's user namespace and
    un-roots the app in a nested one, so the app runs as you and the
    approved commands as root; and it mounts the box's layer in the
    `--root` view (`squash_to_uid`, see `--root` below) for the whole
    sandbox, since the plain view can't be written even by uid 0. That
    makes the permission sticky in effect: files the approved commands
    create carry no ownership record, so a later run of the box without
    `-p root` shows them as root's, read-only — the same rule as for
    anything a `--root` run installs. The dialog keeps you in the loop; it
    is not a boundary between the app and root, since the FUSE daemon
    sees both as the same host uid.
- **The system layer** (no option: a box has one once `usr/`, `etc/`,
  `opt/` or `var/` exist in it, which a `--root` run creates; every start
  of that box then uses it, and `--reset-system` removes it) —
  `/usr`, `/etc`, `/opt` and `/var` (which is otherwise absent) are mounted
  with [fuse-overlayfs](https://github.com/containers/fuse-overlayfs), the
  host dirs as the read-only lower layer and the box's `usr/`, `etc/`,
  `opt/`, `var/` as the writable upper layer. Changes made to the system
  land in the box and never reach the host; the box holds exactly the diff.
  The mounts are made on the host
  side, by you, through the setuid `fusermount3` like any sshfs — under
  `<box>/.mnt/` — and bound into the sandbox, so nothing inside needs a
  capability and Ubuntu's stock bwrap profile stays untouched. A sandbox
  started while another one already has the box's layers mounted in the
  same mode (plain or `--root`) reuses them; the last sandbox to leave the
  box unmounts them, whoever mounted them (every sandbox on a layered box
  holds a lock on `<box>/.mnt.lock` while it runs, and the one that finds
  itself alone on the way out takes the layers down; mounting and taking
  down are serialized through the same lock, so a sandbox starting as
  another one exits sees the box either fully mounted or fully unmounted,
  never half-way). A `--root` sandbox next to a
  running plain one gets its own mounts over the same box: fine for an
  install, but the running app sees the result only after a restart.
  Without `--root` the app sees the system as on the host: root's files —
  the host's and those installed into the box under `--root` — are
  read-only; the app can create files where a normal user could (its home,
  world-writable dirs) and keeps full access to what it created itself.
  Inside the sandbox root and you are the same uid, so this needs
  bookkeeping. Both views are mounted with fuse-overlayfs'
  `xattr_permissions`: the owner and mode an app sets are recorded in an
  xattr and reported from it, while the real file stays readable to the
  daemon (which has no capabilities; a real mode-000 file, as dpkg creates
  its temp files, would otherwise break every lookup in its directory). The
  record is `user.containers.override_stat`. Only the plain view records
  (the `--root` view can't use that mode: it would make the daemon deny
  fake root wherever no record says otherwise), so what `--root` runs
  install, like what is dropped into the box from outside, carries no
  record — and before a plain view is mounted, sandpad labels
  everything unrecorded root's (`0:0`, real mode kept). What the app
  creates gets a record of its own and stays the app's. A `--root` run
  concurrent with a plain one is seen by the running app as the app's own
  files until the app's next start. Needs the `fuse-overlayfs` package.
- `--root` — run the app as root inside the sandbox: uid 0 there is your
  own uid outside, so it grants no host privileges, and files it creates
  are yours on disk. For installers and other `id -u`-checking tools.
  Creates the box's system layer if it has none, mounted with
  `squash_to_uid`: every
  system file reports your uid, which the sandbox shows as root, so an
  installer can edit, replace, hard-link and chown-to-root anything it can
  read — `dpkg -i`, `apt-get install ./pkg.deb`, vendor `install.sh`
  scripts work, maintainer scripts included, and `/var/lib/dpkg` is the
  host's database with the box's changes layered on top — its `status`
  file is rebuilt at every start from the host's current one plus the
  packages the box installed, so host installs and upgrades show up inside
  (a copied-up `status` would otherwise shadow the host's forever). The one hard
  limit: host files only root can read (`/etc/shadow`, other users' data)
  stay unreadable and so can't be copied up, replaced or even deleted
  (fuse-overlayfs copies a file up before whiting it out) — nothing
  unprivileged can change that; purge such leftovers on the host. dpkg's
  and apt's lock files are root-only on the host, so the box gets empty
  shadows of them. Anything a package installs shadows the host's version
  from then on, host upgrades included (e.g. `ld.so.cache` after a postinst
  runs `ldconfig`).
  `--root` runs preload `rootshim.so` (compiled from `rootshim.c` at build
  time and embedded in the binary; handed to bwrap from memory and bound
  inside at `$XDG_RUNTIME_DIR/sandpad-rootshim.so`, so the box holds no
  copy of it), which papers over two consequences of "root" being a bare uid
  without capabilities: a directory created with
  mode 000 — dpkg does that for every directory, chmodding later — can't be
  opened by the FUSE daemon to finish the mkdir, so the shim keeps the
  owner's bits in every mode an installer sets (`rwx` on directories, `rw`
  on files); and `chown` to any uid but your own fails with EINVAL, since
  the sandbox maps a single uid — tar, `dpkg-deb`, `cp -a` and `install`
  all restore ownership when run as root — so the shim reports success
  and the file stays yours, which the plain view shows as root's anyway.
  Statically linked programs and ones AppArmor confines separately (Ubuntu
  ships a profile for `who`, which prints a harmless "cannot be preloaded"
  when a postinst calls it) run without it. No short option on purpose.
  Under `-n` the app is normally un-rooted after pasta's root mapping;
  `--root` keeps the root.
- `--reset-system` — reset the box's system to the host's, keeping its
  home: removes the system layer, i.e. `usr/`, `etc/`, `opt/`, `var/` and
  the overlay's `.work/` and `.mnt/`. The box then sees the host's system
  again, until the next `--root` run. Refused while a sandbox runs on the
  box; layers a sandbox left mounted by dying without cleaning up are taken
  down on the way. Takes no app: `sandpad -b mybox --reset-system`.

## Joining a running sandbox

Every run makes a sandbox of its own, so two of them can't see each other:
a second `sandpad -a firefox` is a second Firefox that can't find the
first, and there is no way to open a shell next to a running app for a
look inside. So by default a run whose box and options match a sandbox
already running joins it instead, like `docker exec`: the command runs in
that sandbox's namespaces — its processes, `/tmp`, IPC, hostname and
network — as the same uid, with the same environment, working directory
(`-d`/`-W`) and session (`-i`) rules a sandbox of its own would give it,
and is killed when the sandbox ends. `-s` starts a separate one as before.

What has to match is every option that decides what the app would see
inside: the box, the `-w`/`-W`/`-r` binds, `-n` and its interface, `-6`,
`-x`, `-p`, `--root`. The command, `-d`, `-i` and the environment don't
count. What a sandbox has because of its app — `/snap` for a snap, an app
outside every bind bound in by itself (see below) — is recorded with it,
and a command joins if the sandbox has what it needs: a shell joins a
snap's sandbox, a snap doesn't join a shell's. `-z ID` replaces all of
that with the name. A starting sandbox records its process under a hash of
the options (or the name) in `$XDG_RUNTIME_DIR/sandpad/`;
a later run follows it down to bwrap's init
and the app, and enters the app's namespaces with one `setns()` on its
pidfd — the way `nsenter` does, and the only way that works on Ubuntu,
whose AppArmor treats a process that has already entered the user
namespace as unprivileged and refuses it the rest. The joined command then
drops every capability the way bwrap does, keeping the binary's file
capabilities as ambient ones, and sets `no_new_privs`. One difference to a
sandbox of its own: bwrap's children run under Ubuntu's `bwrap` AppArmor
profile, a joined command runs under yours. A recorded sandbox that has
exited is recognised by its process start time and ignored; one still
starting is waited for, 5 s at most.

The app name is resolved with `which`, so `sandpad ping` and
`sandpad /usr/bin/ping` run the same binary and (with `-a`) use the same
box. An app that lies outside everything the sandbox shows — not under the
system dirs, `/snap` or a `-w`/`-W`/`-r` bind, typically a binary built
under the real home, which the box home hides — is bound into the sandbox
by itself, read-only, at its own path, and leaves no trace in the box. Only
that one file: an app that needs the files next to it gets its directory
with `-r`.

## Requirements

- `bubblewrap` (`apt install bubblewrap`)
- `passt` (`apt install passt`) — only for `-n`/`--net`. No AppArmor setup
  needed: pasta only attaches to the netns bwrap already created, and joining
  an existing namespace isn't gated by the Ubuntu userns restriction.
- `fuse-overlayfs` (`apt install fuse-overlayfs`) — only for boxes with a
  system layer, i.e. `--root` and everything after it.
- to build: `cargo` and a C compiler (for `rootshim.c` and `ttyexec.c`).
- On Ubuntu 24.04+ unprivileged user namespaces are restricted by AppArmor.
  The `apparmor` package ships `/etc/apparmor.d/bwrap-userns-restrict`,
  which lets bwrap create them and confines everything it starts to a
  profile with no capabilities at all. The sandbox is designed to live with
  that: nothing inside ever needs a capability (the layer mounts are made
  on the host side). Don't add an unconfined `bwrap` profile of your own —
  a profile of the same name is loaded alphabetically after yours anyway,
  so it would silently lose.

## Notes

- X11-only apps (symptom: `could not connect to display`, `Available
  platform plugins are: xcb`) need `-x`.
- Electron/Chromium apps may need their own `--no-sandbox` flag; the outer
  sandbox is still provided by bwrap.
- The app binary's file capabilities (e.g. ping's `cap_net_raw=ep`), which
  `no_new_privs` would otherwise drop at exec, are mirrored into the sandbox
  as ambient capabilities — they apply only inside the sandbox's own
  namespaces. Note that without `-n`'s root mapping, Ubuntu's userns
  hardening still blocks some uses of them (e.g. raw sockets).
- A persistent writable `/usr` (a box with a system layer) means a compromised app can plant
  binaries that run on its next start: the layer is part of the sandbox's
  trust domain, like the home. Files under `<box>/usr` etc. carry
  `user.containers.override_stat` xattrs (recorded owner and mode, see
  the system layer) and possibly fuse-overlayfs whiteout markers; `ls -l` shows them as
  a `+`. Edit the box from outside only while no sandbox has it mounted;
  files you drop in become root's in the plain view at the next start.
- `ping` under `-n` sends packets (via the mirrored `CAP_NET_RAW`), but
  replies only come back if the host allows unprivileged ping sockets, which
  pasta uses to relay ICMP:

  ```sh
  echo 'net.ipv4.ping_group_range = 0 2147483647' | sudo tee /etc/sysctl.d/99-sandboxed-ping.conf
  sudo sysctl --system
  ```

  Without that (or without `-n` at all) `ping` fails — test connectivity with
  `curl` instead.

## Launcher icons

`sandpad-icon.sh` badges an app icon with a mark, `#` by default, or a
flag, so a sandboxed launcher is distinguishable from the real one — and,
with a badge per box, launchers of the same app in different boxes from each
other (there is no way to overlay an icon from a `.desktop` file itself — the
`Icon=` key only takes a name or a path):

```sh
sandpad-icon.sh firefox                        # icon name from the theme
sandpad-icon.sh -o max-sandboxed /usr/share/pixmaps/max.png
sandpad-icon.sh -o google-chrome-work -m W google-chrome   # a "W" badge
sandpad-icon.sh -o google-chrome-rfgov -f ru google-chrome # a flag badge
```

It composites the badge onto the icon (PNG or SVG, name or path), installs
the result at the usual sizes into `~/.local/share/icons/hicolor/` as
`<icon>-sandboxed` (or the `-o` name) and prints that name for the launcher's
`Icon=` line. `-m MARK` sets the badge text: a character or two, longer text
is shrunk to fit. `-f FLAG` fills the badge with a flag instead: `ru` (the
Russian tricolour) and `su` (the Soviet hammer and sickle) are built in,
anything else is a path to a PNG/SVG of your own, scaled to cover the badge
square and cropped to its centre. Needs ImageMagick 7.
