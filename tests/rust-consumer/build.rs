use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Record which toolchain built this consumer so it can report it at runtime.
    // The same crate is built with `+beta` (different toolchain than testlib) and
    // `+stable` (same toolchain); this is how the two runs stay distinguishable.
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown rustc".to_string());
    println!("cargo:rustc-env=CONSUMER_RUSTC={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");

    // Which testlib build to link against. Defaults to the plain release build
    // (std symbols in separate archive members); the tests also point this at
    // "release-lto" (std folded into the crate object) to cover both cases.
    let profile = env::var("TESTLIB_PROFILE").unwrap_or_else(|_| "release".to_string());
    println!("cargo:rerun-if-env-changed=TESTLIB_PROFILE");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // Directory holding the testlib archive. Host builds (no --target) leave it
    // at target/<profile>; cross builds (Android) put it at
    // target/<triplet>/<profile>, so TESTLIB_LIB_DIR lets the Android test point
    // directly at the right directory instead of guessing the triplet here.
    let lib_dir = match env::var("TESTLIB_LIB_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => PathBuf::from(&manifest_dir)
            .join("..")
            .join("testlib")
            .join("target")
            .join(&profile),
    };
    println!("cargo:rerun-if-env-changed=TESTLIB_LIB_DIR");

    // Which archive to link. Defaults to the patched library; the negative test
    // overrides this to the unpatched "testlib" to show that an unpatched
    // archive can fail the link with duplicate-symbol errors.
    let link_lib = env::var("TESTLIB_LINK_LIB").unwrap_or_else(|_| "testlib_patched".to_string());
    println!("cargo:rerun-if-env-changed=TESTLIB_LINK_LIB");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static={link_lib}");

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

    // Rerun if the library changes (patched or unpatched: the negative test
    // links the unpatched one).
    for name in [
        "libtestlib_patched.a",
        "testlib_patched.lib",
        "libtestlib.a",
        "testlib.lib",
    ] {
        println!("cargo:rerun-if-changed={}", lib_dir.join(name).display());
    }
}
