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
- **`--release`** (no LTO): the std runtime symbols (`rust_eh_personality`, ...)
  stay in their own archive members.
- **`--profile release-lto`** (LTO): those symbols are folded into the crate's
  own object, as a fully optimized shipped library usually is. This changes
  whether an unpatched archive collides at link time (see the matrix below).

### 2. `c-consumer/` - C Integration Test
A C program that links against the patched static library:
- Tests all exported functions
- Verifies the library works correctly from C
- Uses Makefile for Linux/macOS, cl.exe for Windows

### 3. `rust-consumer/` - Rust Integration Test
A Rust program that links testlib. It uses the same crates as testlib (rand,
serde, serde_json), so without patching their symbols would collide. One crate
covers a full matrix: testlib built without and **with LTO** (via
`TESTLIB_PROFILE`) against both consumer **toolchains** (`+stable` = same as
testlib, `+beta` = different).

lib-patcher uses an allowlist (keep only `testlib_*`, hide everything else), so
the **patched** library links and runs in every cell of the matrix: on every
platform, both builds, both toolchains.

The **unpatched** matrix shows what patching is for. A static archive only
collides when the consumer must pull an object that (re)defines std symbols
(`rust_eh_personality`, `std::panicking::EMPTY_PANIC`, ...):

| testlib build | consumer toolchain | Linux / Windows | macOS |
| --- | --- | --- | --- |
| non-LTO | same (`+stable`) | **links** | links |
| non-LTO | different (`+beta`) | **fails** | links |
| LTO | same (`+stable`) | **fails** | links |
| LTO | different (`+beta`) | **fails** | links |

- **non-LTO + same toolchain**: std lives in separate archive members that the
  consumer already defines, so they are never pulled and there is no clash.
- **LTO**: std is folded into the crate's own object, which the consumer must
  pull for the public API, so its definitions collide. LTO (thin or fat) is the
  trigger; `codegen-units` does not matter.
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

## What This Covers

- **Symbol patching**: keep only the `testlib_` public API, hide the Rust stdlib
  and dependency symbols.
- **C and Rust consumers**: both link and call the patched library.
- **Conflict-free linking**: the patched library links and runs across the whole
  matrix above (both build styles, both consumer toolchains).
- **Load-bearing patching**: the *unpatched* archive is linked too and must fail
  where the table says it fails, so the patched cells cannot silently pass for the
  wrong reason.
- **Platform coverage**: Linux, macOS, and Windows.
