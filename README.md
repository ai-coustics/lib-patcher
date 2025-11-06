# staticlib-hygiene

Symbol filtering for cross-platform static libraries to prevent linking conflicts.

This library hides internal Rust standard library symbols from static libraries to prevent symbol conflicts when linking multiple Rust static libraries together. This was developed to address the requirement of shipping a compiled C library that gets used from Rust again, where symbol conflicts would otherwise occur.

## Inspiration and Background

This library was inspired by:

- [staticlib-fucker](https://github.com/moelife-coder/staticlib-fucker) - A tool addressing Windows only
- [Alan Wu's blog post on symbol hygiene](https://blog.alirezahayati.com/2024/06/15/symbol-hygiene-for-rust-static-libraries/)
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

#### Allowlist Mode (keep only prefixed symbols)

```bash
staticlib-hygiene allowlist \
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
staticlib-hygiene blocklist \
  --input vendor/libthirdparty.a \
  --output vendor/libthirdparty_patched.a

# Or use custom blocklist
staticlib-hygiene blocklist \
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
use staticlib_hygiene::{patch_lib, FilterMode};
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
use staticlib_hygiene::{patch_lib, FilterMode};

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

## Example

```rust
// mylib.rs - GOOD ✓
#[unsafe(no_mangle)]
pub extern "C" fn mylib_create() -> *mut MyLib { /* ... */ }

#[unsafe(no_mangle)]
pub extern "C" fn mylib_destroy(ptr: *mut MyLib) { /* ... */ }

// These will be hidden automatically (no prefix needed):
fn internal_helper() { /* ... */ }  // Private, will be hidden

// mylib.rs - BAD ✗
#[unsafe(no_mangle)]
pub extern "C" fn create() -> *mut MyLib { /* ... */ }  // No prefix!
// This will be hidden and cause "undefined symbol: create" errors!
```

## Example Projects

This repository includes three example projects in the `examples/` directory.

### Quick Demo

```bash
# Build the library (Rust stable)
cd examples/mylib && cargo build --release

# Try unpatched version (Rust beta) - FAILS with `rust-lld: error: duplicate symbol: rust_eh_personality`
cd ../mylib-test-unpatched && cargo build --release

# Try patched version (Rust beta) - SUCCEEDS
cd ../mylib-test && cargo run --release

cd ../mylib-test-c && ./patch.sh && make run
```

**Result**: The patched version successfully links a library built with one Rust version into a binary built with another Rust version, while the unpatched version fails with duplicate symbol errors.

## License

MIT OR Apache-2.0
