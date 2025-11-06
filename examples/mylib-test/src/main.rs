// External C function declarations from the patched mylib
unsafe extern "C" {
    fn mylib_add(a: i32, b: i32) -> i32;
    fn mylib_multiply(a: i32, b: i32) -> i32;
    fn mylib_get_magic_number() -> i32;

    // This function should NOT be accessible after patching since it doesn't have mylib_ prefix
    // fn some_other_function() -> i32;
}

fn main() {
    unsafe {
        println!("Testing patched mylib functions:");
        println!("mylib_add(5, 3) = {}", mylib_add(5, 3));
        println!("mylib_multiply(4, 7) = {}", mylib_multiply(4, 7));
        println!("mylib_get_magic_number() = {}", mylib_get_magic_number());

        // If we try to call some_other_function, it should fail at link time
        // because it was filtered out by the symbol patching
        // println!("some_other_function() = {}", some_other_function());
    }

    println!("\nSuccess! All mylib_ prefixed functions are accessible.");
}
