#[unsafe(no_mangle)]
pub extern "C" fn mylib_add(a: i32, b: i32) -> i32 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn mylib_multiply(a: i32, b: i32) -> i32 {
    a * b
}

// This is a helper function that should be hidden after patching
fn internal_helper() -> i32 {
    42
}

#[unsafe(no_mangle)]
pub extern "C" fn mylib_get_magic_number() -> i32 {
    internal_helper()
}

// This symbol does NOT have the mylib_ prefix and should be hidden
#[unsafe(no_mangle)]
pub extern "C" fn some_other_function() -> i32 {
    123
}
