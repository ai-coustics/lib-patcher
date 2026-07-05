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
A Rust program that links the patched static library. It uses the same crates as
testlib (rand, serde, serde_json), so without patching their symbols would
collide. The single crate is built with two toolchains to cover both consumer
situations (see the CI workflow, or run it locally with `+beta` / `+stable`):

- **`+beta`** - a *different* toolchain than testlib's stable. This is the
  critical case: testlib's bundled std/dependency objects are pulled alongside
  the consumer's own, so an unpatched archive fails to link.
- **`+stable`** - the *same* toolchain as testlib. This is the case the old
  denylist (hide only std/core/alloc) could not handle.

lib-patcher uses an allowlist (keep only `testlib_*`, hide everything else), so
both build and run cleanly: every non-API symbol is localized/renamed, leaving
nothing to collide.

Whether an **unpatched** testlib fails to link depends on the linker and the
consumer's toolchain:

- **Different toolchain** (`+beta`) on Linux (rust-lld) or Windows (link.exe:
  LNK2005 + LNK1169): fails with duplicate symbol errors (`rust_eh_personality`,
  `std::panicking::EMPTY_PANIC`, ...). This is what makes patching load-bearing.
- **Same toolchain** (`+stable`): the linker resolves the identical archive
  members first-definition-wins, so even an unpatched archive links.
- **macOS** (ld64): resolves archive duplicates first-wins regardless of
  toolchain, so an unpatched library links there too. Hiding symbols still
  matters on macOS for symbol-table hygiene, it just is not link-breaking.

The CI negative test therefore builds with `+beta` and asserts the unpatched link
fails on Linux and Windows; it is skipped on macOS.

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

# 5. Build and run the Rust consumer with both toolchains: a different one from
#    testlib (beta) and the same one (stable). Both must link cleanly.
( cd tests/rust-consumer && cargo +beta run --release && cargo +stable run --release )
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

# 5. Build and run the Rust consumer with both toolchains (different + same).
cd tests\rust-consumer
cargo +beta run --release
cargo +stable run --release
cd ..\..
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
3. **Conflict-Free Linking**: The Rust consumer links the patched library without conflicts under both a different toolchain (beta) and the same one as testlib (stable)
4. **Load-Bearing Patching**: The negative test (built with beta) proves the *unpatched* library fails the same link on Linux and Windows (skipped on macOS, see above), so the positive tests cannot silently become vacuous
5. **Platform Coverage**: Tests run on Linux, macOS, and Windows
6. **Real Dependencies**: Uses actual crates (rand, serde) to ensure realistic symbol counts

## Why This Test Design?

This test suite addresses the real-world scenario:
- Third-party static libraries built with Rust
- Need to integrate into projects that pull in the same stdlib and dependencies
- Symbol conflicts from stdlib and common dependencies
- Cross-platform compatibility requirements

By testing with:
- A library and a consumer that both use stdlib and common crates (rand, serde)
- The consumer built with a different toolchain than testlib (beta)
- The consumer built with the same toolchain as testlib (stable)
- Both C and Rust consumers

We ensure that `lib-patcher` solves the actual problem it was designed for: the
allowlist keeps only the public API and hides everything else, so the consumer's
own copies of those symbols never clash with the library's.
