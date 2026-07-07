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

    // Step 2: Get all defined GLOBAL and WEAK symbols (any visibility)
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

    // Parse readelf output to find defined GLOBAL and WEAK symbols
    let mut symbols_to_hide = Vec::new();
    for line in symbols_output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();

        // readelf -sW output format:
        // Num:    Value          Size Type    Bind   Vis      Ndx Name
        // Example: "   94: 0000000000000000     0 NOTYPE  GLOBAL DEFAULT  UND rust_eh_personality"
        // Example: " 12649: 0000000000000000  1563 FUNC    GLOBAL HIDDEN   301 _ZN10serde_json..."

        if parts.len() >= 8 {
            let bind = parts.get(4).unwrap_or(&"");
            let ndx = parts.get(6).unwrap_or(&"");
            let symbol_name = parts.get(7).unwrap_or(&"");

            // Consider every DEFINED (not UND) GLOBAL or WEAK symbol, regardless of
            // visibility. Weak symbols (e.g. compiler-builtins like __adddf3) are
            // externally visible and conflict just like global ones. Visibility does
            // not narrow this: `nm` (and thus the verifier) reports a defined
            // GLOBAL/WEAK symbol as external whether its visibility is DEFAULT,
            // HIDDEN, PROTECTED, or INTERNAL, so all of them must be localized or the
            // verifier rejects the leftover as a leaked non-prefix global.
            if (*bind == "GLOBAL" || *bind == "WEAK")
                && *ndx != "UND"
                && !symbol_name.is_empty()
                && !should_keep(symbol_name, keep_prefix)
            {
                symbols_to_hide.push(symbol_name.to_string());
            }
        }
    }

    eprintln!("Found {} symbols to hide", symbols_to_hide.len());

    // Step 3: Localize symbols using objcopy
    fs::copy(&intermediate, &final_obj).expect("Failed to copy intermediate object");

    if !symbols_to_hide.is_empty() {
        eprintln!("Localizing symbols...");

        // Pass the names in a file rather than one --localize-symbol arg each: a
        // real Rust staticlib has thousands of long symbol names, which would
        // overflow the argv limit (E2BIG) before objcopy even runs.
        // --localize-symbols reads one name per line.
        let symbols_file = out_dir.join(format!("{}_localize.txt", lib_name));
        fs::write(&symbols_file, symbols_to_hide.join("\n"))
            .expect("Failed to write localize-symbols file");

        let status = Command::new(&objcopy_cmd)
            .arg("--localize-symbols")
            .arg(&symbols_file)
            .arg(&intermediate)
            .arg(&final_obj)
            .status()
            .unwrap_or_else(|_| panic!("Failed to run {}", objcopy_cmd));

        if !status.success() {
            panic!("{} failed during symbol localization", objcopy_cmd);
        }

        eprintln!("  Localized {} symbols", symbols_to_hide.len());
        fs::remove_file(&symbols_file).ok();
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

/// Whether a defined ELF global/weak symbol stays public after patching.
///
/// Strips a leading underscore before the prefix match, matching macOS, Windows,
/// and the verifier (`matches_keep_prefix`): ELF names rarely carry one, but a
/// symbol literally named `_<keep_prefix>...` would otherwise be hidden here yet
/// demanded by the verifier, failing a patch that did the right thing.
fn should_keep(symbol_name: &str, keep_prefix: &str) -> bool {
    let unprefixed = symbol_name.strip_prefix('_').unwrap_or(symbol_name);
    symbol_name.starts_with(keep_prefix)
        || unprefixed.starts_with(keep_prefix)
        || symbol_name.starts_with("DW.ref.") // DWARF references
        || symbol_name.starts_with("_GLOBAL_OFFSET_TABLE_") // Special linker symbol
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

    #[test]
    fn keep_rule_strips_leading_underscore_like_the_verifier() {
        // Bare API name is kept.
        assert!(should_keep("mylib_add", "mylib_"));
        // A leading-underscore spelling must also be kept, or the verifier (which
        // strips the underscore) would demand a symbol this step just hid.
        assert!(should_keep("_mylib_helper", "mylib_"));
        // Compiler/linker internals stay public.
        assert!(should_keep("DW.ref.rust_eh_personality", "mylib_"));
        assert!(should_keep("_GLOBAL_OFFSET_TABLE_", "mylib_"));
        // Unrelated globals are hidden.
        assert!(!should_keep("_ZN4core3fmt3fooE", "mylib_"));
        assert!(!should_keep("other_prefix_fn", "mylib_"));
    }
}
