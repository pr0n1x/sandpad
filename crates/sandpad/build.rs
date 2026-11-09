// rootshim.so and ttyexec are compiled here and embedded into the binary
// (see src/boxdir.rs and src/tty.rs), so a single file is all that has to
// be installed for them.
use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    for (src, flags, name) in [
        ("rootshim.c", &["-shared", "-fPIC"][..], "rootshim.so"),
        ("ttyexec.c", &[][..], "ttyexec"),
    ] {
        println!("cargo:rerun-if-changed={src}");
        let status = Command::new(&cc)
            .args(["-O2", "-Wall", "-Wextra"])
            .args(flags)
            .arg("-o")
            .arg(out.join(name))
            .arg(src)
            .status()
            .unwrap_or_else(|e| panic!("running the C compiler for {src}: {e}"));
        assert!(status.success(), "compiling {src} failed");
    }
}
