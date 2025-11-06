# Symbol Mangling Library for Cross-Platform Static Libraries

This library hides internal Rust standard library symbols from static libraries
to prevent symbol conflicts when linking multiple Rust static libraries together.
This was developed to address the requirement of shipping a compiled C library
that gets used from Rust again, where symbol conflicts would otherwise occur.

## Inspiration and Background

This library was inspired by:
- [staticlib-fucker](https://github.com/zeozeozeo/staticlib-fucker) - A tool addressing windows only
- Alan Wu's blog post on [symbol hygiene](https://alanwu.space/post/symbol-hygiene/)
- The ongoing [Rust issue #104707](https://github.com/rust-lang/rust/issues/104707) discussing symbol visibility in static libraries

## Requirements

**CRITICAL**: All functions you want to remain publicly accessible MUST start
with your chosen prefix (e.g., "mylib_", "aic_", "rb_").

## What This Does

- **Keeps**: Symbols matching your prefix (e.g., `mylib_init`, `mylib_process`)
- **Hides**: Everything else (Rust stdlib symbols like `rust_eh_personality`,
  `__rust_alloc`, internal functions, etc.)

## How to Use

1. **Prefix ALL public functions**:
  ```rust
  #[no_mangle]
  pub extern "C" fn mylib_init() { }  // ✓ Will be kept

  #[no_mangle]
  pub extern "C" fn init() { }        // ✗ Will be hidden!
  ```

2. **Call from build.rs**:
  ```rust
  patch_lib(
      Path::new("target/release/libmylib.a"),
      Path::new(&env::var("OUT_DIR").unwrap()),
      "mylib",
      "mylib_",  // Your prefix - ALL public functions must start with this!
      Path::new("target/release/libmylib_patched.a"),
  );
  ```

## Platform-Specific Tools Required

- **Windows**: `lib.exe` (MSVC toolchain)
- **macOS**: `ld`, `nm`, `ar` (Xcode Command Line Tools)
- **Linux**: `ld`, `objcopy`, `ar` or `llvm-ar`

## Common Issues

### "Undefined symbol" errors after patching
→ You forgot to prefix a public function. Add your prefix to ALL exported functions.

### Multiple definition errors still occur
→ Two libraries are exporting the same prefixed symbol. Use different prefixes
  for different libraries.

### Works without patch, breaks with it
→ You're probably calling an unprefixed function from C. Check that ALL
  FFI functions have your prefix.

## Example

```rust
// mylib.rs - GOOD ✓
#[no_mangle]
pub extern "C" fn mylib_create() -> *mut MyLib { /* ... */ }

#[no_mangle]
pub extern "C" fn mylib_destroy(ptr: *mut MyLib) { /* ... */ }

// These will be hidden automatically (no prefix needed):
fn internal_helper() { /* ... */ }  // Private, will be hidden
```

```rust
// mylib.rs - BAD ✗
#[no_mangle]
pub extern "C" fn create() -> *mut MyLib { /* ... */ }  // No prefix!
// This will be hidden and cause "undefined symbol: create" errors!
```
