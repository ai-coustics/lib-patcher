use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A simple data structure to test serialization
#[derive(Serialize, Deserialize, Debug)]
struct TestData {
    name: String,
    value: i32,
    tags: Vec<String>,
}

/// Add two numbers (exported C API)
#[unsafe(no_mangle)]
pub extern "C" fn testlib_add(a: i32, b: i32) -> i32 {
    a + b
}

/// Multiply two numbers (exported C API)
#[unsafe(no_mangle)]
pub extern "C" fn testlib_multiply(a: i32, b: i32) -> i32 {
    a * b
}

/// Generate a random number using rand crate (exported C API)
/// This ensures rand symbols are included in the library
#[unsafe(no_mangle)]
pub extern "C" fn testlib_random_number(max: i32) -> i32 {
    if max <= 0 {
        return 0;
    }
    let mut rng = rand::thread_rng();
    rng.gen_range(0..max)
}

/// Process JSON data using serde (exported C API)
/// Returns 1 on success, 0 on failure
/// This ensures serde/serde_json symbols are included
#[unsafe(no_mangle)]
pub extern "C" fn testlib_process_json() -> i32 {
    let data = TestData {
        name: "test".to_string(),
        value: 42,
        tags: vec!["tag1".to_string(), "tag2".to_string()],
    };

    match serde_json::to_string(&data) {
        Ok(json_str) => {
            // Try to deserialize it back
            match serde_json::from_str::<TestData>(&json_str) {
                Ok(parsed) => {
                    if parsed.value == 42 {
                        1 // Success
                    } else {
                        0 // Failure
                    }
                }
                Err(_) => 0,
            }
        }
        Err(_) => 0,
    }
}

/// Use HashMap from std to ensure std symbols are included
#[unsafe(no_mangle)]
pub extern "C" fn testlib_use_hashmap() -> i32 {
    let mut map = HashMap::new();
    map.insert("key1", 10);
    map.insert("key2", 20);
    map.insert("key3", 30);

    let sum: i32 = map.values().sum();
    sum // Should return 60
}

/// Format a string using std::fmt
#[unsafe(no_mangle)]
pub extern "C" fn testlib_format_string() -> i32 {
    let formatted = format!("Hello from Rust! Number: {}", 42);
    if formatted.contains("42") { 1 } else { 0 }
}

/// Use Vec operations to generate more std symbols
#[unsafe(no_mangle)]
pub extern "C" fn testlib_vec_operations() -> i32 {
    let mut vec = Vec::new();
    for i in 0..100 {
        vec.push(i);
    }

    let sum: i32 = vec.iter().filter(|&&x| x % 2 == 0).sum();
    sum // Sum of even numbers from 0 to 99
}

/// Internal helper function (not exported, for testing symbol filtering)
fn internal_helper() -> i32 {
    123
}

/// Get a magic number (exported C API)
#[unsafe(no_mangle)]
pub extern "C" fn testlib_get_magic() -> i32 {
    internal_helper()
}

/// This function doesn't have the testlib_ prefix
/// After patching, this should be hidden (made local)
#[unsafe(no_mangle)]
pub extern "C" fn unpatched_function() -> i32 {
    999
}
