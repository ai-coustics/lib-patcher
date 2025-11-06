use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::File;
use object::write::{Object as WriteObject, Relocation, Symbol, SymbolSection};
use object::{
    Object as ObjectTrait, ObjectSection, ObjectSymbol, RelocationTarget, SymbolFlags, SymbolKind,
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
/// use staticlib_hygiene::{patch_lib, FilterMode};
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("target/release/libmylib.a"),
///     Path::new("out"),
///     "mylib",
///     FilterMode::Allowlist { prefix: "mylib_".to_string() },
///     Path::new("libmylib_patched.a"),
/// );
/// ```
///
/// ## Blocklist mode (third-party library)
///
/// ```rust,no_run
/// use staticlib_hygiene::{patch_lib, FilterMode};
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("vendor/libthirdparty.a"),
///     Path::new("out"),
///     "thirdparty",
///     FilterMode::default_blocklist(),
///     Path::new("libthirdparty_patched.a"),
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: FilterMode,
    final_lib: &Path,
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
        "windows" => patch_windows(static_lib, out_dir, lib_name, &mode, final_lib),
        "macos" | "ios" => patch_macos(static_lib, out_dir, lib_name, &mode, final_lib),
        _ => patch_linux(static_lib, out_dir, lib_name, &mode, final_lib),
    }
}

// Windows: Parse and rewrite COFF object files
fn patch_windows(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: &FilterMode,
    final_lib: &Path,
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

    // Create library using lib.exe
    let mut cmd = Command::new("lib.exe");
    cmd.arg("/nologo")
        .arg(format!("/OUT:{}", final_lib.display()));
    for obj in &obj_files {
        cmd.arg(obj);
    }
    assert!(cmd.status().expect("Failed to run lib.exe").success());

    fs::remove_dir_all(&temp_dir).ok();
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

    // Copy sections
    for section in file.sections() {
        let name = section.name_bytes()?.to_vec();
        let kind = section.kind();
        let id = writer.add_section(Vec::new(), name, kind);

        if let Ok(data) = section.uncompressed_data() {
            writer
                .section_mut(id)
                .set_data(data.into_owned(), section.align());
        }

        section_map.insert(section.index().0, id);
    }

    // Copy symbols - filter based on mode
    for symbol in file.symbols() {
        if symbol.kind() == SymbolKind::Section {
            continue;
        }

        let name = symbol.name().unwrap_or("").to_string();

        // Determine if this symbol should be kept as global
        let keep_global = match mode {
            FilterMode::Allowlist { prefix } => name.starts_with(prefix),
            FilterMode::Blocklist { remove } => !remove.contains(&name),
        };

        let section = match symbol.section() {
            object::SymbolSection::Section(idx) => section_map
                .get(&idx.0)
                .map(|&s| SymbolSection::Section(s))
                .unwrap_or(SymbolSection::Undefined),
            object::SymbolSection::Undefined => SymbolSection::Undefined,
            _ => SymbolSection::Undefined,
        };

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
        symbol_map.insert(symbol.index().0, id);
    }

    // Copy relocations
    for section in file.sections() {
        let Some(&new_sec) = section_map.get(&section.index().0) else {
            continue;
        };

        for (offset, reloc) in section.relocations() {
            if let RelocationTarget::Symbol(idx) = reloc.target()
                && let Some(&sym) = symbol_map.get(&idx.0)
            {
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

    Ok(writer.write()?)
}

// macOS/iOS: Use ld -r with exported_symbols_list
fn patch_macos(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: &FilterMode,
    final_lib: &Path,
) {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| {
        if cfg!(target_arch = "aarch64") {
            "aarch64".to_string()
        } else {
            "x86_64".to_string()
        }
    });
    let arch = match target_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let symbols_file = out_dir.join("symbols.txt");

    // Partial link with platform version (required on newer macOS)
    let status = Command::new("ld")
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
        .status()
        .expect("Failed to run ld");
    assert!(status.success(), "ld -r failed");

    // Get all defined global symbols
    // Note: macOS nm uses -U to exclude undefined symbols, not --defined-only
    let nm_out = Command::new("nm")
        .args(["-g", "-U"])
        .arg(&intermediate)
        .output()
        .expect("Failed to run nm");

    let all_symbols: Vec<String> = String::from_utf8_lossy(&nm_out.stdout)
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

    // Debug: print all symbols found
    eprintln!("DEBUG: Found {} symbols total", all_symbols.len());
    if all_symbols.len() <= 20 {
        for sym in &all_symbols {
            eprintln!("DEBUG: Symbol: {}", sym);
        }
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
    let status = Command::new("ld")
        .args(["-arch", arch, "-r", "-o"])
        .arg(&final_obj)
        .arg("-exported_symbols_list")
        .arg(&symbols_file)
        .arg(&intermediate)
        .status()
        .expect("Failed to run ld filter");
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
}

// Linux/Android: Use ld -r + objcopy
fn patch_linux(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    mode: &FilterMode,
    final_lib: &Path,
) {
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Partial link
    let status = Command::new("ld")
        .args(["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib)
        .status()
        .expect("Failed to run ld");
    assert!(status.success(), "ld -r failed");

    // Filter symbols based on mode
    match mode {
        FilterMode::Allowlist { prefix } => {
            // Use objcopy with wildcard to keep only prefixed symbols
            let wildcard = format!("{}*", prefix);
            let status = Command::new("objcopy")
                .arg("--wildcard")
                .arg("--keep-global-symbol")
                .arg(&wildcard)
                .arg(&intermediate)
                .arg(&final_obj)
                .status()
                .expect("Failed to run objcopy");
            assert!(status.success(), "objcopy failed");
        }
        FilterMode::Blocklist { remove } => {
            // Use objcopy to localize specific symbols
            let mut cmd = Command::new("objcopy");
            for symbol in remove {
                cmd.arg("--localize-symbol").arg(symbol);
            }
            cmd.arg(&intermediate).arg(&final_obj);
            let status = cmd.status().expect("Failed to run objcopy");
            assert!(status.success(), "objcopy failed");
        }
    }

    // Create archive (try ar, fallback to llvm-ar)
    let ar_result = Command::new("ar")
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
        assert!(status.success(), "Both ar and llvm-ar failed");
    }

    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();
}
