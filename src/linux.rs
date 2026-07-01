use std::fs;
use std::path::Path;
use std::process::Command;

/// Linux implementation: Uses readelf + objcopy to localize symbols
pub(crate) fn patch_linux(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let (ld_cmd, objcopy_cmd, ar_cmd) = get_linux_toolchain(target_arch);

    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Step 1: Partial link with ld -r
    eprintln!("Creating intermediate object with ld -r...");
    let status = Command::new(&ld_cmd)
        .args(["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib)
        .status()
        .unwrap_or_else(|_| panic!("Failed to run {}", ld_cmd));
    assert!(status.success(), "{} -r failed", ld_cmd);

    // Step 2: Get all GLOBAL and WEAK symbols (both DEFAULT and HIDDEN visibility)
    eprintln!("Extracting symbols to determine what to hide...");
    let readelf_output = Command::new("readelf")
        .args(["-sW"])
        .arg(&intermediate)
        .output()
        .expect("Failed to run readelf");

    if !readelf_output.status.success() {
        panic!("readelf failed");
    }

    let symbols_output = String::from_utf8_lossy(&readelf_output.stdout);

    // Parse readelf output to find GLOBAL and WEAK symbols (both DEFAULT and HIDDEN)
    let mut symbols_to_hide = Vec::new();
    for line in symbols_output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();

        // readelf -sW output format:
        // Num:    Value          Size Type    Bind   Vis      Ndx Name
        // Example: "   94: 0000000000000000     0 NOTYPE  GLOBAL DEFAULT  UND rust_eh_personality"
        // Example: " 12649: 0000000000000000  1563 FUNC    GLOBAL HIDDEN   301 _ZN10serde_json..."

        if parts.len() >= 8 {
            let bind = parts.get(4).unwrap_or(&"");
            let vis = parts.get(5).unwrap_or(&"");
            let ndx = parts.get(6).unwrap_or(&"");
            let symbol_name = parts.get(7).unwrap_or(&"");

            // Consider GLOBAL and WEAK symbols with any visibility (DEFAULT or HIDDEN)
            // that are DEFINED (not UND). Weak symbols (e.g. compiler-builtins like
            // __adddf3) are externally visible and conflict just like global ones, so
            // they must be localized too. HIDDEN symbols also need localizing.
            if (*bind == "GLOBAL" || *bind == "WEAK")
                && (*vis == "DEFAULT" || *vis == "HIDDEN")
                && *ndx != "UND"
                && !symbol_name.is_empty()
            {
                // Check if this symbol should be kept
                let should_keep = symbol_name.starts_with(keep_prefix)
                    || symbol_name.starts_with("DW.ref.")  // DWARF references
                    || symbol_name.starts_with("_GLOBAL_OFFSET_TABLE_"); // Special linker symbol

                if !should_keep {
                    symbols_to_hide.push(symbol_name.to_string());
                }
            }
        }
    }

    eprintln!("Found {} symbols to hide", symbols_to_hide.len());

    // Step 3: Localize symbols using objcopy
    fs::copy(&intermediate, &final_obj).expect("Failed to copy intermediate object");

    if !symbols_to_hide.is_empty() {
        eprintln!("Localizing symbols...");

        // Build a single objcopy command with all symbols to localize
        let mut cmd = Command::new(&objcopy_cmd);
        for symbol in &symbols_to_hide {
            cmd.arg("--localize-symbol").arg(symbol);
        }
        cmd.arg(&intermediate).arg(&final_obj);

        let status = cmd
            .status()
            .unwrap_or_else(|_| panic!("Failed to run {}", objcopy_cmd));

        if !status.success() {
            panic!("{} failed during symbol localization", objcopy_cmd);
        }

        eprintln!("  Localized {} symbols", symbols_to_hide.len());
    }

    // Step 4: Create archive
    eprintln!("Creating final archive...");
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

    // Cleanup
    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();

    eprintln!("✓ Linux patching complete");
}

/// Maps a target architecture to its GNU cross-toolchain triplet prefix
/// (e.g. "aarch64" -> "aarch64-linux-gnu"). Returns `None` for architectures
/// with no known prefix, so the caller falls back to the native tools.
fn linux_triplet_prefix(target_arch: &str) -> Option<&'static str> {
    match target_arch {
        "aarch64" | "arm64" => Some("aarch64-linux-gnu"),
        "arm" | "armv7" => Some("arm-linux-gnueabihf"),
        "x86_64" => Some("x86_64-linux-gnu"),
        "x86" | "i686" => Some("i686-linux-gnu"),
        "riscv64" => Some("riscv64-linux-gnu"),
        "powerpc64" => Some("powerpc64-linux-gnu"),
        "powerpc64le" => Some("powerpc64le-linux-gnu"),
        "s390x" => Some("s390x-linux-gnu"),
        _ => None,
    }
}

/// Determines the appropriate toolchain for the target architecture on Linux
fn get_linux_toolchain(target_arch: &str) -> (String, String, String) {
    let host_arch = std::env::consts::ARCH;

    // If targeting the same architecture as host, use native tools
    if target_arch == host_arch {
        return ("ld".to_string(), "objcopy".to_string(), "ar".to_string());
    }

    // For cross-compilation, determine the GNU triplet prefix
    let triplet_prefix = match linux_triplet_prefix(target_arch) {
        Some(prefix) => prefix,
        None => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_arches_map_to_gnu_triplet_prefixes() {
        assert_eq!(linux_triplet_prefix("aarch64"), Some("aarch64-linux-gnu"));
        assert_eq!(linux_triplet_prefix("arm64"), Some("aarch64-linux-gnu"));
        assert_eq!(linux_triplet_prefix("arm"), Some("arm-linux-gnueabihf"));
        assert_eq!(linux_triplet_prefix("x86_64"), Some("x86_64-linux-gnu"));
        assert_eq!(linux_triplet_prefix("i686"), Some("i686-linux-gnu"));
        assert_eq!(linux_triplet_prefix("riscv64"), Some("riscv64-linux-gnu"));
        assert_eq!(linux_triplet_prefix("s390x"), Some("s390x-linux-gnu"));
    }

    #[test]
    fn unknown_arch_has_no_prefix_so_caller_uses_native_tools() {
        assert_eq!(linux_triplet_prefix("m68k"), None);
    }
}
