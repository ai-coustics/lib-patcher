use clap::Parser;
use lib_patcher::{default_symbol_blocklist, list_symbols, patch_lib};
use std::path::PathBuf;

/// Symbol filtering tool for cross-platform static libraries
///
/// This tool hides specific symbols in static libraries to prevent linking conflicts
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

    /// Symbols to hide (comma-separated)
    /// If not specified, uses default blocklist of common Rust stdlib symbols:
    /// rust_eh_personality, __rust_alloc, __rust_dealloc, __rust_realloc,
    /// __rust_alloc_zeroed, __rust_alloc_error_handler, __rust_no_alloc_shim_is_unstable
    ///
    /// Example: "rust_eh_personality,my_symbol,__rust_alloc"
    #[arg(short = 's', long, value_name = "SYMBOLS", value_delimiter = ',')]
    symbols: Option<Vec<String>>,

    /// Include default symbol blocklist in addition to custom symbols
    /// When used with --symbols, both the default list and custom symbols will be hidden
    #[arg(short = 'd', long)]
    default: bool,

    /// Base name for temporary files (e.g., "mylib")
    #[arg(short, long, value_name = "NAME", default_value = "lib")]
    name: String,

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

    // Store flags before consuming args.symbols
    let is_default = args.symbols.is_none();
    let has_custom = args.symbols.is_some();
    let has_default = args.default || args.symbols.is_none();
    
    // Build symbols list based on --symbols and --default flags
    let symbols = match (args.symbols, args.default) {
        // Only custom symbols provided
        (Some(custom), false) => {
            if custom.is_empty() {
                eprintln!("Error: Symbols list cannot be empty");
                std::process::exit(1);
            }
            custom
        }
        // Both custom symbols and default requested
        (Some(custom), true) => {
            if custom.is_empty() {
                eprintln!("Error: Symbols list cannot be empty");
                std::process::exit(1);
            }
            let mut merged = default_symbol_blocklist();
            merged.extend(custom);
            merged
        }
        // No symbols provided, use default
        (None, _) => default_symbol_blocklist(),
    };

    let temp_dir = get_temp_dir(args.temp_dir);

    println!("Patching static library:");
    println!("  Input:  {}", args.input.display());
    println!("  Output: {}", output.display());

    if is_default {
        println!(
            "  Hiding: {} symbols (default Rust stdlib symbols)",
            symbols.len()
        );
        println!("          rust_eh_personality, __rust_alloc, __rust_dealloc, ...");
    } else if has_custom && has_default {
        println!(
            "  Hiding: {} symbols (default + custom)",
            symbols.len()
        );
        println!("          default + custom symbols combined");
    } else {
        println!("  Hiding: {} symbols (custom)", symbols.len());
        if symbols.len() <= 5 {
            println!("          {}", symbols.join(", "));
        }
    }
    println!("  Temp:   {}", temp_dir.display());

    patch_lib(
        &args.input,
        &temp_dir,
        &args.name,
        &symbols,
        &output,
        None, // Architecture is now auto-detected
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
