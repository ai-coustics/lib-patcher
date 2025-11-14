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
