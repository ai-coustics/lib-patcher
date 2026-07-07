lib-patcher hides non-API symbols in Rust static libraries so they can be linked
into other Rust projects without symbol conflicts.

- Overview, usage, and how it works per platform: README.md
- Test layout and the patched/unpatched link matrix: tests/README.md

The MSVC dev tools (cl.exe/lib.exe) live under
/c/Program Files/Microsoft Visual Studio/2022/Community/VC/Tools/MSVC/<version>/bin/Hostx64/x64
(pick the actual <version> with `ls` on that MSVC dir; it changes with VS updates).
