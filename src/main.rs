use clap::Parser;
use staticlib_hygiene::patch_lib;
use std::path::PathBuf;

/// Symbol filtering tool for cross-platform static libraries
///
/// This tool filters static library symbols to prevent linking conflicts when
/// linking multiple Rust static libraries together. It keeps only symbols
/// matching a specified prefix and hides all internal symbols.
#[derive(Parser, Debug)]
#[command(name = "staticlib-hygiene")]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the input static library (e.g., libmylib.a)
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,

    /// Path to the output patched library (e.g., libmylib_patched.a)
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,

    /// Symbol prefix to keep (e.g., "mylib_")
    /// All symbols starting with this prefix will remain public/global.
    /// All other symbols will be hidden/local.
    #[arg(short = 'p', long, value_name = "PREFIX")]
    prefix: String,

    /// Base name for temporary files (e.g., "mylib")
    #[arg(short = 'n', long, value_name = "NAME", default_value = "lib")]
    name: String,

    /// Directory for temporary files (defaults to system temp dir)
    #[arg(short = 't', long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    // Validate input file exists
    if !args.input.exists() {
        eprintln!("Error: Input file does not exist: {}", args.input.display());
        std::process::exit(1);
    }

    // Validate prefix is not empty
    if args.prefix.is_empty() {
        eprintln!("Error: Prefix cannot be empty");
        std::process::exit(1);
    }

    // Use provided temp dir or create one in system temp
    let temp_dir = args.temp_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("staticlib-hygiene-{}", std::process::id()))
    });

    // Create temp dir if it doesn't exist
    if !temp_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            eprintln!("Error: Failed to create temp directory: {}", e);
            std::process::exit(1);
        }
    }

    println!("Patching static library:");
    println!("  Input:  {}", args.input.display());
    println!("  Output: {}", args.output.display());
    println!("  Prefix: {}", args.prefix);
    println!("  Temp:   {}", temp_dir.display());

    // Call the patch_lib function
    patch_lib(
        &args.input,
        &temp_dir,
        &args.name,
        &args.prefix,
        &args.output,
    );

    println!("✓ Successfully patched library!");
    println!(
        "  Only symbols starting with '{}' are now public.",
        args.prefix
    );
}
