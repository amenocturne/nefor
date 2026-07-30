use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use nefor_mag::error::MagError;
use serde::Serialize;
use serde_json::Value;

const ENVELOPE_VERSION: u8 = 1;

#[derive(Parser)]
#[command(
    name = "mag",
    version,
    about = "Compile MAG programs without starting a Nefor runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile and validate a MAG program; never executes the resulting graph
    Compile(CompileArgs),
}

#[derive(Args)]
struct CompileArgs {
    /// Entry file, resolved beneath --source-dir
    entry: String,

    /// Directory containing the entry file and files read by the program
    #[arg(long)]
    source_dir: PathBuf,

    /// MAG module search root; repeat for multiple roots (defaults to --source-dir)
    #[arg(long = "module-root")]
    module_roots: Vec<PathBuf>,

    /// Registry definition (.lua) or JSON contract snapshot; repeat to combine contracts
    #[arg(long = "registry")]
    registries: Vec<PathBuf>,
}

#[derive(Serialize)]
struct Envelope<T> {
    version: u8,
    ok: bool,
    #[serde(flatten)]
    payload: T,
}

#[derive(Serialize)]
struct Success {
    artifact: nefor_mag::ast::Artifact,
    hash: String,
}

#[derive(Serialize)]
struct Failure {
    error: Diagnostic,
}

#[derive(Serialize)]
struct Diagnostic {
    code: &'static str,
    stage: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Compile(args) => match compile(args) {
            Ok(success) => print_json(&Envelope {
                version: ENVELOPE_VERSION,
                ok: true,
                payload: success,
            }),
            Err(error) => {
                eprintln!("mag: {}", error.message);
                print_json(&Envelope {
                    version: ENVELOPE_VERSION,
                    ok: false,
                    payload: Failure { error },
                });
                std::process::exit(1);
            }
        },
    }
}

fn compile(args: CompileArgs) -> Result<Success, Diagnostic> {
    require_directory(&args.source_dir, "source_dir")?;
    let module_roots = if args.module_roots.is_empty() {
        vec![args.source_dir.clone()]
    } else {
        args.module_roots
    };
    for root in &module_roots {
        require_directory(root, "module_root")?;
    }
    let contracts = load_registries(&args.registries)?;
    let inputs = serde_json::json!({ "foreign_contracts": contracts });
    let loaded = nefor_mag::load_with_inputs_and_module_roots(
        &args.source_dir,
        &args.entry,
        inputs,
        &module_roots,
    )
    .map_err(mag_diagnostic)?;
    nefor_mag::validate_loaded_rules(&loaded).map_err(mag_diagnostic)?;
    Ok(Success {
        artifact: loaded.artifact,
        hash: loaded.hash,
    })
}

fn require_directory(path: &Path, kind: &'static str) -> Result<(), Diagnostic> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(path_diagnostic(
            "path_not_directory",
            path,
            format!("{kind} is not a directory: {}", path.display()),
        )),
        Err(error) => Err(path_diagnostic(
            "path_unavailable",
            path,
            format!("cannot access {kind} {}: {error}", path.display()),
        )),
    }
}

fn load_registries(paths: &[PathBuf]) -> Result<Vec<Value>, Diagnostic> {
    let mut contracts = Vec::new();
    for path in paths {
        if path.extension().and_then(|extension| extension.to_str()) == Some("lua") {
            let value = nefor_mag::registry::load_registry_contracts(path).map_err(|error| {
                path_diagnostic(
                    "registry_load",
                    path,
                    format!("cannot load registry {}: {error}", path.display()),
                )
            })?;
            let entries = value.as_array().cloned().ok_or_else(|| {
                path_diagnostic(
                    "registry_shape",
                    path,
                    format!(
                        "registry {} exported a non-array contract snapshot",
                        path.display()
                    ),
                )
            })?;
            contracts.extend(entries);
            continue;
        }
        let source = std::fs::read_to_string(path).map_err(|error| {
            path_diagnostic(
                "registry_read",
                path,
                format!("cannot read registry {}: {error}", path.display()),
            )
        })?;
        let value: Value = serde_json::from_str(&source).map_err(|error| {
            path_diagnostic(
                "registry_json",
                path,
                format!("invalid registry JSON {}: {error}", path.display()),
            )
        })?;
        let entries = match value {
            Value::Array(entries) => entries,
            Value::Object(mut object) => object
                .remove("foreign_contracts")
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(|| {
                    path_diagnostic(
                        "registry_shape",
                        path,
                        format!(
                            "registry {} must be an array or an object with foreign_contracts",
                            path.display()
                        ),
                    )
                })?,
            _ => {
                return Err(path_diagnostic(
                    "registry_shape",
                    path,
                    format!(
                        "registry {} must be an array or an object with foreign_contracts",
                        path.display()
                    ),
                ))
            }
        };
        contracts.extend(entries);
    }
    Ok(contracts)
}

fn mag_diagnostic(error: MagError) -> Diagnostic {
    let (code, stage) = match error {
        MagError::Lex(_) => ("syntax_lex", "lex"),
        MagError::Parse(_) => ("syntax_parse", "parse"),
        MagError::Type(_) | MagError::Unresolved(_) | MagError::Arity { .. } => {
            ("type_error", "typecheck")
        }
        MagError::Budget(_) => ("evaluation_budget", "evaluate"),
        MagError::Eval(_) => ("evaluation_error", "evaluate"),
    };
    Diagnostic {
        code,
        stage,
        message: error.to_string(),
        path: None,
    }
}

fn path_diagnostic(code: &'static str, path: &Path, message: String) -> Diagnostic {
    Diagnostic {
        code,
        stage: "input",
        message,
        path: Some(path.display().to_string()),
    }
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("mag: cannot serialize JSON response: {error}");
            std::process::exit(2);
        }
    }
}
