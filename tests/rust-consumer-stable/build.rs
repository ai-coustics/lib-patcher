use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_dir = PathBuf::from(&manifest_dir)
        .join("..")
        .join("testlib")
        .join("target")
        .join("release");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=testlib_patched");

    // On Windows, ensure required system import libraries are linked.
    // The testlib depends on getrandom/rand which call into WinAPI
    // (e.g., BCryptGenRandom), so we must explicitly link these libs
    // when consuming the static library from Rust.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=advapi32");
        println!("cargo:rustc-link-lib=userenv");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=ntdll");
    }

    // Rerun if the library changes
    println!(
        "cargo:rerun-if-changed={}",
        lib_dir.join("libtestlib_patched.a").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        lib_dir.join("testlib_patched.lib").display()
    );
}
