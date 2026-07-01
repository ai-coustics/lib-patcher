use std::collections::HashSet;
use std::fs;
use std::path::Path;

use object::read::File;
use object::{Object as ObjectTrait, ObjectSymbol};

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
/// matching `keep_prefix` and not one of the known compiler-internal exemptions.
///
/// Such a symbol (Rust stdlib, a dependency) leaking out as global would defeat
/// the purpose and risk the very conflicts patching is meant to prevent.
///
/// Windows import-library members (e.g. `ProcessPrng` from `bcryptprimitives`)
/// are exempt: their names must match the system DLL exports, so they cannot be
/// renamed, and duplicate imports do not conflict the way defined symbols do.
/// They are recognised by their paired `__imp_<name>` thunk.
fn find_leaked_symbols<'a>(symbols: &'a [String], keep_prefix: &str) -> Vec<&'a str> {
    let import_thunks: HashSet<&str> = symbols
        .iter()
        .filter_map(|s| s.strip_prefix("__imp_"))
        .collect();
    symbols
        .iter()
        .filter(|s| {
            !symbol_is_allowed_global(s, keep_prefix)
                && !s.starts_with("__imp_")
                && !import_thunks.contains(s.as_str())
        })
        .map(String::as_str)
        .collect()
}

/// Returns true if any symbol carries `keep_prefix` (bare, or in the macOS
/// underscore-prefixed spelling), i.e. the public API survived patching.
fn keeps_any_api_symbol(symbols: &[String], keep_prefix: &str) -> bool {
    symbols.iter().any(|s| {
        s.starts_with(keep_prefix) || s.strip_prefix('_').unwrap_or(s).starts_with(keep_prefix)
    })
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
    // keep_prefix and the known compiler-internal exemptions.
    let leaked = find_leaked_symbols(&symbols, keep_prefix);
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

    // 5. The public API must survive patching. The leak check only guarantees
    // nothing *extra* stayed global; it would not notice if the keep_prefix
    // symbols themselves were dropped or mangled (e.g. their defining objects
    // were skipped), leaving a library that verifies clean but is missing its API.
    if !keeps_any_api_symbol(&symbols, keep_prefix) {
        return Err(format!(
            "no symbols matching keep-prefix '{}' remain; patching dropped the public API",
            keep_prefix
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
            find_leaked_symbols(&symbols, KEEP),
            vec!["_ZN4core3fmt3fooE"]
        );
    }

    #[test]
    fn leak_detection_exempts_paired_windows_import_thunks() {
        // A Windows import pair: the __imp_ thunk and its bare name both name a
        // system DLL export that cannot be renamed, so neither counts as a leak.
        let paired = vec![
            "mylib_run".to_string(),
            "__imp_ProcessPrng".to_string(),
            "ProcessPrng".to_string(),
        ];
        assert!(find_leaked_symbols(&paired, KEEP).is_empty());

        // A bare name with no paired __imp_ thunk is still a leak.
        let unpaired = vec!["ProcessPrng".to_string()];
        assert_eq!(find_leaked_symbols(&unpaired, KEEP), vec!["ProcessPrng"]);
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
    fn verify_rejects_a_library_that_lost_its_api() {
        // Non-empty and leak-free (the only global is an allowed compiler symbol),
        // but no keep-prefix symbol survived: patching dropped the public API.
        // The leak check alone passes this; the API-presence check must not.
        let archive =
            build_archive(&[("a.o", elf_object_with_global("DW.ref.rust_eh_personality"))]);
        let patched = write_temp("verify-noapi", &archive);
        let original = write_temp("verify-noapi-orig", &archive);
        let result = verify_patched_lib(&patched.0, &original.0, "myapp_", "linux");
        assert!(
            result.is_err(),
            "a library with no keep-prefix symbols must fail verification"
        );
    }
}
