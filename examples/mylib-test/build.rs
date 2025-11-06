use staticlib_hygiene::patch_lib;
use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Path to the original mylib static library
    let mylib_path = manifest_dir
        .parent()
        .unwrap()
        .join("mylib/target/release/libmylib.a");

    // Path where we'll put the patched library
    let patched_lib = out_dir.join("libmylib_patched.a");

    if !mylib_path.exists() {
        panic!(
            "mylib not found at {:?}. Run 'cargo build --release' in the mylib directory first.",
            mylib_path
        );
    }

    println!(
        "cargo:warning=Patching library from: {}",
        mylib_path.display()
    );
    println!(
        "cargo:warning=Output patched library to: {}",
        patched_lib.display()
    );

    // Patch the library to only expose symbols with the "mylib_" prefix
    patch_lib(&mylib_path, &out_dir, "mylib", "mylib_", &patched_lib);

    println!("cargo:warning=Patching complete!");

    // Tell cargo to link against our patched library
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=mylib_patched");

    println!("cargo:rerun-if-changed=../mylib/src/lib.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
