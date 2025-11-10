use clap::{Parser, Subcommand};
use libcut::{FilterMode, patch_lib};
use std::path::PathBuf;

/// Symbol filtering tool for cross-platform static libraries
///
/// This tool filters static library symbols to prevent linking conflicts when
/// linking multiple Rust static libraries together.
#[derive(Parser, Debug)]
#[command(name = "libcut")]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Allowlist mode: Keep ONLY symbols matching a prefix (for libraries you control)
    ///
    /// All symbols starting with the specified prefix remain public/global.
    /// All other symbols (including Rust stdlib symbols) are hidden/local.
    ///
    /// Requirement: ALL public functions must start with your prefix.
    Allowlist {
        /// Path to the input static library (e.g., libmylib.a)
        #[arg(short, long, value_name = "FILE")]
        input: PathBuf,

        /// Path to the output patched library (e.g., libmylib_patched.a)
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,

        /// Symbol prefix to keep (e.g., "mylib_")
        /// All symbols starting with this prefix will remain public.
        #[arg(short, long, value_name = "PREFIX")]
        prefix: String,

        /// Base name for temporary files (e.g., "mylib")
        #[arg(short, long, value_name = "NAME", default_value = "lib")]
        name: String,

        /// Directory for temporary files (defaults to system temp dir)
        #[arg(short, long, value_name = "DIR")]
        temp_dir: Option<PathBuf>,

        /// Target architecture (e.g., "aarch64", "x86_64", "arm64")
        /// If not specified, uses the current host architecture.
        /// On Linux, this enables cross-architecture patching (requires cross-compilation tools).
        #[arg(short = 'a', long, value_name = "ARCH")]
        arch: Option<String>,
    },

    /// Blocklist mode: Hide specific symbols (for third-party libraries)
    ///
    /// Only the specified symbols are hidden/local.
    /// All other symbols remain public/global.
    ///
    /// If no symbols are specified, uses a default blocklist of common Rust stdlib symbols:
    /// - rust_eh_personality
    /// - __rust_alloc, __rust_dealloc, __rust_realloc
    /// - __rust_alloc_zeroed, __rust_alloc_error_handler
    /// - __rust_no_alloc_shim_is_unstable
    ///
    /// Use this when you can't control function names.
    Blocklist {
        /// Path to the input static library (e.g., libvendor.a)
        #[arg(short, long, value_name = "FILE")]
        input: PathBuf,

        /// Path to the output patched library (e.g., libvendor_patched.a)
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,

        /// Symbols to hide (comma-separated)
        /// If not specified, uses default blocklist of common Rust stdlib symbols.
        /// Example: "rust_eh_personality,my_symbol,__rust_alloc"
        #[arg(short = 's', long, value_name = "SYMBOLS", value_delimiter = ',')]
        symbols: Option<Vec<String>>,

        /// Base name for temporary files (e.g., "vendor")
        #[arg(short, long, value_name = "NAME", default_value = "lib")]
        name: String,

        /// Directory for temporary files (defaults to system temp dir)
        #[arg(short, long, value_name = "DIR")]
        temp_dir: Option<PathBuf>,

        /// Target architecture (e.g., "aarch64", "x86_64", "arm64")
        /// If not specified, uses the current host architecture.
        /// On Linux, this enables cross-architecture patching (requires cross-compilation tools).
        #[arg(short = 'a', long, value_name = "ARCH")]
        arch: Option<String>,
    },
}

fn main() {
    let args = Args::parse();

    match args.command {
        Commands::Allowlist {
            input,
            output,
            prefix,
            name,
            temp_dir,
            arch,
        } => {
            // Validate input file exists
            if !input.exists() {
                eprintln!("Error: Input file does not exist: {}", input.display());
                std::process::exit(1);
            }

            // Validate prefix is not empty
            if prefix.is_empty() {
                eprintln!("Error: Prefix cannot be empty");
                std::process::exit(1);
            }

            let mode = FilterMode::Allowlist {
                prefix: prefix.clone(),
            };
            let temp_dir = get_temp_dir(temp_dir);

            println!("Patching static library (ALLOWLIST MODE):");
            println!("  Input:  {}", input.display());
            println!("  Output: {}", output.display());
            println!(
                "  Prefix: {} (only symbols starting with this will be kept)",
                prefix
            );
            if let Some(ref arch_val) = arch {
                println!("  Arch:   {} (cross-compilation mode)", arch_val);
            }
            println!("  Temp:   {}", temp_dir.display());

            patch_lib(&input, &temp_dir, &name, mode, &output, arch.as_deref());

            println!("✓ Successfully patched library!");
            println!("  Only symbols starting with '{}' are now public.", prefix);
        }

        Commands::Blocklist {
            input,
            output,
            symbols,
            name,
            temp_dir,
            arch,
        } => {
            // Validate input file exists
            if !input.exists() {
                eprintln!("Error: Input file does not exist: {}", input.display());
                std::process::exit(1);
            }

            // Use provided symbols or default blocklist
            let is_default = symbols.is_none();
            let mode = if let Some(symbols) = symbols {
                if symbols.is_empty() {
                    eprintln!("Error: Symbols list cannot be empty");
                    std::process::exit(1);
                }
                FilterMode::Blocklist {
                    remove: symbols.clone(),
                }
            } else {
                FilterMode::default_blocklist()
            };

            let temp_dir = get_temp_dir(temp_dir);

            // Extract the list for display
            let symbols_list = if let FilterMode::Blocklist { remove } = &mode {
                remove.clone()
            } else {
                vec![]
            };

            println!("Patching static library (BLOCKLIST MODE):");
            println!("  Input:  {}", input.display());
            println!("  Output: {}", output.display());

            if is_default {
                println!(
                    "  Hiding: {} symbols (default Rust stdlib symbols)",
                    symbols_list.len()
                );
                println!("          rust_eh_personality, __rust_alloc, __rust_dealloc, ...");
            } else {
                println!("  Hiding: {} symbols", symbols_list.len());
                if symbols_list.len() <= 5 {
                    println!("          {}", symbols_list.join(", "));
                }
            }
            if let Some(ref arch_val) = arch {
                println!("  Arch:   {} (cross-compilation mode)", arch_val);
            }
            println!("  Temp:   {}", temp_dir.display());

            patch_lib(&input, &temp_dir, &name, mode, &output, arch.as_deref());

            println!("✓ Successfully patched library!");
            println!("  {} symbols are now hidden.", symbols_list.len());
        }
    }
}

fn get_temp_dir(temp_dir: Option<PathBuf>) -> PathBuf {
    let dir = temp_dir
        .unwrap_or_else(|| std::env::temp_dir().join(format!("libcut-{}", std::process::id())));

    // Create temp dir if it doesn't exist
    if !dir.exists()
        && let Err(e) = std::fs::create_dir_all(&dir)
    {
        eprintln!("Error: Failed to create temp directory: {}", e);
        std::process::exit(1);
    }

    dir
}
