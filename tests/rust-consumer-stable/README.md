# Rust Consumer (Stable Toolchain)

A variant of the `rust-consumer` test that links the patched `testlib` using the
**same stable toolchain** that built `testlib`. This is the hardest case for
symbol hiding.

## Purpose

`testlib` is built with stable and this consumer is also built with stable, so
every shared dependency compiles to byte-identical symbols:

- Same `serde` / `serde_json` version -> identical mangled symbols
- Same toolchain -> identical std/core/alloc symbols and metadata hashes

Without patching, linking the two together fails with duplicate-symbol errors on
Linux and Windows (macOS's ld64 resolves archive duplicates first-wins instead;
see [`../README.md`](../README.md)). A denylist that only hid std/core/alloc could
not fix this: it cannot tell `testlib`'s `serde_json` apart from the consumer's,
they are binary identical.

## Expected Behavior

lib-patcher uses an allowlist: it keeps only the `testlib_*` public API and
localizes/renames everything else. So even in this worst case the consumer links
and runs cleanly:

```bash
cargo build --release
cargo run --release
# ✓ All tests passed!
# ✓ Successfully linked patched library with the same Rust version
```

## Verification

The patched library exposes only the public API, so no std or dependency symbols
are left to collide (build the CLI first with `cargo build`):

```bash
# The 8 testlib_ functions remain public (plus DW.ref.*, a kept DWARF symbol)
../../target/debug/lib-patcher \
  --input ../testlib/target/release/libtestlib_patched.a --list

# std and dependency symbols are gone
../../target/debug/lib-patcher \
  --input ../testlib/target/release/libtestlib_patched.a --list \
  | grep -E "_ZN3std|serde_json"
# Returns nothing
```

## Why Two Rust Consumers?

- `rust-consumer` links the patched library from a **different** toolchain (beta).
- `rust-consumer-stable` (this one) links it from the **same** toolchain (stable),
  the case a denylist approach used to fail.

Both pass, which shows the allowlist makes patching work regardless of whether the
consumer shares `testlib`'s toolchain and dependency versions.
