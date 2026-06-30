This project is meant to patch static rust libraries so that they can be used in other rust projects.

It works as an allowlist: `--keep-prefix <PREFIX>` keeps only the symbols that start
with your public-API prefix and hides everything else (Rust stdlib and dependency
symbols), which is what prevents the linking conflicts. The older blocklist flags
(`--default` / `--symbols` / `--filter-prefix`, which hid a specific set of symbols
and left the rest public) are still accepted but deprecated; prefer `--keep-prefix`.

There is a CLI tool called lib-patcher and a direct library usage.

Test projects are defined in the tests directory.
The testlib is the library with contains a lot of rust std symbols and needs to be patched.

The rust-consumer is trying to use the patched library with a different rust version.

The rust-consumer-stable is trying to use the patched library with the same rust version.

The c-consumer is trying to use the patched library from a C project.

All of those projects should work.

Currently Linux and macOS work fine, but Windows has some issues. On Windows the tools work differnt so I don't know how to patch it.

The msvc dev tools are in /c/Program Files/Microsoft Visual Studio/2022/Community/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64.
