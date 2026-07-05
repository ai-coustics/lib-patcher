# Integration Tests

This directory contains comprehensive integration tests for `lib-patcher` that verify the tool works correctly across different platforms and scenarios.

## Test Structure

### 1. `testlib/` - Test Static Library
A Rust static library with multiple dependencies to generate many symbols:
- Uses `rand` for random number generation
- Uses `serde` and `serde_json` for serialization
- Uses standard library features (HashMap, Vec, String formatting)
- Exports C-compatible functions with `testlib_` prefix

It is built two ways so the consumer can be tested against both:
- **`--release`** (default): the std runtime symbols (`rust_eh_personality`, ...)
  stay in their own archive members.
- **`--profile release-lto`** (LTO, `codegen-units = 1`): those symbols are
  folded into the crate's own object, as a fully optimized shipped library
  usually is. This changes whether an unpatched archive collides at link time.

### 2. `c-consumer/` - C Integration Test
A C program that links against the patched static library:
- Tests all exported functions
- Verifies the library works correctly from C
- Uses Makefile for Linux/macOS, cl.exe for Windows

### 3. `rust-consumer/` - Rust Integration Test
A Rust program that links testlib. It uses the same crates as testlib (rand,
serde, serde_json), so without patching their symbols would collide. One crate
covers a full matrix: both testlib **build styles** (separate std members vs
LTO-merged, via `TESTLIB_PROFILE`) against both consumer **toolchains**
(`+stable` = same as testlib, `+beta` = different).

lib-patcher uses an allowlist (keep only `testlib_*`, hide everything else), so
the **patched** library links and runs in every cell of the matrix: on every
platform, both build styles, both toolchains.

The **unpatched** matrix shows what patching is for. A static archive only
collides when the consumer must pull an object that (re)defines std symbols
(`rust_eh_personality`, `std::panicking::EMPTY_PANIC`, ...):

| testlib build | consumer toolchain | Linux / Windows | macOS |
| --- | --- | --- | --- |
| separate std members | same (`+stable`) | **links** | links |
| separate std members | different (`+beta`) | **fails** | links |
| LTO-merged | same (`+stable`) | **fails** | links |
| LTO-merged | different (`+beta`) | **fails** | links |

- **Separate std members + same toolchain**: the consumer already defines those
  symbols, so the archive's std members are never pulled and there is no clash.
- **LTO-merged**: std is folded into the crate's own object, which the consumer
  must pull for the public API, so its definitions collide.
- **Different toolchain**: the consumer cannot satisfy testlib's references, so
  testlib's own std objects are pulled in and collide.
- **macOS** (ld64): resolves archive duplicates first-definition-wins, so an
  unpatched library links regardless. Hiding symbols still matters there for
  symbol-table hygiene, it just is not link-breaking, so macOS alone cannot
  prove patching works.

On Linux (rust-lld) and Windows (link.exe: LNK2005 + LNK1169) the failures are
duplicate symbol errors. The CI matrix asserts each cell above (patched links
everywhere; unpatched links or fails as shown). If a "links" cell starts failing
or a "fails" cell starts linking, the linker's archive-member selection or
rustc's LTO packaging changed.

## Running Tests

### Local Testing

Run the happy path from the repository root: build testlib, patch it, and link
the consumers against the patched archive. The CLI is built in debug so its
overflow and `debug_assert` checks stay active while it patches. This covers the
patched cells; the full matrix (LTO build style and the unpatched cases in the
table above) is exercised by CI, but you can reproduce any cell locally with
`TESTLIB_PROFILE` and `TESTLIB_LINK_LIB` (see the Rust consumer's `build.rs`).

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
3. **Conflict-Free Linking**: The patched library links and runs across the full matrix - both testlib build styles (separate std members and LTO-merged) and both consumer toolchains (same and different from testlib)
4. **Load-Bearing Patching**: The same matrix links the *unpatched* archive and asserts it fails where it must (see the table above), so the patched cells cannot silently become vacuous
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
- Two testlib build styles: std in separate archive members, and LTO-merged
- The consumer built both with the same toolchain as testlib and a different one
- Both C and Rust consumers

We ensure that `lib-patcher` solves the actual problem it was designed for: the
allowlist keeps only the public API and hides everything else, so the consumer's
own copies of those symbols never clash with the library's.
