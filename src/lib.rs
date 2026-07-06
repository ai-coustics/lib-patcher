use std::collections::HashSet;
use std::fs;
use std::path::Path;

use object::read::File;
use object::{Object as ObjectTrait, ObjectSection, ObjectSymbol};

/// Patches a static library to hide all symbols except those matching the specified prefix.
///
/// This prevents linking conflicts when using the static library in projects that also
/// depend on Rust stdlib or the same dependencies.
///
/// # Arguments
///
/// * `static_lib` - Path to the input static library (e.g., `libmylib.a`)
/// * `out_dir` - Directory for temporary files (use `$OUT_DIR` in build.rs)
/// * `lib_name` - Base name for temporary files (e.g., "mylib")
/// * `keep_prefix` - Prefix for symbols to keep public (e.g., "mylib_")
/// * `final_lib` - Path where the patched library will be written
/// * `target_arch` - Optional target architecture (e.g., "aarch64", "x86_64"). If `None`, uses host architecture.
/// * `target_triplet` - Optional full Rust target triplet (e.g., "aarch64-apple-ios"). Selects the
///   platform code path when cross-compiling and is required for correct Apple platform selection.
///
/// # Examples
///
/// ```rust,no_run
/// use lib_patcher::patch_lib;
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("vendor/libthirdparty.a"),
///     Path::new("out"),
///     "thirdparty",
///     "thirdparty_",
///     Path::new("libthirdparty_patched.a"),
///     None,
///     None,
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: Option<&str>,
    target_triplet: Option<&str>,
) {
    // An empty prefix matches every symbol (`starts_with("")` is always true), so
    // the patchers would keep everything and the verifier would accept everything,
    // silently emitting an unpatched library. Reject it rather than mislead.
    if keep_prefix.is_empty() {
        eprintln!(
            "Error: keep_prefix must not be empty; an empty prefix keeps every symbol \
             public and would emit an unpatched library."
        );
        std::process::exit(1);
    }

    // Prefer the explicit triplet: when cross-compiling, the target OS differs
    // from both the host and CARGO_CFG_TARGET_OS (which is unset for the CLI).
    let target_os = target_triplet
        .and_then(target_os_from_triplet)
        .map(str::to_string)
        .or_else(|| std::env::var("CARGO_CFG_TARGET_OS").ok())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "windows".to_string()
            } else if cfg!(target_os = "macos") {
                "macos".to_string()
            } else if cfg!(target_os = "ios") {
                "ios".to_string()
            } else {
                "linux".to_string()
            }
        });

    // Detect architecture from the library file
    let detected_arch = detect_archive_arch(static_lib);
    let final_arch = target_arch.map(|s| s.to_string()).unwrap_or(detected_arch);

    match target_os.as_str() {
        "windows" => patch_windows(
            static_lib,
            out_dir,
            lib_name,
            keep_prefix,
            final_lib,
            &final_arch,
        ),
        "macos" | "ios" | "tvos" | "visionos" => patch_macos(
            static_lib,
            out_dir,
            lib_name,
            keep_prefix,
            final_lib,
            &final_arch,
            target_triplet,
        ),
        _ => patch_linux(
            static_lib,
            out_dir,
            lib_name,
            keep_prefix,
            final_lib,
            &final_arch,
        ),
    }

    // Verify the patched library
    eprintln!("\nVerifying patched library...");
    if let Err(e) = verify_patched_lib(final_lib, static_lib, keep_prefix, &target_os) {
        eprintln!("\n❌ VERIFICATION FAILED: {}", e);
        eprintln!("The patched library may be corrupted or incomplete.");
        std::process::exit(1);
    }
}

/// Maps a Rust target triplet to the OS key used by `patch_lib` for dispatch.
///
/// Returns `None` for triplets that do not clearly identify an OS, so the
/// caller can fall back to the environment or host detection.
fn target_os_from_triplet(triplet: &str) -> Option<&'static str> {
    if triplet.contains("windows") {
        Some("windows")
    } else if triplet.contains("apple-ios") {
        Some("ios")
    } else if triplet.contains("apple-tvos") {
        Some("tvos")
    } else if triplet.contains("apple-visionos") {
        Some("visionos")
    } else if triplet.contains("apple") || triplet.contains("darwin") {
        Some("macos")
    } else if triplet.contains("linux") || triplet.contains("android") {
        Some("linux")
    } else {
        None
    }
}

/// Returns true if a global symbol is allowed to remain public after patching.
///
/// Besides the user's `keep_prefix`, a handful of compiler/linker-internal
/// symbols are legitimately left global by the per-platform patchers (and on
/// Windows they are renamed under `keep_prefix`, so they pass anyway).
fn symbol_is_allowed_global(name: &str, keep_prefix: &str) -> bool {
    // macOS prefixes user symbols with an underscore in the symbol table.
    let unprefixed = name.strip_prefix('_').unwrap_or(name);
    name.starts_with(keep_prefix)
        || unprefixed.starts_with(keep_prefix)
        || unprefixed.starts_with("DW.ref.")
        || unprefixed.starts_with("GLOBAL_OFFSET_TABLE_")
        || unprefixed.starts_with("GCC_except_table")
        // COFF section/compiler symbols and MSVC-mangled names.
        || name.starts_with('@')
        || name.starts_with('.')
        || name.starts_with("??")
}

/// Returns the global symbols that violate the allowlist invariant: anything not
/// matching `keep_prefix`, not a known compiler-internal exemption, and not a
/// proven Windows import symbol (see `import_exempt_symbols`).
///
/// Such a symbol (Rust stdlib, a dependency) leaking out as global would defeat
/// the purpose and risk the very conflicts patching is meant to prevent.
fn find_leaked_symbols<'a>(
    symbols: &'a [String],
    keep_prefix: &str,
    import_exempt: &HashSet<String>,
) -> Vec<&'a str> {
    symbols
        .iter()
        .filter(|s| {
            !symbol_is_allowed_global(s, keep_prefix) && !import_exempt.contains(s.as_str())
        })
        .map(String::as_str)
        .collect()
}

/// Collects the symbol names that belong to a Windows import within a *single*
/// archive member, so the leak check can exempt them.
///
/// Import names (e.g. `ProcessPrng`/`__imp_ProcessPrng` from `bcryptprimitives`)
/// must match the system DLL export, so the patchers cannot rename them
/// (llvm-objcopy also fails on some import objects, which is how they reach the
/// output unrenamed); duplicate imports do not conflict the way defined symbols
/// do. For every member that defines an `__imp_<name>` slot, that `__imp_<name>`
/// is exempt, and the bare `<name>` too when the *same* member also defines it
/// (the co-located call thunk).
///
/// Proving the pair from one member's own symbols is the point: the previous
/// archive-wide rule exempted a bare `<name>` whenever any `__imp_<name>`
/// appeared anywhere, so a leaked, unrenamed definition in an unrelated member
/// (e.g. an objcopy-failed COFF kept as-is) was waved through if some other
/// member happened to import a like-named symbol.
fn import_exempt_symbols(lib: &Path) -> HashSet<String> {
    let mut exempt = HashSet::new();
    let Ok(bytes) = fs::read(lib) else {
        return exempt;
    };
    let Ok(archive) = object::read::archive::ArchiveFile::parse(&*bytes) else {
        return exempt;
    };
    for member in archive.members() {
        let Ok(member) = member else { continue };
        let Ok(data) = member.data(&*bytes) else {
            continue;
        };
        let Ok(file) = File::parse(data) else {
            continue;
        };

        // Defined globals in this member alone, so a pair must be co-located.
        let defined: HashSet<&str> = file
            .symbols()
            .filter(|s| s.is_global() && s.is_definition())
            .filter_map(|s| s.name().ok())
            .collect();

        // An import-descriptor member (only `.idata$*` sections) carries a DLL
        // import library's COMDAT plumbing, which the Windows patcher regenerates
        // with its original (unprefixed) names. Exempt every global it defines
        // rather than flag a leak.
        if is_import_descriptor_member(&file) {
            exempt.extend(defined.iter().map(|s| s.to_string()));
            continue;
        }

        for name in &defined {
            if let Some(bare) = name.strip_prefix("__imp_") {
                exempt.insert((*name).to_string());
                if defined.contains(bare) {
                    exempt.insert(bare.to_string());
                }
            }
        }
    }
    exempt
}

/// Whether an archive member is a DLL import-descriptor object: it has at least
/// one section and every section is an import-directory section (`.idata$*`).
/// Such a member carries only import plumbing, never renamable code. Mirrors the
/// classifier the Windows patcher uses to identify import libraries.
fn is_import_descriptor_member<'a>(file: &File<'a, &'a [u8]>) -> bool {
    let mut any = false;
    for section in file.sections() {
        any = true;
        match section.name() {
            Ok(name) if name.starts_with(".idata") => {}
            _ => return false,
        }
    }
    any
}

/// Returns true if `name` carries `keep_prefix`, bare or in the macOS
/// underscore-prefixed spelling (`_mylib_foo` for `mylib_`).
fn matches_keep_prefix(name: &str, keep_prefix: &str) -> bool {
    name.starts_with(keep_prefix)
        || name
            .strip_prefix('_')
            .unwrap_or(name)
            .starts_with(keep_prefix)
}

// Platform-specific implementations
mod linux;
mod macos;
mod windows;

use linux::patch_linux;
use macos::patch_macos;
use windows::patch_windows;

/// Detects the architecture of a static library by examining the first object file
/// Works for all platforms: macOS (Mach-O), Linux (ELF), Windows (COFF/PE)
fn detect_archive_arch(static_lib: &Path) -> String {
    use std::io::Read;

    let archive_file = match fs::File::open(static_lib) {
        Ok(f) => f,
        Err(_) => {
            eprintln!(
                "Warning: Could not open library file for architecture detection, defaulting to x86_64"
            );
            return "x86_64".to_string();
        }
    };

    let mut archive = ar::Archive::new(archive_file);

    // Read the first object file to detect architecture
    while let Some(Ok(mut entry)) = archive.next_entry() {
        let mut data = Vec::new();
        if entry.read_to_end(&mut data).is_err() {
            continue;
        }

        // Try to parse as object file (works for Mach-O, ELF, and COFF)
        if let Ok(file) = File::parse(&*data) {
            use object::Architecture;
            let arch_str = match file.architecture() {
                Architecture::Aarch64 => "aarch64",
                Architecture::X86_64 => "x86_64",
                Architecture::I386 => "x86",
                Architecture::Arm => "arm",
                Architecture::Riscv64 => "riscv64",
                Architecture::PowerPc64 => "powerpc64",
                Architecture::S390x => "s390x",
                _ => "x86_64", // default fallback
            };
            eprintln!("Detected architecture from library: {}", arch_str);
            return arch_str.to_string();
        }
    }

    // If we can't detect, warn and default to x86_64
    eprintln!("Warning: Could not detect architecture from library, defaulting to x86_64");
    "x86_64".to_string()
}

/// Verifies that a patched library meets the expected criteria
fn verify_patched_lib(
    static_lib: &Path,
    original_lib: &Path,
    keep_prefix: &str,
    target_os: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Check that output file exists
    if !static_lib.exists() {
        return Err("Output file was not created".into());
    }

    // 2. Check that output file has reasonable size (not empty, not suspiciously small)
    let output_metadata = fs::metadata(static_lib)?;
    let output_size = output_metadata.len();

    if output_size == 0 {
        return Err("Output file is empty (0 bytes)".into());
    }

    // Check if output is suspiciously small compared to input
    let input_metadata = fs::metadata(original_lib)?;
    let input_size = input_metadata.len();
    let size_ratio = (output_size as f64) / (input_size as f64);

    let min_size_ratio = if target_os == "windows" {
        0.0001 // Very permissive for Windows (allows skipping LLVM bitcode)
    } else {
        0.01 // 1% minimum for other platforms
    };

    if size_ratio < min_size_ratio {
        return Err(format!(
            "Output file is suspiciously small ({} bytes vs {} bytes input, {:.2}% of original)",
            output_size,
            input_size,
            size_ratio * 100.0
        )
        .into());
    }

    // 3. Verify it's a valid archive by listing symbols
    let symbols = list_symbols(static_lib)?;

    if symbols.is_empty() {
        return Err("Output library contains no symbols - patching may have failed".into());
    }

    // 4. Enforce the allowlist invariant: no global symbol may leak past the
    // keep_prefix, the known compiler-internal exemptions, and the proven
    // per-member Windows import symbols.
    let import_exempt = import_exempt_symbols(static_lib);
    let leaked = find_leaked_symbols(&symbols, keep_prefix, &import_exempt);
    if !leaked.is_empty() {
        return Err(format!(
            "{} global symbol(s) do not match keep-prefix '{}' and were not hidden, e.g.: {}",
            leaked.len(),
            keep_prefix,
            leaked
                .iter()
                .take(10)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    // 5. The public API must survive patching. Checking the output names alone is
    // not enough: on Windows every non-API global is renamed under keep_prefix, so
    // the output is full of keep_prefix names even if the real exports were renamed
    // away (e.g. a mistyped --keep-prefix). Compare against the input instead: the
    // patchers keep every keep_prefix symbol verbatim, so *every* symbol matching
    // keep_prefix in the original library must still be present in the output. A
    // missing one means a dropped export (truncated or partial repackage).
    let original_symbols = list_symbols(original_lib)?;
    let original_api: Vec<&str> = original_symbols
        .iter()
        .map(String::as_str)
        .filter(|s| matches_keep_prefix(s, keep_prefix))
        .collect();

    if original_api.is_empty() {
        return Err(format!(
            "keep-prefix '{}' matched no symbols in the input library; check the prefix",
            keep_prefix
        )
        .into());
    }

    let output: HashSet<&str> = symbols.iter().map(String::as_str).collect();
    let missing: Vec<&str> = original_api
        .iter()
        .copied()
        .filter(|s| !output.contains(s))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} of {} input symbol(s) matching keep-prefix '{}' did not survive patching, \
             e.g.: {}",
            missing.len(),
            original_api.len(),
            keep_prefix,
            missing
                .iter()
                .take(10)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    eprintln!(
        "  ✓ Output library contains {} public symbols, all matching '{}'",
        symbols.len(),
        keep_prefix
    );

    Ok(())
}

/// Lists all public/global symbols in a static library
///
/// # Arguments
///
/// * `static_lib` - Path to the static library (e.g., `libmylib.a`, `mylib.lib`)
///
/// # Returns
///
/// A Result containing a sorted Vec of symbol names, or an error message.
///
/// # Examples
///
/// ```rust,no_run
/// use lib_patcher::list_symbols;
/// use std::path::Path;
///
/// let symbols = list_symbols(Path::new("libmylib.a")).unwrap();
/// for sym in symbols {
///     println!("{}", sym);
/// }
/// ```
pub fn list_symbols(static_lib: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use std::io::Read;

    let archive_file = fs::File::open(static_lib)?;
    let mut archive = ar::Archive::new(archive_file);
    let mut symbols = HashSet::new();

    // Extract and read symbols from each object file
    while let Some(Ok(mut entry)) = archive.next_entry() {
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;

        // Parse the object file
        let file = match File::parse(&*data) {
            Ok(f) => f,
            Err(_) => continue, // Skip non-object files
        };

        // Collect global/public symbols
        for symbol in file.symbols() {
            // Only include global/public symbols
            if symbol.is_global()
                && symbol.is_definition()
                && let Ok(name) = symbol.name()
                && !name.is_empty()
            {
                symbols.insert(name.to_string());
            }
        }
    }

    // Convert to sorted vector
    let mut result: Vec<String> = symbols.into_iter().collect();
    result.sort();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::write::{Object, StandardSection, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

    const KEEP: &str = "mylib_";

    /// A minimal ELF object defining one global symbol in `.text`.
    fn elf_object_with_global(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        let off = obj.append_section_data(text, &[0xc3], 1); // ret
        obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off,
            size: 1,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage, // global (STB_GLOBAL)
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// A minimal COFF object defining each of `names` as a global in `.text`,
    /// used to exercise the Windows import-exemption path.
    fn coff_object_with_globals(names: &[&str]) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        let off = obj.append_section_data(text, &[0xc3], 1); // ret
        for name in names {
            obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: off,
                size: 1,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        obj.write().unwrap()
    }

    /// A COFF object whose only section is an import-directory section
    /// (`.idata$2`) defining `name`, mirroring an import library's
    /// `__IMPORT_DESCRIPTOR_<dll>` head/tail member.
    fn coff_import_descriptor_object(name: &str) -> Vec<u8> {
        use object::SectionKind;
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let sec = obj.add_section(Vec::new(), b".idata$2".to_vec(), SectionKind::Data);
        let off = obj.append_section_data(sec, &[0u8; 20], 4);
        obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(sec),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// Wraps `members` (name, bytes) into an `ar` archive in memory.
    fn build_archive(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut builder = ar::Builder::new(&mut buf);
            for (name, data) in members {
                let header = ar::Header::new(name.as_bytes().to_vec(), data.len() as u64);
                builder.append(&header, data.as_slice()).unwrap();
            }
        }
        buf
    }

    /// Writes bytes to a uniquely named temp file and returns a guard that
    /// removes it on drop, so tests do not leak files even when they panic.
    struct TempFile(std::path::PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            fs::remove_file(&self.0).ok();
        }
    }
    fn write_temp(tag: &str, bytes: &[u8]) -> TempFile {
        let mut path = std::env::temp_dir();
        path.push(format!("lib-patcher-test-{}-{}.a", std::process::id(), tag));
        fs::write(&path, bytes).unwrap();
        TempFile(path)
    }

    #[test]
    fn allowlist_keeps_prefix_and_compiler_internals() {
        // Public API, in both the bare and macOS underscore-prefixed spellings.
        assert!(symbol_is_allowed_global("mylib_add", KEEP));
        assert!(symbol_is_allowed_global("_mylib_add", KEEP));
        // Compiler/linker internals the per-platform patchers legitimately keep.
        assert!(symbol_is_allowed_global("DW.ref.rust_eh_personality", KEEP));
        assert!(symbol_is_allowed_global("_GLOBAL_OFFSET_TABLE_", KEEP));
        assert!(symbol_is_allowed_global("GCC_except_table3", KEEP));
        // COFF section/compiler symbols and MSVC-mangled names.
        assert!(symbol_is_allowed_global("@feat.00", KEEP));
        assert!(symbol_is_allowed_global(".weak.foo", KEEP));
        assert!(symbol_is_allowed_global("??_C@_05foo@bar@", KEEP));
    }

    #[test]
    fn allowlist_rejects_stdlib_and_dependency_symbols() {
        // The whole point: nothing outside the API or the internals list passes.
        assert!(!symbol_is_allowed_global(
            "_ZN10serde_json2de10from_traitE",
            KEEP
        ));
        assert!(!symbol_is_allowed_global("rust_eh_personality", KEEP));
        assert!(!symbol_is_allowed_global("other_prefix_fn", KEEP));
    }

    #[test]
    fn triplet_dispatch_selects_the_right_os() {
        assert_eq!(
            target_os_from_triplet("x86_64-pc-windows-gnullvm"),
            Some("windows")
        );
        assert_eq!(
            target_os_from_triplet("x86_64-pc-windows-msvc"),
            Some("windows")
        );
        assert_eq!(target_os_from_triplet("aarch64-apple-ios"), Some("ios"));
        assert_eq!(target_os_from_triplet("aarch64-apple-ios-sim"), Some("ios"));
        assert_eq!(target_os_from_triplet("aarch64-apple-tvos"), Some("tvos"));
        assert_eq!(
            target_os_from_triplet("aarch64-apple-visionos"),
            Some("visionos")
        );
        assert_eq!(
            target_os_from_triplet("aarch64-apple-darwin"),
            Some("macos")
        );
        assert_eq!(
            target_os_from_triplet("x86_64-unknown-linux-gnu"),
            Some("linux")
        );
        assert_eq!(
            target_os_from_triplet("aarch64-linux-android"),
            Some("linux")
        );
        // Apple-platform ordering matters: an iOS Catalyst target must resolve to
        // iOS, not fall through to the generic apple -> macos branch.
        assert_eq!(
            target_os_from_triplet("x86_64-apple-ios-macabi"),
            Some("ios")
        );
        // Unknown targets return None so the caller falls back to env/host.
        assert_eq!(target_os_from_triplet("wasm32-unknown-unknown"), None);
    }

    #[test]
    fn leak_detection_flags_stdlib_but_not_exemptions() {
        let symbols = vec![
            "mylib_add".to_string(),
            "DW.ref.rust_eh_personality".to_string(),
            "_ZN4core3fmt3fooE".to_string(), // a leaked stdlib symbol
        ];
        assert_eq!(
            find_leaked_symbols(&symbols, KEEP, &HashSet::new()),
            vec!["_ZN4core3fmt3fooE"]
        );
    }

    #[test]
    fn leak_detection_exempts_proven_import_symbols() {
        // A Windows import pair named in the exempt set (proven co-located in one
        // member by import_exempt_symbols) is not a leak.
        let symbols = vec![
            "mylib_run".to_string(),
            "__imp_ProcessPrng".to_string(),
            "ProcessPrng".to_string(),
        ];
        let exempt = HashSet::from(["__imp_ProcessPrng".to_string(), "ProcessPrng".to_string()]);
        assert!(find_leaked_symbols(&symbols, KEEP, &exempt).is_empty());

        // The same bare name with no proven import backing it is still a leak.
        let unproven = vec!["ProcessPrng".to_string()];
        assert_eq!(
            find_leaked_symbols(&unproven, KEEP, &HashSet::new()),
            vec!["ProcessPrng"]
        );
    }

    #[test]
    fn import_exempt_symbols_requires_a_colocated_pair() {
        let archive = build_archive(&[
            // A genuine import object: the IAT slot and its call thunk together.
            (
                "imp.o",
                coff_object_with_globals(&["__imp_ProcessPrng", "ProcessPrng"]),
            ),
            // An IAT slot whose bare name is defined in no member.
            ("iat.o", coff_object_with_globals(&["__imp_OnlyIat"])),
        ]);
        let f = write_temp("import-exempt", &archive);
        let exempt = import_exempt_symbols(&f.0);

        assert!(exempt.contains("__imp_ProcessPrng"));
        assert!(exempt.contains("ProcessPrng"));
        // The lone IAT slot is exempt, but its bare name is not conjured up.
        assert!(exempt.contains("__imp_OnlyIat"));
        assert!(!exempt.contains("OnlyIat"));
    }

    #[test]
    fn import_descriptor_members_are_exempt() {
        // The regenerated import library's descriptor globals
        // (`__IMPORT_DESCRIPTOR_<dll>` etc.) carry no keep-prefix but must not be
        // flagged as leaks.
        let archive = build_archive(&[
            ("run.o", coff_object_with_globals(&["mylib_run"])),
            (
                "desc.o",
                coff_import_descriptor_object("__IMPORT_DESCRIPTOR_foo"),
            ),
            (
                "null.o",
                coff_import_descriptor_object("__NULL_IMPORT_DESCRIPTOR"),
            ),
        ]);
        let f = write_temp("import-descriptor", &archive);
        let symbols = list_symbols(&f.0).unwrap();
        let exempt = import_exempt_symbols(&f.0);

        assert!(exempt.contains("__IMPORT_DESCRIPTOR_foo"));
        assert!(exempt.contains("__NULL_IMPORT_DESCRIPTOR"));
        assert!(
            find_leaked_symbols(&symbols, KEEP, &exempt).is_empty(),
            "import-descriptor globals must not be flagged as leaks"
        );

        // The exemption is scoped to the all-`.idata` member: the same name
        // defined in an ordinary code member is still a leak.
        let leak = build_archive(&[(
            "leak.o",
            coff_object_with_globals(&["__IMPORT_DESCRIPTOR_foo"]),
        )]);
        let lf = write_temp("import-descriptor-leak", &leak);
        let lsyms = list_symbols(&lf.0).unwrap();
        let lexempt = import_exempt_symbols(&lf.0);
        assert_eq!(
            find_leaked_symbols(&lsyms, KEEP, &lexempt),
            vec!["__IMPORT_DESCRIPTOR_foo"]
        );
    }

    #[test]
    fn leak_from_a_different_member_than_the_import_is_flagged() {
        // The reviewer's case: one member imports __imp_Foo, an unrelated member
        // leaks a bare, unrenamed Foo (as an objcopy-failed COFF kept as-is
        // would). The bare Foo is not part of the import member, so it must be
        // reported, not waved through by the __imp_ sibling elsewhere.
        let archive = build_archive(&[
            ("imp.o", coff_object_with_globals(&["__imp_Foo"])),
            ("leak.o", coff_object_with_globals(&["Foo"])),
        ]);
        let f = write_temp("leak-diff-member", &archive);
        let symbols = list_symbols(&f.0).unwrap();
        let exempt = import_exempt_symbols(&f.0);

        assert_eq!(find_leaked_symbols(&symbols, KEEP, &exempt), vec!["Foo"]);
    }

    #[test]
    fn colocated_import_pair_is_not_flagged() {
        // The happy shape: the import object carries both spellings, so the pair
        // is exempt and the public API is untouched.
        let archive = build_archive(&[
            ("run.o", coff_object_with_globals(&["mylib_run"])),
            (
                "imp.o",
                coff_object_with_globals(&["__imp_ProcessPrng", "ProcessPrng"]),
            ),
        ]);
        let f = write_temp("import-pair", &archive);
        let symbols = list_symbols(&f.0).unwrap();
        let exempt = import_exempt_symbols(&f.0);

        assert!(find_leaked_symbols(&symbols, KEEP, &exempt).is_empty());
    }

    #[test]
    fn list_symbols_returns_sorted_deduped_globals() {
        let archive = build_archive(&[
            ("b.o", elf_object_with_global("zeta_sym")),
            ("a.o", elf_object_with_global("alpha_sym")),
        ]);
        let f = write_temp("list", &archive);
        let syms = list_symbols(&f.0).unwrap();
        assert_eq!(syms, vec!["alpha_sym".to_string(), "zeta_sym".to_string()]);
    }

    #[test]
    fn list_symbols_skips_non_object_members() {
        // A member that isn't an object file must be skipped, not fatal.
        let archive = build_archive(&[
            ("junk.txt", b"not an object".to_vec()),
            ("a.o", elf_object_with_global("real_sym")),
        ]);
        let f = write_temp("skip", &archive);
        let syms = list_symbols(&f.0).unwrap();
        assert_eq!(syms, vec!["real_sym".to_string()]);
    }

    #[test]
    fn list_symbols_on_empty_archive_is_empty() {
        let f = write_temp("empty", &build_archive(&[]));
        assert!(list_symbols(&f.0).unwrap().is_empty());
    }

    #[test]
    fn detect_archive_arch_reads_the_first_object() {
        let archive = build_archive(&[("a.o", elf_object_with_global("s"))]);
        let f = write_temp("arch", &archive);
        assert_eq!(detect_archive_arch(&f.0), "x86_64");
    }

    #[test]
    fn verify_rejects_a_library_with_a_leaked_global() {
        // End-to-end through the real file path: an archive whose only global
        // does not match the keep-prefix must fail verification (the safety net).
        let archive = build_archive(&[("a.o", elf_object_with_global("definitely_leaked"))]);
        let patched = write_temp("verify-bad", &archive);
        let original = write_temp("verify-bad-orig", &archive);
        let result = verify_patched_lib(&patched.0, &original.0, "myapp_", "linux");
        assert!(result.is_err(), "a leaked global must fail verification");
    }

    #[test]
    fn verify_accepts_a_library_with_only_prefixed_globals() {
        let archive = build_archive(&[("a.o", elf_object_with_global("myapp_public"))]);
        let patched = write_temp("verify-ok", &archive);
        let original = write_temp("verify-ok-orig", &archive);
        let result = verify_patched_lib(&patched.0, &original.0, "myapp_", "linux");
        assert!(
            result.is_ok(),
            "only-prefixed globals must pass: {:?}",
            result
        );
    }

    #[test]
    fn verify_rejects_when_prefix_matched_nothing_in_input() {
        // A mistyped prefix: the input exports `testlib_add`, but the user passed
        // `teslib_`. On Windows that would rename the real export to
        // `teslib_testlib_add`, so the output is full of `teslib_*` names; checking
        // output names alone would pass. Comparing against the input catches it.
        let original = write_temp(
            "verify-typo-orig",
            &build_archive(&[("a.o", elf_object_with_global("testlib_add"))]),
        );
        let patched = write_temp(
            "verify-typo",
            &build_archive(&[("a.o", elf_object_with_global("teslib_testlib_add"))]),
        );
        let result = verify_patched_lib(&patched.0, &original.0, "teslib_", "linux");
        assert!(
            result.is_err(),
            "a prefix matching nothing in the input must fail verification"
        );
    }

    #[test]
    fn verify_rejects_when_only_some_original_api_symbols_survive() {
        // The input exports two API symbols; the output kept only one, as a
        // truncated or partial repackage would. Accepting one survivor would let
        // the dropped export fail later in the consumer, so require all of them.
        let original = write_temp(
            "verify-partial-orig",
            &build_archive(&[
                ("a.o", elf_object_with_global("myapp_one")),
                ("b.o", elf_object_with_global("myapp_two")),
            ]),
        );
        let patched = write_temp(
            "verify-partial",
            &build_archive(&[("a.o", elf_object_with_global("myapp_one"))]),
        );
        let result = verify_patched_lib(&patched.0, &original.0, "myapp_", "linux");
        assert!(
            result.is_err(),
            "dropping any original API symbol must fail verification"
        );
    }

    #[test]
    fn verify_rejects_when_the_original_api_did_not_survive() {
        // The input's public API (`myapp_keep`) is gone from the output, which only
        // contains a different prefixed name (as a renamed internal would be). The
        // leak check passes and output names match the prefix, but the real API was
        // not preserved, so verification must fail.
        let original = write_temp(
            "verify-lost-orig",
            &build_archive(&[("a.o", elf_object_with_global("myapp_keep"))]),
        );
        let patched = write_temp(
            "verify-lost",
            &build_archive(&[("a.o", elf_object_with_global("myapp_other"))]),
        );
        let result = verify_patched_lib(&patched.0, &original.0, "myapp_", "linux");
        assert!(
            result.is_err(),
            "output must preserve an original keep-prefix symbol, not just any"
        );
    }
}
