use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolved ELF toolchain for the shared symbol-hiding routine. Linux and
/// Android are both ELF and differ only in which binaries run and whether the
/// partial link needs an explicit `-m <emulation>`, so each platform builds one
/// of these and hands it to `patch_elf`.
pub(crate) struct ElfToolchain {
    /// Partial linker (`ld`, a cross `*-ld`, or the NDK `ld.lld`).
    pub ld: PathBuf,
    /// Extra args placed before `-r` (the NDK path passes `-m <emulation>`).
    pub ld_pre_args: Vec<String>,
    /// Symbol reader (`readelf` or the NDK `llvm-readelf`).
    pub readelf: PathBuf,
    /// Symbol localizer (`objcopy` or the NDK `llvm-objcopy`).
    pub objcopy: PathBuf,
    /// Archiver (`ar`, a cross `*-ar`, or the NDK `llvm-ar`).
    pub ar: PathBuf,
    /// Archiver to retry with if `ar` fails (Linux falls back to `llvm-ar`).
    pub ar_fallback: Option<PathBuf>,
    /// Platform label for log lines ("Linux" / "Android").
    pub label: &'static str,
}

/// Hides all non-API symbols in an ELF static library: partially link the
/// archive into one relocatable object, localize every defined GLOBAL/WEAK
/// symbol that is not part of the kept API, then re-archive the object.
///
/// Shared by the Linux and Android paths, which supply only the toolchain (see
/// `ElfToolchain`); the symbol-selection and keep rules are identical because
/// the verifier treats both platforms' output the same way.
pub(crate) fn patch_elf(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    tools: &ElfToolchain,
) {
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Step 1: Partial link into one relocatable object.
    eprintln!(
        "Creating intermediate object with {} -r...",
        tools.ld.display()
    );
    let output = Command::new(&tools.ld)
        .args(&tools.ld_pre_args)
        .args(["-r", "-o"])
        .arg(&intermediate)
        .arg("--whole-archive")
        .arg(static_lib)
        .output()
        .unwrap_or_else(|_| panic!("Failed to run {}", tools.ld.display()));
    if !output.stderr.is_empty() {
        eprintln!(
            "{} stderr: {}",
            tools.ld.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(output.status.success(), "{} -r failed", tools.ld.display());

    // Step 2: Get all defined GLOBAL and WEAK symbols (any visibility).
    eprintln!("Extracting symbols to determine what to hide...");
    let readelf_output = Command::new(&tools.readelf)
        .args(["-sW"])
        .arg(&intermediate)
        .output()
        .unwrap_or_else(|_| panic!("Failed to run {}", tools.readelf.display()));
    if !readelf_output.status.success() {
        if !readelf_output.stderr.is_empty() {
            eprintln!(
                "{} stderr: {}",
                tools.readelf.display(),
                String::from_utf8_lossy(&readelf_output.stderr)
            );
        }
        panic!("{} failed", tools.readelf.display());
    }

    let symbols_output = String::from_utf8_lossy(&readelf_output.stdout);
    let symbols_to_hide = symbols_to_hide(&symbols_output, keep_prefix);

    eprintln!("Found {} symbols to hide", symbols_to_hide.len());

    // Step 3: Localize symbols using objcopy. objcopy reads `intermediate` and
    // writes a fresh `final_obj`, so it only needs a standalone copy when there
    // is nothing to localize (an unconditional copy would just be overwritten).
    if symbols_to_hide.is_empty() {
        fs::copy(&intermediate, &final_obj).expect("Failed to copy intermediate object");
    } else {
        eprintln!("Localizing symbols...");

        // Pass the names in a file rather than one --localize-symbol arg each: a
        // real Rust staticlib has thousands of long symbol names, which would
        // overflow the argv limit (E2BIG) before objcopy even runs.
        // --localize-symbols reads one name per line.
        let symbols_file = out_dir.join(format!("{}_localize.txt", lib_name));
        fs::write(&symbols_file, symbols_to_hide.join("\n"))
            .expect("Failed to write localize-symbols file");

        let status = Command::new(&tools.objcopy)
            .arg("--localize-symbols")
            .arg(&symbols_file)
            .arg(&intermediate)
            .arg(&final_obj)
            .status()
            .unwrap_or_else(|_| panic!("Failed to run {}", tools.objcopy.display()));

        if !status.success() {
            panic!(
                "{} failed during symbol localization",
                tools.objcopy.display()
            );
        }

        eprintln!("  Localized {} symbols", symbols_to_hide.len());
        fs::remove_file(&symbols_file).ok();
    }

    // Step 4: Create the final archive, falling back to a second archiver if the
    // primary one fails (Linux retries with llvm-ar).
    eprintln!("Creating final archive...");
    let ar_ok = Command::new(&tools.ar)
        .args(["rcs"])
        .arg(final_lib)
        .arg(&final_obj)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ar_ok {
        match &tools.ar_fallback {
            Some(fallback) => {
                let status = Command::new(fallback)
                    .args(["rcs"])
                    .arg(final_lib)
                    .arg(&final_obj)
                    .status()
                    .unwrap_or_else(|_| panic!("Failed to run {}", fallback.display()));
                assert!(
                    status.success(),
                    "Both {} and {} failed",
                    tools.ar.display(),
                    fallback.display()
                );
            }
            None => panic!("{} failed", tools.ar.display()),
        }
    }

    // Cleanup
    fs::remove_file(&intermediate).ok();
    fs::remove_file(&final_obj).ok();

    eprintln!("✓ {} patching complete", tools.label);
}

/// Parses `readelf -sW` output and returns the names of every defined GLOBAL or
/// WEAK symbol that must be localized (i.e. not kept by `should_keep`).
///
/// Visibility is deliberately NOT considered. A defined GLOBAL/WEAK symbol is
/// externally visible and conflicts just like a plain global whether its
/// visibility is DEFAULT, HIDDEN, PROTECTED, or INTERNAL: `nm` (and thus the
/// verifier) reports all of them as external, so all must be localized or the
/// verifier rejects the leftover as a leaked non-prefix global.
fn symbols_to_hide(readelf_output: &str, keep_prefix: &str) -> Vec<String> {
    let mut symbols_to_hide = Vec::new();
    for line in readelf_output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();

        // readelf -sW / llvm-readelf -sW share these columns:
        // Num:    Value          Size Type    Bind   Vis      Ndx Name
        // Example: "   94: 0000000000000000     0 NOTYPE  GLOBAL DEFAULT  UND rust_eh_personality"
        if parts.len() >= 8 {
            let bind = parts.get(4).unwrap_or(&"");
            let ndx = parts.get(6).unwrap_or(&"");
            let symbol_name = parts.get(7).unwrap_or(&"");

            if (*bind == "GLOBAL" || *bind == "WEAK")
                && *ndx != "UND"
                && !symbol_name.is_empty()
                && !should_keep(symbol_name, keep_prefix)
            {
                symbols_to_hide.push(symbol_name.to_string());
            }
        }
    }
    symbols_to_hide
}

/// Whether a defined ELF global/weak symbol stays public after patching.
///
/// Strips a leading underscore before the prefix match (via
/// `matches_keep_prefix`), matching macOS, Windows, and the verifier: ELF names
/// rarely carry one, but a symbol literally named `_<keep_prefix>...` would
/// otherwise be hidden here yet demanded by the verifier, failing a patch that
/// did the right thing.
fn should_keep(symbol_name: &str, keep_prefix: &str) -> bool {
    crate::matches_keep_prefix(symbol_name, keep_prefix)
        || symbol_name.starts_with("DW.ref.") // DWARF references
        || symbol_name.starts_with("_GLOBAL_OFFSET_TABLE_") // Special linker symbol
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn hides_defined_non_api_globals_of_any_visibility() {
        // PROTECTED and INTERNAL defined globals must be hidden too: visibility
        // does not narrow the set, or the verifier flags the leftover as a leak.
        let out = r#"
Symbol table '.symtab' contains 7 entries:
   Num:    Value          Size Type    Bind   Vis      Ndx Name
     1: 0000000000000000    10 FUNC    GLOBAL DEFAULT    1 internal_fn
     2: 0000000000000000    10 FUNC    GLOBAL DEFAULT    1 testlib_api
     3: 0000000000000000     0 NOTYPE  GLOBAL DEFAULT  UND rust_eh_personality
     4: 0000000000000000     4 OBJECT  WEAK   HIDDEN     2 weak_defined
     5: 0000000000000000     0 NOTYPE  WEAK   DEFAULT  UND weak_external
     6: 0000000000000000    10 FUNC    GLOBAL PROTECTED   1 protected_fn
     7: 0000000000000000    10 FUNC    GLOBAL INTERNAL    1 internal_vis_fn
"#;
        assert_eq!(
            symbols_to_hide(out, "testlib_"),
            vec![
                "internal_fn".to_string(),
                "weak_defined".to_string(),
                "protected_fn".to_string(),
                "internal_vis_fn".to_string(),
            ]
        );
    }
}
