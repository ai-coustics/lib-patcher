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
pub(crate) fn target_os_from_triplet(triplet: &str) -> Option<&'static str> {
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
        || unprefixed.starts_with("_GLOBAL_OFFSET_TABLE_")
        || unprefixed.starts_with("GCC_except_table")
        // COFF section/compiler symbols and MSVC-mangled names.
        || name.starts_with('@')
        || name.starts_with('.')
        || name.starts_with("??")
}

// Include platform-specific implementations from allowlist module
mod linux;
mod macos;
mod windows;

use linux::patch_linux;
use macos::patch_macos;
use windows::patch_windows;

/// Detects the architecture of a static library by examining the first object file
/// Works for all platforms: macOS (Mach-O), Linux (ELF), Windows (COFF/PE)
pub(crate) fn detect_archive_arch(static_lib: &Path) -> String {
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

    // 4. Enforce the allowlist invariant: every remaining global symbol must
    // either carry the keep_prefix or be one of the known compiler-internal
    // symbols. Anything else (Rust stdlib, dependencies) leaking out as global
    // would defeat the purpose and risk the conflicts we are trying to prevent.
    //
    // Windows import-library members (e.g. ProcessPrng from bcryptprimitives)
    // are exempt: their names must match the system DLL exports, so they cannot
    // be renamed, and duplicate imports do not conflict the way defined symbols
    // do. They are recognised by their paired `__imp_<name>` thunk.
    let import_thunks: HashSet<&str> = symbols
        .iter()
        .filter_map(|s| s.strip_prefix("__imp_"))
        .collect();
    let leaked: Vec<&String> = symbols
        .iter()
        .filter(|s| {
            !symbol_is_allowed_global(s, keep_prefix)
                && !s.starts_with("__imp_")
                && !import_thunks.contains(s.as_str())
        })
        .collect();

    if !leaked.is_empty() {
        return Err(format!(
            "{} global symbol(s) do not match keep-prefix '{}' and were not hidden, e.g.: {}",
            leaked.len(),
            keep_prefix,
            leaked
                .iter()
                .take(10)
                .map(|s| s.as_str())
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
