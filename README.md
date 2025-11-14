# lib-patcher

Symbol filtering for cross-platform static libraries to prevent linking conflicts.

This library hides internal Rust standard library symbols from static libraries to prevent symbol conflicts when linking multiple Rust static libraries together. This was developed to address the requirement of shipping a compiled C library that gets used from Rust again, where symbol conflicts would otherwise occur.

## Inspiration and Background

This library was inspired by:

- [staticlib-fucker](https://github.com/zeozeozeo/staticlib-fucker) - A Windows-only tool for addressing this problem
- [Alan Wu's blog post on symbol hygiene](https://alanwu.space/post/symbol-hygiene/)
- The ongoing [Rust issue #104707](https://github.com/rust-lang/rust/issues/104707) discussing symbol visibility in static libraries

## What This Does

This library hides specific symbols in static libraries to prevent conflicts:

- **Keeps**: All symbols EXCEPT those in the blocklist
- **Hides**: ONLY specific symbols (e.g., `rust_eh_personality`, `__rust_alloc`)
- **Use case**: Third-party libraries where you need to hide conflicting symbols

## How to Use

### As a CLI Tool (Post-Build)

#### Install CLI

```sh
cargo install --git ssh://git@github.com/ai-coustics/lib-patcher.git lib-patcher
```

#### Basic Usage

```bash
# Use default blocklist (hides common Rust stdlib symbols)
lib-patcher \
  --input vendor/libthirdparty.a \
  --output vendor/libthirdparty_patched.a

# Or use custom blocklist
lib-patcher \
  --input vendor/lib2.a \
  --output vendor/lib2_patched.a \
  --symbols "rust_eh_personality,my_conflict,__rust_alloc"
```

**Options:**
- `--input, -i`: Path to input static library (required)
- `--output, -o`: Path to output patched library (required)
- `--symbols, -s`: Comma-separated list of symbols to hide (optional, defaults to stdlib symbols)
- `--name, -n`: Base name for temporary files (optional, default: "lib")
- `--temp-dir, -t`: Directory for temporary files (optional)
- `--arch, -a`: Target architecture for cross-compilation (optional)
- `--list, -l`: List all public symbols instead of patching

**Default blocklist includes:**
- `rust_eh_personality`, `__rust_alloc`, `__rust_dealloc`, `__rust_realloc`
- `__rust_alloc_zeroed`, `__rust_alloc_error_handler`, `__rust_no_alloc_shim_is_unstable`

#### List Symbols

```bash
# List all public symbols in a library
lib-patcher --input libmylib.a --list
```

### As a Rust Library (build.rs)

You can also use it programmatically in your `build.rs`:

```rust
use lib_patcher::{patch_lib, default_symbol_blocklist};
use std::env;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    // Use default blocklist (hides common Rust stdlib symbols)
    patch_lib(
        Path::new("vendor/libthirdparty.a"),
        Path::new(&out_dir),
        "thirdparty",
        &default_symbol_blocklist(),
        Path::new("libthirdparty_patched.a"),
        None,
    );

    // Or use a custom blocklist
    let symbols = vec![
        "rust_eh_personality".to_string(),
        "my_conflict".to_string(),
    ];
    patch_lib(
        Path::new("vendor/lib2.a"),
        Path::new(&out_dir),
        "lib2",
        &symbols,
        Path::new("lib2_patched.a"),
        None,
    );

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=thirdparty_patched");
}
```

## Platform-Specific Tools Required

- **Windows**: `lib.exe` (MSVC toolchain) or `llvm-lib`
- **macOS**: `ld`, `nm`, `ar` (Xcode Command Line Tools)
- **Linux**: `ld`, `objcopy`, `ar` or `llvm-ar`

## Common Issues

### Multiple definition errors still occur

→ Add the conflicting symbol to your blocklist. Use `lib-patcher --input libmylib.a --list` to identify which symbols are conflicting.

### All symbols were removed

→ You may have accidentally hidden all symbols. Review your blocklist.

### Works without patch, breaks with it

→ A symbol you're using was added to the blocklist by mistake. Check which symbols are being hidden.

## Testing

This repository includes comprehensive integration tests in the `tests/` directory that verify the tool works correctly across all platforms.

### Running Tests

```bash
# Run the full test suite
cd tests
./run_tests.sh
```

Or run individual steps:

```bash
# 1. Build the CLI tool
cargo build --release

# 2. Build test library (with rand, serde, lots of std symbols)
cd tests/testlib && cargo build --release

# 3. Patch it
./target/release/lib-patcher \
  --input tests/testlib/target/release/libtestlib.a \
  --output tests/testlib/target/release/libtestlib_patched.a

# 4. Test from C
cd tests/c-consumer && make && ./testlib-test

# 5. Test from Rust (different version - this is the key test!)
cd tests/rust-consumer && cargo +beta run --release
```

### What Gets Tested

The test suite verifies:
- **Real dependencies**: Uses rand, serde, serde_json to generate realistic symbol counts
- **Symbol hiding**: Hides Rust stdlib symbols while keeping library functions
- **C interop**: C code can successfully link and call the patched library
- **Cross-version Rust**: A Rust program with a **different stdlib version** can link without conflicts
- **Platform coverage**: CI runs on Linux, macOS, and Windows

The rust-consumer test is the critical one - it's built with Rust beta and uses rand 0.9, while testlib is built with Rust stable and uses rand 0.8. Without patching, this would fail with symbol conflicts.

## License

MIT OR Apache-2.0
