use std::fs;
use std::path::Path;
use std::process::Command;

use object::read::archive::ArchiveFile;

/// macOS / iOS / tvOS / visionOS implementation: Uses ld -r + exported_symbols_list
pub(crate) fn patch_macos(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
    triplet: Option<&str>,
) {
    let arch = match target_arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x86_64",
        a => a,
    };

    let (platform, min_ver, sdk_ver) = apple_platform_version(triplet, arch);

    let temp_obj_dir = out_dir.join(format!("{}_objs", lib_name));
    let intermediate = out_dir.join(format!("{}_temp.o", lib_name));
    let final_obj = out_dir.join(format!("{}_final.o", lib_name));

    // Step 1: Extract every archive member to its own index-named file.
    //
    // `ar x` extracts members under their own names into one directory, so two
    // members sharing a base name (e.g. a `foo.o` from each of two bundled C
    // deps) silently overwrite each other and one object's symbols never reach
    // `ld -r`. Read the members via the object crate and write each to a unique
    // `<idx>.o` instead, mirroring the Windows path.
    fs::create_dir_all(&temp_obj_dir).expect("Failed to create temp object directory");

    eprintln!("Extracting objects from archive...");
    let archive_bytes = fs::read(static_lib).expect("Failed to read static lib");
    let archive = ArchiveFile::parse(&*archive_bytes)
        .unwrap_or_else(|e| panic!("Failed to parse static lib as archive: {}", e));

    let mut obj_files = Vec::new();
    for member in archive.members() {
        let member = member.expect("Failed to read archive member");
        // Skip the archive symbol table and extended-name members.
        let name = String::from_utf8_lossy(member.name());
        if name == "/" || name == "//" || name.starts_with("__.SYMDEF") {
            continue;
        }
        let data = member
            .data(&*archive_bytes)
            .expect("Failed to read archive member data");
        let obj_path = temp_obj_dir.join(format!("{}.o", obj_files.len()));
        fs::write(&obj_path, data).expect("Failed to write object file");
        obj_files.push(obj_path);
    }

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
        .arg(platform)
        .arg(min_ver)
        .arg(sdk_ver)
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

    // Parse nm output (`nm -g -U`, defined globals) to find symbols to hide.
    let symbols_to_hide = symbols_to_hide_from_nm(&nm_stdout, keep_prefix);

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
    let keep_symbols = keep_symbols_from_nm(&all_symbols_stdout, &symbols_to_hide);

    if keep_symbols.is_empty() {
        eprintln!("Warning: No symbols will be kept global. This may not be intended.");
    }

    fs::write(&symbols_file, keep_symbols.join("\n")).expect("Failed to write symbols file");

    // Step 5: Filter symbols using ld -r with -exported_symbols_list
    eprintln!("Filtering symbols...");
    let status = Command::new("xcrun")
        .arg("ld")
        .arg("-arch")
        .arg(arch)
        .arg("-r")
        .arg("-platform_version")
        .arg(platform)
        .arg(min_ver)
        .arg(sdk_ver)
        .arg("-o")
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

/// Parses `nm -g -U` output (defined globals) and returns the *raw* symbol names
/// to hide: everything not matching `keep_prefix` or a compiler-internal
/// exemption. nm lines look like `0000000000000000 T _symbol_name`.
///
/// The leading underscore is stripped only for the keep/hide decision; the name
/// is stored exactly as nm reports it, so `keep_symbols_from_nm` can subtract it
/// from the raw `nm -g` names. Re-adding an underscore would miss a global with
/// no leading underscore (e.g. hand-written asm), leaving it exported and then
/// failing verification.
fn symbols_to_hide_from_nm(nm_stdout: &str, keep_prefix: &str) -> Vec<String> {
    let mut symbols_to_hide = Vec::new();
    for line in nm_stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let symbol_type = parts[1];
            let symbol_name = parts[2];

            // Only process defined symbols (T, D, S, B, etc. - uppercase means global)
            if symbol_type.chars().next().unwrap_or('_').is_uppercase() {
                let unprefixed = symbol_name.strip_prefix('_').unwrap_or(symbol_name);
                let should_keep = unprefixed.starts_with(keep_prefix)
                    || unprefixed.starts_with("DW.ref.")
                    || unprefixed.starts_with("GCC_except_table");

                if !should_keep {
                    symbols_to_hide.push(symbol_name.to_string());
                }
            }
        }
    }
    symbols_to_hide
}

/// Parses `nm -g` output (all globals) and returns the exported allowlist: every
/// global whose raw name is not in `symbols_to_hide`. Both sides use the raw
/// nm spelling, so the subtraction is exact.
fn keep_symbols_from_nm(nm_stdout: &str, symbols_to_hide: &[String]) -> Vec<String> {
    let mut keep_symbols = Vec::new();
    for line in nm_stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let symbol_type = parts[1];
            let symbol_name = parts[2];

            if symbol_type.chars().next().unwrap_or('_').is_uppercase()
                && !symbols_to_hide.contains(&symbol_name.to_string())
            {
                keep_symbols.push(symbol_name.to_string());
            }
        }
    }
    keep_symbols
}

/// Returns the `ld -platform_version` arguments for an Apple target triplet.
///
/// Returns `(platform, min_version, sdk_version)`. The sdk_version is set to a
/// recent-enough value; ld is lenient about it during partial (`-r`) links.
fn apple_platform_version(
    triplet: Option<&str>,
    arch: &str,
) -> (&'static str, &'static str, &'static str) {
    let Some(triplet) = triplet else {
        return if arch == "arm64" {
            ("macos", "11.0", "14.0")
        } else {
            ("macos", "10.13", "14.0")
        };
    };

    if triplet.contains("apple-tvos") {
        // x86_64-apple-tvos is the Intel tvOS Simulator: tvOS never ran on Intel
        // devices, so there is no -sim suffix to key on.
        return if triplet.ends_with("-sim") || arch == "x86_64" {
            ("tvos-simulator", "15.0", "17.0")
        } else {
            ("tvos", "15.0", "17.0")
        };
    }

    if triplet.contains("apple-visionos") {
        return if triplet.contains("-sim") {
            ("xros-simulator", "1.0", "2.0")
        } else {
            ("xros", "1.0", "2.0")
        };
    }

    if triplet.contains("apple-ios") {
        // Order matters: check Catalyst (-macabi) first, then simulator.
        // x86_64-apple-ios is the Intel iOS Simulator and carries no -sim suffix
        // (iOS never ran on Intel devices), so treat x86_64 as simulator too.
        if triplet.ends_with("-macabi") {
            return ("mac-catalyst", "15.0", "17.0");
        } else if triplet.ends_with("-sim") || arch == "x86_64" {
            return ("ios-simulator", "15.0", "17.0");
        } else {
            return ("ios", "15.0", "17.0");
        }
    }

    if arch == "arm64" {
        ("macos", "11.0", "14.0")
    } else {
        ("macos", "10.13", "14.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_underscore_global_is_hidden_and_not_re_exported() {
        // A defined global with no leading underscore (e.g. hand-written asm) and
        // one with the usual underscore. Only the API symbol may survive; the
        // round-trip must hide the rest regardless of the underscore spelling.
        let nm_g_u = "\
0000000000000000 T foo\n\
0000000000000010 T _bar\n\
0000000000000020 T _mylib_add\n";
        let hide = symbols_to_hide_from_nm(nm_g_u, "mylib_");
        // Raw spellings preserved; the no-underscore `foo` is included as `foo`.
        assert_eq!(hide, vec!["foo".to_string(), "_bar".to_string()]);

        // Step 4 subtracts the hide set from the full global list. `foo` must not
        // leak back into the exported allowlist.
        let keep = keep_symbols_from_nm(nm_g_u, &hide);
        assert_eq!(keep, vec!["_mylib_add".to_string()]);
        assert!(
            !keep.contains(&"foo".to_string()),
            "a no-underscore global must not remain exported"
        );
    }

    #[test]
    fn macos_default_baseline_depends_on_arch() {
        // With no triplet, Apple silicon baselines at macOS 11; x86_64 goes back
        // to 10.13.
        assert_eq!(
            apple_platform_version(None, "arm64"),
            ("macos", "11.0", "14.0")
        );
        assert_eq!(
            apple_platform_version(None, "x86_64"),
            ("macos", "10.13", "14.0")
        );
    }

    #[test]
    fn ios_device_simulator_and_catalyst_are_distinguished() {
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-ios"), "arm64").0,
            "ios"
        );
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-ios-sim"), "arm64").0,
            "ios-simulator"
        );
        assert_eq!(
            apple_platform_version(Some("x86_64-apple-ios-macabi"), "x86_64").0,
            "mac-catalyst"
        );
    }

    #[test]
    fn intel_ios_and_tvos_triples_are_simulators_without_a_sim_suffix() {
        // x86_64-apple-ios / -tvos are the Intel simulator targets: those OSes
        // never ran on Intel devices, so there is no -sim suffix to key on and
        // arch alone identifies the simulator.
        assert_eq!(
            apple_platform_version(Some("x86_64-apple-ios"), "x86_64").0,
            "ios-simulator"
        );
        assert_eq!(
            apple_platform_version(Some("x86_64-apple-tvos"), "x86_64").0,
            "tvos-simulator"
        );
        // The Intel Catalyst triple still maps to mac-catalyst, not the simulator.
        assert_eq!(
            apple_platform_version(Some("x86_64-apple-ios-macabi"), "x86_64").0,
            "mac-catalyst"
        );
    }

    #[test]
    fn tvos_and_visionos_map_to_ld_platform_names() {
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-tvos"), "arm64").0,
            "tvos"
        );
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-tvos-sim"), "arm64").0,
            "tvos-simulator"
        );
        // visionOS uses ld's "xros" platform name.
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-visionos"), "arm64").0,
            "xros"
        );
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-visionos-sim"), "arm64").0,
            "xros-simulator"
        );
    }

    #[test]
    fn darwin_triplet_falls_through_to_macos_baseline() {
        assert_eq!(
            apple_platform_version(Some("aarch64-apple-darwin"), "arm64"),
            ("macos", "11.0", "14.0")
        );
        assert_eq!(
            apple_platform_version(Some("x86_64-apple-darwin"), "x86_64"),
            ("macos", "10.13", "14.0")
        );
    }
}
