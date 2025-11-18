use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

/// macOS/iOS implementation: Uses ld -r + exported_symbols_list
pub(crate) fn patch_macos(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let arch = match target_arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let temp_obj_dir = out_dir.join(format!("{}_objs", lib_name));
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Step 1: Extract all object files from archive
    fs::create_dir_all(&temp_obj_dir).expect("Failed to create temp object directory");

    let static_lib_abs = if static_lib.is_absolute() {
        static_lib.to_path_buf()
    } else {
        env::current_dir()
            .expect("Failed to get current directory")
            .join(static_lib)
    };

    eprintln!("Extracting objects from archive...");
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

    // Step 2: Link all objects with ld -r
    eprintln!("Creating intermediate object with ld -r...");
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

    if !output.status.success() {
        panic!("ld -r failed");
    }

    // Step 3: Get all global defined symbols
    eprintln!("Extracting symbols to determine what to hide...");
    let nm_out = Command::new("xcrun")
        .arg("nm")
        .args(["-g", "-U"]) // -g = global only, -U = defined only (no undefined)
        .arg(&intermediate)
        .output()
        .expect("Failed to run xcrun nm");

    if !nm_out.status.success() {
        eprintln!("nm stderr: {}", String::from_utf8_lossy(&nm_out.stderr));
        panic!("nm failed on intermediate object");
    }

    let nm_stdout = String::from_utf8_lossy(&nm_out.stdout);

    // Parse nm output to find symbols to hide
    // Format: "0000000000000000 T _symbol_name" or "0000000000000000 D _symbol_name"
    let mut symbols_to_hide = Vec::new();
    for line in nm_stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let symbol_type = parts[1];
            let mut symbol_name = parts[2];

            // Only process defined symbols (T, D, S, B, etc. - uppercase means global)
            if symbol_type.chars().next().unwrap_or('_').is_uppercase() {
                // macOS symbols often have leading underscore
                symbol_name = symbol_name.strip_prefix('_').unwrap_or(symbol_name);

                // Check if this symbol should be kept
                let should_keep = symbol_name.starts_with(keep_prefix)
                    || symbol_name.starts_with("DW.ref.")
                    || symbol_name.starts_with("GCC_except_table");

                if !should_keep {
                    // Add back the underscore for the symbols file
                    symbols_to_hide.push(format!("_{}", symbol_name));
                }
            }
        }
    }

    eprintln!("Found {} symbols to hide", symbols_to_hide.len());

    // Step 4: Create a symbols file with symbols to keep (for ld -exported_symbols_list)
    let symbols_file = out_dir.join("keep_symbols.txt");

    // Get symbols to keep by getting all symbols and removing the ones to hide
    let all_symbols_out = Command::new("xcrun")
        .arg("nm")
        .args(["-g"])
        .arg(&intermediate)
        .output()
        .expect("Failed to run xcrun nm for all symbols");

    let all_symbols_stdout = String::from_utf8_lossy(&all_symbols_out.stdout);
    let mut keep_symbols = Vec::new();

    for line in all_symbols_stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let symbol_type = parts[1];
            let symbol_name = parts[2];

            if symbol_type.chars().next().unwrap_or('_').is_uppercase() {
                if !symbols_to_hide.contains(&symbol_name.to_string()) {
                    keep_symbols.push(symbol_name.to_string());
                }
            }
        }
    }

    if keep_symbols.is_empty() {
        eprintln!("Warning: No symbols will be kept global. This may not be intended.");
    }

    fs::write(&symbols_file, keep_symbols.join("\n")).expect("Failed to write symbols file");

    // Step 5: Filter symbols using ld -r with -exported_symbols_list
    eprintln!("Filtering symbols...");
    let status = Command::new("xcrun")
        .arg("ld")
        .args(["-arch", arch, "-r", "-o"])
        .arg(&final_obj)
        .arg("-exported_symbols_list")
        .arg(&symbols_file)
        .arg(&intermediate)
        .status()
        .expect("Failed to run xcrun ld filter");

    if !status.success() {
        panic!("ld symbol filter failed");
    }

    // Step 6: Create archive
    eprintln!("Creating final archive...");
    let status = Command::new("ar")
        .args(["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status()
        .expect("Failed to run ar");

    if !status.success() {
        panic!("ar failed");
    }

    // Cleanup
    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();
    fs::remove_file(&symbols_file).ok();
    fs::remove_dir_all(&temp_obj_dir).ok();

    eprintln!("✓ macOS patching complete");
}
