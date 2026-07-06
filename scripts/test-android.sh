#!/usr/bin/env bash
set -euo pipefail

ANDROID_API="${ANDROID_API:-33}"

if command -v setup-cargo-config.sh >/dev/null 2>&1; then
  setup-cargo-config.sh
fi

cargo build
cargo test

declare -A clang_by_target=(
  [aarch64-linux-android]="aarch64-linux-android${ANDROID_API}-clang"
  [armv7-linux-androideabi]="armv7a-linux-androideabi${ANDROID_API}-clang"
  [i686-linux-android]="i686-linux-android${ANDROID_API}-clang"
  [x86_64-linux-android]="x86_64-linux-android${ANDROID_API}-clang"
)

targets=(
  aarch64-linux-android
  armv7-linux-androideabi
  i686-linux-android
  x86_64-linux-android
)

for target in "${targets[@]}"; do
  echo "==> Building testlib for ${target}"
  cargo +stable build \
    --manifest-path tests/testlib/Cargo.toml \
    --release \
    --target "${target}"

  lib_dir="tests/testlib/target/${target}/release"
  input="${lib_dir}/libtestlib.a"
  output="${lib_dir}/libtestlib_patched.a"

  echo "==> Patching testlib for ${target}"
  ./target/debug/lib-patcher \
    --input "${input}" \
    --output "${output}" \
    --keep-prefix testlib_ \
    --triplet "${target}"

  echo "==> Linking Android C consumer for ${target}"
  "${clang_by_target[$target]}" \
    -Wall -Wextra -std=c11 \
    -o "/tmp/testlib-test-${target}" \
    tests/c-consumer/main.c \
    "${output}" \
    -ldl -llog -lm

  echo "==> Verifying public symbols for ${target}"
  leaked=$(
    ./target/debug/lib-patcher --input "${output}" --list \
      | awk '/^Found / {capture=1; next} capture && NF {print}' \
      | grep -Ev '^(testlib_|DW\.ref\.|_GLOBAL_OFFSET_TABLE_|GCC_except_table)' \
      || true
  )
  if [ -n "${leaked}" ]; then
    echo "Unexpected public symbols leaked from ${output}:" >&2
    echo "${leaked}" >&2
    exit 1
  fi
done

echo "Android patch/link smoke tests passed."
