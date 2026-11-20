use std::env;
fn main() {
    let dir =
        env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for build scripts");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rerun-if-changed=linker.ld");
}
