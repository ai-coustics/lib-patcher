//! Producer B: a second Rust staticlib, near-identical to producer A but with
//! the `prodb_` prefix. It embeds the same std runtime and serde_json symbols.
//! Linking A and B together is what collides without the nightly flags.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Payload {
    name: String,
    value: i32,
}

/// Exported C API: multiply two numbers.
#[unsafe(no_mangle)]
pub extern "C" fn prodb_mul(a: i32, b: i32) -> i32 {
    a * b
}

/// Exported C API: round-trip a value through serde_json.
#[unsafe(no_mangle)]
pub extern "C" fn prodb_roundtrip(value: i32) -> i32 {
    let p = Payload {
        name: format!("producer-b-{value}"),
        value,
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: Payload = serde_json::from_str(&json).expect("deserialize");
    back.value
}
