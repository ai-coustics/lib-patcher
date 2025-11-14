# Integration Tests

This directory contains comprehensive integration tests for `lib-patcher` that verify the tool works correctly across different platforms and scenarios.

## Test Structure

### 1. `testlib/` - Test Static Library
A Rust static library with multiple dependencies to generate many symbols:
- Uses `rand` for random number generation
- Uses `serde` and `serde_json` for serialization
- Uses standard library features (HashMap, Vec, String formatting)
- Exports C-compatible functions with `testlib_` prefix
- Contains an unprefixed function (`unpatched_function`) to test blocklist mode

### 2. `c-consumer/` - C Integration Test
A C program that links against the patched static library:
- Tests all exported functions
- Verifies the library works correctly from C
- Uses Makefile for Linux/macOS, cl.exe for Windows

### 3. `rust-consumer/` - Rust Integration Test
A Rust program built with a **different Rust version** (beta vs stable):
- Tests that the patched library doesn't conflict with the consumer's stdlib
- Uses its own versions of rand, serde, etc.
- Verifies no symbol conflicts occur after patching
- This is the critical test for the blocklist mode functionality

## Running Tests

### Local Testing

```bash
# From the tests directory
./run_tests.sh
```

Or manually:

```bash
# 1. Build lib-patcher
cargo build --release

# 2. Build testlib
cd tests/testlib
cargo build --release

# 3. Patch the library
./target/release/lib-patcher blocklist \
  --input tests/testlib/target/release/libtestlib.a \
  --output tests/testlib/target/release/libtestlib_patched.a

# 4. Run C test
cd tests/c-consumer
make
./testlib-test

# 5. Run Rust test
cd tests/rust-consumer
cargo +beta build --release
cargo +beta run --release
```

### CI Testing

Tests run automatically on:
- Ubuntu (Linux x86_64)
- macOS (both x86_64 and arm64 runners)
- Windows (x86_64)

See `.github/workflows/test.yml` for the full CI configuration.

## What Gets Tested

1. **Symbol Patching**: The library is patched in blocklist mode, hiding Rust stdlib symbols
2. **C FFI**: C code can successfully link and call the patched library
3. **Cross-Version Rust**: A Rust program with different stdlib/dependency versions can link without conflicts
4. **Platform Coverage**: Tests run on Linux, macOS, and Windows
5. **Real Dependencies**: Uses actual crates (rand, serde) to ensure realistic symbol counts

## Why This Test Design?

The previous examples folder was too simplistic. This test suite addresses the real-world scenario:
- Third-party static libraries built with Rust
- Need to integrate into projects with different Rust versions
- Symbol conflicts from stdlib and common dependencies
- Cross-platform compatibility requirements

By testing with:
- A library built with **stable** Rust
- A consumer built with **beta** Rust
- Heavy use of stdlib and common crates
- Both C and Rust consumers

We ensure that `lib-patcher` solves the actual problem it was designed for.
