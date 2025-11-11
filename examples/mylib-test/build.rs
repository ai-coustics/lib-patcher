use lib_patcher::{FilterMode, patch_lib};
use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Get target architecture
    let target = env::var("TARGET").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    println!("cargo:warning=Building for target: {}", target);
    println!("cargo:warning=Target architecture: {}", target_arch);

    // Determine the target-specific subdirectory
    let target_dir = if target.contains("aarch64") || target.contains("arm64") {
        "aarch64-pc-windows-msvc"
    } else {
        "release"
    };

    // Path to the original mylib static library (platform-specific naming)
    let (mylib_path, patched_lib, target_arch_param) = if cfg!(target_os = "windows") {
        let lib_path = if target.contains("aarch64") || target.contains("arm64") {
            manifest_dir
                .parent()
                .unwrap()
                .join(format!("mylib/target/{}/release/mylib.lib", target_dir))
        } else {
            manifest_dir
                .parent()
                .unwrap()
                .join("mylib/target/release/mylib.lib")
        };

        (
            lib_path,
            out_dir.join("mylib_patched.lib"),
            Some(target_arch.as_str()),
        )
    } else {
        let lib_path = if target.contains("aarch64") {
            manifest_dir
                .parent()
                .unwrap()
                .join(format!("mylib/target/{}/release/libmylib.a", target))
        } else {
            manifest_dir
                .parent()
                .unwrap()
                .join("mylib/target/release/libmylib.a")
        };

        (
            lib_path,
            out_dir.join("libmylib_patched.a"),
            Some(target_arch.as_str()),
        )
    };

    if !mylib_path.exists() {
        panic!(
            "mylib not found at {:?}. Run 'cargo build --release{}' in the mylib directory first.",
            mylib_path,
            if target.contains("aarch64") {
                " --target aarch64-pc-windows-msvc"
            } else {
                ""
            }
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
    patch_lib(
        &mylib_path,
        &out_dir,
        "mylib",
        FilterMode::Allowlist {
            prefix: "mylib_".to_string(),
        },
        &patched_lib,
        target_arch_param,
    );

    println!("cargo:warning=Patching complete!");

    // Tell cargo to link against our patched library
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=mylib_patched");

    println!("cargo:rerun-if-changed=../mylib/src/lib.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
