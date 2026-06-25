use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mag", about = "MAG — Meta-Algebraic Grammar compiler")]
struct Cli {
    /// Path to .mag source file
    source: PathBuf,

    /// Directory for template and module resolution
    #[arg(short = 's', long, default_value = ".")]
    source_dir: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    let source = match std::fs::read_to_string(&cli.source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", cli.source.display());
            std::process::exit(1);
        }
    };

    match nefor_mag::compile(&source, &cli.source_dir) {
        Ok(ir) => {
            println!("{}", serde_json::to_string_pretty(&ir).unwrap());
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
