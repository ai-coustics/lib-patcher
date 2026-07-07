#!/usr/bin/env bash
set -euo pipefail

# Builds testlib for the Android Rust targets, patches each archive, and proves
# the patched library behaves the same as it does on the desktop platforms:
#
#   * All ABIs get build + patch + C-consumer link + a symbol-leak check.
#   * The "run" ABIs additionally get the full consumer matrix that the desktop
#     jobs run - {release, release-lto} x {stable, beta} x {patched, unpatched} -
#     and their patched executables are staged into ANDROID_STAGE_DIR so a
#     separate emulator job can actually run them and check the results.
#
# This script only builds and links (the build container has no emulator); the
# staged binaries are executed by scripts/run-android-emulator.sh.

# Must be <= the emulator API level that runs the staged binaries (see the
# android-run job in .github/workflows/test.yml); a binary built for a higher
# minSdk than the device can fail to load.
ANDROID_API="${ANDROID_API:-30}"
# Where patched, runnable executables are collected for the emulator job.
STAGE_DIR="${ANDROID_STAGE_DIR:-android-artifacts}"

# Every ABI is built, patched, C-linked and symbol-checked.
build_targets=(
  aarch64-linux-android
  armv7-linux-androideabi
  i686-linux-android
  x86_64-linux-android
)

# ABIs that additionally get the full consumer matrix and whose patched binaries
# are staged to run on an emulator. x86_64 runs KVM-accelerated on the standard
# runners; arm64 execution needs a native arm runner (a separate job), so full
# software emulation is intentionally avoided here.
run_targets=(
  x86_64-linux-android
)

declare -A clang_by_target=(
  [aarch64-linux-android]="aarch64-linux-android${ANDROID_API}-clang"
  [armv7-linux-androideabi]="armv7a-linux-androideabi${ANDROID_API}-clang"
  [i686-linux-android]="i686-linux-android${ANDROID_API}-clang"
  [x86_64-linux-android]="x86_64-linux-android${ANDROID_API}-clang"
)

# Public API symbols that must survive patching on every ABI.
api_symbols=(
  testlib_add
  testlib_multiply
  testlib_random_number
  testlib_process_json
  testlib_use_hashmap
  testlib_format_string
  testlib_vec_operations
  testlib_get_magic
)

is_run_target() {
  local t=$1 r
  for r in "${run_targets[@]}"; do
    [ "$r" = "$t" ] && return 0
  done
  return 1
}

fail=0

# report EXPECTED ACTUAL CASE BUILD TOOLCHAIN: same aligned one-line format as
# the desktop matrix in test.yml. Matches get "(as expected)"; mismatches set
# the global failure flag.
report() {
  local expected=$1 actual=$2 mark verb suffix=""
  if [ "$expected" = "$actual" ]; then mark="✓"; suffix=" (as expected)"; else mark="✗"; fail=1; fi
  [ "$actual" = link ] && verb="linked" || verb="failed to link"
  printf '%-9s %-8s %-7s  %s %s%s\n' "$3" "$4" "$5" "$mark" "$verb" "$suffix"
}

# verify_symbols ARCHIVE TARGET: assert the public API is all present and that
# nothing else leaks. Deliberately does NOT swallow a --list failure - a crash
# or a reformatted listing must fail the test, not pass vacuously.
verify_symbols() {
  local archive=$1 target=$2 listing symbols leaked sym missing=()
  echo "==> Verifying public symbols for ${target}"
  listing=$(./target/debug/lib-patcher --input "$archive" --list)
  symbols=$(printf '%s\n' "$listing" | awk '/^Found / {capture=1; next} capture && NF {print}')

  # Positive check: every exported API symbol is still public.
  for sym in "${api_symbols[@]}"; do
    if ! printf '%s\n' "$symbols" | grep -qx "$sym"; then
      missing+=("$sym")
    fi
  done
  if [ "${#missing[@]}" -ne 0 ]; then
    echo "Expected public symbols missing from ${archive}: ${missing[*]}" >&2
    exit 1
  fi

  # Negative check: nothing but the API and known linker-generated globals.
  leaked=$(
    printf '%s\n' "$symbols" \
      | grep -Ev '^(testlib_|DW\.ref\.|_GLOBAL_OFFSET_TABLE_|GCC_except_table)' \
      || true
  )
  if [ -n "$leaked" ]; then
    echo "Unexpected public symbols leaked from ${archive}:" >&2
    echo "$leaked" >&2
    exit 1
  fi
}

patch_archive() {
  local input=$1 output=$2 target=$3
  ./target/debug/lib-patcher \
    --input "$input" \
    --output "$output" \
    --keep-prefix testlib_ \
    --triplet "$target"
}

# run_full_matrix TARGET: mirror the desktop consumer matrix for one ABI and
# stage the patched executables for the emulator job.
run_full_matrix() {
  local target=$1 profile build tc lib_dir exp
  echo "==> Consumer matrix for ${target} (patched + unpatched, both build styles, both toolchains)"

  # release was already built + patched by the caller; add release-lto.
  cargo +stable build \
    --manifest-path tests/testlib/Cargo.toml \
    --profile release-lto \
    --target "$target"
  local lto_dir="tests/testlib/target/${target}/release-lto"
  patch_archive "${lto_dir}/libtestlib.a" "${lto_dir}/libtestlib_patched.a" "$target"

  local built_bin="tests/rust-consumer/target/${target}/release/rust-consumer"

  for profile in release release-lto; do
    build=$([ "$profile" = release ] && echo non-lto || echo lto)
    lib_dir="$PWD/tests/testlib/target/${target}/${profile}"
    for tc in stable beta; do
      # Patched: always expected to link; stage the binary to run on the emulator.
      if TESTLIB_LIB_DIR="$lib_dir" TESTLIB_PROFILE="$profile" \
         cargo +$tc build --manifest-path tests/rust-consumer/Cargo.toml \
           --release --target "$target" >out.log 2>&1; then
        report link link patched "$build" "+$tc"
        cp "$built_bin" "${STAGE_DIR}/rust-consumer-${profile}-${tc}"
      else
        report link fail patched "$build" "+$tc"
        cat out.log
      fi

      # Unpatched: link-time expectation follows the Linux column of the table in
      # tests/README.md (Android is ELF + lld, same as Linux): only non-LTO with
      # the same toolchain links; the rest fail with duplicate-symbol errors.
      exp=fail
      if [ "$profile" = release ] && [ "$tc" = stable ]; then exp=link; fi
      if TESTLIB_LIB_DIR="$lib_dir" TESTLIB_PROFILE="$profile" TESTLIB_LINK_LIB=testlib \
         cargo +$tc build --manifest-path tests/rust-consumer/Cargo.toml \
           --release --target "$target" >out.log 2>&1; then
        report "$exp" link unpatched "$build" "+$tc"
      else
        report "$exp" fail unpatched "$build" "+$tc"
        if ! grep -Eiq "duplicate symbol|multiple definition|defined multiple times" out.log; then
          echo "    (but the failure was not a duplicate-symbol error:)"
          cat out.log
          fail=1
        fi
      fi
    done
  done
}

if command -v setup-cargo-config.sh >/dev/null 2>&1; then
  setup-cargo-config.sh
fi

cargo build
cargo test

# The matrix needs a second toolchain (beta) to stand in for "a different Rust
# version than testlib"; testlib itself pins stable via rust-toolchain.toml.
if [ "${#run_targets[@]}" -gt 0 ]; then
  rustup toolchain install beta --profile minimal
  for target in "${run_targets[@]}"; do
    rustup target add "$target" --toolchain stable
    rustup target add "$target" --toolchain beta
  done
fi

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

for target in "${build_targets[@]}"; do
  echo "==> Building testlib for ${target} (release)"
  cargo +stable build \
    --manifest-path tests/testlib/Cargo.toml \
    --release \
    --target "${target}"

  lib_dir="tests/testlib/target/${target}/release"
  input="${lib_dir}/libtestlib.a"
  output="${lib_dir}/libtestlib_patched.a"

  echo "==> Patching testlib for ${target} (release)"
  patch_archive "$input" "$output" "$target"

  echo "==> Linking Android C consumer for ${target}"
  c_bin="/tmp/testlib-test-${target}"
  "${clang_by_target[$target]}" \
    -Wall -Wextra -std=c11 \
    -o "${c_bin}" \
    tests/c-consumer/main.c \
    "${output}" \
    -ldl -llog -lm

  verify_symbols "$output" "$target"

  if is_run_target "$target"; then
    run_full_matrix "$target"
    # Stage the C consumer (release, patched) for the emulator job too.
    cp "${c_bin}" "${STAGE_DIR}/c-consumer"
  fi
done

echo
if [ "$fail" = 0 ]; then
  echo "Android patch/link + consumer-matrix checks passed."
  echo "Staged executables for the emulator job in ${STAGE_DIR}:"
  ls -1 "$STAGE_DIR"
else
  echo "::error::one or more Android matrix expectations were not met (see ✗ above)"
fi
exit $fail
