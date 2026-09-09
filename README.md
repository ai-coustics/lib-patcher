# lib-patcher

Cross-platform symbol filtering for Rust static libraries.

Hides all symbols except those with a specified prefix to prevent linking conflicts when linking multiple Rust static libraries together. This is useful when shipping a precompiled Rust static library that consumers link into their own Rust projects, where symbol conflicts would otherwise occur.

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
cargo install lib-patcher
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
- `--output, -o`: Path to output patched library (required when patching)
- `--keep-prefix, -k`: Prefix for symbols to keep public (required when patching)
- `--triplet, -T`: Full Rust target triplet (e.g. `x86_64-pc-windows-gnullvm`, `aarch64-apple-ios`, `aarch64-linux-android`). Needed when cross-compiling: it selects the platform code path and the correct Apple platform version/Android NDK tooling. Without it the host OS is assumed.
- `--name, -n`: Base name for temporary files (optional, default: "lib")
- `--temp-dir, -t`: Directory for temporary files (optional)
- `--list, -l`: List all public symbols instead of patching

`--keep-prefix` is required when patching. Invoking the patcher without it is
an error.

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

Add `lib-patchet` as a build dependency to your project:

```sh
cargo add --build lib-patcher
```

Then use it programmatically in your `build.rs`:

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

- **Windows**: `llvm-objcopy` or `rust-objcopy` to rename symbols (from an LLVM
  install, or `rustup component add llvm-tools`), **plus** an archiver: `lib.exe`
  (MSVC toolchain), `llvm-lib`, or `llvm-ar`. MSVC's `lib.exe` alone is not
  sufficient: it archives but cannot rename symbols.
- **macOS**: `ld`, `nm`, `ar` (Xcode Command Line Tools)
- **Linux**: `ld`, `objcopy`, `readelf`, `ar` or `llvm-ar`
- **Android**: Android NDK LLVM tools (`ld.lld`, `llvm-readelf`,
  `llvm-objcopy`, `llvm-ar`). Set `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`,
  or `NDK_HOME` to the NDK root.

## How It Works

### Platform-Specific Implementation

#### Linux
1. Links all objects with `ld -r` into a single intermediate object
2. Uses `readelf` to extract all GLOBAL and WEAK symbols (both DEFAULT and HIDDEN visibility)
3. Filters to find symbols that don't match the prefix
4. Uses `objcopy --localize-symbols` (names passed in a file) to make them local/private
5. Creates final archive with `ar`

#### Android
1. Selects the Android NDK from `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or `NDK_HOME`
2. Links all objects with NDK `ld.lld -r` into a single intermediate ELF object
3. Uses NDK `llvm-readelf` to extract all GLOBAL and WEAK defined symbols
4. Uses NDK `llvm-objcopy --localize-symbols` to make non-API symbols local/private
5. Creates the final archive with NDK `llvm-ar`

#### macOS
1. Extracts all object files from the archive
2. Links with `ld -r` into a single intermediate object
3. Uses `nm` to get all global defined symbols
4. Creates an `-exported_symbols_list` with symbols to keep
5. Uses `ld -r -exported_symbols_list` to filter
6. Creates final archive with `ar`

#### Windows
1. Parses the COFF archive and extracts each object file
2. Scans every object for defined global symbols
3. For each symbol that does **not** start with the keep-prefix, generates a rename to `<keep-prefix><symbol>` (MSVC-mangled `??`-symbols are left untouched)
4. Applies the renames per object with `llvm-objcopy --redefine-syms`, so the renamed symbols no longer collide with the consumer's identically-named symbols
5. Re-creates the library with `lib.exe` / `llvm-lib` / `llvm-ar`

### What Gets Hidden

Everything except your public API (symbols starting with the keep-prefix) is taken
out of the way:

- On **Linux/macOS/Android** non-matching symbols are localized so they are no longer
  externally visible.
- On **Windows** non-matching symbols are renamed under the keep-prefix instead of
  localized (COFF makes localizing weak/COMDAT symbols unsafe), which removes the
  collision just the same.

The following are always left alone because they are needed for linking:
- Symbols starting with your specified prefix (your public API)
- DWARF debug symbols (`DW.ref.*`, `GCC_except_table`)
- Special linker symbols (`_GLOBAL_OFFSET_TABLE_`, `@`-prefixed symbols)

### When Patching Is Needed

Whether an unpatched library actually breaks the link depends on the platform:

- **Linux and Windows**: the link fails with duplicate-symbol errors whenever the
  consumer must pull an object that redefines Rust std symbols. That happens with a
  different toolchain, or with an LTO-built library even on the same toolchain.
- **macOS**: `ld64` resolves archive duplicates first-definition-wins, so an
  unpatched library links regardless. Patching is still worth doing: it keeps the
  library's internal and dependency symbols out of the consumer's symbol table, so
  nothing can bind to them by accident and the exported surface stays limited to
  your API; the result no longer depends on the linker happening to resolve
  duplicates in your favor; and you ship the same patched artifact through one build
  step on every platform.

## Testing

Integration tests build a Rust static library with real dependencies (rand,
serde, serde_json), patch it to keep only its handful of public API functions
while hiding the thousands of stdlib and dependency symbols, then link it from
both C and Rust consumers on Linux, macOS, and Windows. Android is covered by a
Docker/NDK smoke test that builds the same library for all Android Rust targets,
patches each archive, and links a C consumer with the NDK clang wrappers. The
Rust consumer pulls in its own copies of the same dependencies, so it links
cleanly only because patching hid the library's copies.

To prove those tests are load-bearing, the same consumer also links the
*unpatched* archive and asserts it fails with duplicate-symbol errors where it
must.

See [`tests/README.md`](tests/README.md) for the test layout, the full
patched/unpatched link matrix, Android Docker smoke-test instructions, and
step-by-step build instructions per platform, and
[`.github/workflows/test.yml`](.github/workflows/test.yml) for the CI runs.

## Inspiration

This library was inspired by:
- [staticlib-fucker](https://github.com/zeozeozeo/staticlib-fucker) - Windows-only tool for this problem
- [Alan Wu's blog post on symbol hygiene](https://alanwu.space/post/symbol-hygiene/)
- The ongoing [Rust issue #104707](https://github.com/rust-lang/rust/issues/104707) discussing symbol visibility

## License

MIT OR Apache-2.0
