//! Producer A: a Rust staticlib exposing a small C-ABI surface under the
//! `proda_` prefix. It pulls in std (formatting, alloc, panic machinery) and a
//! dependency (serde_json) so the archive carries the usual pile of internal
//! Rust symbols. Producer B is a near-identical crate under a different prefix;
//! linking both into one consumer is the reproduction.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Payload {
    name: String,
    value: i32,
}

/// Exported C API: add two numbers.
#[unsafe(no_mangle)]
pub extern "C" fn proda_add(a: i32, b: i32) -> i32 {
    a + b
}

/// Exported C API: round-trip a value through serde_json. Drags serde_json,
/// alloc and formatting symbols into the crate object. Returns the value back.
#[unsafe(no_mangle)]
pub extern "C" fn proda_roundtrip(value: i32) -> i32 {
    let p = Payload {
        name: format!("producer-a-{value}"),
        value,
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: Payload = serde_json::from_str(&json).expect("deserialize");
    back.value
}
