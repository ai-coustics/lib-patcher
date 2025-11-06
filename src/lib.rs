use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::File;
use object::write::{Object as WriteObject, Relocation, Symbol, SymbolSection};
use object::{
    Object as ObjectTrait, ObjectSection, ObjectSymbol, RelocationTarget, SymbolFlags, SymbolKind,
};

/// Patches a static library to hide all symbols except those matching the given prefix.
///
/// # Arguments
///
/// * `static_lib` - Path to the input static library (e.g., `libmylib.a`)
/// * `out_dir` - Directory for temporary files (use `$OUT_DIR` in build.rs)
/// * `lib_name` - Base name for temporary files (e.g., "mylib")
/// * `symbol_prefix` - Prefix for symbols to keep (e.g., "mylib_")
/// * `final_lib` - Path where the patched library will be written
///
/// # Panics
///
/// Panics if required platform tools are not available or if any command fails.
///
/// # Example
///
/// ```rust
/// // In build.rs
/// use std::env;
/// use std::path::Path;
///
/// patch_lib(
///     Path::new("target/release/libmylib.a"),
///     Path::new(&env::var("OUT_DIR").unwrap()),
///     "mylib",
///     "mylib_",  // ← ALL your public functions must start with this!
///     Path::new("target/release/libmylib_patched.a"),
/// );
/// ```
pub fn patch_lib(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbol_prefix: &str, // e.g., "aic_" or "rb_"
    final_lib: &Path,
) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");

    match target_os.as_str() {
        "windows" => patch_windows(static_lib, out_dir, lib_name, symbol_prefix, final_lib),
        "macos" | "ios" => patch_macos(static_lib, out_dir, lib_name, symbol_prefix, final_lib),
        _ => patch_linux(static_lib, out_dir, lib_name, symbol_prefix, final_lib),
    }
}

// Windows: Parse and rewrite COFF object files
fn patch_windows(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    symbol_prefix: &str,
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

        let patched = match patch_coff_object(&data, symbol_prefix) {
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
    symbol_prefix: &str,
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

    // Copy symbols
    for symbol in file.symbols() {
        if symbol.kind() == SymbolKind::Section {
            continue;
        }

        let name = symbol.name().unwrap_or("").to_string();

        // Keep only symbols with our prefix or turn others into local symbols
        let keep = name.starts_with(symbol_prefix);

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
            scope: if keep {
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
            if let RelocationTarget::Symbol(idx) = reloc.target() {
                if let Some(&sym) = symbol_map.get(&idx.0) {
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
    symbol_prefix: &str,
    final_lib: &Path,
) {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let arch = match target_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let symbols_file = out_dir.join("symbols.txt");

    // Partial link
    let status = Command::new("ld")
        .args(&["-arch", arch, "-r", "-o"])
        .arg(&intermediate)
        .arg("-all_load")
        .arg(static_lib)
        .status()
        .expect("Failed to run ld");
    assert!(status.success(), "ld -r failed");

    // Get symbols to keep
    let nm_out = Command::new("nm")
        .args(&["-g", "-defined-only"])
        .arg(&intermediate)
        .output()
        .expect("Failed to run nm");

    let symbols: Vec<String> = String::from_utf8_lossy(&nm_out.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[1].chars().any(|c| c.is_uppercase()) {
                let sym = parts[2];
                if sym.starts_with(symbol_prefix) || sym.starts_with(&format!("_{}", symbol_prefix))
                {
                    return Some(sym.to_string());
                }
            }
            None
        })
        .collect();

    fs::write(&symbols_file, symbols.join("\n")).expect("Failed to write symbols file");

    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Filter symbols
    let status = Command::new("ld")
        .args(&["-arch", arch, "-r", "-o"])
        .arg(&final_obj)
        .arg("-exported_symbols_list")
        .arg(&symbols_file)
        .arg(&intermediate)
        .status()
        .expect("Failed to run ld filter");
    assert!(status.success(), "ld symbol filter failed");

    // Create archive
    let status = Command::new("ar")
        .args(&["rcs"])
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
    symbol_prefix: &str,
    final_lib: &Path,
) {
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Partial link
    let status = Command::new("ld")
        .args(&["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib)
        .status()
        .expect("Failed to run ld");
    assert!(status.success(), "ld -r failed");

    // Filter symbols
    let wildcard = format!("{}*", symbol_prefix);
    let status = Command::new("objcopy")
        .arg("--wildcard")
        .arg("--keep-global-symbol")
        .arg(&wildcard)
        .arg(&intermediate)
        .arg(&final_obj)
        .status()
        .expect("Failed to run objcopy");
    assert!(status.success(), "objcopy failed");

    // Create archive (try ar, fallback to llvm-ar)
    let ar_result = Command::new("ar")
        .args(&["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status();

    if !ar_result.map(|s| s.success()).unwrap_or(false) {
        let status = Command::new("llvm-ar")
            .args(&["rcs"])
            .arg(final_lib)
            .arg(&final_obj)
            .status()
            .expect("Failed to run llvm-ar");
        assert!(status.success(), "Both ar and llvm-ar failed");
    }

    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();
}
