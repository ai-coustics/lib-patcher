#!/bin/bash
set -e

echo "Building mylib..."
cargo build --release

echo "Patching mylib..."
cd ../..
./target/release/lib-patcher allowlist \
  --input examples/mylib/target/release/libmylib.a \
  --output examples/mylib/target/release/libmylib_patched.a \
  --prefix mylib_

echo "✓ Patched library created at: examples/mylib/target/release/libmylib_patched.a"
