use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::File;
use object::{Object as ObjectTrait, ObjectComdat};

pub(crate) struct WindowsLibTool {
    pub tool: String,
    pub machine_type: Option<String>,
    pub is_llvm: bool,
}

/// Windows implementation: Patches COFF symbol tables directly
pub(crate) fn patch_windows(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let temp_dir = out_dir.join(format!("{}_objs", lib_name));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    eprintln!("Reading archive and patching COFF objects...");
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
        match patch_coff_object(&data, keep_prefix) {
            Ok(patched_data) => {
                let patched_path = temp_dir.join(format!("{}_patched.obj", idx));
                fs::write(&patched_path, patched_data).expect("Failed to write patched object");
                obj_files.push(patched_path);
            }
            Err(e) => {
                eprintln!("Warning: Skipping object ({})", e);
            }
        }
    }

    eprintln!("Creating final library from {} objects...", obj_files.len());

    // Determine which library tool to use
    let lib_cmd = get_windows_lib_tool(Some(target_arch));

    // Convert final_lib to absolute path
    let final_lib_abs = if final_lib.is_absolute() {
        final_lib.to_path_buf()
    } else {
        env::current_dir()
            .expect("Failed to get current directory")
            .join(final_lib)
    };

    let mut cmd = Command::new(&lib_cmd.tool);

    if lib_cmd.is_llvm {
        cmd.arg("rc");
        cmd.arg(&final_lib_abs);
        cmd.current_dir(&temp_dir);

        for obj in &obj_files {
            if let Some(filename) = obj.file_name() {
                cmd.arg(filename);
            }
        }
    } else {
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!("/OUT:{}", final_lib_abs.display()));
        cmd.current_dir(&temp_dir);

        for obj in &obj_files {
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

    eprintln!("Temp directory: {}", temp_dir.display());
    eprintln!("✓ Windows patching complete");
}

fn patch_coff_object(
    data: &[u8],
    keep_prefix: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use object::LittleEndian as LE;
    use object::pe;
    use object::read::coff::CoffHeader;

    let mut data = data.to_vec();

    // Check if this is big obj format
    let is_bigobj = data.len() >= 4 && data[0..2] == [0x00, 0x00] && data[2..4] == [0xFF, 0xFF];

    if is_bigobj {
        return patch_coff_bigobj(data, keep_prefix);
    }

    let mut offset = 0u64;
    let header = pe::ImageFileHeader::parse(&data[..], &mut offset)?;

    let symbol_table_offset = header.pointer_to_symbol_table.get(LE) as usize;
    let symbol_count = header.number_of_symbols.get(LE) as usize;

    if symbol_table_offset == 0 || symbol_count == 0 {
        return Ok(data);
    }

    let symbol_table_end = symbol_table_offset + (symbol_count * 18);
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
        let symbol_offset = symbol_table_offset + (i * 18);

        if symbol_offset + 18 > data.len() {
            return Err(format!("Symbol {} extends beyond file", i).into());
        }

        let storage_class = data[symbol_offset + 16];

        // Only process EXTERNAL (global) symbols
        if storage_class != 2 {
            continue;
        }

        // Get symbol name
        let symbol_entry = &data[symbol_offset..symbol_offset + 18];
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

        // Determine if symbol should be kept
        let is_comdat = comdat_symbols.contains(&i);
        let should_keep = name.starts_with(keep_prefix)
            || name.starts_with("DW.ref.")
            || name.starts_with('@') // Special symbols
            || (is_comdat && name.starts_with("DW.")); // Keep COMDAT DWARF symbols

        if !should_keep {
            // Change storage class to STATIC (local)
            data[symbol_offset + 16] = 3;
            modified_count += 1;
        }
    }

    eprintln!("  Modified {} symbols in COFF object", modified_count);
    Ok(data)
}

fn patch_coff_bigobj(
    mut data: Vec<u8>,
    keep_prefix: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if data.len() < 56 {
        return Err("File too small for big obj format".into());
    }

    let symbol_table_offset = u32::from_le_bytes([data[48], data[49], data[50], data[51]]) as usize;
    let symbol_count = u32::from_le_bytes([data[52], data[53], data[54], data[55]]) as usize;

    if symbol_table_offset == 0 || symbol_count == 0 {
        return Ok(data);
    }

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

        let is_comdat = comdat_symbols.contains(&i);
        let should_keep = name.starts_with(keep_prefix)
            || name.starts_with("DW.ref.")
            || name.starts_with('@')
            || (is_comdat && name.starts_with("DW."));

        if !should_keep {
            data[symbol_offset + 18] = 3; // STATIC
            modified_count += 1;
        }
    }

    eprintln!("  Modified {} symbols in bigobj", modified_count);
    Ok(data)
}

fn read_coff_string(data: &[u8], offset: usize) -> Result<String, Box<dyn std::error::Error>> {
    let end = data[offset..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(data.len() - offset);
    Ok(String::from_utf8_lossy(&data[offset..offset + end]).to_string())
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
                env::consts::ARCH.to_string()
            }
        });

    let host_arch = env::consts::ARCH;

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

    // Fall back to llvm-ar as a last resort
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
