// External C function declarations from the UNPATCHED mylib
unsafe extern "C" {
    fn mylib_add(a: i32, b: i32) -> i32;
    fn mylib_multiply(a: i32, b: i32) -> i32;
    fn mylib_get_magic_number() -> i32;

    // This function IS accessible in the unpatched version
    fn some_other_function() -> i32;
}

fn main() {
    unsafe {
        println!("Testing UNPATCHED mylib functions:");
        println!("mylib_add(5, 3) = {}", mylib_add(5, 3));
        println!("mylib_multiply(4, 7) = {}", mylib_multiply(4, 7));
        println!("mylib_get_magic_number() = {}", mylib_get_magic_number());

        // In the unpatched version, this symbol is still exposed
        println!("some_other_function() = {}", some_other_function());
    }

    println!("\nNote: The unpatched library exposes ALL symbols, including some_other_function.");
    println!("This can lead to symbol conflicts when linking multiple libraries.");
}
