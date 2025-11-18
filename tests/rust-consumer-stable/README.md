# Rust Consumer (Stable Toolchain)

This is a variant of the `rust-consumer` test that uses the **same stable toolchain** as the `testlib` library.

## Purpose

This test demonstrates the **expected limitation** when using lib-patcher with identical toolchains.

## Expected Behavior

When you run:
```bash
cargo build --release
```

You will see **linker errors** for dependency symbols like:
- `ryu::buffer::Buffer::format::...`
- `gimli::arch::PowerPc64::name_to_register::...`
- `serde_json::...`
- etc.

## Why This Happens

Both `testlib` (built with stable) and this consumer (also stable) compile their dependencies identically:
- Same `rand` version → identical symbols
- Same `serde_json` version → identical symbols
- Same toolchain → identical metadata hashes

The lib-patcher **successfully hides all std/core/alloc symbols** (you won't see conflicts for those), but it **cannot** distinguish between "testlib's serde_json" and "consumer's serde_json" - they're binary identical.

## Verification

Check that std symbols are properly hidden:
```bash
# List symbols in the patched library
../../target/release/lib-patcher \
  --input ../testlib/target/release/libtestlib_patched.a \
  --list | grep "_ZN3std"
# Returns nothing - std symbols are successfully hidden ✓

# But dependency symbols remain
../../target/release/lib-patcher \
  --input ../testlib/target/release/libtestlib_patched.a \
  --list | grep "serde_json"
# Shows serde_json symbols - these cause the conflicts
```

## Solution

Use different toolchains (see `../rust-consumer/rust-toolchain.toml`):
```toml
[toolchain]
channel = "beta"  # Different from testlib's stable
```

With different toolchains, the test passes successfully!

## Conclusion

This test **validates that the limitation exists** and shows it's an expected behavior, not a bug. The recommended approach is to use different toolchain versions, which is what the main test suite (`rust-consumer`) does.