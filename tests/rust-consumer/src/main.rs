use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// External functions from the patched testlib. This consumer is built with
// either the same toolchain as testlib (stable) or a different one (beta);
// CONSUMER_RUSTC (set by build.rs) records which.
unsafe extern "C" {
    fn testlib_add(a: i32, b: i32) -> i32;
    fn testlib_multiply(a: i32, b: i32) -> i32;
    fn testlib_random_number(max: i32) -> i32;
    fn testlib_process_json() -> i32;
    fn testlib_use_hashmap() -> i32;
    fn testlib_format_string() -> i32;
    fn testlib_vec_operations() -> i32;
    fn testlib_get_magic() -> i32;
}

#[derive(Serialize, Deserialize)]
struct LocalData {
    message: String,
    count: u32,
}

fn main() {
    println!(
        "Testing patched testlib from Rust (consumer built with {})",
        env!("CONSUMER_RUSTC")
    );
    println!("=====================================================\n");

    let mut errors = 0;

    // Test that we can use our own Rust stdlib/dependencies without conflicts
    println!("First, testing that this Rust binary's own features work:");

    // Use our own rand (potentially different version)
    let mut rng = rand::rng();
    let local_random: i32 = rng.random_range(0..100);
    println!("  Local rand::gen_range(0..100) = {} ✓", local_random);

    // Use our own HashMap
    let mut local_map: HashMap<&str, i32> = HashMap::new();
    local_map.insert("a", 1);
    local_map.insert("b", 2);
    println!("  Local HashMap works ✓");

    // Use our own serde_json
    let local_data = LocalData {
        message: "test".to_string(),
        count: 42,
    };
    let _json = serde_json::to_string(&local_data).expect("Failed to serialize");
    println!("  Local serde_json works ✓");

    println!("\nNow testing the patched static library:");

    // Test the patched library functions
    unsafe {
        let result_add = testlib_add(10, 5);
        print!("  testlib_add(10, 5) = {} ", result_add);
        if result_add == 15 {
            println!("✓");
        } else {
            println!("✗ (expected 15)");
            errors += 1;
        }

        let result_multiply = testlib_multiply(6, 7);
        print!("  testlib_multiply(6, 7) = {} ", result_multiply);
        if result_multiply == 42 {
            println!("✓");
        } else {
            println!("✗ (expected 42)");
            errors += 1;
        }

        let random = testlib_random_number(100);
        print!("  testlib_random_number(100) = {} ", random);
        if (0..100).contains(&random) {
            println!("✓");
        } else {
            println!("✗ (expected 0-99)");
            errors += 1;
        }

        let json_result = testlib_process_json();
        print!("  testlib_process_json() = {} ", json_result);
        if json_result == 1 {
            println!("✓");
        } else {
            println!("✗ (expected 1)");
            errors += 1;
        }

        let hashmap_result = testlib_use_hashmap();
        print!("  testlib_use_hashmap() = {} ", hashmap_result);
        if hashmap_result == 60 {
            println!("✓");
        } else {
            println!("✗ (expected 60)");
            errors += 1;
        }

        let format_result = testlib_format_string();
        print!("  testlib_format_string() = {} ", format_result);
        if format_result == 1 {
            println!("✓");
        } else {
            println!("✗ (expected 1)");
            errors += 1;
        }

        let vec_result = testlib_vec_operations();
        print!("  testlib_vec_operations() = {} ", vec_result);
        if vec_result == 2450 {
            println!("✓");
        } else {
            println!("✗ (expected 2450)");
            errors += 1;
        }

        let magic = testlib_get_magic();
        print!("  testlib_get_magic() = {} ", magic);
        if magic == 123 {
            println!("✓");
        } else {
            println!("✗ (expected 123)");
            errors += 1;
        }
    }

    println!("\n=====================================================");
    if errors == 0 {
        println!("✓ All tests passed!");
        println!(
            "✓ Successfully linked patched library (consumer built with {})",
            env!("CONSUMER_RUSTC")
        );
        println!("✓ No symbol conflicts detected!");
    } else {
        println!("✗ {} test(s) failed!", errors);
        std::process::exit(1);
    }
}
