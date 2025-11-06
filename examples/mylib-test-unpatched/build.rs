use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Path to the UNPATCHED mylib static library
    let mylib_path = manifest_dir.parent().unwrap().join("mylib/target/release");

    if !mylib_path.join("libmylib.a").exists() {
        panic!(
            "mylib not found at {:?}. Run 'cargo build --release' in the mylib directory first.",
            mylib_path
        );
    }

    println!(
        "cargo:warning=Linking UNPATCHED library from: {}",
        mylib_path.display()
    );
    println!("cargo:warning=This should demonstrate symbol conflicts or ABI issues!");

    // Link directly against the unpatched library
    println!("cargo:rustc-link-search=native={}", mylib_path.display());
    println!("cargo:rustc-link-lib=static=mylib");

    println!("cargo:rerun-if-changed=../mylib/src/lib.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
