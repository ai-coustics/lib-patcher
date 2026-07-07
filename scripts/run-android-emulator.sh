#!/usr/bin/env bash
set -euo pipefail

# Runs the patched Android executables staged by scripts/test-android.sh on a
# booted emulator (via adb) and asserts each one actually works: exit code 0 and
# the "All tests passed!" line the consumers print. This is the piece the build
# container cannot do, and it is what turns the Android job from "it links" into
# "it links and runs correctly", matching the desktop platforms.

STAGE_DIR="${ANDROID_STAGE_DIR:-android-artifacts}"
DEVICE_DIR=/data/local/tmp

if [ ! -d "$STAGE_DIR" ] || [ -z "$(ls -A "$STAGE_DIR" 2>/dev/null)" ]; then
  echo "::error::no staged executables found in ${STAGE_DIR}" >&2
  exit 1
fi

adb wait-for-device

fail=0
for exe in "$STAGE_DIR"/*; do
  name=$(basename "$exe")
  echo "==> Running ${name} on the emulator"

  adb push "$exe" "${DEVICE_DIR}/${name}" >/dev/null
  adb shell "chmod 755 ${DEVICE_DIR}/${name}"

  # adb shell exit-code propagation is unreliable across versions, so append an
  # explicit marker and parse it out of the captured output.
  out=$(adb shell "${DEVICE_DIR}/${name}; echo EXIT:\$?")
  echo "$out"
  code=$(printf '%s\n' "$out" | sed -n 's/.*EXIT:\([0-9][0-9]*\).*/\1/p' | tail -n1)

  if [ "$code" != "0" ]; then
    echo "::error::${name} exited with status '${code}' on the emulator"
    fail=1
  elif ! printf '%s\n' "$out" | grep -q "All tests passed!"; then
    echo "::error::${name} exited 0 but did not report all tests passed"
    fail=1
  fi

  adb shell "rm -f ${DEVICE_DIR}/${name}" || true
done

echo
if [ "$fail" = 0 ]; then
  echo "All staged Android executables ran and passed on the emulator."
else
  echo "::error::one or more Android executables failed on the emulator"
fi
exit $fail
