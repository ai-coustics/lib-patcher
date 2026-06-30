use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::File;
use object::{Object as ObjectTrait, ObjectSymbol};

/// Default blocklist of common problematic Rust stdlib symbols
///
/// Includes:
/// - `rust_eh_personality` - Exception handling (main conflict source)
/// - `__rust_no_alloc_shim_is_unstable` - Allocation shim marker
/// - `__rust_alloc`, `__rust_dealloc`, `__rust_realloc` - Allocator functions
/// - `__rust_alloc_zeroed` - Zero-initialized allocation
/// - Rust stdlib symbols that may appear in COMDATs
///
/// These symbols commonly conflict when linking multiple Rust staticlibs.
pub fn default_symbol_blocklist() -> Vec<String> {
    vec![
        // Core Rust runtime/allocator symbols that commonly conflict
        "rust_eh_personality".to_string(),
        "__rust_no_alloc_shim_is_unstable".to_string(),
        "__rust_alloc".to_string(),
        "__rust_dealloc".to_string(),
        "__rust_realloc".to_string(),
        "__rust_alloc_zeroed".to_string(),
        "__rust_alloc_error_handler".to_string(),
    ]
}

/// Symbols that must stay `External` on the COFF path even if they match the hide
/// list, because they are defined in one object and referenced from a sibling.
///
/// The COFF backend localizes per object without a relink (unlike the Linux/Apple
/// `ld -r` paths), so localizing such a symbol orphans the definition and turns the
/// sibling's reference into a malformed `Static` + `SectionNumber == 0` entry. That
/// covers `ring`'s asm routines (`ring_core_*`) and `rust_eh_personality` (every
/// landing pad references it).
///
/// Only consulted when [`protect_cross_object_symbols`] is set (Windows GNU ABI);
/// these symbols are still hidden on MSVC and on Linux/Apple.
fn coff_must_stay_global(name: &str) -> bool {
    name == "rust_eh_personality" || name.starts_with("ring_core_")
}

/// Whether the COFF backend should keep [`coff_must_stay_global`] symbols `External`
/// rather than localizing them.
///
/// This is needed for the Windows GNU ABI targets (`*-windows-gnu`,
/// `*-windows-gnullvm`), where `ring`'s asm routines and `rust_eh_personality` are
/// referenced across object boundaries and localizing them breaks static linking.
/// The MSVC targets use a different exception-handling model and don't hit this, so
/// the flag stays off there and those symbols are localized as before.
fn protect_cross_object_symbols(target_os: &str, triplet: Option<&str>) -> bool {
    target_os == "windows" && triplet.is_some_and(|t| t.contains("windows-gnu"))
}

/// Patches a static library to hide specific symbols.
///
/// # Arguments
///
/// * `static_lib` - Path to the input static library (e.g., `libmylib.a`)
/// * `out_dir` - Directory for temporary files (use `$OUT_DIR` in build.rs)
/// * `lib_name` - Base name for temporary files (e.g., "mylib")
/// * `symbols_to_hide` - List of symbol names to hide/localize
/// * `final_lib` - Path where the patched library will be written
/// * `target_arch` - Optional target architecture (e.g., "aarch64", "x86_64"). If `None`, uses host architecture.
/// * `target_triplet` - Optional full Rust target triplet (e.g., "aarch64-apple-ios"). Required for
///   correct Apple platform selection; without it the host OS is assumed.
///
/// # Panics
///
/// Panics if required platform tools are not available or if any command fails.
///
/// # Examples
///
/// ## Using default blocklist (hides common Rust stdlib symbols)
///
/// ```rust,no_run
/// use lib_patcher::{patch_lib, default_symbol_blocklist};
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("vendor/libthirdparty.a"),
///     Path::new("out"),
///     "thirdparty",
///     &default_symbol_blocklist(),
///     Path::new("libthirdparty_patched.a"),
///     None,
///     None,
/// );
/// ```
///
/// ## Using custom blocklist
///
/// ```rust,no_run
/// use lib_patcher::patch_lib;
/// use std::path::Path;
///
/// let symbols = vec!["rust_eh_personality".to_string(), "my_conflict".to_string()];
/// patch_lib(
///     Path::new("vendor/libthirdparty.a"),
///     Path::new("out"),
///     "thirdparty",
///     &symbols,
///     Path::new("libthirdparty_patched.a"),
///     None,
///     None,
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: Option<&str>,
    target_triplet: Option<&str>,
) {
    // Prefer the explicit triplet: when cross-compiling, the target OS differs
    // from both the host and CARGO_CFG_TARGET_OS (which is unset for the CLI).
    let target_os = target_triplet
        .and_then(target_os_from_triplet)
        .map(str::to_string)
        .or_else(|| env::var("CARGO_CFG_TARGET_OS").ok())
        .unwrap_or_else(|| {
            // Fall back to detecting the current OS if not in a cargo build context
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

    let protect_cross_object = protect_cross_object_symbols(&target_os, target_triplet);

    match target_os.as_str() {
        "windows" => patch_windows(
            static_lib,
            out_dir,
            lib_name,
            symbols_to_hide,
            final_lib,
            &final_arch,
            protect_cross_object,
        ),
        "macos" | "ios" | "tvos" | "visionos" => patch_apple(
            static_lib,
            out_dir,
            lib_name,
            symbols_to_hide,
            final_lib,
            &final_arch,
            target_triplet,
        ),
        _ => patch_linux(
            static_lib,
            out_dir,
            lib_name,
            symbols_to_hide,
            final_lib,
            &final_arch,
        ),
    }

    // Verify the patched library
    eprintln!("\nVerifying patched library...");
    if let Err(e) = verify_patched_lib(
        final_lib,
        static_lib,
        symbols_to_hide,
        &target_os,
        protect_cross_object,
    ) {
        eprintln!("\n❌ VERIFICATION FAILED: {}", e);
        eprintln!("The patched library may be corrupted or incomplete.");
        std::process::exit(1);
    }
}

// Windows: Parse and rewrite COFF object files
fn patch_windows(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: &str,
    protect_cross_object: bool,
) {
    let temp_dir = out_dir.join(format!("{}_objs", lib_name));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let archive_bytes = fs::read(static_lib).expect("Failed to read static lib");
    let archive = match object::read::archive::ArchiveFile::parse(&*archive_bytes) {
        Ok(a) => a,
        Err(e) => panic!("Failed to parse static lib as COFF archive: {}", e),
    };
    let mut obj_files = Vec::new();

    // Extract and patch each object file
    for member in archive.members() {
        let member = member.expect("Failed to read archive member");
        let data = member
            .data(archive_bytes.as_slice())
            .expect("Failed to read member data");

        let idx = obj_files.len();

        // Patch the COFF object file
        match patch_coff_object(&data, symbols_to_hide, protect_cross_object) {
            Ok(patched_data) => {
                let patched_path = temp_dir.join(format!("{}_patched.obj", idx));
                fs::write(&patched_path, patched_data).expect("Failed to write patched object");
                obj_files.push(patched_path);
            }
            Err(e) => {
                eprintln!("Warning: Skipping object ({})", e);
                // Skip files that can't be patched (e.g., import libs, LLVM bitcode)
            }
        }
    }

    // Determine which library tool to use and the machine type
    let lib_cmd = get_windows_lib_tool(Some(target_arch));

    // Create library
    // Convert final_lib to absolute path before changing directory
    let final_lib_abs = if final_lib.is_absolute() {
        final_lib.to_path_buf()
    } else {
        // Make relative path absolute by prepending current directory
        std::env::current_dir()
            .expect("Failed to get current directory")
            .join(final_lib)
    };

    let mut cmd = Command::new(&lib_cmd.tool);

    if lib_cmd.is_llvm {
        // LLVM-ar syntax (use llvm-ar instead of llvm-lib to avoid path issues)
        // Create archive with 'rc' flags: r=insert/replace, c=create
        cmd.arg("rc");
        cmd.arg(&final_lib_abs);
        cmd.current_dir(&temp_dir);

        for obj in &obj_files {
            // Use just the filename relative to temp_dir
            if let Some(filename) = obj.file_name() {
                cmd.arg(filename);
            }
        }
    } else {
        // MSVC lib.exe syntax
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!("/OUT:{}", final_lib_abs.display()));
        cmd.current_dir(&temp_dir);

        for obj in &obj_files {
            // Use just the filename relative to temp_dir
            if let Some(filename) = obj.file_name() {
                cmd.arg(filename);
            }
        }
    }

    let status = cmd.status().unwrap_or_else(|_| {
        panic!(
            "Failed to run {}. For cross-architecture patching, install LLVM tools.",
            lib_cmd.tool
        )
    });

    if !status.success() {
        eprintln!(
            "ERROR: {} failed with exit code: {:?}",
            lib_cmd.tool,
            status.code()
        );
        eprintln!("Temp directory kept for debugging: {}", temp_dir.display());
        panic!("{} failed", lib_cmd.tool);
    }

    // Keep temp dir for debugging
    // fs::remove_dir_all(&temp_dir).ok();
    eprintln!("Temp directory: {}", temp_dir.display());
}

struct WindowsLibTool {
    tool: String,
    machine_type: Option<String>,
    is_llvm: bool,
}

/// Determines the appropriate library tool for Windows
fn get_windows_lib_tool(target_arch: Option<&str>) -> WindowsLibTool {
    // Determine target architecture
    let target_arch_str = target_arch
        .map(|s| s.to_string())
        .or_else(|| env::var("CARGO_CFG_TARGET_ARCH").ok())
        .unwrap_or_else(|| {
            if cfg!(target_arch = "aarch64") {
                "aarch64".to_string()
            } else if cfg!(target_arch = "x86_64") {
                "x86_64".to_string()
            } else if cfg!(target_arch = "x86") {
                "x86".to_string()
            } else {
                std::env::consts::ARCH.to_string()
            }
        });

    let host_arch = std::env::consts::ARCH;

    // Map architecture to MSVC machine type
    let machine_type = match target_arch_str.as_str() {
        "aarch64" | "arm64" => Some("ARM64".to_string()),
        "x86_64" => Some("X64".to_string()),
        "x86" | "i686" => Some("X86".to_string()),
        "arm" => Some("ARM".to_string()),
        _ => None,
    };

    // Check if we're doing cross-architecture
    let is_cross = target_arch_str != host_arch;

    // Prefer MSVC lib.exe when available (produces COFF libraries accepted by link.exe)
    if Command::new("lib.exe").arg("/?").output().is_ok() {
        eprintln!("Using lib.exe for Windows build");
        return WindowsLibTool {
            tool: "lib.exe".to_string(),
            machine_type,
            is_llvm: false,
        };
    }

    // Next prefer llvm-lib (COFF-compatible, command-line compatible with lib.exe)
    if Command::new("llvm-lib").arg("/?").output().is_ok() {
        if is_cross {
            eprintln!(
                "Using llvm-lib for cross-architecture Windows build ({} -> {})",
                host_arch, target_arch_str
            );
        } else {
            eprintln!("Using llvm-lib for Windows build");
        }
        return WindowsLibTool {
            tool: "llvm-lib".to_string(),
            machine_type,
            is_llvm: false, // llvm-lib uses lib.exe-style flags
        };
    }

    // Fall back to llvm-ar as a last resort. Note: this produces GNU ar archives,
    // which MSVC link.exe may not accept. Prefer lib.exe/llvm-lib when possible.
    if Command::new("llvm-ar").arg("--version").output().is_ok() {
        if is_cross {
            eprintln!(
                "Using llvm-ar for cross-architecture Windows build ({} -> {})",
                host_arch, target_arch_str
            );
        } else {
            eprintln!("Using llvm-ar for Windows build (warning: produces GNU ar archives)");
        }
        return WindowsLibTool {
            tool: "llvm-ar".to_string(),
            machine_type,
            is_llvm: true,
        };
    }

    // No suitable tool found
    panic!(
        "No library archiver tool found for Windows. Please install one of:\n\
         1. MSVC Build Tools (recommended): provides lib.exe\n\
         2. LLVM tools: provides llvm-lib (preferred) or llvm-ar (fallback)"
    );
}

fn patch_coff_object(
    data: &[u8],
    symbols_to_hide: &[String],
    protect_cross_object: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // For Windows COFF, we patch the symbol table directly instead of rewriting
    // the entire object to preserve weak symbol auxiliary data and other COFF-specific info
    patch_coff_symbol_table(data, symbols_to_hide, protect_cross_object)
}

fn patch_coff_symbol_table(
    data: &[u8],
    symbols_to_hide: &[String],
    protect_cross_object: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use object::LittleEndian as LE;
    use object::pe;
    use object::read::coff::CoffHeader;

    // Parse COFF header
    let mut data = data.to_vec();

    // Check if this is big obj format by looking at the signature
    // Big obj format starts with: 00 00 FF FF (sig1), then 01 02 (sig2) for ANON_OBJECT_HEADER_BIGOBJ
    let is_bigobj = data.len() >= 4 && data[0..2] == [0x00, 0x00] && data[2..4] == [0xFF, 0xFF];

    if is_bigobj {
        return patch_coff_bigobj_symbol_table(data, symbols_to_hide, protect_cross_object);
    }

    let mut offset = 0u64;
    let header = pe::ImageFileHeader::parse(&data[..], &mut offset)?;

    let symbol_table_offset = header.pointer_to_symbol_table.get(LE) as usize;
    let symbol_count = header.number_of_symbols.get(LE) as usize;

    if symbol_table_offset == 0 || symbol_count == 0 {
        // No symbol table, return unchanged
        return Ok(data);
    }

    // Validate bounds
    let symbol_table_end = symbol_table_offset + (symbol_count * 18);
    if symbol_table_end > data.len() {
        return Err(format!(
            "Symbol table extends beyond file: {} > {}",
            symbol_table_end,
            data.len()
        )
        .into());
    }

    // Get string table offset (right after symbol table)
    let string_table_offset = symbol_table_end;

    let mut modified_count = 0;

    // Patch each symbol entry
    for i in 0..symbol_count {
        let symbol_offset = symbol_table_offset + (i * 18);

        // Bounds check
        if symbol_offset + 18 > data.len() {
            return Err(format!("Symbol {} extends beyond file", i).into());
        }

        // Read symbol entry (we need to re-borrow to avoid holding reference)
        let storage_class = data[symbol_offset + 16];

        // Check if this is a global symbol (storage class 2 = IMAGE_SYM_CLASS_EXTERNAL)
        if storage_class != 2 {
            // Skip this symbol and its auxiliary entries
            continue;
        }

        // Get symbol name
        let symbol_entry = &data[symbol_offset..symbol_offset + 18];

        // Get symbol name (first 8 bytes)
        let name = if symbol_entry[0..4] == [0, 0, 0, 0] {
            // Long name - read from string table
            let string_offset = u32::from_le_bytes([
                symbol_entry[4],
                symbol_entry[5],
                symbol_entry[6],
                symbol_entry[7],
            ]) as usize;
            if string_table_offset + string_offset >= data.len() {
                return Err(format!(
                    "String table offset out of bounds: {}",
                    string_table_offset + string_offset
                )
                .into());
            }
            read_coff_string(&data, string_table_offset + string_offset)?
        } else {
            // Short name - inline in symbol table
            let end = symbol_entry[0..8].iter().position(|&b| b == 0).unwrap_or(8);
            String::from_utf8_lossy(&symbol_entry[0..end]).to_string()
        };

        // Check if symbol should remain global
        let is_special = name.starts_with('@');

        // SectionNumber (i16 at offset 12); 0 == IMAGE_SYM_UNDEFINED, i.e. only
        // referenced here, not defined. Localizing it would produce a malformed
        // `Static` + section-0 entry, so always keep undefined symbols `External`.
        let section_number = i16::from_le_bytes([data[symbol_offset + 12], data[symbol_offset + 13]]);
        let is_undefined = section_number == 0;

        let matches_filter = !symbols_to_hide.iter().any(|pattern| {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                name.starts_with(prefix)
            } else {
                &name == pattern
            }
        });

        // Keep global unless it's a defined symbol that matches the hide list and
        // isn't otherwise protected (special `@`-symbol, undefined reference, or a
        // cross-object symbol when that protection is active).
        let keep_global = is_special
            || is_undefined
            || (protect_cross_object && coff_must_stay_global(&name))
            || matches_filter;

        if !keep_global {
            // Change storage class to 3 (IMAGE_SYM_CLASS_STATIC = local/private)
            data[symbol_offset + 16] = 3;
            modified_count += 1;
            eprintln!("  Hiding symbol: {}", name);
        }

        // Note: We iterate through all symbol table entries. Auxiliary symbol entries
        // will have storage_class != 2, so they'll be skipped above
    }

    eprintln!("Modified {} symbols in object", modified_count);
    Ok(data)
}

fn patch_coff_bigobj_symbol_table(
    mut data: Vec<u8>,
    symbols_to_hide: &[String],
    protect_cross_object: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // Big obj format has a 56-byte header
    // Offset 48: pointer to symbol table (u32)
    // Offset 52: number of symbols (u32)
    if data.len() < 56 {
        return Err("File too small for big obj format".into());
    }

    let symbol_table_offset = u32::from_le_bytes([data[48], data[49], data[50], data[51]]) as usize;
    let symbol_count = u32::from_le_bytes([data[52], data[53], data[54], data[55]]) as usize;

    if symbol_table_offset == 0 || symbol_count == 0 {
        return Ok(data);
    }

    // Big obj format uses 20-byte symbol entries (instead of 18)
    let symbol_table_end = symbol_table_offset + (symbol_count * 20);
    if symbol_table_end > data.len() {
        return Err(format!(
            "Symbol table extends beyond file: {} > {}",
            symbol_table_end,
            data.len()
        )
        .into());
    }

    let string_table_offset = symbol_table_end;

    let mut modified_count = 0;

    for i in 0..symbol_count {
        let symbol_offset = symbol_table_offset + (i * 20);

        if symbol_offset + 20 > data.len() {
            return Err(format!("Symbol {} extends beyond file", i).into());
        }

        // In big obj format:
        // Bytes 0-7: Name (8 bytes)
        // Bytes 8-11: Value (4 bytes)
        // Bytes 12-15: SectionNumber (4 bytes, not 2!)
        // Bytes 16-17: Type (2 bytes)
        // Byte 18: StorageClass
        // Byte 19: NumberOfAuxSymbols

        let storage_class = data[symbol_offset + 18];

        if storage_class != 2 {
            continue;
        }

        // Get symbol name
        let symbol_entry = &data[symbol_offset..symbol_offset + 20];
        let name = if symbol_entry[0..4] == [0, 0, 0, 0] {
            let string_offset = u32::from_le_bytes([
                symbol_entry[4],
                symbol_entry[5],
                symbol_entry[6],
                symbol_entry[7],
            ]) as usize;
            if string_table_offset + string_offset >= data.len() {
                return Err(format!(
                    "String table offset out of bounds: {}",
                    string_table_offset + string_offset
                )
                .into());
            }
            read_coff_string(&data, string_table_offset + string_offset)?
        } else {
            let end = symbol_entry[0..8].iter().position(|&b| b == 0).unwrap_or(8);
            String::from_utf8_lossy(&symbol_entry[0..end]).to_string()
        };

        let is_special = name.starts_with('@');

        // SectionNumber here is a 4-byte (i32) field at offset 12; 0 == undefined.
        // See the note in `patch_coff_symbol_table`.
        let section_number = i32::from_le_bytes([
            data[symbol_offset + 12],
            data[symbol_offset + 13],
            data[symbol_offset + 14],
            data[symbol_offset + 15],
        ]);
        let is_undefined = section_number == 0;

        let matches_filter = !symbols_to_hide.iter().any(|pattern| {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                name.starts_with(prefix)
            } else {
                &name == pattern
            }
        });

        // See the note in `patch_coff_symbol_table` for the keep-global rule.
        let keep_global = is_special
            || is_undefined
            || (protect_cross_object && coff_must_stay_global(&name))
            || matches_filter;

        if !keep_global {
            data[symbol_offset + 18] = 3; // IMAGE_SYM_CLASS_STATIC
            modified_count += 1;
            eprintln!("  Hiding symbol (bigobj): {}", name);
        }
    }

    eprintln!("Modified {} symbols in bigobj", modified_count);
    Ok(data)
}

fn read_coff_string(data: &[u8], offset: usize) -> Result<String, Box<dyn std::error::Error>> {
    let end = data[offset..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(data.len() - offset);
    Ok(String::from_utf8_lossy(&data[offset..offset + end]).to_string())
}

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

/// Returns the `ld -platform_version` arguments for an Apple target triplet.
///
/// Returns `(platform, min_version, sdk_version)`. The sdk_version is set to a
/// recent-enough value; ld is lenient about it during partial (`-r`) links.
fn apple_platform_version(triplet: Option<&str>, arch: &str) -> (&'static str, &'static str, &'static str) {
    let Some(triplet) = triplet else {
        return if arch == "arm64" {
            ("macos", "11.0", "14.0")
        } else {
            ("macos", "10.13", "14.0")
        };
    };

    if triplet.contains("apple-tvos") {
        return if triplet.contains("-sim") {
            ("tvos-simulator", "15.0", "17.0")
        } else {
            ("tvos", "15.0", "17.0")
        };
    }

    if triplet.contains("apple-visionos") {
        return if triplet.contains("-sim") {
            ("xros-simulator", "1.0", "2.0")
        } else {
            ("xros", "1.0", "2.0")
        };
    }

    if triplet.contains("apple-ios") {
        if triplet.ends_with("-sim") {
            return ("ios-simulator", "15.0", "17.0");
        } else if triplet.ends_with("-macabi") {
            return ("mac-catalyst", "15.0", "17.0");
        } else {
            return ("ios", "15.0", "17.0");
        }
    }

    if arch == "arm64" {
        ("macos", "11.0", "14.0")
    } else {
        ("macos", "10.13", "14.0")
    }
}

// Apple (macOS / iOS / tvOS / visionOS): Use ld -r with exported_symbols_list
fn patch_apple(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: &str,
    triplet: Option<&str>,
) {
    // Map Rust architecture names to ld arch names
    let arch = match target_arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let (platform, min_ver, sdk_ver) = apple_platform_version(triplet, arch);

    let temp_obj_dir = out_dir.join(format!("{}_objs", lib_name));
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let symbols_file = out_dir.join("symbols.txt");

    // Extract all object files from the archive
    // This is necessary because ld -r -all_load on macOS doesn't preserve all symbols correctly
    fs::create_dir_all(&temp_obj_dir).expect("Failed to create temp object directory");

    // Convert static_lib to absolute path for ar extraction
    let static_lib_abs = if static_lib.is_absolute() {
        static_lib.to_path_buf()
    } else {
        env::current_dir()
            .expect("Failed to get current directory")
            .join(static_lib)
    };

    let extract_status = Command::new("ar")
        .arg("x")
        .arg(&static_lib_abs)
        .current_dir(&temp_obj_dir)
        .status()
        .expect("Failed to run ar extract");

    if !extract_status.success() {
        panic!("ar extract failed");
    }

    // Collect all extracted object files
    let obj_files: Vec<_> = fs::read_dir(&temp_obj_dir)
        .expect("Failed to read temp object directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("o") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    if obj_files.is_empty() {
        panic!("No object files found in archive");
    }

    // Create the intermediate object with ld -r, linking all extracted object files
    // Use xcrun to ensure we get the right ld that supports the target architecture
    let mut ld_cmd = Command::new("xcrun");
    ld_cmd
        .arg("ld")
        .arg("-arch").arg(arch)
        .arg("-r")
        .arg("-platform_version").arg(platform).arg(min_ver).arg(sdk_ver)
        .arg("-o")
        .arg(&intermediate);

    for obj in &obj_files {
        ld_cmd.arg(obj);
    }

    let output = ld_cmd.output().expect("Failed to run xcrun ld");

    if !output.stderr.is_empty() {
        eprintln!("ld stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.stdout.is_empty() {
        eprintln!("ld stdout: {}", String::from_utf8_lossy(&output.stdout));
    }

    if !output.status.success() {
        panic!("ld -r failed");
    }

    // Now get all symbols from the intermediate object (after linking)
    let nm_out = Command::new("xcrun")
        .arg("nm")
        .args(["-g", "-U"])
        .arg(&intermediate)
        .output()
        .expect("Failed to run xcrun nm");

    if !nm_out.status.success() {
        eprintln!("nm stderr: {}", String::from_utf8_lossy(&nm_out.stderr));
        panic!("nm failed on intermediate object");
    }

    let nm_stdout = String::from_utf8_lossy(&nm_out.stdout);

    let mut all_symbols: Vec<String> = nm_stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[1].chars().any(|c| c.is_uppercase()) {
                Some(parts[2].to_string())
            } else {
                None
            }
        })
        .collect();

    // Remove duplicates
    all_symbols.sort();
    all_symbols.dedup();

    // Filter symbols based on symbols_to_hide
    let symbols_to_keep: Vec<String> = all_symbols
        .into_iter()
        .filter(|sym| {
            // Remove both with and without underscore prefix
            let without_underscore = sym.strip_prefix('_').unwrap_or(sym);
            !symbols_to_hide.contains(sym)
                && !symbols_to_hide.iter().any(|r| r == without_underscore)
        })
        .collect();

    if symbols_to_keep.is_empty() {
        eprintln!("Warning: All symbols were removed. This may not be intended.");
    }

    fs::write(&symbols_file, symbols_to_keep.join("\n")).expect("Failed to write symbols file");

    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Filter symbols
    let status = Command::new("xcrun")
        .arg("ld")
        .arg("-arch").arg(arch)
        .arg("-r")
        .arg("-platform_version").arg(platform).arg(min_ver).arg(sdk_ver)
        .arg("-o").arg(&final_obj)
        .arg("-exported_symbols_list").arg(&symbols_file)
        .arg(&intermediate)
        .status()
        .expect("Failed to run xcrun ld filter");
    assert!(status.success(), "ld symbol filter failed");

    // Create archive
    let status = Command::new("ar")
        .args(["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status()
        .expect("Failed to run ar");
    assert!(status.success(), "ar failed");

    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();
    fs::remove_file(&symbols_file).ok();
    fs::remove_dir_all(&temp_obj_dir).ok();
}

// Linux/Android: Use ld -r + objcopy
fn patch_linux(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: &str,
) {
    // Determine the appropriate binutils prefix for cross-compilation
    let (ld_cmd, objcopy_cmd, ar_cmd) = get_linux_toolchain(target_arch);

    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Partial link
    let status = Command::new(&ld_cmd)
        .args(["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib)
        .status()
        .unwrap_or_else(|_| panic!("Failed to run {}", ld_cmd));
    assert!(status.success(), "{} -r failed", ld_cmd);

    // Filter symbols - use objcopy to localize specific symbols
    let mut cmd = Command::new(&objcopy_cmd);
    for symbol in symbols_to_hide {
        cmd.arg("--localize-symbol").arg(symbol);
    }
    cmd.arg(&intermediate).arg(&final_obj);
    let status = cmd
        .status()
        .unwrap_or_else(|_| panic!("Failed to run {}", objcopy_cmd));
    assert!(status.success(), "{} failed", objcopy_cmd);

    // Create archive (try specified ar, fallback to llvm-ar)
    let ar_result = Command::new(&ar_cmd)
        .args(["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status();

    if !ar_result.map(|s| s.success()).unwrap_or(false) {
        let status = Command::new("llvm-ar")
            .args(["rcs"])
            .arg(final_lib)
            .arg(&final_obj)
            .status()
            .expect("Failed to run llvm-ar");
        assert!(status.success(), "Both {} and llvm-ar failed", ar_cmd);
    }

    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();
}

/// Determines the appropriate toolchain for the target architecture on Linux
///
/// Returns a tuple of (ld_command, objcopy_command, ar_command)
fn get_linux_toolchain(target_arch: &str) -> (String, String, String) {
    let host_arch = std::env::consts::ARCH;

    // If targeting the same architecture as host, use native tools
    if target_arch == host_arch {
        return ("ld".to_string(), "objcopy".to_string(), "ar".to_string());
    }

    // For cross-compilation, determine the GNU triplet prefix
    let triplet_prefix = match target_arch {
        "aarch64" | "arm64" => "aarch64-linux-gnu",
        "arm" | "armv7" => "arm-linux-gnueabihf",
        "x86_64" => "x86_64-linux-gnu",
        "x86" | "i686" => "i686-linux-gnu",
        "riscv64" => "riscv64-linux-gnu",
        "powerpc64" => "powerpc64-linux-gnu",
        "powerpc64le" => "powerpc64le-linux-gnu",
        "s390x" => "s390x-linux-gnu",
        _ => {
            eprintln!(
                "Warning: Unknown target architecture '{}', falling back to native tools",
                target_arch
            );
            return ("ld".to_string(), "objcopy".to_string(), "ar".to_string());
        }
    };

    let ld = format!("{}-ld", triplet_prefix);
    let objcopy = format!("{}-objcopy", triplet_prefix);
    let ar = format!("{}-ar", triplet_prefix);

    // Check if cross-compilation tools exist, fallback to native if not
    if Command::new(&ld).arg("--version").output().is_ok() {
        eprintln!("Using cross-compilation toolchain: {}-*", triplet_prefix);
        (ld, objcopy, ar)
    } else {
        eprintln!(
            "Warning: Cross-compilation tools for {} not found (tried {}), falling back to native tools",
            target_arch, ld
        );
        eprintln!(
            "To install: sudo apt-get install binutils-{}",
            triplet_prefix
        );
        ("ld".to_string(), "objcopy".to_string(), "ar".to_string())
    }
}

/// Verifies that a patched library meets the expected criteria
///
/// # Arguments
///
/// * `static_lib` - Path to the patched static library
/// * `original_lib` - Path to the original static library (for size comparison)
/// * `symbols_to_hide` - The list of symbols that should be hidden
///
/// # Returns
///
/// A Result with Ok(()) if verification passes, or an error message if it fails
fn verify_patched_lib(
    static_lib: &Path,
    original_lib: &Path,
    symbols_to_hide: &[String],
    target_os: &str,
    protect_cross_object: bool,
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
    // Windows may have smaller output due to skipping unpatchable LLVM bitcode objects
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
            "Output file is suspiciously small ({} bytes vs {} bytes input, {:.2}% of original). This may indicate a patching error.",
            output_size, input_size, size_ratio * 100.0
        ).into());
    }

    // 3. Verify it's a valid archive by listing symbols
    let symbols = list_symbols(static_lib)?;

    if symbols.is_empty() {
        return Err("Output library contains no symbols - patching may have failed".into());
    }

    // 4. Verify the filtering worked as expected
    // Check that blocked symbols are either gone or made local
    // (We can't easily check if they're local vs gone, but at least they shouldn't be global)
    // Cross-object symbols (e.g. `ring_core_*`, `rust_eh_personality`) are
    // deliberately kept global on the Windows GNU path, so exempt them there.
    let still_global: Vec<_> = symbols
        .iter()
        .filter(|s| symbols_to_hide.contains(&s.to_string()))
        .filter(|s| !(protect_cross_object && coff_must_stay_global(s)))
        .collect();

    if !still_global.is_empty() {
        return Err(format!(
            "The following symbols are still global after patching: {}",
            still_global
                .iter()
                .take(5)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    eprintln!(
        "  ✓ Verified {} symbols are no longer global",
        symbols_to_hide.len()
    );

    eprintln!(
        "  ✓ Output library contains {} total public symbols",
        symbols.len()
    );

    Ok(())
}

/// Filters symbols from a static library by prefix.
///
/// Returns all public symbols that start with any of the given prefixes.
///
/// # Arguments
///
/// * `static_lib` - Path to the static library (e.g., `libmylib.a`, `mylib.lib`)
/// * `prefixes` - List of prefixes to filter by
///
/// # Returns
///
/// A Result containing a sorted Vec of symbol names that match any prefix.
///
/// # Examples
///
/// ```rust,no_run
/// use lib_patcher::filter_symbols_by_prefix;
/// use std::path::Path;
///
/// let prefixes = vec!["_Z".to_string(), "rust_".to_string()];
/// let symbols = filter_symbols_by_prefix(Path::new("libmylib.a"), &prefixes).unwrap();
/// for sym in symbols {
///     println!("{}", sym);
/// }
/// ```
pub fn filter_symbols_by_prefix(
    static_lib: &Path,
    prefixes: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let all_symbols = list_symbols(static_lib)?;
    
    let filtered: Vec<String> = all_symbols
        .into_iter()
        .filter(|sym| {
            prefixes.iter().any(|prefix| {
                // Strip leading underscore for macOS compatibility
                let sym_without_underscore = sym.strip_prefix('_').unwrap_or(sym);
                sym.starts_with(prefix) || sym_without_underscore.starts_with(prefix)
            })
        })
        .collect();
    
    Ok(filtered)
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
    //! Regression tests for the Windows/COFF symbol-localization path.
    //!
    //! They reproduce the `ring` static-link failure: an asm routine (`ring_core_*`)
    //! defined in one object and referenced from a sibling. The old code localized it
    //! in both, turning the reference into a malformed `Static` + `SectionNumber = 0`
    //! entry that lld rejects and GNU ld binds to address 0.

    use super::*;
    use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind,
        SymbolScope,
    };

    const RING_SYM: &str = "ring_core_0_17_14__sha256_block_data_order_hw";

    /// `(name, storage_class, section_number)` for every primary symbol entry,
    /// parsed straight from the COFF symbol table (auxiliary entries skipped).
    fn coff_symbols(data: &[u8]) -> Vec<(String, u8, i16)> {
        use object::LittleEndian as LE;
        use object::pe;
        use object::read::coff::CoffHeader;

        let mut offset = 0u64;
        let header = pe::ImageFileHeader::parse(data, &mut offset).unwrap();
        let sym_off = header.pointer_to_symbol_table.get(LE) as usize;
        let count = header.number_of_symbols.get(LE) as usize;
        let str_off = sym_off + count * 18;

        let mut out = Vec::new();
        let mut i = 0;
        while i < count {
            let so = sym_off + i * 18;
            let entry = &data[so..so + 18];
            let storage = entry[16];
            let section = i16::from_le_bytes([entry[12], entry[13]]);
            let n_aux = entry[17] as usize;
            let name = if entry[0..4] == [0, 0, 0, 0] {
                let strofs = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]) as usize;
                read_coff_string(data, str_off + strofs).unwrap()
            } else {
                let end = entry[0..8].iter().position(|&b| b == 0).unwrap_or(8);
                String::from_utf8_lossy(&entry[0..end]).to_string()
            };
            out.push((name, storage, section));
            i += 1 + n_aux;
        }
        out
    }

    /// Object A: *defines* `name` in `.text` as an external (global) symbol,
    /// mirroring the ring object that contains the asm routine body.
    fn make_def_object(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        let off = obj.append_section_data(text, &[0x90, 0x90, 0xc3], 16); // nop; nop; ret
        obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off,
            size: 3,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// Object B: *references* `name` via a REL32 relocation (an external undefined
    /// symbol), mirroring the ring object that calls into the asm routine.
    fn make_ref_object(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        // e8 <rel32> = call rel32; c3 = ret. The rel32 placeholder is relocated.
        let off = obj.append_section_data(text, &[0xe8, 0, 0, 0, 0, 0xc3], 16);
        let sym = obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        obj.add_relocation(
            text,
            Relocation {
                offset: off + 1,
                symbol: sym,
                addend: -4,
                flags: RelocationFlags::Coff {
                    typ: object::pe::IMAGE_REL_AMD64_REL32,
                },
            },
        )
        .unwrap();
        obj.write().unwrap()
    }

    fn find<'a>(syms: &'a [(String, u8, i16)], name: &str) -> &'a (String, u8, i16) {
        syms.iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("symbol `{name}` not found in patched object"))
    }

    /// The structural invariant: a `Static` (3) symbol in section 0 is malformed.
    /// This is the precise, tool-agnostic signature of the bug.
    fn assert_no_static_undefined(syms: &[(String, u8, i16)]) {
        for (n, sc, sec) in syms {
            assert!(
                !(*sc == 3 && *sec == 0),
                "symbol `{n}` is StorageClass=Static + SectionNumber=0 (malformed undefined)"
            );
        }
    }

    #[test]
    fn ring_intra_library_symbols_stay_global() {
        // The consumer hides ring's symbols to avoid clashing with its own copy.
        let hide = vec!["ring_core_*".to_string()];

        // Defining object: must stay External and in its section so siblings bind.
        let def = patch_coff_object(&make_def_object(RING_SYM), &hide, true).unwrap();
        let def_syms = coff_symbols(&def);
        assert_no_static_undefined(&def_syms);
        let (_, sc, sec) = find(&def_syms, RING_SYM);
        assert_eq!(*sc, 2, "ring_core definition must stay External (global)");
        assert!(*sec > 0, "ring_core definition must stay in its section");

        // Referencing object: must stay External undefined (section 0), not Static.
        let r = patch_coff_object(&make_ref_object(RING_SYM), &hide, true).unwrap();
        let ref_syms = coff_symbols(&r);
        assert_no_static_undefined(&ref_syms);
        let (_, sc, sec) = find(&ref_syms, RING_SYM);
        assert_eq!(*sc, 2, "ring_core reference must stay External (global)");
        assert_eq!(*sec, 0, "ring_core reference must remain undefined");
    }

    #[test]
    fn rust_eh_personality_stays_global() {
        // In the default blocklist, but referenced cross-object by every landing
        // pad, so it must not be localized on the COFF path.
        let hide = default_symbol_blocklist();
        let r = patch_coff_object(&make_ref_object("rust_eh_personality"), &hide, true).unwrap();
        let syms = coff_symbols(&r);
        assert_no_static_undefined(&syms);
        let (_, sc, sec) = find(&syms, "rust_eh_personality");
        assert_eq!(*sc, 2, "rust_eh_personality reference must stay External");
        assert_eq!(*sec, 0);
    }

    #[test]
    fn undefined_reference_is_never_localized() {
        // Holds even with cross-object protection off: an undefined reference is
        // never localized, since that is what creates the malformed entry.
        let hide = vec!["_ZN3std6foobar".to_string()];
        let name = "_ZN3std6foobar";
        let r = patch_coff_object(&make_ref_object(name), &hide, false).unwrap();
        let syms = coff_symbols(&r);
        assert_no_static_undefined(&syms);
        let (_, sc, sec) = find(&syms, name);
        assert_eq!(*sc, 2, "undefined reference must stay External");
        assert_eq!(*sec, 0);
    }

    #[test]
    fn ordinary_defined_symbol_is_still_localized() {
        // Guard that the fix didn't disable hiding: a defined, non-protected symbol
        // on the hide list must still be flipped to Static.
        let hide = vec!["unpatched_function".to_string()];
        let name = "unpatched_function";
        let def = patch_coff_object(&make_def_object(name), &hide, false).unwrap();
        let syms = coff_symbols(&def);
        assert_no_static_undefined(&syms); // defined (section > 0), so not malformed
        let (_, sc, sec) = find(&syms, name);
        assert_eq!(*sc, 3, "a defined, non-protected hidden symbol must become Static");
        assert!(*sec > 0);
    }

    #[test]
    fn cross_object_protection_gates_on_windows_gnu_abi() {
        let gnu = |t| protect_cross_object_symbols("windows", Some(t));
        assert!(gnu("x86_64-pc-windows-gnu"));
        assert!(gnu("x86_64-pc-windows-gnullvm"));
        assert!(!gnu("x86_64-pc-windows-msvc"));
        // Off for non-Windows and when the triplet is unknown.
        assert!(!protect_cross_object_symbols("linux", Some("x86_64-unknown-linux-gnu")));
        assert!(!protect_cross_object_symbols("windows", None));
    }

    #[test]
    fn ring_definition_is_localized_without_protection() {
        // On non-GNU Windows (e.g. MSVC) the protection is off, so a hide-list match
        // is localized as usual; the `is_undefined` guard still prevents the
        // malformed entry on the reference side.
        let hide = vec!["ring_core_*".to_string()];

        let def = patch_coff_object(&make_def_object(RING_SYM), &hide, false).unwrap();
        let def_syms = coff_symbols(&def);
        assert_no_static_undefined(&def_syms);
        assert_eq!(find(&def_syms, RING_SYM).1, 3, "definition must localize to Static");

        let r = patch_coff_object(&make_ref_object(RING_SYM), &hide, false).unwrap();
        let ref_syms = coff_symbols(&r);
        assert_no_static_undefined(&ref_syms);
        let (_, sc, sec) = find(&ref_syms, RING_SYM);
        assert_eq!(*sc, 2, "reference must stay External undefined, not Static+section0");
        assert_eq!(*sec, 0);
    }
}
