use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Android implementation: ELF symbol filtering using the Android NDK LLVM tools.
///
/// Android archives are regular `ar` archives containing ELF objects, so the
/// symbol-hiding strategy matches Linux: partially link the archive into one
/// relocatable object, localize every non-API global/weak definition, then
/// re-archive the object. The important difference is toolchain selection: use
/// the target-neutral LLVM tools from the Android NDK instead of host/GNU Linux
/// binutils.
pub(crate) fn patch_android(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let tools = get_android_toolchain(target_arch);

    let intermediate = out_dir.join(format!("{}_android_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_android_final.o", lib_name));

    // Step 1: Partial link with NDK ld.lld -r.
    eprintln!("Creating Android intermediate object with ld.lld -r...");
    let mut ld_cmd = Command::new(&tools.ld_lld);
    ld_cmd
        .args(android_lld_emulation_args(target_arch))
        .args(["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib);

    let output = ld_cmd
        .output()
        .unwrap_or_else(|_| panic!("Failed to run {}", tools.ld_lld.display()));
    if !output.stderr.is_empty() {
        eprintln!("ld.lld stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "{} -r failed",
        tools.ld_lld.display()
    );

    // Step 2: Get all GLOBAL and WEAK defined symbols.
    eprintln!("Extracting Android symbols to determine what to hide...");
    let readelf_output = Command::new(&tools.readelf)
        .args(["-sW"])
        .arg(&intermediate)
        .output()
        .unwrap_or_else(|_| panic!("Failed to run {}", tools.readelf.display()));

    if !readelf_output.status.success() {
        if !readelf_output.stderr.is_empty() {
            eprintln!(
                "llvm-readelf stderr: {}",
                String::from_utf8_lossy(&readelf_output.stderr)
            );
        }
        panic!("{} failed", tools.readelf.display());
    }

    let symbols_output = String::from_utf8_lossy(&readelf_output.stdout);
    let symbols_to_hide = symbols_to_hide_from_readelf(&symbols_output, keep_prefix);

    eprintln!("Found {} Android symbols to hide", symbols_to_hide.len());

    // Step 3: Localize symbols using NDK llvm-objcopy.
    fs::copy(&intermediate, &final_obj).expect("Failed to copy Android intermediate object");

    if !symbols_to_hide.is_empty() {
        eprintln!("Localizing Android symbols...");

        let symbols_file = out_dir.join(format!("{}_android_localize.txt", lib_name));
        fs::write(&symbols_file, symbols_to_hide.join("\n"))
            .expect("Failed to write Android localize-symbols file");

        let status = Command::new(&tools.objcopy)
            .arg("--localize-symbols")
            .arg(&symbols_file)
            .arg(&intermediate)
            .arg(&final_obj)
            .status()
            .unwrap_or_else(|_| panic!("Failed to run {}", tools.objcopy.display()));

        if !status.success() {
            panic!(
                "{} failed during Android symbol localization",
                tools.objcopy.display()
            );
        }

        eprintln!("  Localized {} Android symbols", symbols_to_hide.len());
        fs::remove_file(&symbols_file).ok();
    }

    // Step 4: Create archive with NDK llvm-ar.
    eprintln!("Creating final Android archive...");
    let status = Command::new(&tools.ar)
        .args(["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status()
        .unwrap_or_else(|_| panic!("Failed to run {}", tools.ar.display()));

    assert!(status.success(), "{} failed", tools.ar.display());

    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();

    eprintln!("✓ Android patching complete");
}

struct AndroidToolchain {
    ld_lld: PathBuf,
    readelf: PathBuf,
    objcopy: PathBuf,
    ar: PathBuf,
}

fn get_android_toolchain(target_arch: &str) -> AndroidToolchain {
    let ndk = android_ndk_home().unwrap_or_else(|| {
        panic!(
            "Android target '{}' requires ANDROID_NDK_HOME, ANDROID_NDK_ROOT, or NDK_HOME to point to an Android NDK",
            target_arch
        )
    });
    let host_tag = android_ndk_host_tag();
    let bin = ndk
        .join("toolchains")
        .join("llvm")
        .join("prebuilt")
        .join(&host_tag)
        .join("bin");

    let tools = AndroidToolchain {
        ld_lld: bin.join(exe("ld.lld")),
        readelf: bin.join(exe("llvm-readelf")),
        objcopy: bin.join(exe("llvm-objcopy")),
        ar: bin.join(exe("llvm-ar")),
    };

    for tool in [&tools.ld_lld, &tools.readelf, &tools.objcopy, &tools.ar] {
        if !tool.exists() {
            panic!(
                "Required Android NDK tool not found: {}\nChecked NDK host tag '{}'. Set ANDROID_NDK_HOME to a valid NDK if needed.",
                tool.display(),
                host_tag
            );
        }
    }

    eprintln!(
        "Using Android NDK toolchain: {} (target arch {})",
        bin.display(),
        target_arch
    );
    tools
}

fn android_ndk_home() -> Option<PathBuf> {
    ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "NDK_HOME"]
        .into_iter()
        .filter_map(|key| env::var_os(key).map(PathBuf::from))
        .find(|path| !path.as_os_str().is_empty())
}

fn android_ndk_host_tag() -> String {
    if cfg!(target_os = "linux") {
        "linux-x86_64".to_string()
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "darwin-arm64".to_string()
        } else {
            "darwin-x86_64".to_string()
        }
    } else if cfg!(target_os = "windows") {
        "windows-x86_64".to_string()
    } else {
        panic!("Unsupported host OS for Android NDK tool discovery");
    }
}

fn exe(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn android_lld_emulation_args(target_arch: &str) -> Vec<&'static str> {
    match target_arch {
        "aarch64" | "arm64" => vec!["-m", "aarch64elf"],
        "arm" | "armv7" => vec!["-m", "armelf_linux_eabi"],
        "x86" | "i686" => vec!["-m", "elf_i386"],
        "x86_64" => vec!["-m", "elf_x86_64"],
        _ => Vec::new(),
    }
}

fn symbols_to_hide_from_readelf(readelf_output: &str, keep_prefix: &str) -> Vec<String> {
    let mut symbols_to_hide = Vec::new();
    for line in readelf_output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();

        // llvm-readelf -sW output format is compatible with readelf for these
        // columns:
        // Num: Value Size Type Bind Vis Ndx Name
        if parts.len() >= 8 {
            let bind = parts.get(4).unwrap_or(&"");
            let vis = parts.get(5).unwrap_or(&"");
            let ndx = parts.get(6).unwrap_or(&"");
            let symbol_name = parts.get(7).unwrap_or(&"");

            // Only localize defined externally-visible symbols. Undefined GLOBAL/WEAK
            // references must remain external so they can bind to libc, libdl, the
            // consumer, or compiler/runtime providers during the final Android link.
            if (*bind == "GLOBAL" || *bind == "WEAK")
                && (*vis == "DEFAULT" || *vis == "HIDDEN")
                && *ndx != "UND"
                && !symbol_name.is_empty()
            {
                let should_keep = symbol_name.starts_with(keep_prefix)
                    || symbol_name.starts_with("DW.ref.")
                    || symbol_name.starts_with("_GLOBAL_OFFSET_TABLE_");

                if !should_keep {
                    symbols_to_hide.push(symbol_name.to_string());
                }
            }
        }
    }
    symbols_to_hide
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lld_emulation_args_cover_android_arches() {
        assert_eq!(android_lld_emulation_args("aarch64"), ["-m", "aarch64elf"]);
        assert_eq!(android_lld_emulation_args("arm64"), ["-m", "aarch64elf"]);
        assert_eq!(
            android_lld_emulation_args("armv7"),
            ["-m", "armelf_linux_eabi"]
        );
        assert_eq!(android_lld_emulation_args("i686"), ["-m", "elf_i386"]);
        assert_eq!(android_lld_emulation_args("x86"), ["-m", "elf_i386"]);
        assert_eq!(android_lld_emulation_args("x86_64"), ["-m", "elf_x86_64"]);
        assert!(android_lld_emulation_args("mips64").is_empty());
    }

    #[test]
    fn readelf_parser_hides_only_defined_non_api_symbols() {
        let out = r#"
Symbol table '.symtab' contains 5 entries:
   Num:    Value          Size Type    Bind   Vis      Ndx Name
     1: 0000000000000000    10 FUNC    GLOBAL DEFAULT    1 internal_fn
     2: 0000000000000000    10 FUNC    GLOBAL DEFAULT    1 testlib_api
     3: 0000000000000000     0 NOTYPE  GLOBAL DEFAULT  UND rust_eh_personality
     4: 0000000000000000     4 OBJECT  WEAK   HIDDEN     2 weak_defined
     5: 0000000000000000     0 NOTYPE  WEAK   DEFAULT  UND weak_external
"#;
        assert_eq!(
            symbols_to_hide_from_readelf(out, "testlib_"),
            vec!["internal_fn".to_string(), "weak_defined".to_string()]
        );
    }
}
