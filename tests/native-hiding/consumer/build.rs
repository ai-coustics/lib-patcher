//! Link the two producer staticlibs into this Rust binary.
//!
//! Both archives are located via `PRODUCER_A_LIB_DIR` / `PRODUCER_B_LIB_DIR`
//! (set by the driver script to each producer's `target/release`). Whether the
//! link succeeds is the whole experiment: without the nightly flags the two
//! copies of std collide; with `-Zstaticlib-rename-internal-symbols` they do
//! not.

use std::env;

fn main() {
    let dir_a = env::var("PRODUCER_A_LIB_DIR").expect("PRODUCER_A_LIB_DIR must be set");
    let dir_b = env::var("PRODUCER_B_LIB_DIR").expect("PRODUCER_B_LIB_DIR must be set");

    println!("cargo:rustc-link-search=native={dir_a}");
    println!("cargo:rustc-link-search=native={dir_b}");
    println!("cargo:rustc-link-lib=static=producer_a");
    println!("cargo:rustc-link-lib=static=producer_b");

    println!("cargo:rerun-if-env-changed=PRODUCER_A_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PRODUCER_B_LIB_DIR");
    println!("cargo:rerun-if-changed=build.rs");
}
