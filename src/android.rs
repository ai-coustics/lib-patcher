use std::env;
use std::path::{Path, PathBuf};

use crate::elf::{patch_elf, ElfToolchain};

/// Android implementation: ELF symbol filtering using the Android NDK LLVM tools.
///
/// Android archives are regular `ar` archives containing ELF objects, so the
/// symbol-hiding strategy is identical to Linux and runs through the shared
/// `patch_elf` routine. The only Android-specific parts are toolchain selection
/// (the target-neutral LLVM tools from the NDK instead of host/GNU binutils) and
/// the explicit `-m <emulation>` the NDK `ld.lld` needs for the partial link.
pub(crate) fn patch_android(
    static_lib: &Path,
    out_dir: &Path,
    lib_name: &str,
    keep_prefix: &str,
    final_lib: &Path,
    target_arch: &str,
) {
    let ndk = get_android_toolchain(target_arch);

    let tools = ElfToolchain {
        ld: ndk.ld_lld,
        ld_pre_args: android_lld_emulation_args(target_arch)
            .into_iter()
            .map(str::to_string)
            .collect(),
        readelf: ndk.readelf,
        objcopy: ndk.objcopy,
        ar: ndk.ar,
        ar_fallback: None,
        label: "Android",
    };

    patch_elf(static_lib, out_dir, lib_name, keep_prefix, final_lib, &tools);
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
}
