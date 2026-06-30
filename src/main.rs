use clap::Parser;
use lib_patcher::{
    default_symbol_blocklist, filter_symbols_by_prefix, list_symbols, patch_lib,
    patch_lib_blocklist,
};
use std::path::PathBuf;

/// Symbol filtering tool for cross-platform static libraries
///
/// Hides all symbols except those with a specified prefix to prevent linking conflicts
/// when linking multiple Rust static libraries together.
///
/// Two modes are supported:
/// - Allowlist (preferred): `--keep-prefix PREFIX` keeps only matching symbols and
///   hides everything else.
/// - Blocklist (deprecated): `--default` / `--symbols` / `--filter-prefix` hide a
///   specific set of symbols and leave everything else public. Kept for backward
///   compatibility; prefer `--keep-prefix`.
#[derive(Parser, Debug)]
#[command(name = "lib-patcher")]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the input static library (e.g., libmylib.a)
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,

    /// Path to the output patched library (e.g., libmylib_patched.a)
    /// Not required when using --list
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Keep only symbols with this prefix, hide everything else
    /// Example: --keep-prefix "mylib_" will keep mylib_add, mylib_multiply public
    /// and hide all other symbols including Rust stdlib and dependencies.
    ///
    /// Mutually exclusive with the deprecated blocklist flags
    /// (--default / --symbols / --filter-prefix).
    #[arg(
        short = 'k',
        long,
        value_name = "PREFIX",
        conflicts_with_all = ["symbols", "filter_prefix", "default"]
    )]
    keep_prefix: Option<String>,

    /// [DEPRECATED: use --keep-prefix] Symbols to hide (comma-separated).
    /// Blocklist mode: the listed symbols are hidden, everything else stays public.
    #[arg(short = 's', long, value_name = "SYMBOLS", value_delimiter = ',')]
    symbols: Option<Vec<String>>,

    /// [DEPRECATED: use --keep-prefix] Hide symbols by prefix (comma-separated).
    /// Blocklist mode: all symbols starting with any given prefix are hidden.
    #[arg(short = 'f', long, value_name = "PREFIXES", value_delimiter = ',')]
    filter_prefix: Option<Vec<String>>,

    /// [DEPRECATED: use --keep-prefix] Include the default stdlib blocklist.
    /// Blocklist mode: hides common Rust stdlib symbols (rust_eh_personality,
    /// __rust_alloc, ...) in addition to any --symbols / --filter-prefix.
    #[arg(short = 'd', long)]
    default: bool,

    /// Base name for temporary files (e.g., "mylib")
    #[arg(short, long, value_name = "NAME", default_value = "lib")]
    name: String,

    /// Full Rust target triplet (e.g., "x86_64-pc-windows-gnullvm", "aarch64-apple-ios").
    /// Selects the platform code path when cross-compiling and is required for correct
    /// Apple platform selection.
    #[arg(short = 'T', long, value_name = "TRIPLET")]
    triplet: Option<String>,

    /// Directory for temporary files (defaults to system temp dir)
    #[arg(short, long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,

    /// List all public/global symbols in the input library instead of patching
    #[arg(short = 'l', long)]
    list: bool,
}

fn main() {
    let args = Args::parse();

    // Validate input file exists
    if !args.input.exists() {
        eprintln!("Error: Input file does not exist: {}", args.input.display());
        std::process::exit(1);
    }

    // List symbols mode
    if args.list {
        println!("Listing public symbols in: {}", args.input.display());
        println!();

        match list_symbols(&args.input) {
            Ok(symbols) => {
                if symbols.is_empty() {
                    println!("No public symbols found.");
                } else {
                    println!("Found {} public symbols:\n", symbols.len());
                    for sym in symbols {
                        println!("{}", sym);
                    }
                }
            }
            Err(e) => {
                eprintln!("Error: Failed to list symbols: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    // Patching mode - output is required
    let output = match args.output {
        Some(path) => path,
        None => {
            eprintln!("Error: --output is required when patching");
            std::process::exit(1);
        }
    };

    let temp_dir = get_temp_dir(args.temp_dir);

    // Mode dispatch: --keep-prefix selects the allowlist path; its absence falls
    // back to the deprecated blocklist path (including the implicit default
    // blocklist when no symbol flags are given), preserving the old CLI's
    // behavior for callers that have not migrated yet.
    match args.keep_prefix {
        Some(keep_prefix) => patch_allowlist(
            &args.input,
            &temp_dir,
            &args.name,
            &keep_prefix,
            &output,
            args.triplet.as_deref(),
        ),
        None => patch_blocklist(
            &args.input,
            &temp_dir,
            &args.name,
            args.symbols,
            args.filter_prefix,
            args.default,
            &output,
            args.triplet.as_deref(),
        ),
    }
}

/// Allowlist mode: keep only symbols matching `keep_prefix`, hide everything else.
fn patch_allowlist(
    input: &std::path::Path,
    temp_dir: &std::path::Path,
    name: &str,
    keep_prefix: &str,
    output: &std::path::Path,
    triplet: Option<&str>,
) {
    println!("Patching static library:");
    println!("  Input:   {}", input.display());
    println!("  Output:  {}", output.display());
    println!("  Keeping: Symbols starting with '{}'", keep_prefix);
    println!("  Hiding:  Everything else (Rust stdlib, dependencies, internal symbols)");
    println!("  Temp:    {}", temp_dir.display());

    patch_lib(
        input,
        temp_dir,
        name,
        keep_prefix,
        output,
        None, // Architecture is auto-detected
        triplet,
    );

    println!("✓ Successfully patched library!");
    println!("  All symbols except '{}*' are now hidden.", keep_prefix);
}

/// Deprecated blocklist mode: hide a specific set of symbols, leave the rest public.
///
/// This reproduces the pre-allowlist CLI behavior for callers that have not yet
/// migrated to `--keep-prefix`. It is intentionally kept bug-for-bug compatible
/// with the old tool (e.g. it misses v0-mangled allocator shims); prefer the
/// allowlist mode.
#[allow(clippy::too_many_arguments)]
fn patch_blocklist(
    input: &std::path::Path,
    temp_dir: &std::path::Path,
    name: &str,
    symbols_arg: Option<Vec<String>>,
    filter_prefix_arg: Option<Vec<String>>,
    use_default: bool,
    output: &std::path::Path,
    triplet: Option<&str>,
) {
    eprintln!(
        "warning: blocklist mode (--default/--symbols/--filter-prefix, or no symbol\n\
         \x20        flags at all) is deprecated; prefer --keep-prefix <PREFIX>, which keeps\n\
         \x20        only your public API and reliably hides everything else."
    );

    // Build symbols list from various sources (matches the old CLI exactly).
    let mut symbols = Vec::new();
    let mut has_default = false;
    let mut has_custom = false;
    let mut has_prefix = false;

    // Add default symbols if requested or if nothing else is specified
    if use_default || (symbols_arg.is_none() && filter_prefix_arg.is_none()) {
        symbols.extend(default_symbol_blocklist());
        has_default = true;
    }

    // Add custom symbols if provided
    if let Some(custom) = symbols_arg {
        if custom.is_empty() {
            eprintln!("Error: Symbols list cannot be empty");
            std::process::exit(1);
        }
        symbols.extend(custom);
        has_custom = true;
    }

    // Add symbols filtered by prefix
    if let Some(prefixes) = filter_prefix_arg {
        if prefixes.is_empty() {
            eprintln!("Error: Prefix list cannot be empty");
            std::process::exit(1);
        }

        match filter_symbols_by_prefix(input, &prefixes) {
            Ok(filtered) => {
                println!(
                    "  Found {} symbols matching prefixes: {}",
                    filtered.len(),
                    prefixes.join(", ")
                );
                symbols.extend(filtered);
                has_prefix = true;
            }
            Err(e) => {
                eprintln!("Error: Failed to filter symbols by prefix: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Ensure we have at least some symbols to hide
    if symbols.is_empty() {
        eprintln!(
            "Error: No symbols to hide. Specify --symbols, --filter-prefix, or use --default"
        );
        std::process::exit(1);
    }

    // Remove duplicates
    symbols.sort();
    symbols.dedup();

    println!("Patching static library:");
    println!("  Input:  {}", input.display());
    println!("  Output: {}", output.display());

    // Build description of what symbols are being hidden
    let mut sources = Vec::new();
    if has_default {
        sources.push("default");
    }
    if has_custom {
        sources.push("custom");
    }
    if has_prefix {
        sources.push("prefix-filtered");
    }

    println!(
        "  Hiding: {} symbols ({})",
        symbols.len(),
        sources.join(" + ")
    );

    if has_default && !has_custom && !has_prefix {
        println!("          rust_eh_personality, __rust_alloc, __rust_dealloc, ...");
    } else if symbols.len() <= 5 {
        println!("          {}", symbols.join(", "));
    }

    println!("  Temp:   {}", temp_dir.display());

    patch_lib_blocklist(
        input, temp_dir, name, &symbols, output,
        None, // Architecture is auto-detected from the library
        triplet,
    );

    println!("✓ Successfully patched library!");
    println!("  {} symbols are now hidden.", symbols.len());
}

fn get_temp_dir(temp_dir: Option<PathBuf>) -> PathBuf {
    let dir = temp_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("lib-patcher-{}", std::process::id()))
    });

    // Create temp dir if it doesn't exist
    if !dir.exists()
        && let Err(e) = std::fs::create_dir_all(&dir)
    {
        eprintln!("Error: Failed to create temp directory: {}", e);
        std::process::exit(1);
    }

    dir
}
