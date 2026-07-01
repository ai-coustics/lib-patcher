# Integration Tests

This directory contains comprehensive integration tests for `lib-patcher` that verify the tool works correctly across different platforms and scenarios.

## Test Structure

### 1. `testlib/` - Test Static Library
A Rust static library with multiple dependencies to generate many symbols:
- Uses `rand` for random number generation
- Uses `serde` and `serde_json` for serialization
- Uses standard library features (HashMap, Vec, String formatting)
- Exports C-compatible functions with `testlib_` prefix

### 2. `c-consumer/` - C Integration Test
A C program that links against the patched static library:
- Tests all exported functions
- Verifies the library works correctly from C
- Uses Makefile for Linux/macOS, cl.exe for Windows

### 3. `rust-consumer/` - Rust Integration Test
A Rust program that links the patched static library:
- Uses its **own versions** of rand, serde, etc. (e.g. rand 0.9 vs testlib's 0.8)
- Tests that the patched library doesn't conflict with the consumer's stdlib or
  its (possibly identical) dependency symbols
- Verifies no symbol conflicts occur after patching
- This is the critical test for the symbol hiding functionality

lib-patcher uses an allowlist (keep only `testlib_*`, hide everything else), so
this works regardless of whether the consumer is built with the same or a
different Rust toolchain: every non-API symbol is localized/renamed, so there is
nothing left to collide.

## Running Tests

### Local Testing

Run the full flow from the repository root. The test library is built with
`--release` because a release static library is what users actually ship and
patch; the CLI is built in debug so its overflow and `debug_assert` checks
stay active while it patches.

#### Linux / macOS

```bash
# 1. Build the lib-patcher CLI (debug: keeps runtime checks on)
cargo build

# 2. Build the test library
( cd tests/testlib && cargo build --release )

# 3. Patch it: keep only the testlib_ public API, hide everything else
./target/debug/lib-patcher \
  --input tests/testlib/target/release/libtestlib.a \
  --output tests/testlib/target/release/libtestlib_patched.a \
  --keep-prefix testlib_

# 4. Build and run the C consumer
( cd tests/c-consumer && make && ./testlib-test )

# 5. Build and run the Rust consumer
( cd tests/rust-consumer && cargo build --release && cargo run --release )
```

#### Windows

Run from a Visual Studio Developer PowerShell so that `cl.exe` and `lib.exe`
are on `PATH`. Patching also needs `llvm-objcopy` or `rust-objcopy` on `PATH`
(from an LLVM install or `rustup component add llvm-tools`) to rename symbols;
`lib.exe` alone cannot. The static library uses the `.lib` extension here, and
`/machine:` must match the target architecture (`x64`, `ARM64`, ...).

```powershell
# 1. Build the lib-patcher CLI (debug: keeps runtime checks on)
cargo build

# 2. Build the test library
cd tests\testlib; cargo build --release; cd ..\..

# 3. Patch it: keep only the testlib_ public API, hide everything else
.\target\debug\lib-patcher.exe `
  --input tests\testlib\target\release\testlib.lib `
  --output tests\testlib\target\release\testlib_patched.lib `
  --keep-prefix testlib_

# 4. Build and run the C consumer (ProcessPrng is imported from bcryptprimitives)
cd tests\c-consumer
lib /def:bcryptprimitives.def /out:bcryptprimitives.lib /machine:x64
cl /Fe:testlib-test.exe main.c ..\testlib\target\release\testlib_patched.lib `
  ws2_32.lib advapi32.lib userenv.lib bcrypt.lib ntdll.lib synchronization.lib bcryptprimitives.lib
.\testlib-test.exe
cd ..\..

# 5. Build and run the Rust consumer
cd tests\rust-consumer; cargo build --release; cargo run --release; cd ..\..
```

### CI Testing

Tests run automatically on:
- Ubuntu (Linux x86_64)
- macOS (`macos-latest`, Apple silicon)
- Windows (x86_64)

See `.github/workflows/test.yml` for the full CI configuration.

## What Gets Tested

1. **Symbol Patching**: The library is patched to keep only the `testlib_` public API and hide everything else (Rust stdlib and dependency symbols)
2. **C FFI**: C code can successfully link and call the patched library
3. **Conflict-Free Linking**: A Rust program with its own (possibly identical) stdlib/dependency versions can link the patched library without conflicts
4. **Platform Coverage**: Tests run on Linux, macOS, and Windows
5. **Real Dependencies**: Uses actual crates (rand, serde) to ensure realistic symbol counts

## Why This Test Design?

This test suite addresses the real-world scenario:
- Third-party static libraries built with Rust
- Need to integrate into projects that pull in the same stdlib and dependencies
- Symbol conflicts from stdlib and common dependencies
- Cross-platform compatibility requirements

By testing with:
- A library and a consumer that both use stdlib and common crates (rand, serde)
- A consumer that brings its own dependency versions
- Both C and Rust consumers

We ensure that `lib-patcher` solves the actual problem it was designed for: the
allowlist keeps only the public API and hides everything else, so the consumer's
own copies of those symbols never clash with the library's.
