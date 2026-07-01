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

/// Inserts the names of all globally-visible, *defined* symbols into `out`.
///
/// Undefined references are skipped: they have no definition to rename, and the
/// rename map (applied to every object) rewrites their use sites to match
/// whichever object defines the symbol. References that nothing in the archive
/// defines (e.g. `rust_eh_personality`, supplied by the consumer's `libstd`)
/// never enter the map and keep binding as before.
fn collect_defined_globals(data: &[u8], out: &mut HashSet<String>) {
    if let Ok(file) = File::parse(data) {
        for symbol in file.symbols() {
            if symbol.is_global()
                && !symbol.is_undefined()
                && let Ok(name) = symbol.name()
            {
                out.insert(name.to_string());
            }
        }
    }
}

/// Whether `data` is a regular COFF object the renamer can rewrite and the
/// archiver can store.
///
/// Non-COFF members (LLVM bitcode, import descriptors) return `false`.
/// `llvm-objcopy` rejects them, and passing them to `lib.exe` can crash the
/// librarian (`LNK1000`). They duplicate the native COFF members, so they are
/// dropped from the output.
fn is_patchable_coff(data: &[u8]) -> bool {
    matches!(File::parse(data), Ok(file) if file.format() == object::BinaryFormat::Coff)
}

/// Decides how a *defined* global symbol is treated under the allowlist.
///
/// Returns `Some(new_name)` to rename the symbol under `keep_prefix` (the
/// default for anything outside the public API), or `None` to leave it: it
/// already carries the prefix (it *is* public API), or it is an MSVC-mangled
/// name (`??...`) we must not touch.
///
/// The map is applied to every object via `llvm-objcopy --redefine-syms`, so a
/// symbol defined in one object and referenced from a sibling is renamed
/// identically on both sides and stays linkable. This covers `ring`'s
/// cross-object asm routines and their `i686`-decorated `_`-prefixed spellings,
/// which need no special-casing because the rename is purely name-based.
fn rename_target(symbol: &str, keep_prefix: &str) -> Option<String> {
    // 32-bit Windows decorates extern "C"/no_mangle exports with a leading
    // underscore (e.g. `_testlib_add`), so test the stripped form too, as the
    // verifier does. Otherwise public API would be renamed out of reach.
    let unprefixed = symbol.strip_prefix('_').unwrap_or(symbol);
    if symbol.starts_with(keep_prefix) || unprefixed.starts_with(keep_prefix) {
        return None;
    }
    // MSVC-mangled names (??...) must be left alone; everything else (including
    // .weak symbols, which would otherwise trigger LNK2005) gets renamed.
    if symbol.starts_with("??") {
        return None;
    }
    Some(format!("{}{}", keep_prefix, symbol))
}

/// Builds the archive-wide rename map: every defined global that `rename_target`
/// moves under `keep_prefix`, paired with a target name unique within the archive.
///
/// `rename_target` alone maps an internal `foo` onto `<keep_prefix>foo`, but that
/// name may already be a public API symbol (e.g. internal `add` -> `myapp_add`
/// when `myapp_add` is exported) or an earlier rename. Applying such a map would
/// leave two definitions of the same name, which `list_symbols` cannot see (it
/// dedups) but the consumer's linker hits as a duplicate symbol or a wrong bind.
/// So a colliding target gets a numeric suffix until it is unique. Symbols are
/// processed in sorted order to keep the map reproducible.
fn build_renames(defined: &HashSet<String>, keep_prefix: &str) -> Vec<(String, String)> {
    let mut symbols: Vec<&String> = defined.iter().collect();
    symbols.sort();

    let mut taken: HashSet<String> = defined.iter().cloned().collect();
    let mut renames = Vec::new();
    for symbol in symbols {
        let Some(base) = rename_target(symbol, keep_prefix) else {
            continue;
        };
        let mut target = base.clone();
        let mut n = 1u32;
        while taken.contains(&target) {
            target = format!("{}_{}", base, n);
            n += 1;
        }
        taken.insert(target.clone());
        renames.push((symbol.clone(), target));
    }
    renames
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
        obj_files.push((obj_path, is_patchable_coff(data)));

        // Parse object file to find defined symbols
        collect_defined_globals(data, &mut defined_symbols);
    }

    eprintln!("Extracted {} objects.", obj_files.len());
    eprintln!("Found {} defined symbols.", defined_symbols.len());

    // Step 2: Generate renames (collision-aware; see build_renames).
    let rename_pairs = build_renames(&defined_symbols, keep_prefix);
    let kept_count = defined_symbols
        .iter()
        .filter(|s| s.starts_with(keep_prefix))
        .count();

    eprintln!(
        "Renaming {} symbols (kept {} already prefixed).",
        rename_pairs.len(),
        kept_count
    );

    if rename_pairs.is_empty() {
        eprintln!("No symbols to rename. Copying file...");
        fs::copy(static_lib, final_lib).expect("Failed to copy library");
        return;
    }

    let renames_path = temp_dir.join("renames.txt");
    let mut f = fs::File::create(&renames_path).expect("Failed to create renames file");
    for (from, to) in &rename_pairs {
        writeln!(f, "{} {}", from, to).expect("Failed to write rename line");
    }

    // Step 3: Run llvm-objcopy on EACH object
    let objcopy = find_objcopy_tool();
    eprintln!("Using objcopy: {}", objcopy.display());
    eprintln!("Renaming symbols in objects...");

    let mut patched_files = Vec::new();

    for (i, (obj_path, is_coff)) in obj_files.iter().enumerate() {
        // Drop non-COFF members (LLVM bitcode, import descriptors): they carry no
        // symbols to rename, llvm-objcopy can't process them, and archiving them
        // can crash lib.exe (LNK1000). The native code lives in COFF members.
        if !is_coff {
            eprintln!("Skipping non-COFF object {} (not archived).", i);
            continue;
        }

        let patched_path = temp_dir.join(format!("{}_patched.obj", i));

        let status = Command::new(&objcopy)
            .arg(format!("--redefine-syms={}", renames_path.display()))
            .arg(obj_path)
            .arg(&patched_path)
            .status()
            .expect("Failed to execute llvm-objcopy");

        if status.success() {
            patched_files.push(patched_path);
        } else {
            // A genuine COFF object objcopy couldn't rewrite: keep the original
            // (valid COFF, just unrenamed) so its code is preserved. lib.exe
            // handles COFF fine, so this won't crash the librarian.
            eprintln!(
                "Warning: llvm-objcopy failed on COFF object {}; keeping it unrenamed.",
                i
            );
            patched_files.push(obj_path.clone());
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
    // Run from the temp dir and pass bare filenames so the archive stores
    // relative member names. Passing absolute paths would embed the build
    // location (and PID-based temp dir) into the library, making it
    // non-reproducible and tied to where it was built. All patched_files live
    // in temp_dir, so their file names are unambiguous.
    cmd.current_dir(&temp_dir);

    if lib_cmd.is_llvm {
        cmd.arg("rc");
        cmd.arg(&final_lib_abs);
        for obj in &patched_files {
            cmd.arg(obj.file_name().expect("patched object has no file name"));
        }
    } else {
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!("/OUT:{}", final_lib_abs.display()));
        for obj in &patched_files {
            cmd.arg(obj.file_name().expect("patched object has no file name"));
        }
    }

    let status = cmd.status().unwrap_or_else(|_| {
        panic!(
            "Failed to run {}. For cross-architecture patching, install LLVM tools.",
            lib_cmd.tool
        )
    });

    if !status.success() {
        panic!(
            "{} failed with exit code: {:?}",
            lib_cmd.tool,
            status.code()
        );
    }

    eprintln!("✓ Windows patching complete (via renaming)");
}

fn find_objcopy_tool() -> PathBuf {
    if let Ok(path) = which::which("llvm-objcopy") {
        return path;
    }
    if let Ok(path) = which::which("rust-objcopy") {
        return path;
    }

    let vs_llvm_paths = [
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
        r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin\llvm-objcopy.exe",
    ];

    for path_str in &vs_llvm_paths {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return path;
        }
    }

    if let Ok(output) = Command::new("rustc").arg("--print").arg("sysroot").output() {
        let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let sysroot_path = PathBuf::from(sysroot);
        let rustlib = sysroot_path.join("lib").join("rustlib");
        if let Ok(entries) = fs::read_dir(&rustlib) {
            for entry in entries.flatten() {
                let bin_objcopy = entry.path().join("bin").join("rust-objcopy.exe");
                if bin_objcopy.exists() {
                    return bin_objcopy;
                }
            }
        }
    }

    panic!(
        "Windows patching renames symbols with llvm-objcopy (or rust-objcopy), but neither \
         was found. MSVC's lib.exe/llvm-lib can archive but cannot rename symbols, so an \
         objcopy is required. Install LLVM (provides llvm-objcopy) or add the Rust llvm-tools \
         component with `rustup component add llvm-tools` (provides rust-objcopy)."
    );
}

/// Maps a target architecture to its MSVC `/MACHINE` type (e.g. "x86_64" ->
/// "X64"). Returns `None` for architectures with no known machine flag, in which
/// case the librarian is invoked without an explicit `/MACHINE`.
fn msvc_machine_type(arch: &str) -> Option<&'static str> {
    match arch {
        "aarch64" | "arm64" => Some("ARM64"),
        "x86_64" => Some("X64"),
        "x86" | "i686" => Some("X86"),
        "arm" => Some("ARM"),
        _ => None,
    }
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
    let machine_type = msvc_machine_type(&target_arch_str).map(String::from);

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

#[cfg(test)]
mod tests {
    //! Tests for the Windows/COFF renaming path.
    //!
    //! The COFF backend keeps only the public API global by *renaming* every other
    //! defined symbol under `keep_prefix` (via `llvm-objcopy --redefine-syms`). The
    //! rename map is keyed by name and applied to every object, so a symbol defined
    //! in one object and referenced from a sibling (e.g. `ring`'s `ring_core_*` asm
    //! routines) is rewritten identically on both sides and stays linkable. These
    //! tests pin that down on synthetic COFF objects built with `object::write`, the
    //! same def/ref shape that reproduced the original `ring` static-link failure.

    use super::*;
    use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind,
        SymbolScope,
    };

    const RING_SYM: &str = "ring_core_0_17_14__sha256_block_data_order_hw";
    const PREFIX: &str = "myapp_";

    /// Reads a NUL-terminated string out of the COFF string table.
    fn read_cstr(data: &[u8], off: usize) -> String {
        let end = data[off..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| off + p)
            .unwrap_or(data.len());
        String::from_utf8_lossy(&data[off..end]).to_string()
    }

    /// `(name, storage_class, section_number)` for every primary symbol entry,
    /// parsed straight from the COFF symbol table (auxiliary entries skipped).
    fn coff_symbols(data: &[u8]) -> Vec<(String, u8, i16)> {
        use object::LittleEndian as LE;
        use object::pe;
        use object::read::coff::CoffHeader;

        let mut offset = 0u64;
        let header = pe::ImageFileHeader::parse(data, &mut offset).unwrap();
        let sym_off = header.pointer_to_symbol_table.get(LE) as usize;
        let count = header.number_of_symbols.get(LE) as usize;
        let str_off = sym_off + count * 18;

        let mut out = Vec::new();
        let mut i = 0;
        while i < count {
            let so = sym_off + i * 18;
            let entry = &data[so..so + 18];
            let storage = entry[16];
            let section = i16::from_le_bytes([entry[12], entry[13]]);
            let n_aux = entry[17] as usize;
            let name = if entry[0..4] == [0, 0, 0, 0] {
                let strofs = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]) as usize;
                read_cstr(data, str_off + strofs)
            } else {
                let end = entry[0..8].iter().position(|&b| b == 0).unwrap_or(8);
                String::from_utf8_lossy(&entry[0..end]).to_string()
            };
            out.push((name, storage, section));
            i += 1 + n_aux;
        }
        out
    }

    /// Object A: *defines* `name` in `.text` as an external (global) symbol,
    /// mirroring the ring object that contains the asm routine body.
    fn make_def_object(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        let off = obj.append_section_data(text, &[0x90, 0x90, 0xc3], 16); // nop; nop; ret
        obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: off,
            size: 3,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// Object B: *references* `name` via a REL32 relocation (an external undefined
    /// symbol), mirroring the ring object that calls into the asm routine.
    fn make_ref_object(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        // e8 <rel32> = call rel32; c3 = ret. The rel32 placeholder is relocated.
        let off = obj.append_section_data(text, &[0xe8, 0, 0, 0, 0, 0xc3], 16);
        let sym = obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        obj.add_relocation(
            text,
            Relocation {
                offset: off + 1,
                symbol: sym,
                addend: -4,
                flags: RelocationFlags::Coff {
                    typ: object::pe::IMAGE_REL_AMD64_REL32,
                },
            },
        )
        .unwrap();
        obj.write().unwrap()
    }

    /// Builds the rename map exactly as `patch_windows` does: collect the defined
    /// globals across every object, then run the collision-aware map builder.
    fn rename_map(
        objects: &[Vec<u8>],
        keep_prefix: &str,
    ) -> std::collections::HashMap<String, String> {
        let mut defined = HashSet::new();
        for obj in objects {
            collect_defined_globals(obj, &mut defined);
        }
        build_renames(&defined, keep_prefix).into_iter().collect()
    }

    #[test]
    fn harness_builds_expected_def_and_ref_objects() {
        // Sanity-check the synthetic objects: the def is a global (storage class 2)
        // symbol in a real section, the ref is the same name left undefined (section 0).
        let def = coff_symbols(&make_def_object(RING_SYM));
        let (_, sc, sec) = def.iter().find(|(n, _, _)| n == RING_SYM).unwrap();
        assert_eq!(*sc, 2, "definition must be External");
        assert!(*sec > 0, "definition must live in a section");

        let r = coff_symbols(&make_ref_object(RING_SYM));
        let (_, sc, sec) = r.iter().find(|(n, _, _)| n == RING_SYM).unwrap();
        assert_eq!(*sc, 2, "reference must be External");
        assert_eq!(*sec, 0, "reference must be undefined");
    }

    #[test]
    fn defined_globals_collected_but_undefined_refs_are_not() {
        // Only the defining object contributes RING_SYM; the referencing object
        // contributes nothing, so an undefined-only symbol never enters the map.
        let mut from_def = HashSet::new();
        collect_defined_globals(&make_def_object(RING_SYM), &mut from_def);
        assert!(from_def.contains(RING_SYM));

        let mut from_ref = HashSet::new();
        collect_defined_globals(&make_ref_object(RING_SYM), &mut from_ref);
        assert!(
            !from_ref.contains(RING_SYM),
            "an undefined reference must not be treated as a definition"
        );
    }

    #[test]
    fn ring_intra_library_symbol_is_renamed_consistently() {
        // The defining object and the sibling that references it are processed into
        // one rename map. Because the map is keyed by name, both sides get rewritten
        // to the same `keep_prefix`ed name and stay linkable.
        let map = rename_map(
            &[make_def_object(RING_SYM), make_ref_object(RING_SYM)],
            PREFIX,
        );
        assert_eq!(
            map.get(RING_SYM).map(String::as_str),
            Some(format!("{PREFIX}{RING_SYM}").as_str()),
            "ring's cross-object symbol must be renamed under keep_prefix"
        );
    }

    #[test]
    fn underscore_decorated_ring_symbol_is_renamed() {
        // 32-bit Windows GNU (i686) decorates externals with a leading underscore.
        // The rename is purely name-based, so the decorated spelling is handled with
        // no special-casing.
        let decorated = format!("_{RING_SYM}");
        let map = rename_map(&[make_def_object(&decorated)], PREFIX);
        assert_eq!(
            map.get(&decorated).map(String::as_str),
            Some(format!("{PREFIX}{decorated}").as_str()),
        );
    }

    #[test]
    fn rust_eh_personality_reference_is_left_to_libstd() {
        // `rust_eh_personality` is defined by the consumer's `libstd`, so in a
        // patched archive it appears only as an undefined cross-object reference.
        // It must NOT be renamed, or it would no longer bind to libstd's copy.
        let map = rename_map(&[make_ref_object("rust_eh_personality")], PREFIX);
        assert!(
            map.is_empty(),
            "an undefined `rust_eh_personality` reference must not be renamed"
        );
    }

    #[test]
    fn internal_symbol_is_not_renamed_onto_an_existing_public_symbol() {
        // A public `myapp_add` and an internal `add` in the same archive: naively
        // renaming `add` -> `myapp_add` would leave two definitions of the public
        // symbol. The collision must be disambiguated instead.
        let map = rename_map(
            &[make_def_object("add"), make_def_object("myapp_add")],
            PREFIX,
        );
        assert_eq!(
            map.get("myapp_add"),
            None,
            "the public symbol is kept as-is"
        );
        let renamed = map.get("add").expect("the internal symbol must be renamed");
        assert_ne!(
            renamed, "myapp_add",
            "must not collide with the public symbol"
        );
        assert!(
            renamed.starts_with(PREFIX),
            "the renamed internal still lives under keep_prefix"
        );
    }

    #[test]
    fn ordinary_defined_symbol_is_renamed() {
        // A defined symbol outside the public API is renamed under keep_prefix.
        let map = rename_map(&[make_def_object("some_internal_function")], PREFIX);
        assert_eq!(
            map.get("some_internal_function").map(String::as_str),
            Some(format!("{PREFIX}some_internal_function").as_str()),
        );
    }

    #[test]
    fn only_coff_objects_are_archived() {
        // Real COFF objects are patchable and kept.
        assert!(is_patchable_coff(&make_def_object(RING_SYM)));
        assert!(is_patchable_coff(&make_ref_object(RING_SYM)));
        // Non-COFF members must be rejected so they are never fed to lib.exe:
        // LLVM bitcode (BC\xC0\xDE magic) and arbitrary garbage.
        assert!(!is_patchable_coff(&[0x42, 0x43, 0xc0, 0xde, 0, 0, 0, 0]));
        assert!(!is_patchable_coff(b"not an object file"));
    }

    #[test]
    fn keep_prefix_and_mangled_symbols_stay_public() {
        // Already-public API keeps its name; MSVC-mangled (??...) names are untouched.
        assert_eq!(rename_target("myapp_add", PREFIX), None);
        assert_eq!(rename_target("??_C@_05foo@bar@", PREFIX), None);
        // Everything else is renamed.
        assert_eq!(
            rename_target("internal", PREFIX),
            Some("myapp_internal".to_string())
        );
    }

    #[test]
    fn arch_maps_to_msvc_machine_type() {
        assert_eq!(msvc_machine_type("aarch64"), Some("ARM64"));
        assert_eq!(msvc_machine_type("arm64"), Some("ARM64"));
        assert_eq!(msvc_machine_type("x86_64"), Some("X64"));
        assert_eq!(msvc_machine_type("i686"), Some("X86"));
        assert_eq!(msvc_machine_type("arm"), Some("ARM"));
        // Unknown arch: the librarian runs without an explicit /MACHINE.
        assert_eq!(msvc_machine_type("mips"), None);
    }

    #[test]
    fn underscore_decorated_public_symbol_stays_public() {
        // 32-bit Windows decorates extern "C"/no_mangle exports with a leading
        // underscore. `_testlib_add` is public API under `--keep-prefix testlib_`
        // and must not be renamed (which would hide it as `testlib__testlib_add`).
        assert_eq!(rename_target("_testlib_add", "testlib_"), None);
        assert_eq!(rename_target("testlib_add", "testlib_"), None);
        // A non-public underscore-decorated symbol is still renamed.
        assert_eq!(
            rename_target("_internal", "testlib_"),
            Some("testlib__internal".to_string())
        );
    }

    /// Guards `patch_windows`'s object-parsing path against real toolchain COFF.
    /// When `LIB_PATCHER_REAL_COFF_ARCHIVE` points at a real Windows static
    /// library (CI sets it on the Windows runner), assert the object crate parses
    /// its members the way `patch_windows` relies on: at least one patchable COFF
    /// object, and the public API symbol is collected.
    ///
    /// The synthetic `object::write` COFF used by the other tests can keep parsing
    /// fine even when the object crate mishandles real rustc/MSVC archives, so
    /// this exercises a real archive to catch that class of breakage at the
    /// parsing stage rather than as an unresolved-symbol link failure in the C
    /// consumer.
    #[test]
    fn parses_real_windows_archive_when_provided() {
        let Ok(path) = env::var("LIB_PATCHER_REAL_COFF_ARCHIVE") else {
            return; // fixture not provided (non-Windows CI / local): nothing to check
        };
        let bytes = fs::read(&path).expect("failed to read the fixture archive");
        let archive = object::read::archive::ArchiveFile::parse(&*bytes)
            .expect("a real static library must parse as an archive");

        let mut any_coff = false;
        let mut defined = HashSet::new();
        for member in archive.members() {
            let data = member
                .expect("archive member")
                .data(&*bytes)
                .expect("member data");
            if is_patchable_coff(data) {
                any_coff = true;
            }
            collect_defined_globals(data, &mut defined);
        }

        assert!(any_coff, "real archive must contain patchable COFF members");
        assert!(
            defined.contains("testlib_add"),
            "collect_defined_globals must find the public API in a real archive"
        );
    }
}
