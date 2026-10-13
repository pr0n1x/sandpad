//! The box: everything the app keeps, laid out like `/`. `<box>$HOME` is
//! bound as the sandbox home; `<box>/usr`, `/etc`, `/opt`, `/var` are the
//! writable upper layers of the system, once a `--root` run created them.

use crate::util::{
    die, exists, gid, has_mounts_under, has_xattr, is_mountpoint, realpath_m, run, set_xattr, uid,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The system dirs that get a layer.
const LAYERED: [&str; 4] = ["usr", "etc", "opt", "var"];

/// fuse-overlayfs' ownership record (xattr_permissions=2): "uid:gid:mode"
const OWNER_XATTR: &str = "user.containers.override_stat";

/// Compiled by build.rs from rootshim.c; see there for why --root needs it.
static ROOTSHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rootshim.so"));
/// Where it is bound inside: under /usr, a path AppArmor lets every program
/// load libraries from (some coreutils applets are confined separately).
pub const SHIM_INSIDE: &str = "/usr/lib/sandbox-rootshim.so";

pub struct BoxDir {
    path: PathBuf,
    /// `<box>$HOME`, bound as the sandbox $HOME
    pub home: PathBuf,
}

impl BoxDir {
    /// The box directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// -b: a bare NAME is a box under ~/sandboxes, anything with a slash is a
/// directory; -a: per-app box named after the binary's path with dashes for
/// slashes (/usr/bin/foo -> usr-bin-foo); default: the shared box
/// ~/sandboxes/default.
pub fn resolve(opt: Option<&str>, app_box: bool, app: Option<&Path>, home: &Path) -> BoxDir {
    let sandboxes = home.join("sandboxes");
    let path = match opt {
        Some(s) if s.contains('/') => realpath_m(Path::new(s)),
        Some(name) => sandboxes.join(name),
        None if app_box => {
            let app = app.unwrap_or_else(|| die(2, "-a needs an app to name the box after"));
            let slug = app
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-");
            sandboxes.join(slug)
        }
        None => sandboxes.join("default"),
    };
    let rel_home = home.strip_prefix("/").unwrap_or(home);
    let box_home = path.join(rel_home);
    fs::create_dir_all(&box_home)
        .unwrap_or_else(|e| die(1, format!("cannot create {}: {e}", box_home.display())));
    BoxDir {
        path,
        home: box_home,
    }
}

/// --reset-system: back to the host's system, home untouched. Not while the
/// layers are mounted (a sandbox is running on the box): the daemons would
/// keep serving from directories pulled away under them.
pub fn reset_system(b: &BoxDir) {
    if has_mounts_under(&b.path.join(".mnt")) {
        die(
            1,
            format!(
                "{} is in use (its layers are mounted); stop its sandboxes first",
                b.path.display()
            ),
        );
    }
    make_removable(&b.path.join(".work")); // overlay scratch dirs can be mode 000
    for d in LAYERED.iter().chain([".work", ".mnt"].iter()) {
        let p = b.path.join(d);
        if p.exists() {
            fs::remove_dir_all(&p)
                .unwrap_or_else(|e| die(1, format!("cannot remove {}: {e}", p.display())));
        }
    }
}

/// `chmod -R u+rwX`: give the owner access to everything under `dir`, so
/// it can be removed (the kernel overlay used to leave mode-000 work dirs).
fn make_removable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(meta.mode() | 0o700));
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            match fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() => make_removable(&p),
                Ok(m) if m.is_file() => {
                    let _ = fs::set_permissions(&p, fs::Permissions::from_mode(m.mode() | 0o600));
                }
                _ => {}
            }
        }
    }
}

/// A box has a system layer once a --root run created it.
pub fn has_layer(b: &BoxDir) -> bool {
    LAYERED.iter().any(|d| b.path.join(d).is_dir())
}

pub struct Layer {
    /// bwrap arguments binding the layered views (or the plain ro-binds)
    pub args: Vec<OsString>,
    /// the fuse-overlayfs mounts this process made; unmounted at exit
    pub mounts: Vec<PathBuf>,
}

/// The plain system: host dirs bound read-only, no /var.
pub fn plain() -> Layer {
    Layer {
        args: [
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/opt",
            "/opt",
            "--ro-bind",
            "/etc",
            "/etc",
        ]
        .iter()
        .map(OsString::from)
        .collect(),
        mounts: Vec::new(),
    }
}

/// Mount the box's system layer with fuse-overlayfs and return the binds.
///
/// The system dirs become fuse-overlayfs mounts — host dir as the read-only
/// lower layer, `<box>/DIR` as the writable upper, scratch space in
/// `<box>/.work` — so the app sees a writable system while the host never
/// changes. The mounts are made here on the host side, by this user through
/// the setuid fusermount3 like any sshfs, and bound into the sandbox: no
/// capability is needed inside, so Ubuntu's bwrap profile (which denies the
/// children all capabilities) stays as it is. `--root` adds squash_to_uid:
/// every file reports your uid, which the sandbox shows as root, so an
/// installer can edit, chown and replace whatever it can read (root-only
/// host files stay out of reach: nothing unprivileged reads them).
///
/// Ownership: inside, root and you are the same uid, so without bookkeeping
/// a file an installer creates would look like the app's own, writable. The
/// plain view is therefore mounted with xattr_permissions=2: fuse-overlayfs
/// records the owner of what the app creates or chowns in an xattr and
/// reports from it (and keeps the real file accessible to itself: it has no
/// capabilities, and a real mode-000 file would break lookups). The --root
/// view can't use that mode (the daemon would deny fake root wherever no
/// record says otherwise), so what --root runs install, like what is dropped
/// into the box from outside, carries no record — and before the plain view
/// is mounted everything unrecorded is labeled root's. Installed files are
/// thus read-only system files for the app; what the app creates is its.
///
/// A mount already present (another sandbox on this box, in the same mode:
/// the squashed and the plain view live under .mnt/root and .mnt/user) is
/// reused, not remounted; only mounts made here are unmounted at exit,
/// lazily, so a sandbox still using one keeps it alive.
pub fn mount_layer(b: &BoxDir, root: bool) -> Layer {
    if !exists("fuse-overlayfs") {
        die(
            1,
            "this box has a system layer, which needs fuse-overlayfs (apt install fuse-overlayfs)",
        );
    }
    // /var only exists in the sandbox with a layer: package managers need it.
    // dpkg's and apt's lock files are root-only on the host, so they can't be
    // copied up to be opened for writing; shadow them with empty box files
    for f in [
        "var/lib/dpkg/lock",
        "var/lib/dpkg/lock-frontend",
        "var/lib/dpkg/triggers/Lock",
        "var/lib/apt/lists/lock",
        "var/cache/apt/archives/lock",
    ] {
        let host = Path::new("/").join(f);
        let shadow = b.path.join(f);
        if host.exists() && fs::File::open(&host).is_err() && !shadow.exists() {
            if let Some(parent) = shadow.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::File::create(&shadow);
        }
    }
    let _ = fs::create_dir_all(b.path.join(".work"));
    if !is_mountpoint(&b.path.join(".mnt/user/var"))
        && !is_mountpoint(&b.path.join(".mnt/root/var"))
    {
        rebuild_dpkg_status(b);
    }
    if !root {
        mark_root(b);
    }
    let mode = if root { "root" } else { "user" };
    let mut layer = Layer {
        args: Vec::new(),
        mounts: Vec::new(),
    };
    for d in LAYERED {
        let dir = Path::new("/").join(d);
        if !dir.is_dir() {
            continue;
        }
        let mnt = b.path.join(".mnt").join(mode).join(d);
        let upper = b.path.join(d);
        let work = b.path.join(".work").join(d);
        for p in [&mnt, &upper, &work] {
            fs::create_dir_all(p)
                .unwrap_or_else(|e| die(1, format!("cannot create {}: {e}", p.display())));
        }
        if !is_mountpoint(&mnt) {
            let mut opts = format!(
                "lowerdir={},upperdir={},workdir={}",
                dir.display(),
                upper.display(),
                work.display()
            );
            if root {
                opts += &format!(",squash_to_uid={},squash_to_gid={}", uid(), gid());
            } else {
                opts += ",xattr_permissions=2";
            }
            if !run("fuse-overlayfs", &["-o", &opts, &mnt.to_string_lossy()]) {
                die(
                    1,
                    format!("fuse-overlayfs failed to mount {}", dir.display()),
                );
            }
            layer.mounts.push(mnt.clone());
        }
        layer
            .args
            .extend(["--bind".into(), mnt.into_os_string(), dir.into_os_string()]);
    }
    layer
}

/// Lazily unmount the layers this process mounted.
pub fn unmount(mounts: &[PathBuf]) {
    for m in mounts {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(m)
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// --root: the shim, written to the box's scratch dir and bound inside.
/// The mountpoint file is created here with a normal mode, so the empty
/// file it leaves in the box can be labeled like the rest.
///
/// Call before mount_layer: the file is written straight into the upper
/// layer, and a running fuse-overlayfs must not find it there behind its
/// back. If something walks the fresh mount first (an IDE's file watcher
/// scanning a new directory does), the daemon has /usr/lib cached as
/// lower-only; bwrap's create of the mountpoint then makes it copy the
/// directory up by renaming a scratch copy over `<box>/usr/lib`, which
/// fails with ENOTEMPTY because that directory exists and holds this file,
/// and bwrap dies with "Can't create file at /usr/lib/sandbox-rootshim.so".
pub fn shim_args(b: &BoxDir) -> Vec<OsString> {
    let so = b.path.join(".work/rootshim.so");
    let _ = fs::create_dir_all(b.path.join(".work"));
    if fs::read(&so).map(|cur| cur != ROOTSHIM).unwrap_or(true) {
        fs::write(&so, ROOTSHIM)
            .unwrap_or_else(|e| die(1, format!("cannot write {}: {e}", so.display())));
    }
    let inside_in_box = b.path.join(SHIM_INSIDE.trim_start_matches('/'));
    let _ = fs::create_dir_all(inside_in_box.parent().unwrap());
    if !inside_in_box.exists() {
        let _ = fs::File::create(&inside_in_box);
    }
    vec!["--ro-bind".into(), so.into_os_string(), SHIM_INSIDE.into()]
}

/// Before a plain mount: label everything unrecorded root's, real mode kept.
/// A stamp keeps the pass to what changed since the last one.
fn mark_root(b: &BoxDir) {
    let _ = fs::create_dir_all(b.path.join(".work"));
    let stamp = b.path.join(".work/marked");
    let since = fs::metadata(&stamp)
        .ok()
        .map(|m| (m.mtime(), m.mtime_nsec()));
    fn walk(dir: &Path, since: Option<(i64, i64)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(m) = fs::symlink_metadata(&p) else {
                continue;
            };
            if m.is_dir() {
                walk(&p, since);
            }
            if !(m.is_dir() || m.is_file()) {
                continue;
            }
            if let Some((s, sn)) = since {
                if (m.ctime(), m.ctime_nsec()) <= (s, sn) {
                    continue;
                }
            }
            if !has_xattr(&p, OWNER_XATTR) {
                let _ = set_xattr(&p, OWNER_XATTR, &format!("0:0:{:o}", m.mode()));
            }
        }
    }
    for d in LAYERED {
        let top = b.path.join(d);
        if let Ok(m) = fs::symlink_metadata(&top) {
            let fresh = since
                .map(|(s, sn)| (m.ctime(), m.ctime_nsec()) > (s, sn))
                .unwrap_or(true);
            if fresh && !has_xattr(&top, OWNER_XATTR) {
                let _ = set_xattr(&top, OWNER_XATTR, &format!("0:0:{:o}", m.mode()));
            }
        }
        walk(&top, since);
    }
    let _ = fs::File::create(&stamp);
}

/// dpkg's status is one file, so the first dpkg run in the box copies the
/// host's up and the copy then shadows it: packages the host installs or
/// upgrades later look missing inside. Rebuild it before mounting, when no
/// sandbox has the box mounted: the host's current status, with the
/// paragraphs of the packages the box installed itself (those have a file
/// list in the box's info/) replacing or appended.
fn rebuild_dpkg_status(b: &BoxDir) {
    let db = b.path.join("var/lib/dpkg");
    let Ok(box_status) = fs::read_to_string(db.join("status")) else {
        return;
    };
    let Ok(host_status) = fs::read_to_string("/var/lib/dpkg/status") else {
        return;
    };
    // the packages the box installed: pkg.list, or pkg:arch.list for non-native ones
    let box_pkgs: HashSet<String> = fs::read_dir(db.join("info"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|n| n.strip_suffix(".list").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let tmp = db.join("status.tmp");
    if fs::write(&tmp, merge_status(&host_status, &box_status, &box_pkgs)).is_ok() {
        let _ = fs::rename(&tmp, db.join("status"));
    }
}

/// The host's status with the box's paragraphs for `box_pkgs` replacing the
/// host's, or appended in the box's order when the host has none.
fn merge_status(host_status: &str, box_status: &str, box_pkgs: &HashSet<String>) -> String {
    // a paragraph's package as the two names its file list may have
    fn key(para: &str) -> (String, String) {
        let mut pkg = "";
        let mut arch = "";
        for line in para.lines() {
            if let Some(v) = line.strip_prefix("Package: ") {
                pkg = v
            }
            if let Some(v) = line.strip_prefix("Architecture: ") {
                arch = v
            }
        }
        (pkg.to_string(), format!("{pkg}:{arch}"))
    }
    let paragraphs = |s: &str| -> Vec<String> {
        s.split("\n\n")
            .map(str::trim_end)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut own: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new(); // keep box paragraphs in their own order when appended
    for p in paragraphs(box_status) {
        let (name, full) = key(&p);
        if box_pkgs.contains(&name) || box_pkgs.contains(&full) {
            if !own.contains_key(&full) {
                order.push(full.clone());
            }
            own.insert(full, p);
        }
    }
    let mut out = String::new();
    for p in paragraphs(host_status) {
        let (_, full) = key(&p);
        match own.remove(&full) {
            Some(ours) => {
                out += &ours;
            }
            None => {
                out += &p;
            }
        }
        out += "\n\n";
    }
    for full in order {
        if let Some(p) = own.remove(&full) {
            out += &p;
            out += "\n\n";
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn box_naming() {
        let d = scratch("box-naming");
        let home = d.join("home/user");
        let sb = home.join("sandboxes");
        assert_eq!(resolve(None, false, None, &home).path, sb.join("default"));
        assert_eq!(
            resolve(Some("mybox"), false, None, &home).path,
            sb.join("mybox")
        );
        let explicit = d.join("elsewhere/box");
        assert_eq!(
            resolve(Some(explicit.to_str().unwrap()), false, None, &home).path,
            explicit
        );
        let app = Path::new("/usr/bin/foo");
        assert_eq!(
            resolve(None, true, Some(app), &home).path,
            sb.join("usr-bin-foo")
        );
        // -b wins over -a; the sandbox home is <box>$HOME and gets created
        let b = resolve(Some("named"), true, Some(app), &home);
        assert_eq!(b.path, sb.join("named"));
        assert_eq!(
            b.home,
            sb.join("named")
                .join(home.strip_prefix("/").unwrap_or(&home))
        );
        assert!(b.home.is_dir());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn layer_detection_and_reset() {
        let d = scratch("box-reset");
        let b = resolve(
            Some(d.join("box").to_str().unwrap()),
            false,
            None,
            Path::new("/home/user"),
        );
        assert!(!has_layer(&b));
        fs::create_dir_all(b.path.join("etc/opt")).unwrap();
        fs::write(b.path.join("etc/opt/x"), "x").unwrap();
        fs::create_dir_all(b.path.join(".work/usr/work")).unwrap();
        fs::set_permissions(
            b.path.join(".work/usr/work"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        fs::write(b.home.join("keep"), "k").unwrap();
        assert!(has_layer(&b));
        reset_system(&b);
        assert!(!has_layer(&b));
        assert!(!b.path.join(".work").exists());
        assert_eq!(fs::read_to_string(b.home.join("keep")).unwrap(), "k");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shim_is_written_and_bound() {
        let d = scratch("box-shim");
        let b = resolve(
            Some(d.join("box").to_str().unwrap()),
            false,
            None,
            Path::new("/home/user"),
        );
        let args = shim_args(&b);
        assert_eq!(args[0], "--ro-bind");
        assert_eq!(args[2], SHIM_INSIDE);
        assert_eq!(
            fs::read(b.path.join(".work/rootshim.so")).unwrap(),
            ROOTSHIM
        );
        assert!(b.path.join("usr/lib/sandbox-rootshim.so").is_file()); // the mountpoint file
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn marking_labels_unrecorded_files_only() {
        let d = scratch("box-mark");
        let b = resolve(
            Some(d.join("box").to_str().unwrap()),
            false,
            None,
            Path::new("/home/user"),
        );
        fs::create_dir_all(b.path.join("usr/bin")).unwrap();
        let installed = b.path.join("usr/bin/tool");
        fs::write(&installed, "x").unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        let apps_own = b.path.join("usr/bin/mine");
        fs::write(&apps_own, "y").unwrap();
        set_xattr(&apps_own, OWNER_XATTR, "1000:1000:100644").unwrap();
        mark_root(&b);
        let rec = |p: &Path| {
            let mut buf = vec![0u8; 64];
            let n = unsafe {
                libc::lgetxattr(
                    std::ffi::CString::new(p.to_str().unwrap())
                        .unwrap()
                        .as_ptr(),
                    c"user.containers.override_stat".as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
        };
        assert_eq!(rec(&installed), "0:0:100755");
        assert_eq!(
            rec(&b.path.join("usr/bin")),
            format!(
                "0:0:{:o}",
                fs::metadata(b.path.join("usr/bin")).unwrap().mode()
            )
        );
        assert_eq!(rec(&apps_own), "1000:1000:100644"); // untouched
        assert!(b.path.join(".work/marked").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn status_merge() {
        let host = "Package: bash\nStatus: install ok installed\nArchitecture: amd64\n\n\
                    Package: libc6\nStatus: install ok installed\nArchitecture: i386\n\n\
                    Package: diag.plugin\nStatus: deinstall ok config-files\nArchitecture: amd64\n";
        let boxs = "Package: bash\nStatus: deinstall ok config-files\nArchitecture: amd64\n\n\
                    Package: diag.plugin\nStatus: install ok installed\nArchitecture: amd64\n\n\
                    Package: gosuslugi-plugin\nStatus: install ok installed\nArchitecture: amd64\n\n\
                    Package: libc6\nStatus: install ok installed\nArchitecture: i386\n";
        let box_pkgs: HashSet<String> = ["diag.plugin", "gosuslugi-plugin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let merged = merge_status(host, boxs, &box_pkgs);
        let paras: Vec<&str> = merged.split("\n\n").filter(|p| !p.is_empty()).collect();
        assert_eq!(paras.len(), 4);
        // host entries win for packages the box didn't install (bash's stale copy is dropped)
        assert!(paras[0].starts_with("Package: bash\nStatus: install ok installed"));
        assert!(paras[1].starts_with("Package: libc6"));
        // the box's record replaces the host's for a box-installed package
        assert!(paras[2].starts_with("Package: diag.plugin\nStatus: install ok installed"));
        // and is appended when the host has none
        assert!(paras[3].starts_with("Package: gosuslugi-plugin"));
        assert!(merged.ends_with("\n\n"));
        // foreign-arch file lists are named pkg:arch
        let only_i386: HashSet<String> = ["libc6:i386".to_string()].into();
        let m = merge_status(host, boxs, &only_i386);
        assert!(m.contains("Package: libc6\nStatus: install ok installed\nArchitecture: i386"));
    }
}
