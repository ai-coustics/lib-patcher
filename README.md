# lib-patcher

Symbol filtering for cross-platform static libraries to prevent linking conflicts.

This library hides internal Rust standard library symbols from static libraries to prevent symbol conflicts when linking multiple Rust static libraries together. This was developed to address the requirement of shipping a compiled C library that gets used from Rust again, where symbol conflicts would otherwise occur.

## Inspiration and Background

This library was inspired by:

- [staticlib-fucker](https://github.com/zeozeozeo/staticlib-fucker) - A Windows-only tool for addressing this problem
- [Alan Wu's blog post on symbol hygiene](https://alanwu.space/post/symbol-hygiene/)
- The ongoing [Rust issue #104707](https://github.com/rust-lang/rust/issues/104707) discussing symbol visibility in static libraries

## What This Does

This library provides two filtering modes:

### Allowlist Mode (for libraries you control)
- **Keeps**: ONLY symbols matching your prefix (e.g., `mylib_init`, `mylib_process`)
- **Hides**: Everything else (Rust stdlib symbols, internal functions, etc.)
- **Requirement**: ALL public functions MUST start with your chosen prefix

### Blocklist Mode (for third-party libraries)
- **Keeps**: All symbols EXCEPT those in the blocklist
- **Hides**: ONLY specific symbols (e.g., `rust_eh_personality`, `__rust_alloc`)
- **Use case**: When you can't rename functions but need to hide conflicting symbols

## How to Use

### As a CLI Tool (Post-Build)

The CLI provides two subcommands for the two filtering modes.

### Install CLI

```sh
cargo install --git ssh://git@github.com/ai-coustics/lib-patcher.git lib-patcher
````

#### Allowlist Mode (keep only prefixed symbols)

```bash
lib-patcher allowlist \
  --input target/release/libmylib.a \
  --output target/release/libmylib_patched.a \
  --prefix mylib_
```

**Options:**
- `--input, -i`: Path to input static library (required)
- `--output, -o`: Path to output patched library (required)
- `--prefix, -p`: Symbol prefix to keep (required)
- `--name, -n`: Base name for temporary files (optional, default: "lib")
- `--temp-dir, -t`: Directory for temporary files (optional)

#### Blocklist Mode (hide specific symbols)

```bash
# Use default blocklist (hides common Rust stdlib symbols)
lib-patcher blocklist \
  --input vendor/libthirdparty.a \
  --output vendor/libthirdparty_patched.a

# Or use custom blocklist
lib-patcher blocklist \
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

**Default blocklist includes:**
- `rust_eh_personality`, `__rust_alloc`, `__rust_dealloc`, `__rust_realloc`
- `__rust_alloc_zeroed`, `__rust_alloc_error_handler`, `__rust_no_alloc_shim_is_unstable`

For detailed CLI usage including CI/CD integration examples, see [CLI_USAGE.md](CLI_USAGE.md).

### As a Rust Library (build.rs)

You can also use it programmatically in your `build.rs`. There are two filtering modes:

#### Mode 1: Allowlist (for libraries you control)

Keeps ONLY symbols with your prefix. All other symbols are hidden.

```rust
use lib_patcher::{patch_lib, FilterMode};
use std::env;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    patch_lib(
        Path::new("target/release/libmylib.a"),
        Path::new(&out_dir),
        "mylib",
        FilterMode::Allowlist {
            prefix: "mylib_".to_string(),
        },
        Path::new("target/release/libmylib_patched.a"),
    );

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=mylib_patched");
}
```

**Important**: ALL public functions must start with your prefix:
```rust
#[unsafe(no_mangle)]
pub extern "C" fn mylib_init() { }  // ✓ Will be kept

#[unsafe(no_mangle)]
pub extern "C" fn init() { }        // ✗ Will be hidden!
```

#### Mode 2: Blocklist (for third-party libraries)

Hides ONLY specific symbols. Everything else remains visible.

```rust
use lib_patcher::{patch_lib, FilterMode};

// Use default blocklist (hides common Rust stdlib symbols)
patch_lib(
    Path::new("vendor/libthirdparty.a"),
    Path::new(&out_dir),
    "thirdparty",
    FilterMode::default_blocklist(),
    Path::new("libthirdparty_patched.a"),
);

// Or use a custom blocklist
patch_lib(
    Path::new("vendor/lib2.a"),
    Path::new(&out_dir),
    "lib2",
    FilterMode::Blocklist {
        remove: vec![
            "rust_eh_personality".to_string(),
            "my_conflict".to_string(),
        ],
    },
    Path::new("lib2_patched.a"),
);
```

## Platform-Specific Tools Required

- **Windows**: `lib.exe` (MSVC toolchain)
- **macOS**: `ld`, `nm`, `ar` (Xcode Command Line Tools)
- **Linux**: `ld`, `objcopy`, `ar` or `llvm-ar`

## Common Issues

### "Undefined symbol" errors after patching (Allowlist mode)

→ You forgot to prefix a public function. Add your prefix to ALL exported functions, or switch to Blocklist mode if you can't control the function names.

### Multiple definition errors still occur (Blocklist mode)

→ Add the conflicting symbol to your blocklist. Use `nm` to identify which symbols are conflicting.

### All symbols were removed

→ In Allowlist mode: Check that your functions actually start with the prefix you specified.
→ In Blocklist mode: You may have accidentally hidden all symbols. Review your blocklist.

### Works without patch, breaks with it

→ In Allowlist mode: You're probably calling an unprefixed function from C. Check that ALL FFI functions have your prefix.
→ In Blocklist mode: A symbol you're using was added to the blocklist by mistake.

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

# 3. Patch it in blocklist mode
./target/release/lib-patcher blocklist \
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
- **Blocklist mode**: Hides Rust stdlib symbols while keeping library functions
- **C interop**: C code can successfully link and call the patched library
- **Cross-version Rust**: A Rust program with a **different stdlib version** can link without conflicts
- **Platform coverage**: CI runs on Linux, macOS, and Windows

The rust-consumer test is the critical one - it's built with Rust beta and uses rand 0.9, while testlib is built with Rust stable and uses rand 0.8. Without patching, this would fail with symbol conflicts.

## License

MIT OR Apache-2.0
