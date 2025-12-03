use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::ObjectComdat;
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
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: Option<&str>,
) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_else(|_| {
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

    match target_os.as_str() {
        "windows" => patch_windows(
            static_lib,
            out_dir,
            lib_name,
            symbols_to_hide,
            final_lib,
            &final_arch,
        ),
        "macos" | "ios" => patch_macos(
            static_lib,
            out_dir,
            lib_name,
            symbols_to_hide,
            final_lib,
            &final_arch,
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
    if let Err(e) = verify_patched_lib(final_lib, static_lib, symbols_to_hide, &target_os) {
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
        match patch_coff_object(&data, symbols_to_hide) {
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
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // For Windows COFF, we patch the symbol table directly instead of rewriting
    // the entire object to preserve weak symbol auxiliary data and other COFF-specific info
    patch_coff_symbol_table(data, symbols_to_hide)
}

fn patch_coff_symbol_table(
    data: &[u8],
    symbols_to_hide: &[String],
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
        return patch_coff_bigobj_symbol_table(data, symbols_to_hide);
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

    // Collect COMDAT symbols that should stay global
    let file = File::parse(data.as_slice())?;
    let mut comdat_symbols = HashSet::new();
    for comdat in file.comdats() {
        comdat_symbols.insert(comdat.symbol().0);
    }

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

        let matches_filter = !symbols_to_hide.iter().any(|pattern| {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                name.starts_with(prefix)
            } else {
                &name == pattern
            }
        });

        // Keep COMDAT leader symbols global to preserve linker selection semantics,
        // UNLESS they're explicitly in the blocklist (e.g., Rust stdlib symbols)
        let is_comdat = comdat_symbols.contains(&i);
        let keep_global = is_special || (is_comdat && matches_filter) || matches_filter;

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

    // Collect COMDAT symbols
    let file = File::parse(data.as_slice())?;
    let mut comdat_symbols = HashSet::new();
    for comdat in file.comdats() {
        comdat_symbols.insert(comdat.symbol().0);
    }

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

        let matches_filter = !symbols_to_hide.iter().any(|pattern| {
            if pattern.ends_with('*') {
                let prefix = &pattern[..pattern.len() - 1];
                name.starts_with(prefix)
            } else {
                &name == pattern
            }
        });

        // Keep COMDAT leader symbols global to preserve linker selection semantics,
        // UNLESS they're explicitly in the blocklist (e.g., Rust stdlib symbols)
        let is_comdat = comdat_symbols.contains(&i);
        let keep_global = is_special || (is_comdat && matches_filter) || matches_filter;

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

// macOS/iOS: Use ld -r with exported_symbols_list
fn patch_macos(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbols_to_hide: &[String],
    final_lib: &Path,
    target_arch: &str,
) {
    // Map Rust architecture names to macOS ld names
    let arch = match target_arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

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
        .arg("-arch")
        .arg(arch)
        .arg("-r")
        .arg("-platform_version")
        .arg("macos")
        .arg(if arch == "arm64" { "11.0" } else { "10.13" })
        .arg("14.0")
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
        .args(["-arch", arch, "-r", "-o"])
        .arg(&final_obj)
        .arg("-exported_symbols_list")
        .arg(&symbols_file)
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
    let still_global: Vec<_> = symbols
        .iter()
        .filter(|s| symbols_to_hide.contains(&s.to_string()))
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
