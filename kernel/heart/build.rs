//! Build script for the NovaX kernel.
//!
//! Besides linking the kernel against Limine's linker script, it compiles
//! every userspace server into a standalone static ELF. The programs are
//! plain `rustc` invocations (no cargo, no workspace membership), so the
//! kernel can `include_bytes!` them.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .expect("kernel/heart must live two levels below the workspace root");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    // Linker script for the kernel itself.
    let ld = root.join("boot/limine/linker.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());

    // Cargo automatically sets RUSTC to the exact compiler it is using.
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());

    // Shared syscall ABI included by every program.
    let abi = root.join("userspace/common/abi.rs");
    println!("cargo:rerun-if-changed={}", abi.display());

    let user_progs = [
        "client",
        "server",
        "serial",
        "kbd",
        "init",
        "shell",
        "net",
        "fb",
        "supervisor",
        "v02_server",
        "v02_client",
    ];
    for prog in user_progs {
        let user_dir = root.join(format!("userspace/{prog}"));
        let src = user_dir.join("src/main.rs");
        let user_ld = user_dir.join("linker.ld");
        let dest = out_dir.join(format!("{prog}.elf"));

        let status = Command::new(&rustc)
            .current_dir(&user_dir)
            .args([
                "--edition",
                "2021",
                "--crate-type",
                "bin",
                "--target",
                "x86_64-unknown-none",
                "-C",
                "panic=abort",
                "-C",
                "opt-level=2",
                "-C",
                "relocation-model=static",
                "-C",
                "code-model=small",
                "-C",
                "no-redzone=yes",
            ])
            .arg("-C")
            .arg(format!("link-arg=-T{}", user_ld.display()))
            .arg(&src)
            .arg("-o")
            .arg(&dest)
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn rustc for {prog}: {e}"));

        if !status.success() {
            panic!("{prog} build failed");
        }

        println!("cargo:rerun-if-changed={}", src.display());
        println!("cargo:rerun-if-changed={}", user_ld.display());
    }
}
