use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_dir = PathBuf::from(&manifest_dir)
        .join("..")
        .join("testlib")
        .join("target")
        .join("release");

    // Which archive to link. Defaults to the patched library; the CI negative
    // test overrides this to the unpatched "testlib" to prove that an unpatched
    // archive fails the link with duplicate-symbol errors.
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
