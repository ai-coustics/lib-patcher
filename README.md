# lib-patcher

Cross-platform symbol filtering for Rust static libraries.

Hides all symbols except those with a specified prefix to prevent linking conflicts when linking multiple Rust static libraries together. This was developed to address the requirement of shipping a compiled C library that gets used from Rust again, where symbol conflicts would otherwise occur.

## The Problem

When you link a Rust static library into another Rust project, symbol conflicts occur if both use the same Rust stdlib version or dependencies:

```
error: duplicate symbol: rust_eh_personality
error: duplicate symbol: serde_json::ser::indent
...
```

## The Solution

**lib-patcher** hides all internal symbols and only exposes your public API:

- ✅ **Keeps**: Only symbols with your specified prefix (e.g., `mylib_*`)
- ✅ **Hides**: Everything else (Rust stdlib, dependencies, internal symbols)
- ✅ **Works**: Even when consumer uses the same Rust version

## How to Use

### As a CLI Tool (Post-Build)

#### Install

```sh
cargo install --git ssh://git@github.com/ai-coustics/lib-patcher.git lib-patcher
```

#### Basic Usage

```bash
# Patch library to only expose symbols starting with "mylib_"
lib-patcher \
  --input libmylib.a \
  --output libmylib_patched.a \
  --keep-prefix "mylib_"
```

**CLI Options:**
- `--input, -i`: Path to input static library (required)
- `--output, -o`: Path to output patched library (required)
- `--keep-prefix, -k`: Prefix for symbols to keep public (required)
- `--triplet, -T`: Full Rust target triplet (e.g. `x86_64-pc-windows-gnullvm`, `aarch64-apple-ios`). Needed when cross-compiling: it selects the platform code path and the correct Apple platform version. Without it the host OS is assumed.
- `--name, -n`: Base name for temporary files (optional, default: "lib")
- `--temp-dir, -t`: Directory for temporary files (optional)
- `--list, -l`: List all public symbols instead of patching

#### Cross-compiling

When patching a library built for a target other than the host, pass the
triplet so the right platform tooling is used:

```bash
lib-patcher \
  --input target/x86_64-pc-windows-gnullvm/release/libmylib.a \
  --output libmylib_patched.a \
  --keep-prefix "mylib_" \
  --triplet x86_64-pc-windows-gnullvm
```

#### List Symbols

```bash
# See what symbols are currently public
lib-patcher --input libmylib.a --list
```

### As a Rust Library (build.rs)

Use it programmatically in your `build.rs`:

```rust
use lib_patcher::patch_lib;
use std::env;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    patch_lib(
        Path::new("vendor/libthirdparty.a"),
        Path::new(&out_dir),
        "thirdparty",
        "thirdparty_",  // Keep only symbols starting with "thirdparty_"
        Path::new("libthirdparty_patched.a"),
        None,  // Auto-detect architecture
        None,  // Triplet: None uses the host OS (set when cross-compiling)
    );

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=thirdparty_patched");
}
```

## Platform-Specific Tools Required

- **Windows**: `lib.exe` (MSVC toolchain) or `llvm-lib` / `llvm-ar`
- **macOS**: `ld`, `nm`, `ar` (Xcode Command Line Tools)
- **Linux**: `ld`, `objcopy`, `readelf`, `ar` or `llvm-ar`

## How It Works

### Platform-Specific Implementation

#### Linux
1. Links all objects with `ld -r` into a single intermediate object
2. Uses `readelf` to extract all GLOBAL symbols (both DEFAULT and HIDDEN visibility)
3. Filters to find symbols that don't match the prefix
4. Uses `objcopy --localize-symbol` to make them local/private
5. Creates final archive with `ar`

#### macOS
1. Extracts all object files from the archive
2. Links with `ld -r` into a single intermediate object
3. Uses `nm` to get all global defined symbols
4. Creates an `-exported_symbols_list` with symbols to keep
5. Uses `ld -r -exported_symbols_list` to filter
6. Creates final archive with `ar`

#### Windows
1. Parses the COFF archive to extract each object file
2. For each object, directly patches the COFF symbol table
3. Changes storage class from EXTERNAL (2) to STATIC (3) for symbols to hide
4. Preserves COMDAT and DWARF symbols for proper linking
5. Re-creates the library with `lib.exe` / `llvm-lib` / `llvm-ar`

### What Gets Hidden

Everything except:
- Symbols starting with your specified prefix (your public API)
- DWARF debug symbols (`DW.ref.*`, `GCC_except_table`)
- Special linker symbols (`_GLOBAL_OFFSET_TABLE_`, `@`-prefixed symbols)

## Testing

Comprehensive integration tests verify the tool works across all platforms:

```bash
# Build the CLI tool
cargo build --release

# Build test library (uses rand, serde_json, lots of std symbols)
cd tests/testlib && cargo build --release

# Patch it
./target/release/lib-patcher \
  --input tests/testlib/target/release/libtestlib.a \
  --output tests/testlib/target/release/libtestlib_patched.a \
  --keep-prefix testlib_

# Test from C
cd tests/c-consumer && make && ./testlib-test

# Test from Rust with same stdlib version
cd tests/rust-consumer-stable && cargo build --release && ./target/release/rust-consumer

# Test from Rust with different stdlib version
cd tests/rust-consumer && cargo build --release && ./target/release/rust-consumer
```

### What Gets Tested

- **Real dependencies**: Uses rand, serde, serde_json to generate realistic symbol conflicts
- **Symbol hiding**: Hides ~2800+ Rust stdlib/dependency symbols while keeping 8 API functions
- **C interop**: C code successfully links and calls the patched library
- **Same-version Rust**: A Rust program with the **same stdlib version** can link without conflicts
- **Platform coverage**: CI runs on Linux, macOS, and Windows

The `rust-consumer-stable` test is the critical one - it uses the same Rust version and same dependencies (serde_json, rand) as the library. Without patching, this would fail with duplicate symbol errors. With patching, it works perfectly.

## Inspiration

This library was inspired by:
- [staticlib-fucker](https://github.com/zeozeozeo/staticlib-fucker) - Windows-only tool for this problem
- [Alan Wu's blog post on symbol hygiene](https://alanwu.space/post/symbol-hygiene/)
- The ongoing [Rust issue #104707](https://github.com/rust-lang/rust/issues/104707) discussing symbol visibility

## License

MIT OR Apache-2.0
