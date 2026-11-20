use std::env;

fn main() {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for build scripts");
    let linker_script = format!("{}/linker.ld", manifest_dir);
    println!("cargo:rustc-link-arg=-T{}", linker_script);
    println!("cargo:rerun-if-changed=linker.ld");
}
