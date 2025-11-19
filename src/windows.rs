use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::read::File;
use object::{Object as ObjectTrait, ObjectSymbol};

pub(crate) struct WindowsLibTool {
    pub tool: String,
    pub machine_type: Option<String>,
    pub is_llvm: bool,
}

/// Windows implementation: Renames symbols using llvm-objcopy on extracted objects
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

    eprintln!("Reading archive...");
    let archive_bytes = fs::read(static_lib).expect("Failed to read static lib");
    let archive = match object::read::archive::ArchiveFile::parse(&*archive_bytes) {
        Ok(a) => a,
        Err(e) => panic!("Failed to parse static lib as COFF archive: {}", e),
    };

    let mut obj_files = Vec::new();
    let mut defined_symbols = HashSet::new();

    // Step 1: Extract objects and collect defined symbols
    eprintln!("Extracting objects and scanning symbols...");
    for member in archive.members() {
        let member = member.expect("Failed to read archive member");
        let data = member
            .data(archive_bytes.as_slice())
            .expect("Failed to read member data");

        let name = String::from_utf8_lossy(member.name());
        if name == "/" || name == "//" {
            continue;
        }

        let idx = obj_files.len();
        let obj_path = temp_dir.join(format!("{}.obj", idx));
        fs::write(&obj_path, data).expect("Failed to write object file");
        obj_files.push(obj_path);

        // Parse object file to find defined symbols
        if let Ok(file) = File::parse(data) {
            for symbol in file.symbols() {
                if symbol.is_global() && !symbol.is_undefined() {
                    if let Ok(name) = symbol.name() {
                        defined_symbols.insert(name.to_string());
                    }
                }
            }
        }
    }

    eprintln!("Extracted {} objects.", obj_files.len());
    eprintln!("Found {} defined symbols.", defined_symbols.len());

    // Step 2: Generate renames
    let mut renames = Vec::new();
    let mut kept_count = 0;
    let mut renamed_count = 0;

    for symbol in defined_symbols {
        if symbol.starts_with(keep_prefix) {
            kept_count += 1;
            continue;
        }

        // Skip special compiler symbols (heuristic)
        // We MUST rename .weak symbols to avoid LNK2005 conflicts
        if symbol.starts_with("??") {
             continue;
        }

        let new_name = format!("{}{}", keep_prefix, symbol);
        renames.push(format!("{} {}", symbol, new_name));
        renamed_count += 1;
    }

    eprintln!(
        "Renaming {} symbols (kept {} already prefixed).",
        renamed_count, kept_count
    );

    if renames.is_empty() {
        eprintln!("No symbols to rename. Copying file...");
        fs::copy(static_lib, final_lib).expect("Failed to copy library");
        return;
    }

    let renames_path = temp_dir.join("renames.txt");
    let mut f = fs::File::create(&renames_path).expect("Failed to create renames file");
    for line in renames {
        writeln!(f, "{}", line).expect("Failed to write rename line");
    }

    // Step 3: Run llvm-objcopy on EACH object
    let objcopy = find_objcopy_tool();
    eprintln!("Using objcopy: {}", objcopy.display());
    eprintln!("Renaming symbols in objects...");

    let mut patched_files = Vec::new();

    for (i, obj_path) in obj_files.iter().enumerate() {
        let patched_path = temp_dir.join(format!("{}_patched.obj", i));
        
        let status = Command::new(&objcopy)
            .arg(format!("--redefine-syms={}", renames_path.display()))
            .arg(obj_path)
            .arg(&patched_path)
            .status()
            .expect("Failed to execute llvm-objcopy");

        if !status.success() {
            eprintln!("Warning: llvm-objcopy failed on object {}. Skipping.", i);
            // Fallback: use original object if patch fails (might be non-COFF or weird)
            patched_files.push(obj_path.clone());
        } else {
            patched_files.push(patched_path);
        }
    }

    // Step 4: Repackage
    eprintln!("Creating final library...");

    let lib_cmd = get_windows_lib_tool(Some(target_arch));
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
        for obj in &patched_files {
            cmd.arg(obj);
        }
    } else {
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!("/OUT:{}", final_lib_abs.display()));
        for obj in &patched_files {
            cmd.arg(obj);
        }
    }

    let status = cmd.status().unwrap_or_else(|_| {
        panic!(
            "Failed to run {}. For cross-architecture patching, install LLVM tools.",
            lib_cmd.tool
        )
    });

    if !status.success() {
        panic!("{} failed with exit code: {:?}", lib_cmd.tool, status.code());
    }

    eprintln!("✓ Windows patching complete (via renaming)");
}

fn find_objcopy_tool() -> PathBuf {
    if let Ok(path) = which::which("llvm-objcopy") { return path; }
    if let Ok(path) = which::which("rust-objcopy") { return path; }

    let vs_llvm_paths = [
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
    ];

    for path_str in &vs_llvm_paths {
        let path = PathBuf::from(path_str);
        if path.exists() { return path; }
    }

    if let Ok(output) = Command::new("rustc").arg("--print").arg("sysroot").output() {
        let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let sysroot_path = PathBuf::from(sysroot);
        let rustlib = sysroot_path.join("lib").join("rustlib");
        if let Ok(entries) = fs::read_dir(&rustlib) {
            for entry in entries.flatten() {
                let bin_objcopy = entry.path().join("bin").join("rust-objcopy.exe");
                if bin_objcopy.exists() { return bin_objcopy; }
            }
        }
    }

    panic!("Could not find 'llvm-objcopy' or 'rust-objcopy'.");
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
    let _is_cross = target_arch_str != host_arch;

    // 1. Try finding lib.exe in PATH
    if Command::new("lib.exe").arg("/?").output().is_ok() {
        eprintln!("Using lib.exe for Windows build");
        return WindowsLibTool {
            tool: "lib.exe".to_string(),
            machine_type,
            is_llvm: false,
        };
    }

    // 2. Try finding llvm-lib in PATH
    if Command::new("llvm-lib").arg("/?").output().is_ok() {
        eprintln!("Using llvm-lib for Windows build");
        return WindowsLibTool {
            tool: "llvm-lib".to_string(),
            machine_type,
            is_llvm: false,
        };
    }

    // 3. Look in Visual Studio LLVM locations for llvm-lib
    let vs_llvm_paths = [
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin\llvm-lib.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\bin\llvm-lib.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin\llvm-lib.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin\llvm-lib.exe",
    ];

    for path_str in &vs_llvm_paths {
        let path = PathBuf::from(path_str);
        if path.exists() {
            eprintln!("Using llvm-lib at {}", path.display());
            return WindowsLibTool {
                tool: path.to_string_lossy().to_string(),
                machine_type,
                is_llvm: false, // llvm-lib uses lib.exe flags
            };
        }
    }

    // 5. Fall back to llvm-ar
    if Command::new("llvm-ar").arg("--version").output().is_ok() {
        eprintln!("Using llvm-ar for Windows build");
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
