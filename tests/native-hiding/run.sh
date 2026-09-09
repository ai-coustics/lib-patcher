#!/usr/bin/env bash
#
# Experiment: can the nightly rustc flags -Zstaticlib-hide-internal-symbols
# (rust#155338) and -Zstaticlib-rename-internal-symbols (rust#156950) do
# lib-patcher's job natively, straight out of the compiler?
#
# The reproduction is the strongest, most objective form of the original
# problem (rust#104707, see the repo README "The Problem"): two Rust *staticlibs*
# that each embed std, linked together into one Rust binary.
#
#   producer-a  -> libproducer_a.a   (C API proda_*, + std + serde_json, LTO)
#   producer-b  -> libproducer_b.a   (C API prodb_*, + std + serde_json, LTO)
#   consumer    -> a Rust bin linking BOTH archives and calling both APIs
#
# LTO folds the std runtime symbols (rust_eh_personality, std::panicking, ...)
# into each crate's own object, which the linker must pull to satisfy the public
# API -- so both archives define those symbols and collide. This mirrors the
# main test suite's "release-lto" cell.
#
# On ELF (Linux) this script proves the full before/after:
#   * WITHOUT the flags: the link FAILS with duplicate-symbol errors, and the
#     archive leaks hundreds of internal symbols as GLOBAL/DEFAULT.
#   * WITH the flags: the link SUCCEEDS and runs, the internals are GLOBAL/HIDDEN
#     and renamed with a per-crate suffix, and the proda_/prodb_ C API is
#     untouched (GLOBAL/DEFAULT, no suffix).
#
# On macOS (ld64) the unpatched link does NOT fail -- ld64 resolves archive
# duplicates first-definition-wins (see the repo README / tests matrix), so the
# clash cannot be reproduced there. On macOS the script therefore only asserts
# the "after" direction: the flagged build links and runs and keeps the API.
#
# Requires a nightly toolchain that has both -Z flags. Run from anywhere.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOOLCHAIN="${NIGHTLY_TOOLCHAIN:-nightly}"
CARGO="rustup run $TOOLCHAIN cargo"

OS="$(uname -s)"
LOGDIR="$(mktemp -d)"
trap 'rm -rf "$LOGDIR"' EXIT
A_DIR="$HERE/producer-a/target/release"
B_DIR="$HERE/producer-b/target/release"
A_LIB="$A_DIR/libproducer_a.a"
B_LIB="$B_DIR/libproducer_b.a"

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAILED=1; }
info() { printf '  ---- %s\n' "$1"; }
FAILED=0

# Count unique externally-visible DEFINED symbols in an ELF archive whose name
# does ('~') or does not ('!~') match a regex. "Externally visible" = GLOBAL or
# WEAK binding with DEFAULT visibility and a real section index (defined). This
# is exactly what another object can bind to at link time; STV_HIDDEN symbols
# are excluded because they are not externally bindable.
#   $1 archive  $2 operator (~ or !~)  $3 name regex
count_default_visible() {
  readelf -sW "$1" 2>/dev/null | awk -v op="$2" -v re="$3" '
    ($5=="GLOBAL"||$5=="WEAK") && $6=="DEFAULT" && $7!="UND" &&
    ($4=="FUNC"||$4=="OBJECT"||$4=="NOTYPE"||$4=="IFUNC") {
      if (op=="~"  && $8 ~ re)  print $8;
      if (op=="!~" && $8 !~ re) print $8;
    }' | sort -u | wc -l | tr -d ' '
}

# Confirm the toolchain actually carries the two flags; a clear, early error
# beats a confusing build failure later.
step_check_flags() {
  echo "== Toolchain and flags =="
  rustc "+$TOOLCHAIN" --version
  local help
  help="$(rustc "+$TOOLCHAIN" -Z help 2>&1)"
  for f in staticlib-hide-internal-symbols staticlib-rename-internal-symbols; do
    if grep -q "$f" <<<"$help"; then pass "flag present: -Z$f"; else
      fail "flag MISSING: -Z$f (need a newer nightly)"; fi
  done
}

build_producers() {
  local flags="$1"
  for p in producer-a producer-b; do
    ( cd "$HERE/$p" && rm -rf target && RUSTFLAGS="$flags" $CARGO build --release >/dev/null 2>&1 )
  done
}

# Try to link the consumer against both archives. Prints the log to $1.
link_consumer() {
  local logfile="$1"
  ( cd "$HERE/consumer" && rm -rf target \
      && PRODUCER_A_LIB_DIR="$A_DIR" PRODUCER_B_LIB_DIR="$B_DIR" \
         $CARGO build --release ) >"$logfile" 2>&1
}

run_consumer() {
  local logfile="$1"
  ( cd "$HERE/consumer" \
      && PRODUCER_A_LIB_DIR="$A_DIR" PRODUCER_B_LIB_DIR="$B_DIR" \
         $CARGO run --release ) >"$logfile" 2>&1
}

# --- BEFORE: no flags -------------------------------------------------------
step_before() {
  echo "== BEFORE: producers built WITHOUT the flags =="
  build_producers ""

  # Symbol leak (ELF): the internals are externally bindable -- GLOBAL/WEAK
  # binding with DEFAULT visibility. nm -g counts them per archive member; the
  # readelf pass counts unique externally-visible names.
  if command -v readelf >/dev/null 2>&1; then
    local flat leaked
    flat=$(nm -g --defined-only "$A_LIB" 2>/dev/null | grep -cvE ' proda_' || true)
    leaked=$(count_default_visible "$A_LIB" '!~' proda_)
    info "producer-a: $flat non-API external defs (nm -g, per-member); $leaked unique externally-visible internal names (readelf)"
    if [ "$leaked" -ge 1 ] && [ "$flat" -gt 100 ]; then
      pass "internal symbols leak as externally-visible (namespace pollution reproduced)"
    else
      fail "expected leaked externally-visible internals (flat=$flat leaked=$leaked)"
    fi
    if readelf -sW "$A_LIB" 2>/dev/null \
        | awk '$4=="FUNC" && $5=="GLOBAL" && $6=="DEFAULT" && $8=="rust_eh_personality"{f=1} END{exit !f}'; then
      pass "rust_eh_personality is GLOBAL/DEFAULT (unhidden, unrenamed)"
    else
      info "rust_eh_personality not a bare GLOBAL/DEFAULT symbol (ok on non-ELF)"
    fi
  fi

  # The link.
  local log="$LOGDIR/before-link.log"
  if link_consumer "$log"; then
    if [ "$OS" = "Darwin" ]; then
      pass "link succeeded unpatched (expected on macOS: ld64 is first-definition-wins)"
    else
      fail "link SUCCEEDED without flags -- the clash did not reproduce (see $log)"
    fi
  else
    if grep -qiE 'duplicate symbol|multiple definition' "$log"; then
      pass "link FAILED with duplicate-symbol errors (the clash reproduced)"
      grep -iE 'duplicate symbol' "$log" | head -4 | sed 's/^/       /'
    else
      fail "link failed but NOT with a duplicate-symbol error (see $log)"
    fi
  fi
}

# --- AFTER: both flags ------------------------------------------------------
step_after() {
  echo "== AFTER: producers built WITH both flags =="
  build_producers "-Zstaticlib-hide-internal-symbols -Zstaticlib-rename-internal-symbols"

  if command -v readelf >/dev/null 2>&1; then
    # No non-API symbol is externally visible any more (all HIDDEN), while the
    # API stays externally visible.
    local leaked_after api_after
    leaked_after=$(count_default_visible "$A_LIB" '!~' proda_)
    api_after=$(count_default_visible "$A_LIB" '~' proda_)
    info "producer-a after flags: $leaked_after externally-visible internals, $api_after externally-visible proda_ API"
    if [ "$leaked_after" -eq 0 ]; then
      pass "no internal symbol is externally visible any more (all STV_HIDDEN)"
    else
      fail "expected 0 externally-visible internals, saw $leaked_after"
    fi
    if [ "$api_after" -ge 2 ]; then
      pass "exported C API (proda_*) stays externally visible / GLOBAL DEFAULT"
    else
      fail "exported C API lost external visibility (proda_ visible: $api_after)"
    fi
    # rust_eh_personality is now GLOBAL/HIDDEN.
    if readelf -sW "$A_LIB" 2>/dev/null \
        | awk '$4=="FUNC" && $5=="GLOBAL" && $6=="HIDDEN" && $8 ~ /^rust_eh_personality/{f=1} END{exit !f}'; then
      pass "rust_eh_personality is now GLOBAL/HIDDEN (STV_HIDDEN set)"
    else
      fail "expected rust_eh_personality to be GLOBAL/HIDDEN after flags"
    fi
    # Renamed with a per-crate suffix, and the two crates differ.
    local sa sb
    sa=$(readelf -sW "$A_LIB" 2>/dev/null | grep -oE 'rust_eh_personality\.rs[0-9a-f]+' | head -1)
    sb=$(readelf -sW "$B_LIB" 2>/dev/null | grep -oE 'rust_eh_personality\.rs[0-9a-f]+' | head -1)
    info "producer-a suffix: ${sa:-<none>}   producer-b suffix: ${sb:-<none>}"
    if [ -n "$sa" ] && [ -n "$sb" ] && [ "$sa" != "$sb" ]; then
      pass "internal symbols renamed with distinct per-crate suffixes"
    else
      fail "expected distinct per-crate rename suffixes (a=$sa b=$sb)"
    fi
  fi

  # The link + run.
  local log="$LOGDIR/after-run.log"
  if run_consumer "$log"; then
    if grep -q "All tests passed!" "$log"; then
      pass "consumer links AND runs against both flagged staticlibs"
      grep -E 'proda_|prodb_|All tests' "$log" | sed 's/^/       /'
    else
      fail "consumer ran but did not report success (see $log)"
    fi
  else
    fail "consumer FAILED to link/run WITH the flags (see $log)"
    tail -20 "$log" | sed 's/^/       /'
  fi
}

# --- Which flag does which job (ELF only, informational-but-checked) --------
step_isolate() {
  [ "$OS" = "Darwin" ] && return 0
  echo "== Flag roles (which flag resolves the clash) =="

  build_producers "-Zstaticlib-hide-internal-symbols"
  local log="$LOGDIR/hide-only-link.log"
  if link_consumer "$log"; then
    fail "hide-only unexpectedly linked (hiding should not dedupe two definitions)"
  else
    pass "hide-only still FAILS the link (visibility alone does not dedupe)"
  fi

  build_producers "-Zstaticlib-rename-internal-symbols"
  log="$LOGDIR/rename-only-link.log"
  if link_consumer "$log"; then
    pass "rename-only LINKS (the rename is what resolves the two-staticlib clash)"
  else
    fail "rename-only failed to link (see $log)"
  fi
}

step_check_flags
step_before
step_after
step_isolate

echo
if [ "$FAILED" = 0 ]; then
  echo "RESULT: all assertions met -- the nightly flags reproduce lib-patcher's core behavior."
  exit 0
else
  echo "::error::RESULT: one or more assertions failed (see FAIL lines above)."
  exit 1
fi
