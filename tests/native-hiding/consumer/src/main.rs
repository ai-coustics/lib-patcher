//! Consumer binary that links BOTH producer staticlibs and calls both C APIs.
//! If it builds and prints "All tests passed!", the two staticlibs linked
//! together without a symbol clash.

unsafe extern "C" {
    fn proda_add(a: i32, b: i32) -> i32;
    fn proda_roundtrip(value: i32) -> i32;
    fn prodb_mul(a: i32, b: i32) -> i32;
    fn prodb_roundtrip(value: i32) -> i32;
}

fn main() {
    let mut errors = 0;

    unsafe {
        let add = proda_add(10, 5);
        print!("proda_add(10, 5) = {add} ");
        if add == 15 {
            println!("ok");
        } else {
            println!("BAD (expected 15)");
            errors += 1;
        }

        let ra = proda_roundtrip(42);
        print!("proda_roundtrip(42) = {ra} ");
        if ra == 42 {
            println!("ok");
        } else {
            println!("BAD (expected 42)");
            errors += 1;
        }

        let mul = prodb_mul(6, 7);
        print!("prodb_mul(6, 7) = {mul} ");
        if mul == 42 {
            println!("ok");
        } else {
            println!("BAD (expected 42)");
            errors += 1;
        }

        let rb = prodb_roundtrip(99);
        print!("prodb_roundtrip(99) = {rb} ");
        if rb == 99 {
            println!("ok");
        } else {
            println!("BAD (expected 99)");
            errors += 1;
        }
    }

    if errors == 0 {
        println!("All tests passed!");
    } else {
        eprintln!("{errors} test(s) failed");
        std::process::exit(1);
    }
}
