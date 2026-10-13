// rootshim.so is compiled here and embedded into the binary (see src/boxdir.rs),
// so a single file is all that has to be installed.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=rootshim.c");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("rootshim.so");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(cc)
        .args(["-O2", "-Wall", "-Wextra", "-shared", "-fPIC", "-o"])
        .arg(&out)
        .arg("rootshim.c")
        .status()
        .expect("running the C compiler for rootshim.c");
    assert!(status.success(), "compiling rootshim.c failed");
}
