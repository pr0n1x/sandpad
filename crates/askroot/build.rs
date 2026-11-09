fn main() {
    // the libexec dir is baked into the lib at build time
    println!("cargo:rerun-if-env-changed=SANDPAD_LIBEXEC");
}
