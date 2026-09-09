# Native symbol hiding: can nightly rustc replace lib-patcher?

This directory is an **experiment**, not part of lib-patcher's own test suite. It
answers a concrete question: two nightly rustc flags recently landed that look
like they do lib-patcher's job inside the compiler. Do they actually resolve the
[original problem](../../README.md#the-problem) (rust#104707)?

- `-Zstaticlib-hide-internal-symbols` (rust#155338) — sets ELF `STV_HIDDEN`
  (Mach-O equivalent on Apple) on every defined symbol that is **not** in
  rustc's C-ABI export set.
- `-Zstaticlib-rename-internal-symbols` (rust#156950) — suffixes those same
  non-exported symbols with a per-crate hash so two Rust staticlibs don't clash.

Both are **nightly-only**, **ELF + Apple only** (Windows/COFF warns and no-ops),
and key off the compiler's export set rather than an arbitrary prefix.

## The reproduction

The strongest, most objective form of the problem: two Rust **staticlibs** that
each embed std, linked together into one Rust binary.

```
producer-a  ->  libproducer_a.a   C API proda_*  + std + serde_json (LTO)
producer-b  ->  libproducer_b.a   C API prodb_*  + std + serde_json (LTO)
consumer    ->  a Rust bin linking BOTH archives and calling both APIs
```

LTO folds the std runtime symbols (`rust_eh_personality`,
`std::panicking::EMPTY_PANIC`, ...) into each crate's own object, which the
linker must pull to satisfy the public API — so both archives define those
symbols and collide. This is the same trigger the main suite calls
`release-lto`.

## Running it

Needs a nightly toolchain carrying both `-Z` flags (they merged ~June 2026).

```bash
rustup toolchain install nightly
bash tests/native-hiding/run.sh
```

On **Linux (ELF)** the script proves the full before/after:

- **Without** the flags: the link **fails** with duplicate-symbol errors, and
  each archive leaks its internals as externally-visible (GLOBAL/DEFAULT)
  symbols.
- **With** the flags: the link **succeeds** and runs; the internals are
  `GLOBAL HIDDEN` and renamed with a distinct per-crate suffix
  (`rust_eh_personality.rs<hashA>` vs `.rs<hashB>`), while the `proda_`/`prodb_`
  C API stays `GLOBAL DEFAULT` and unrenamed.

It also isolates the roles: **hide-only still fails** the link (visibility alone
does not deduplicate two definitions); **rename-only links** (the rename is what
resolves the two-staticlib clash).

On **macOS (Apple)** `ld64` resolves archive duplicates first-definition-wins
(see the repo README / `tests/README.md`), so the unpatched clash cannot be
reproduced; there the script only asserts that the flagged build links and runs
and keeps its API.

## Verdict

The flags natively cover lib-patcher's core job on **ELF and Apple**:
`rename` removes the two-staticlib duplicate-symbol clash, `hide` keeps internal
and dependency symbols out of the exported surface. They do **not** yet cover:

- **Windows / COFF** — the flags warn and no-op there; lib-patcher still renames
  COFF symbols itself.
- **Stable toolchains** — both flags are nightly-only.
- **Arbitrary `--keep-prefix`** — the flags key off rustc's own C-ABI export
  set, not a user-chosen prefix, and only work on sources you compile (not a
  vendored third-party `.a`).

So lib-patcher can be **simplified** (lean on the compiler where the flags
apply) but not **retired**.
