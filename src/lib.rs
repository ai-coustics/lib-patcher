use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::File;
use object::write::{Object as WriteObject, Relocation, Symbol, SymbolSection};
use object::{
    Object as ObjectTrait, ObjectSection, ObjectSymbol, RelocationTarget, SectionKind, SymbolFlags,
    SymbolKind,
};

/// Filtering strategy for symbol visibility
#[derive(Debug, Clone)]
pub enum FilterMode {
    /// Keep ONLY symbols matching the prefix. Hide everything else.
    ///
    /// Use this when you control the library and want maximum safety.
    /// All public functions MUST start with your prefix.
    Allowlist { prefix: String },

    /// Remove ONLY the listed symbols. Keep everything else.
    ///
    /// Use this for third-party libraries where you can't change function names.
    Blocklist { remove: Vec<String> },
}

impl FilterMode {
    /// Default blocklist of common problematic Rust stdlib symbols
    ///
    /// Includes:
    /// - `rust_eh_personality` - Exception handling (main conflict source)
    /// - `__rust_no_alloc_shim_is_unstable` - Allocation shim marker
    /// - `__rust_alloc`, `__rust_dealloc`, `__rust_realloc` - Allocator functions
    /// - `__rust_alloc_zeroed` - Zero-initialized allocation
    ///
    /// These symbols commonly conflict when linking multiple Rust staticlibs.
    pub fn default_blocklist() -> Self {
        FilterMode::Blocklist {
            remove: vec![
                "rust_eh_personality".to_string(),
                "__rust_no_alloc_shim_is_unstable".to_string(),
                "__rust_alloc".to_string(),
                "__rust_dealloc".to_string(),
                "__rust_realloc".to_string(),
                "__rust_alloc_zeroed".to_string(),
                "__rust_alloc_error_handler".to_string(),
            ],
        }
    }
}

/// Patches a static library to filter symbols based on the specified mode.
///
/// # Arguments
///
/// * `static_lib` - Path to the input static library (e.g., `libmylib.a`)
/// * `out_dir` - Directory for temporary files (use `$OUT_DIR` in build.rs)
/// * `lib_name` - Base name for temporary files (e.g., "mylib")
/// * `mode` - Filtering mode (Allowlist or Blocklist)
/// * `final_lib` - Path where the patched library will be written
/// * `target_arch` - Optional target architecture (e.g., "aarch64", "x86_64"). If `None`, uses host architecture.
///
/// # Panics
///
/// Panics if required platform tools are not available or if any command fails.
///
/// # Examples
///
/// ## Allowlist mode (library you control)
///
/// ```rust,no_run
/// use lib_patcher::{patch_lib, FilterMode};
/// use std::path::Path;
///
/// // Native architecture
/// patch_lib(
///     Path::new("target/release/libmylib.a"),
///     Path::new("out"),
///     "mylib",
///     FilterMode::Allowlist { prefix: "mylib_".to_string() },
///     Path::new("libmylib_patched.a"),
///     None,
/// );
///
/// // Cross-architecture (e.g., arm64 library on x86_64 host)
/// patch_lib(
///     Path::new("target/aarch64-unknown-linux-gnu/release/libmylib.a"),
///     Path::new("out"),
///     "mylib",
///     FilterMode::Allowlist { prefix: "mylib_".to_string() },
///     Path::new("libmylib_patched.a"),
///     Some("aarch64"),
/// );
/// ```
///
/// ## Blocklist mode (third-party library)
///
/// ```rust,no_run
/// use lib_patcher::{patch_lib, FilterMode};
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("vendor/libthirdparty.a"),
///     Path::new("out"),
///     "thirdparty",
///     FilterMode::default_blocklist(),
///     Path::new("libthirdparty_patched.a"),
///     None,
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: FilterMode,
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

    match target_os.as_str() {
        "windows" => patch_windows(static_lib, out_dir, lib_name, &mode, final_lib, target_arch),
        "macos" | "ios" => {
            patch_macos(static_lib, out_dir, lib_name, &mode, final_lib, target_arch)
        }
        _ => patch_linux(static_lib, out_dir, lib_name, &mode, final_lib, target_arch),
    }
    // Verify the patched library
    eprintln!("\nVerifying patched library...");
    if let Err(e) = verify_patched_lib(final_lib, static_lib, &mode) {
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
    mode: &FilterMode,
    final_lib: &Path,
    target_arch: Option<&str>,
) {
    use std::io::Read;

    let temp_dir = out_dir.join(format!("{}_objs", lib_name));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let archive_file = fs::File::open(static_lib).expect("Failed to open static lib");
    let mut archive = ar::Archive::new(archive_file);
    let mut obj_files = Vec::new();

    // Extract and patch each object file
    while let Some(Ok(mut entry)) = archive.next_entry() {
        let mut data = Vec::new();
        entry.read_to_end(&mut data).expect("Failed to read entry");

        let patched = match patch_coff_object(&data, mode) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let out_path = temp_dir.join(format!("{}.obj", obj_files.len()));
        fs::write(&out_path, patched).expect("Failed to write object");
        obj_files.push(out_path);
    }

    // Determine which library tool to use and the machine type
    let lib_cmd = get_windows_lib_tool(target_arch);

    // Create library
    let mut cmd = Command::new(&lib_cmd.tool);

    if lib_cmd.is_llvm {
        // llvm-ar uses standard ar syntax
        cmd.arg("rcs");
        cmd.arg(final_lib);
    } else {
        // MSVC lib.exe syntax
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!("/OUT:{}", final_lib.display()));
    }

    for obj in &obj_files {
        cmd.arg(obj);
    }

    let status = cmd.status().unwrap_or_else(|_| {
        panic!(
            "Failed to run {}. For cross-architecture patching, install LLVM tools.",
            lib_cmd.tool
        )
    });
    assert!(status.success(), "{} failed", lib_cmd.tool);

    fs::remove_dir_all(&temp_dir).ok();
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

    // Try to use llvm-ar first (available via rustup component add llvm-tools-preview)
    // This works for both native and cross-compilation scenarios
    if Command::new("llvm-ar").arg("--version").output().is_ok() {
        if is_cross {
            eprintln!(
                "Using llvm-ar for cross-architecture Windows build ({} -> {})",
                host_arch, target_arch_str
            );
        } else {
            eprintln!("Using llvm-ar for Windows build");
        }
        return WindowsLibTool {
            tool: "llvm-ar".to_string(),
            machine_type,
            is_llvm: true,
        };
    }

    // Fall back to lib.exe (requires MSVC Build Tools)
    if Command::new("lib.exe").arg("/?").output().is_ok() {
        eprintln!("Using lib.exe for Windows build");
        return WindowsLibTool {
            tool: "lib.exe".to_string(),
            machine_type,
            is_llvm: false,
        };
    }

    // No suitable tool found
    panic!(
        "No library archiver tool found for Windows. Please install one of:\n\
         1. LLVM tools (recommended): rustup component add llvm-tools-preview\n\
         2. MSVC Build Tools: https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022"
    );
}

fn patch_coff_object(
    data: &[u8],
    mode: &FilterMode,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::collections::HashMap;

    let file = File::parse(data)?;
    let mut writer = WriteObject::new(file.format(), file.architecture(), file.endianness());

    let mut section_map = HashMap::new();
    let mut symbol_map = HashMap::new();
    let mut section_sym_map = HashMap::new();

    // Copy sections and create section symbols
    for section in file.sections() {
        let name_bytes = match section.name_bytes() {
            Ok(n) if !n.is_empty() => n,
            _ => continue,
        };
        let name = name_bytes.to_vec();
        let kind = section.kind();
        let id = writer.add_section(Vec::new(), name.clone(), kind);

        let align = section.align();
        if kind != SectionKind::UninitializedData
            && let Ok(data) = section.uncompressed_data()
        {
            let data_bytes = data.into_owned();
            if !data_bytes.is_empty() {
                writer.section_mut(id).set_data(data_bytes, align);
            }
        }

        // Create section symbol
        let sec_sym = Symbol {
            name: name.clone(),
            value: 0,
            size: 0,
            kind: SymbolKind::Section,
            scope: object::SymbolScope::Compilation,
            weak: false,
            section: SymbolSection::Section(id),
            flags: SymbolFlags::None,
        };
        let sec_sym_id = writer.add_symbol(sec_sym);

        section_map.insert(section.index().0, id);
        section_sym_map.insert(section.index().0, sec_sym_id);
    }

    // Copy symbols - filter based on mode
    for symbol in file.symbols() {
        let orig_idx = symbol.index().0;

        // Map section symbols to our created section symbols
        if symbol.kind() == SymbolKind::Section {
            if let object::SymbolSection::Section(sec_idx) = symbol.section() {
                if let Some(&sec_sym_id) = section_sym_map.get(&sec_idx.0) {
                    symbol_map.insert(orig_idx, sec_sym_id);
                    continue;
                } else {
                    continue;
                }
            } else {
                continue;
            }
        }

        let name = symbol.name().unwrap_or("").to_string();

        // Always keep special MSVC symbols (like @feat.00)
        let is_special_symbol = name.starts_with('@');

        // Determine if this symbol should be kept as global
        let keep_global = is_special_symbol
            || match mode {
                FilterMode::Allowlist { prefix } => name.starts_with(prefix),
                FilterMode::Blocklist { remove } => !remove.contains(&name),
            };

        let section = match symbol.section() {
            object::SymbolSection::Section(idx) => section_map
                .get(&idx.0)
                .map(|&s| SymbolSection::Section(s))
                .unwrap_or(SymbolSection::Undefined),
            object::SymbolSection::Undefined => SymbolSection::Undefined,
            object::SymbolSection::Absolute => SymbolSection::Absolute,
            object::SymbolSection::Common => SymbolSection::Common,
            _ => SymbolSection::Undefined,
        };

        // Skip empty-named symbols in sections (except special symbols)
        if name.is_empty() && matches!(section, SymbolSection::Section(_)) && !is_special_symbol {
            continue;
        }

        let wsym = Symbol {
            name: name.into_bytes(),
            value: symbol.address(),
            size: symbol.size(),
            kind: symbol.kind(),
            scope: if keep_global {
                symbol.scope()
            } else {
                object::SymbolScope::Compilation
            },
            weak: symbol.is_weak(),
            section,
            flags: SymbolFlags::None,
        };

        let id = writer.add_symbol(wsym);
        symbol_map.insert(orig_idx, id);
    }

    // Copy relocations
    for section in file.sections() {
        let Some(&new_sec) = section_map.get(&section.index().0) else {
            continue;
        };

        for (offset, reloc) in section.relocations() {
            if let RelocationTarget::Symbol(idx) = reloc.target() {
                // Try symbol_map first, then section_sym_map
                let target_sym = symbol_map
                    .get(&idx.0)
                    .or_else(|| section_sym_map.get(&idx.0))
                    .copied();

                if let Some(sym) = target_sym {
                    let flags = object::write::RelocationFlags::Generic {
                        kind: reloc.kind(),
                        encoding: reloc.encoding(),
                        size: reloc.size(),
                    };
                    writer.add_relocation(
                        new_sec,
                        Relocation {
                            offset,
                            symbol: sym,
                            addend: reloc.addend(),
                            flags,
                        },
                    )?;
                }
            }
        }
    }

    Ok(writer.write()?)
}

// macOS/iOS: Use ld -r with exported_symbols_list
fn patch_macos(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: &FilterMode,
    final_lib: &Path,
    target_arch: Option<&str>,
) {
    // Determine target architecture from parameter, env var, or host detection
    let target_arch_str = target_arch
        .map(|s| s.to_string())
        .or_else(|| env::var("CARGO_CFG_TARGET_ARCH").ok())
        .unwrap_or_else(|| {
            // Fall back to detecting the current architecture
            if cfg!(target_arch = "aarch64") {
                "aarch64".to_string()
            } else {
                "x86_64".to_string()
            }
        });
    let arch = match target_arch_str.as_str() {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let symbols_file = out_dir.join("symbols.txt");

    // Extract object files from archive to run nm on them (avoids LLVM version mismatch)
    let extract_dir = out_dir.join(format!("{}_objs", lib_name));
    fs::create_dir_all(&extract_dir).expect("Failed to create extract dir");

    // Convert to absolute path before changing directory (needed for macOS ar)
    let static_lib_abs = std::fs::canonicalize(static_lib)
        .expect("Failed to resolve absolute path for static library");

    let extract_status = Command::new("ar")
        .arg("-x")
        .arg(&static_lib_abs)
        .current_dir(&extract_dir)
        .status()
        .expect("Failed to extract archive");
    assert!(extract_status.success(), "ar -x failed");

    // Get all symbols from the extracted object files
    let mut all_symbols = Vec::new();
    for entry in fs::read_dir(&extract_dir).expect("Failed to read extract dir") {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();

        if !path.is_file() || path.file_name().unwrap() == "__.SYMDEF" {
            continue;
        }

        let nm_out = Command::new("xcrun")
            .arg("nm")
            .args(["-g", "-U"])
            .arg(&path)
            .output()
            .expect("Failed to run xcrun nm");

        if nm_out.status.success() {
            let symbols: Vec<String> = String::from_utf8_lossy(&nm_out.stdout)
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
            all_symbols.extend(symbols);
        }
    }

    // Remove duplicates
    all_symbols.sort();
    all_symbols.dedup();

    eprintln!(
        "DEBUG: Found {} unique symbols from object files",
        all_symbols.len()
    );

    // Now create the intermediate object with ld -r
    // Use xcrun to ensure we get the right ld that supports the target architecture
    let output = Command::new("xcrun")
        .arg("ld")
        .arg("-arch")
        .arg(arch)
        .arg("-r")
        .arg("-platform_version")
        .arg("macos")
        .arg(if arch == "arm64" { "11.0" } else { "10.13" })
        .arg("14.0")
        .arg("-o")
        .arg(&intermediate)
        .arg("-all_load")
        .arg(static_lib)
        .output()
        .expect("Failed to run xcrun ld");

    eprintln!("DEBUG: ld exit status: {}", output.status);
    if !output.status.success() {
        eprintln!("ld stderr: {}", String::from_utf8_lossy(&output.stderr));
        eprintln!("ld stdout: {}", String::from_utf8_lossy(&output.stdout));
        panic!("ld -r failed");
    }

    // Filter symbols based on mode
    let symbols_to_keep: Vec<String> = match mode {
        FilterMode::Allowlist { prefix } => {
            eprintln!("DEBUG: Filtering with prefix '{}'", prefix);
            all_symbols
                .into_iter()
                .filter(|sym| {
                    // macOS prefixes symbols with underscore, so _mylib_add needs to match "mylib_"
                    let sym_without_underscore = sym.strip_prefix('_').unwrap_or(sym);
                    let matches =
                        sym.starts_with(prefix) || sym_without_underscore.starts_with(prefix);
                    if matches {
                        eprintln!("DEBUG: Keeping symbol: {}", sym);
                    }
                    matches
                })
                .collect()
        }
        FilterMode::Blocklist { remove } => all_symbols
            .into_iter()
            .filter(|sym| {
                // Remove both with and without underscore prefix
                let without_underscore = sym.strip_prefix('_').unwrap_or(sym);
                !remove.contains(sym) && !remove.iter().any(|r| r == without_underscore)
            })
            .collect(),
    };

    if symbols_to_keep.is_empty() {
        match mode {
            FilterMode::Allowlist { prefix } => {
                panic!(
                    "No symbols found matching prefix '{}'. Did you forget to prefix your public functions?",
                    prefix
                );
            }
            FilterMode::Blocklist { .. } => {
                eprintln!("Warning: All symbols were removed. This may not be intended.");
            }
        }
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
    fs::remove_dir_all(&extract_dir).ok();
}

// Linux/Android: Use ld -r + objcopy
fn patch_linux(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: &FilterMode,
    final_lib: &Path,
    target_arch: Option<&str>,
) {
    // Determine target architecture from parameter, env var, or host detection
    let target_arch_str = target_arch
        .map(|s| s.to_string())
        .or_else(|| env::var("CARGO_CFG_TARGET_ARCH").ok())
        .unwrap_or_else(|| {
            // Fall back to detecting the current architecture
            if cfg!(target_arch = "aarch64") {
                "aarch64".to_string()
            } else if cfg!(target_arch = "x86_64") {
                "x86_64".to_string()
            } else if cfg!(target_arch = "arm") {
                "arm".to_string()
            } else if cfg!(target_arch = "x86") {
                "x86".to_string()
            } else {
                std::env::consts::ARCH.to_string()
            }
        });

    // Determine the appropriate binutils prefix for cross-compilation
    let (ld_cmd, objcopy_cmd, ar_cmd) = get_linux_toolchain(&target_arch_str);

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

    // Filter symbols based on mode
    match mode {
        FilterMode::Allowlist { prefix } => {
            // Use objcopy with wildcard to keep only prefixed symbols
            let wildcard = format!("{}*", prefix);
            let status = Command::new(&objcopy_cmd)
                .arg("--wildcard")
                .arg("--keep-global-symbol")
                .arg(&wildcard)
                .arg(&intermediate)
                .arg(&final_obj)
                .status()
                .unwrap_or_else(|_| panic!("Failed to run {}", objcopy_cmd));
            assert!(status.success(), "{} failed", objcopy_cmd);
        }
        FilterMode::Blocklist { remove } => {
            // Use objcopy to localize specific symbols
            let mut cmd = Command::new(&objcopy_cmd);
            for symbol in remove {
                cmd.arg("--localize-symbol").arg(symbol);
            }
            cmd.arg(&intermediate).arg(&final_obj);
            let status = cmd
                .status()
                .unwrap_or_else(|_| panic!("Failed to run {}", objcopy_cmd));
            assert!(status.success(), "{} failed", objcopy_cmd);
        }
    }

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
/// * `mode` - The filter mode that was applied
///
/// # Returns
///
/// A Result with Ok(()) if verification passes, or an error message if it fails
fn verify_patched_lib(
    static_lib: &Path,
    original_lib: &Path,
    mode: &FilterMode,
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

    // Check if output is suspiciously small compared to input (less than 1% could indicate a problem)
    let input_metadata = fs::metadata(original_lib)?;
    let input_size = input_metadata.len();
    let size_ratio = (output_size as f64) / (input_size as f64);

    if size_ratio < 0.01 {
        return Err(format!(
            "Output file is suspiciously small ({} bytes vs {} bytes input, {}% of original). This may indicate a patching error.",
            output_size, input_size, (size_ratio * 100.0) as u32
        ).into());
    }

    // 3. Verify it's a valid archive by listing symbols
    let symbols = list_symbols(static_lib)?;

    if symbols.is_empty() {
        return Err("Output library contains no symbols - patching may have failed".into());
    }

    // 4. Verify the filtering worked as expected
    match mode {
        FilterMode::Allowlist { prefix } => {
            // Check that at least one symbol with the prefix exists
            let matching_symbols: Vec<_> =
                symbols.iter().filter(|s| s.starts_with(prefix)).collect();

            if matching_symbols.is_empty() {
                return Err(format!(
                    "No symbols found with prefix '{}' in output library. Expected at least one public symbol with this prefix.",
                    prefix
                ).into());
            }

            eprintln!(
                "  ✓ Found {} public symbols with prefix '{}'",
                matching_symbols.len(),
                prefix
            );
        }
        FilterMode::Blocklist { remove } => {
            // Check that blocked symbols are either gone or made local
            // (We can't easily check if they're local vs gone, but at least they shouldn't be global)
            let still_global: Vec<_> = symbols
                .iter()
                .filter(|s| remove.contains(&s.to_string()))
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

            eprintln!("  ✓ Verified {} symbols are no longer global", remove.len());
        }
    }

    eprintln!(
        "  ✓ Output library contains {} total public symbols",
        symbols.len()
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
