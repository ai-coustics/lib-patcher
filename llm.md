This project is meant to patch static rust libraries so that they can be used in other rust projects.

It works as an allowlist: `--keep-prefix <PREFIX>` keeps only the symbols that start
with your public-API prefix and hides everything else (Rust stdlib and dependency
symbols), which is what prevents the linking conflicts. `--keep-prefix` is required
when patching; invoking the patcher any other way is an error.

There is a CLI tool called lib-patcher and a direct library usage.

Test projects are defined in the tests directory.
The testlib is the library with contains a lot of rust std symbols and needs to be patched.

The rust-consumer is trying to use the patched library, pulling in its own
versions of the same dependencies (rand, serde, serde_json) to prove there are no
symbol conflicts. (The allowlist hides every non-API symbol, so this works
regardless of the consumer's Rust toolchain.)

The c-consumer is trying to use the patched library from a C project.

All of those projects should work.

All three platforms are supported. Linux and macOS localize the non-API symbols;
Windows renames them under the keep-prefix instead, because COFF makes localizing
weak/COMDAT symbols unsafe.

The msvc dev tools are in /c/Program Files/Microsoft Visual Studio/2022/Community/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64.
