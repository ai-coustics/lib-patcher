use clap::Parser;
use lib_patcher::{list_symbols, patch_lib};
use std::path::PathBuf;

/// Symbol filtering tool for cross-platform static libraries
///
/// Hides all symbols except those with a specified prefix to prevent linking conflicts
/// when linking multiple Rust static libraries together.
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
    #[arg(
        short = 'k',
        long,
        value_name = "PREFIX",
        required_unless_present = "list"
    )]
    keep_prefix: Option<String>,

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

    // Patching mode - output and prefix are required
    let output = match args.output {
        Some(path) => path,
        None => {
            eprintln!("Error: --output is required when patching");
            std::process::exit(1);
        }
    };

    let keep_prefix = args.keep_prefix.expect("--keep-prefix is required");

    let temp_dir = get_temp_dir(args.temp_dir);

    println!("Patching static library:");
    println!("  Input:   {}", args.input.display());
    println!("  Output:  {}", output.display());
    println!("  Keeping: Symbols starting with '{}'", keep_prefix);
    println!("  Hiding:  Everything else (Rust stdlib, dependencies, internal symbols)");
    println!("  Temp:    {}", temp_dir.display());

    patch_lib(
        &args.input,
        &temp_dir,
        &args.name,
        &keep_prefix,
        &output,
        None, // Architecture is auto-detected
        args.triplet.as_deref(),
    );

    println!("✓ Successfully patched library!");
    println!("  All symbols except '{}*' are now hidden.", keep_prefix);
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
