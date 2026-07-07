use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::read::File;
use object::read::coff::{ImportFile, ImportName, ImportType};
use object::{Object as ObjectTrait, ObjectSymbol};

pub(crate) struct WindowsLibTool {
    pub tool: String,
    pub machine_type: Option<String>,
    pub is_llvm: bool,
    /// Whether the tool can regenerate DLL import libraries: `/def:` plus merging
    /// `.lib` inputs. True for lib.exe-style tools (`llvm-lib`, `lib.exe`); false
    /// for `llvm-ar`, which has neither. See `regenerate_import_libs`.
    pub handles_import_members: bool,
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
                && symbol.is_definition()
                && let Ok(name) = symbol.name()
            {
                out.insert(name.to_string());
            }
        }
    }
}

/// Inserts the names of all globally-visible, external-reference symbols into `out`.
///
/// These are external references the archive expects something else to define
/// (a consumer callback, a system import). They are collected so a rename target
/// never lands on one: renaming an internal onto a referenced name would satisfy
/// that external reference with the internal definition instead of the intended
/// provider.
///
/// COFF weak externals are section-0 references but `object` does not report them
/// as `is_undefined()`, so reserve every global non-definition rather than only
/// plain undefined externals.
fn collect_undefined_globals(data: &[u8], out: &mut HashSet<String>) {
    if let Ok(file) = File::parse(data) {
        for symbol in file.symbols() {
            if symbol.is_global()
                && !symbol.is_definition()
                && let Ok(name) = symbol.name()
            {
                out.insert(name.to_string());
            }
        }
    }
}

/// How an input archive member is treated when repackaging.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MemberKind {
    /// A regular COFF object: rename its symbols and archive it.
    Coff,
    /// A COFF short-import member: the `Foo`/`__imp_Foo` thunks for a DLL export.
    /// Decoded so the DLL's import library can be regenerated. See
    /// [`is_import_member`].
    ShortImport,
    /// An import-descriptor object: the `.idata$*` head/tail members
    /// (`__IMPORT_DESCRIPTOR_<dll>`, `__NULL_IMPORT_DESCRIPTOR`,
    /// `<dll>_NULL_THUNK_DATA`) of a DLL import library. Dropped; the regenerated
    /// library brings a fresh one. See [`is_import_member`].
    ImportDescriptor,
    /// Anything else (LLVM bitcode): dropped. `llvm-objcopy` rejects these and
    /// `lib.exe` can crash on them (`LNK1000`).
    Other,
}

impl MemberKind {
    /// Whether the member is part of a DLL import library (short-import or
    /// descriptor), which is regenerated rather than renamed or re-archived: a
    /// re-archived import member makes `link.exe` build an empty import directory,
    /// faulting the consumer at load (`0xC0000005`). See `regenerate_import_libs`.
    fn is_import_member(self) -> bool {
        matches!(self, MemberKind::ShortImport | MemberKind::ImportDescriptor)
    }
}

/// Classifies an archive member so the repackager knows whether to rename it,
/// copy it through unchanged, or drop it. See [`MemberKind`].
fn classify_member(data: &[u8]) -> MemberKind {
    match object::FileKind::parse(data) {
        Ok(object::FileKind::Coff) if is_import_descriptor_object(data) => {
            MemberKind::ImportDescriptor
        }
        Ok(object::FileKind::Coff) => MemberKind::Coff,
        Ok(object::FileKind::CoffImport) => MemberKind::ShortImport,
        _ => MemberKind::Other,
    }
}

/// Whether a COFF object is an import-descriptor member: at least one section,
/// and every section an import-directory section (`.idata$*`). Such objects carry
/// only DLL import plumbing, never Rust code. Shares the classifier the verifier
/// uses (`crate::is_import_descriptor_member`) so the drop decision here and the
/// leak-exemption there cannot diverge.
fn is_import_descriptor_object(data: &[u8]) -> bool {
    matches!(File::parse(data), Ok(file) if crate::is_import_descriptor_member(&file))
}

/// How a DLL import binds: by ordinal, or by the DLL-side export name (which may
/// differ from the local `symbol` for a decorated or `EXPORTAS` import).
enum Import {
    Ordinal(u16),
    Name(String),
}

/// A single DLL import decoded from a COFF short-import member.
struct ImportEntry {
    /// The DLL the symbol is imported from (e.g. `bcryptprimitives.dll`).
    dll: String,
    /// The local symbol name the consumer references (drives `__imp_<symbol>`).
    symbol: String,
    /// How it binds in the DLL.
    import: Import,
    /// A data import (variable/constant) rather than a function. The `.def`
    /// entry needs the `DATA` attribute so the regenerated import is not
    /// rebuilt as a function, which would link incorrectly.
    is_data: bool,
}

/// Decodes a COFF short-import member (`IMPORT_OBJECT` format), or `None` if
/// `data` is not one. `object`'s `ImportFile` resolves the DLL-side name across
/// every name type (plain, undecorated, `EXPORTAS`) and the ordinal case.
fn decode_short_import(data: &[u8]) -> Option<ImportEntry> {
    let file = ImportFile::parse(data).ok()?;
    let import = match file.import() {
        ImportName::Ordinal(n) => Import::Ordinal(n),
        ImportName::Name(n) => Import::Name(String::from_utf8_lossy(n).into_owned()),
    };
    Some(ImportEntry {
        dll: String::from_utf8_lossy(file.dll()).into_owned(),
        symbol: String::from_utf8_lossy(file.symbol()).into_owned(),
        import,
        // Code is a function; Data/Const are variables/constants that need the
        // `.def` DATA attribute.
        is_data: !matches!(file.import_type(), ImportType::Code),
    })
}

/// Regenerates an import library per DLL from `entries`, returning the generated
/// `.lib` paths (inside `temp_dir`).
///
/// rustc's bundled import members cannot be re-archived (see
/// [`MemberKind::is_import_member`]), so rebuild each DLL's import library from a
/// `.def` via the librarian's `/def:` mode, the canonical `link.exe`-consumable
/// path. `lib_cmd` must be lib.exe-style; `/def:` is not an `llvm-ar` flag, hence
/// the `handles_import_members` gate.
fn regenerate_import_libs(
    entries: &[ImportEntry],
    temp_dir: &Path,
    lib_cmd: &WindowsLibTool,
) -> Vec<PathBuf> {
    let mut by_dll: BTreeMap<&str, Vec<&ImportEntry>> = BTreeMap::new();
    for entry in entries {
        by_dll.entry(entry.dll.as_str()).or_default().push(entry);
    }

    let mut libs = Vec::new();
    for (n, (dll, syms)) in by_dll.iter().enumerate() {
        let def_path = temp_dir.join(format!("imp{}.def", n));
        let lib_path = temp_dir.join(format!("imp{}.lib", n));

        let mut f = fs::File::create(&def_path).expect("Failed to create .def file");
        writeln!(f, "LIBRARY {}", dll).expect("Failed to write .def");
        writeln!(f, "EXPORTS").expect("Failed to write .def");
        let mut seen = HashSet::new();
        for entry in syms {
            if !seen.insert(entry.symbol.as_str()) {
                continue;
            }
            // `name` -> import by that name; `name=export` when the DLL exports it
            // under a different name (decorated/EXPORTAS); `name @ord NONAME` for
            // an ordinal import. A trailing ` DATA` marks a variable/constant so
            // it is not rebuilt as a function import.
            let data = if entry.is_data { " DATA" } else { "" };
            match &entry.import {
                Import::Ordinal(ord) => writeln!(f, "  {} @{} NONAME{}", entry.symbol, ord, data),
                Import::Name(name) if *name == entry.symbol => {
                    writeln!(f, "  {}{}", entry.symbol, data)
                }
                Import::Name(name) => writeln!(f, "  {}={}{}", entry.symbol, name, data),
            }
            .expect("Failed to write .def");
        }
        drop(f);

        let mut cmd = Command::new(&lib_cmd.tool);
        cmd.current_dir(temp_dir);
        cmd.arg("/nologo");
        if let Some(machine) = &lib_cmd.machine_type {
            cmd.arg(format!("/MACHINE:{}", machine));
        }
        cmd.arg(format!(
            "/def:{}",
            def_path.file_name().unwrap().to_string_lossy()
        ));
        cmd.arg(format!(
            "/out:{}",
            lib_path.file_name().unwrap().to_string_lossy()
        ));

        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            // This DLL's short-import members are already dropped from
            // patched_files, so continuing emits an archive missing them.
            // Verification only checks defined API symbols and would pass,
            // leaving consumers with unresolved externals. Fail loudly, like
            // step 4.
            panic!(
                "{} failed to regenerate the import library for {} ({} symbols). \
                 Its imports have already been dropped from the archive, so \
                 refusing to emit an incomplete library.",
                lib_cmd.tool,
                dll,
                seen.len()
            );
        }
        eprintln!(
            "Regenerated import library for {} ({} symbols).",
            dll,
            seen.len()
        );
        libs.push(lib_path);
    }
    libs
}

/// Whether `symbol` is an import library's COMDAT head/tail symbol, which must
/// keep its name. Matched by substring/suffix since the 32-bit toolchain prefixes
/// an extra underscore.
fn is_import_machinery(symbol: &str) -> bool {
    symbol.contains("__IMPORT_DESCRIPTOR_")
        || symbol.contains("__NULL_IMPORT_DESCRIPTOR")
        || symbol.ends_with("_NULL_THUNK_DATA")
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
    // Public API keeps its name. 32-bit Windows decorates extern "C"/no_mangle
    // exports with a leading underscore (e.g. `_testlib_add`), so the shared
    // matcher tests the stripped form too; otherwise public API would be renamed
    // out of reach.
    if crate::matches_keep_prefix(symbol, keep_prefix) {
        return None;
    }
    // MSVC-mangled names (??...) must be left alone; everything else (including
    // .weak symbols, which would otherwise trigger LNK2005) gets renamed.
    if symbol.starts_with("??") {
        return None;
    }
    // COFF @-prefixed linker symbols (e.g. `@feat.00`) are consumed specially by
    // the linker; renaming them drops the feature metadata they carry. The
    // verifier classifies them as required linker symbols too (see
    // symbol_is_allowed_global).
    if symbol.starts_with('@') {
        return None;
    }
    // DLL import plumbing must keep its name so it folds with the consumer's real
    // import library; renaming it corrupts the import directory. See
    // MemberKind::is_import_member.
    if is_import_machinery(symbol) {
        return None;
    }
    Some(format!("{}{}", keep_prefix, symbol))
}

/// Builds the archive-wide rename map: every defined global that `rename_target`
/// moves under `keep_prefix`, paired with a target name unique within the archive.
///
/// `rename_target` alone maps an internal `foo` onto `<keep_prefix>foo`, but that
/// name may already exist in the archive: as a public API symbol (e.g. internal
/// `add` -> `myapp_add` when `myapp_add` is exported), as an undefined external
/// reference (`myapp_add` a callback/import expects the consumer to provide), or
/// as an earlier rename. Renaming onto a defined name leaves two definitions;
/// renaming onto a referenced name captures that reference with the internal
/// definition. Neither is visible to `list_symbols` (it lists defined names and
/// dedups), but the consumer's linker hits a duplicate symbol or a wrong bind. So
/// every existing name (defined and undefined) is reserved, and a colliding
/// target gets a numeric suffix until unique. Symbols are processed in sorted
/// order to keep the map reproducible.
fn build_renames(
    defined: &HashSet<String>,
    undefined: &HashSet<String>,
    keep_prefix: &str,
) -> Vec<(String, String)> {
    let mut symbols: Vec<&String> = defined.iter().collect();
    symbols.sort();

    let mut taken: HashSet<String> = defined.iter().chain(undefined.iter()).cloned().collect();
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
    let mut undefined_symbols = HashSet::new();

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
        obj_files.push((obj_path, classify_member(data)));

        // Parse object file to find defined symbols, plus undefined references
        // that rename targets must not collide with.
        collect_defined_globals(data, &mut defined_symbols);
        collect_undefined_globals(data, &mut undefined_symbols);
    }

    eprintln!("Extracted {} objects.", obj_files.len());
    eprintln!("Found {} defined symbols.", defined_symbols.len());

    // Step 2: Generate renames (collision-aware; see build_renames).
    let rename_pairs = build_renames(&defined_symbols, &undefined_symbols, keep_prefix);
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

    // Pick the librarian up front: it decides whether import libraries can be
    // regenerated, which gates how import members are handled below.
    let has_imports = obj_files.iter().any(|(_, kind)| kind.is_import_member());
    let lib_cmd = get_windows_lib_tool(Some(target_arch), has_imports);

    // Step 3: Run llvm-objcopy on EACH object
    let objcopy = find_objcopy_tool();
    eprintln!("Using objcopy: {}", objcopy.display());
    eprintln!("Renaming symbols in objects...");

    let mut patched_files = Vec::new();
    // Short-import members are decoded here; their import libraries are
    // regenerated below (see regenerate_import_libs). Descriptor members are
    // dropped, the regenerated .libs carry fresh ones.
    let mut import_entries: Vec<ImportEntry> = Vec::new();

    for (i, (obj_path, kind)) in obj_files.iter().enumerate() {
        match kind {
            // Non-COFF members (LLVM bitcode): nothing to rename, and archiving
            // them can crash lib.exe. See MemberKind.
            MemberKind::Other => {
                eprintln!("Skipping non-COFF object {} (not archived).", i);
                continue;
            }
            MemberKind::ShortImport => {
                let data = fs::read(obj_path).expect("Failed to re-read import member");
                match decode_short_import(&data) {
                    Some(entry) => import_entries.push(entry),
                    None => eprintln!("Warning: could not decode short-import member {}.", i),
                }
                continue;
            }
            // Dropped; regenerated from the short-import members below.
            MemberKind::ImportDescriptor => continue,
            MemberKind::Coff => {}
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

    // Step 4: Regenerate import libraries. A librarian that lacks `/def:`
    // (llvm-ar) cannot rebuild them, so we would have to drop every decoded
    // import. That silently strips the archive's DLL imports and leaves
    // consumers with unresolved externals, and verification won't catch it (it
    // only checks defined API symbols). Fail loudly instead and require
    // llvm-lib or lib.exe.
    let mut import_libs = Vec::new();
    if !import_entries.is_empty() {
        if lib_cmd.handles_import_members {
            eprintln!(
                "Regenerating import libraries from {} imports...",
                import_entries.len()
            );
            import_libs = regenerate_import_libs(&import_entries, &temp_dir, &lib_cmd);
        } else {
            panic!(
                "{} cannot regenerate the {} DLL import(s) in this archive \
                 (needs llvm-lib or lib.exe). Dropping them would leave consumers \
                 with unresolved externals, so refusing to emit an incomplete \
                 library. Install LLVM tools (llvm-lib) or the MSVC Build Tools \
                 (lib.exe).",
                lib_cmd.tool,
                import_entries.len(),
            );
        }
    }

    // Step 5: Repackage
    eprintln!("Creating final library...");

    let final_lib_abs = if final_lib.is_absolute() {
        final_lib.to_path_buf()
    } else {
        env::current_dir()
            .expect("Failed to get current directory")
            .join(final_lib)
    };

    let mut cmd = Command::new(&lib_cmd.tool);
    // Run from the temp dir and pass bare filenames so the archive stores
    // relative member names. Absolute paths would embed the build location (and
    // PID-based temp dir), making the library non-reproducible. The import .libs
    // are passed as inputs so the librarian merges their members into the output.
    cmd.current_dir(&temp_dir);

    if lib_cmd.is_llvm {
        // ar-style: never has import libs (handles_import_members is false).
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
        for lib in &import_libs {
            cmd.arg(lib.file_name().expect("import library has no file name"));
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

/// Searches a `rustlib` directory for the bundled rust-objcopy, trying the
/// host's executable name: `rust-objcopy` on Unix, `rust-objcopy.exe` on Windows.
/// The tool runs on the patching host, so the suffix follows the host, not the
/// Windows target being patched.
fn find_rust_objcopy_in_rustlib(rustlib: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(rustlib).ok()?.flatten() {
        let bin = entry.path().join("bin");
        for name in ["rust-objcopy", "rust-objcopy.exe"] {
            let candidate = bin.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
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
        let rustlib = PathBuf::from(sysroot).join("lib").join("rustlib");
        if let Some(path) = find_rust_objcopy_in_rustlib(&rustlib) {
            return path;
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

/// Determines the appropriate library tool for Windows.
///
/// When `need_import_support` is set (the archive has import members), prefers a
/// lib.exe-style librarian that can regenerate import libraries. `llvm-ar` is
/// still probed last so hosts without lib.exe/llvm-lib get a clear diagnostic,
/// but patching then aborts rather than dropping the imports (see the step 4
/// panic in `patch_windows`). See `handles_import_members`.
fn get_windows_lib_tool(target_arch: Option<&str>, need_import_support: bool) -> WindowsLibTool {
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

    // Map architecture to MSVC machine type
    let machine_type = msvc_machine_type(&target_arch_str).map(String::from);

    let msvc_lib = || {
        Command::new("lib.exe").arg("/?").output().is_ok().then(|| {
            eprintln!("Using lib.exe for Windows build");
            WindowsLibTool {
                tool: "lib.exe".to_string(),
                machine_type: machine_type.clone(),
                is_llvm: false,
                handles_import_members: true,
            }
        })
    };

    // llvm-lib uses lib.exe-style flags; try PATH, then bundled VS LLVM locations.
    let llvm_lib = || {
        if Command::new("llvm-lib").arg("/?").output().is_ok() {
            eprintln!("Using llvm-lib for Windows build");
            return Some(WindowsLibTool {
                tool: "llvm-lib".to_string(),
                machine_type: machine_type.clone(),
                is_llvm: false,
                handles_import_members: true,
            });
        }
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
                return Some(WindowsLibTool {
                    tool: path.to_string_lossy().to_string(),
                    machine_type: machine_type.clone(),
                    is_llvm: false, // llvm-lib uses lib.exe flags
                    handles_import_members: true,
                });
            }
        }
        None
    };

    // llvm-ar uses ar-style flags.
    let llvm_ar = || {
        Command::new("llvm-ar")
            .arg("--version")
            .output()
            .is_ok()
            .then(|| {
                eprintln!("Using llvm-ar for Windows build");
                WindowsLibTool {
                    tool: "llvm-ar".to_string(),
                    machine_type: machine_type.clone(),
                    is_llvm: true,
                    handles_import_members: false,
                }
            })
    };

    // With imports, prefer a lib.exe-style librarian that can regenerate them
    // (llvm-lib, then lib.exe); llvm-ar last, since it would drop them. Without
    // imports, lib.exe first.
    let probes: [&dyn Fn() -> Option<WindowsLibTool>; 3] = if need_import_support {
        [&llvm_lib, &msvc_lib, &llvm_ar]
    } else {
        [&msvc_lib, &llvm_lib, &llvm_ar]
    };
    for probe in probes {
        if let Some(tool) = probe() {
            return tool;
        }
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

    /// Object C: carries a COFF weak-external reference to `name`. In `object`
    /// 0.39.x this is global and not `is_undefined()`, but it is still section 0
    /// and must be treated as an external reference, not as a definition to rename.
    fn make_weak_ref_object(name: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: true,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// Builds the rename map exactly as `patch_windows` does: collect the defined
    /// globals across every object, then run the collision-aware map builder.
    fn rename_map(
        objects: &[Vec<u8>],
        keep_prefix: &str,
    ) -> std::collections::HashMap<String, String> {
        let mut defined = HashSet::new();
        let mut undefined = HashSet::new();
        for obj in objects {
            collect_defined_globals(obj, &mut defined);
            collect_undefined_globals(obj, &mut undefined);
        }
        build_renames(&defined, &undefined, keep_prefix)
            .into_iter()
            .collect()
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
    fn weak_external_reference_is_not_collected_as_definition() {
        // COFF weak externals are external references with storage class
        // IMAGE_SYM_CLASS_WEAK_EXTERNAL. `object` reports them as global and not
        // `is_undefined()`, so this pins that we use `is_definition()` instead.
        let weak = make_weak_ref_object("rust_eh_personality");
        let syms = coff_symbols(&weak);
        let (_, sc, sec) = syms
            .iter()
            .find(|(n, _, _)| n == "rust_eh_personality")
            .unwrap();
        assert_eq!(*sc, object::pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL);
        assert_eq!(*sec, 0, "weak external must be section-0 reference");

        let mut defined = HashSet::new();
        collect_defined_globals(&weak, &mut defined);
        assert!(
            !defined.contains("rust_eh_personality"),
            "a COFF weak external reference must not be renamed as a definition"
        );

        let mut undefined = HashSet::new();
        collect_undefined_globals(&weak, &mut undefined);
        assert!(
            undefined.contains("rust_eh_personality"),
            "a COFF weak external reference should still reserve its original name"
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
    fn internal_is_not_renamed_onto_an_undefined_external_reference() {
        // A defined internal `add`, a defined API `myapp_run`, and an undefined
        // external reference to `myapp_add` (a callback/import the consumer is
        // expected to provide). Renaming `add` -> `myapp_add` would satisfy that
        // external reference with the internal definition, so the referenced name
        // must be reserved even though it is not defined in the archive.
        let map = rename_map(
            &[
                make_def_object("myapp_run"),
                make_ref_object("myapp_add"),
                make_def_object("add"),
            ],
            PREFIX,
        );
        let renamed = map.get("add").expect("internal `add` must be renamed");
        assert_ne!(
            renamed, "myapp_add",
            "must not capture the undefined external reference"
        );
        assert!(renamed.starts_with(PREFIX));
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

    /// Hand-builds a minimal COFF short-import member (the `Foo`/`__imp_Foo`
    /// shape `lib.exe` emits for a DLL import) with a given name type: a
    /// `symbol\0 dll\0` string block, plus an `export\0` string for `EXPORTAS`.
    /// `object::File::parse` rejects these, so they are classified separately.
    fn make_short_import(
        symbol: &str,
        dll: &str,
        ordinal_or_hint: u16,
        name_type: u16,
        import_type: u16,
        export: Option<&str>,
    ) -> Vec<u8> {
        let mut str_data = Vec::new();
        str_data.extend_from_slice(symbol.as_bytes());
        str_data.push(0);
        str_data.extend_from_slice(dll.as_bytes());
        str_data.push(0);
        if let Some(export) = export {
            str_data.extend_from_slice(export.as_bytes());
            str_data.push(0);
        }

        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_le_bytes()); // sig1 = IMAGE_FILE_MACHINE_UNKNOWN
        data.extend_from_slice(&object::pe::IMPORT_OBJECT_HDR_SIG2.to_le_bytes()); // sig2
        data.extend_from_slice(&0u16.to_le_bytes()); // version
        data.extend_from_slice(&object::pe::IMAGE_FILE_MACHINE_AMD64.to_le_bytes()); // machine
        data.extend_from_slice(&0u32.to_le_bytes()); // time_date_stamp
        data.extend_from_slice(&(str_data.len() as u32).to_le_bytes()); // size_of_data
        data.extend_from_slice(&ordinal_or_hint.to_le_bytes());
        data.extend_from_slice(
            &(import_type | (name_type << object::pe::IMPORT_OBJECT_NAME_SHIFT)).to_le_bytes(),
        );
        data.extend_from_slice(&str_data);
        data
    }

    /// A plain by-name short-import member (the common case).
    fn make_short_import_member(symbol: &str, dll: &str) -> Vec<u8> {
        make_short_import(
            symbol,
            dll,
            0,
            object::pe::IMPORT_OBJECT_NAME,
            object::pe::IMPORT_OBJECT_CODE,
            None,
        )
    }

    #[test]
    fn regular_coff_objects_are_patched() {
        // Real COFF objects are patchable and kept.
        assert_eq!(
            classify_member(&make_def_object(RING_SYM)),
            MemberKind::Coff
        );
        assert_eq!(
            classify_member(&make_ref_object(RING_SYM)),
            MemberKind::Coff
        );
        // Non-object members are dropped so they are never fed to lib.exe:
        // LLVM bitcode (BC\xC0\xDE magic) and arbitrary garbage.
        assert_eq!(
            classify_member(&[0x42, 0x43, 0xc0, 0xde, 0, 0, 0, 0]),
            MemberKind::Other
        );
        assert_eq!(classify_member(b"not an object file"), MemberKind::Other);
    }

    #[test]
    fn short_import_members_are_classified() {
        // A short-import member is not a regular COFF object; it is classified so
        // the repackager decodes and regenerates it instead of renaming it.
        let member = make_short_import_member("CreateFileW", "kernel32.dll");
        assert!(
            File::parse(&*member).is_err(),
            "short-import members are not regular COFF objects"
        );
        assert_eq!(classify_member(&member), MemberKind::ShortImport);
    }

    #[test]
    fn short_import_member_decodes_to_dll_and_symbol() {
        // The regeneration path decodes each short-import member back into its
        // (dll, symbol); a plain by-name import binds under the same name.
        let member = make_short_import_member("ProcessPrng", "bcryptprimitives.dll");
        let entry = decode_short_import(&member).expect("must decode a short-import member");
        assert_eq!(entry.symbol, "ProcessPrng");
        assert_eq!(entry.dll, "bcryptprimitives.dll");
        assert!(
            matches!(&entry.import, Import::Name(n) if n == "ProcessPrng"),
            "a plain import binds by its own name"
        );

        // A regular COFF object is not a short-import member.
        assert!(decode_short_import(&make_def_object(RING_SYM)).is_none());
    }

    #[test]
    fn short_import_member_decodes_ordinal_and_exportas() {
        // An ordinal import carries no name; the ordinal drives the binding.
        let ord = make_short_import(
            "Foo",
            "some.dll",
            7,
            object::pe::IMPORT_OBJECT_ORDINAL,
            object::pe::IMPORT_OBJECT_CODE,
            None,
        );
        let entry = decode_short_import(&ord).expect("decode ordinal import");
        assert!(matches!(entry.import, Import::Ordinal(7)));

        // An EXPORTAS import binds under a DLL-side name distinct from the symbol.
        let exportas = make_short_import(
            "LocalName",
            "some.dll",
            0,
            object::pe::IMPORT_OBJECT_NAME_EXPORTAS,
            object::pe::IMPORT_OBJECT_CODE,
            Some("RealExport"),
        );
        let entry = decode_short_import(&exportas).expect("decode EXPORTAS import");
        assert_eq!(entry.symbol, "LocalName");
        assert!(matches!(&entry.import, Import::Name(n) if n == "RealExport"));
    }

    #[test]
    fn short_import_member_decodes_data_type() {
        // A code import is a function; a data import (variable/constant) must be
        // flagged so its .def entry carries the DATA attribute.
        let code = make_short_import_member("SomeFn", "some.dll");
        assert!(!decode_short_import(&code).unwrap().is_data);

        let data = make_short_import(
            "SomeVar",
            "some.dll",
            0,
            object::pe::IMPORT_OBJECT_NAME,
            object::pe::IMPORT_OBJECT_DATA,
            None,
        );
        assert!(decode_short_import(&data).unwrap().is_data);
    }

    /// Builds a COFF object whose only section is an import-directory section
    /// (`.idata$2`) defining `symbol`, mirroring the `__IMPORT_DESCRIPTOR_<dll>`
    /// head/tail members rustc emits alongside the short-import members.
    fn make_import_descriptor_object(symbol: &str) -> Vec<u8> {
        let mut obj = Object::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
        let sec = obj.add_section(Vec::new(), b".idata$2".to_vec(), object::SectionKind::Data);
        let off = obj.append_section_data(sec, &[0u8; 20], 4);
        obj.add_symbol(Symbol {
            name: symbol.as_bytes().to_vec(),
            value: off,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(sec),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    #[test]
    fn import_descriptor_members_are_classified_and_not_renamed() {
        // An all-`.idata` COFF object is the import library's descriptor head/tail.
        // It parses as regular COFF but is classified as an import member (dropped
        // and regenerated), and its COMDAT symbols must never be renamed.
        let member = make_import_descriptor_object("__IMPORT_DESCRIPTOR_foo");
        assert!(
            File::parse(&*member).is_ok(),
            "descriptor members are regular COFF objects"
        );
        assert_eq!(classify_member(&member), MemberKind::ImportDescriptor);
        assert!(MemberKind::ImportDescriptor.is_import_member());
        assert!(MemberKind::ShortImport.is_import_member());
        assert!(!MemberKind::Coff.is_import_member());
        assert!(!MemberKind::Other.is_import_member());

        // A descriptor's defined global must never enter the rename map, or its
        // renamed COMDAT would no longer fold with the real import library.
        let map = rename_map(&[member], PREFIX);
        assert!(
            map.is_empty(),
            "import-descriptor symbols must not be renamed"
        );
    }

    #[test]
    fn import_machinery_symbols_are_not_renamed() {
        // The COMDAT head/tail symbols an import library shares across every copy
        // for a DLL must keep their names. Bare and 32-bit underscore-decorated.
        for s in [
            "__IMPORT_DESCRIPTOR_bcryptprimitives",
            "___IMPORT_DESCRIPTOR_bcryptprimitives",
            "__NULL_IMPORT_DESCRIPTOR",
            "___NULL_IMPORT_DESCRIPTOR",
            "bcryptprimitives_NULL_THUNK_DATA",
        ] {
            assert_eq!(rename_target(s, PREFIX), None, "{s} must not be renamed");
        }
    }

    #[test]
    fn coff_linker_symbols_are_not_renamed() {
        // @-prefixed COFF symbols (e.g. @feat.00) are consumed specially by the
        // linker; renaming them drops the feature metadata they carry.
        assert_eq!(rename_target("@feat.00", PREFIX), None);
        assert_eq!(rename_target("@comp.id", PREFIX), None);
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

    #[test]
    fn finds_bundled_rust_objcopy_without_exe_suffix() {
        // The Rust llvm-tools component installs the tool as `rust-objcopy` (no
        // .exe) on Unix hosts. The sysroot search must find that spelling, not
        // only the Windows `.exe` one, or cross-patching a Windows archive from
        // Linux/macOS fails even though the documented tool is present.
        let base = env::temp_dir().join(format!("lib-patcher-rustlib-{}", std::process::id()));
        let bin = base.join("x86_64-unknown-linux-gnu").join("bin");
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("rust-objcopy");
        fs::write(&tool, b"").unwrap();

        let found = find_rust_objcopy_in_rustlib(&base);
        fs::remove_dir_all(&base).ok();

        assert_eq!(found.as_deref(), Some(tool.as_path()));
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
            if classify_member(data) == MemberKind::Coff {
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
