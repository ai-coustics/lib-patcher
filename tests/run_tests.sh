#!/bin/bash
set -e

echo "============================================"
echo "Running lib-patcher integration tests"
echo "============================================"
echo ""

# Get the directory of this script
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "Step 1: Build lib-patcher CLI tool"
echo "------------------------------------"
cd "$PROJECT_ROOT"
cargo build --release
echo "✓ lib-patcher built"
echo ""

echo "Step 2: Build testlib static library"
echo "------------------------------------"
cd "$SCRIPT_DIR/testlib"
cargo build --release
echo "✓ testlib built"
echo ""

echo "Step 3: Patch testlib"
echo "------------------------------------"
if [[ "$OSTYPE" == "msys" || "$OSTYPE" == "win32" ]]; then
    # Windows
    LIB_INPUT="$SCRIPT_DIR/testlib/target/release/testlib.lib"
    LIB_OUTPUT="$SCRIPT_DIR/testlib/target/release/testlib_patched.lib"
    "$PROJECT_ROOT/target/release/lib-patcher.exe" \
        --input "$LIB_INPUT" \
        --output "$LIB_OUTPUT" \
        --keep-prefix testlib_
else
    # Linux/macOS
    LIB_INPUT="$SCRIPT_DIR/testlib/target/release/libtestlib.a"
    LIB_OUTPUT="$SCRIPT_DIR/testlib/target/release/libtestlib_patched.a"
    "$PROJECT_ROOT/target/release/lib-patcher" \
        --input "$LIB_INPUT" \
        --output "$LIB_OUTPUT" \
        --keep-prefix testlib_
fi
echo "✓ testlib patched"
echo ""

echo "Step 4: Test from C"
echo "------------------------------------"
cd "$SCRIPT_DIR/c-consumer"
if [[ "$OSTYPE" == "msys" || "$OSTYPE" == "win32" ]]; then
    # Windows - use cl.exe
    cl /Fe:testlib-test.exe main.c "$LIB_OUTPUT" ws2_32.lib advapi32.lib userenv.lib bcrypt.lib ntdll.lib
    ./testlib-test.exe
else
    # Linux/macOS - use Makefile
    make clean
    make
    ./testlib-test
fi
echo "✓ C test passed"
echo ""

echo "Step 5: Test from Rust (different version)"
echo "------------------------------------"
cd "$SCRIPT_DIR/rust-consumer"
cargo build --release
cargo run --release
echo "✓ Rust test passed"
echo ""

echo "============================================"
echo "✓ All integration tests passed!"
echo "============================================"
