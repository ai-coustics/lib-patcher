use std::path::{Path, PathBuf};
use std::process::Command;

use crate::elf::{patch_elf, ElfToolchain};

/// Linux implementation: uses readelf + objcopy (the shared ELF routine) to
/// localize symbols, resolving native or GNU cross tools for the target arch.
pub(crate) fn patch_linux(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let (ld_cmd, objcopy_cmd, ar_cmd) = get_linux_toolchain(target_arch);

    // readelf reads the ELF symbol table cross-arch, so the native `readelf` is
    // fine even for a cross target; only ld/objcopy/ar need the triplet tools.
    let tools = ElfToolchain {
        ld: PathBuf::from(ld_cmd),
        ld_pre_args: Vec::new(),
        readelf: PathBuf::from("readelf"),
        objcopy: PathBuf::from(objcopy_cmd),
        ar: PathBuf::from(ar_cmd),
        ar_fallback: Some(PathBuf::from("llvm-ar")),
        label: "Linux",
    };

    patch_elf(static_lib, out_dir, lib_name, keep_prefix, final_lib, &tools);
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
