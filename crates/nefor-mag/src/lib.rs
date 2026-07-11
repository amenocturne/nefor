pub mod ast;
mod checker;
pub mod env;
pub mod error;
pub mod eval;
pub mod json;
pub mod lexer;
pub mod parser;
pub mod schema;
pub mod types;

use ast::{Artifact, Value};
use env::Env;
use error::MagError;
use sha2::{Digest, Sha256};
use std::path::Path;

const EVALUATION_STEP_LIMIT: u64 = 100_000;

pub fn compile(source: &str, source_dir: &Path) -> Result<Artifact, MagError> {
    compile_with_inputs(
        source,
        source_dir,
        serde_json::Value::Object(Default::default()),
    )
}

pub fn compile_with_inputs(
    source: &str,
    source_dir: &Path,
    inputs: serde_json::Value,
) -> Result<Artifact, MagError> {
    compile_with_inputs_and_module_roots(source, source_dir, inputs, &[source_dir.to_path_buf()])
}

pub fn compile_with_inputs_and_module_roots(
    source: &str,
    source_dir: &Path,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
) -> Result<Artifact, MagError> {
    let _fuel = eval::fuel::install(EVALUATION_STEP_LIMIT);
    let mut env =
        Env::new_with_stdlib_source_dir_and_module_roots(source_dir, module_roots.to_vec());
    env.define("inputs", json::json_to_value(&inputs));
    let exprs = parser::parse(&lexer::tokenize(source)?)?;
    extract_artifact(eval::eval_program(&mut env, &exprs)?, "top-level program")
}

#[derive(Debug, Clone)]
pub struct LoadedProgram {
    pub env: Env,
    pub artifact: Artifact,
    pub hash: String,
}

pub fn load(source_dir: &Path, entry: &str) -> Result<LoadedProgram, MagError> {
    load_with_inputs(
        source_dir,
        entry,
        serde_json::Value::Object(Default::default()),
    )
}

pub fn load_with_inputs(
    source_dir: &Path,
    entry: &str,
    inputs: serde_json::Value,
) -> Result<LoadedProgram, MagError> {
    load_with_inputs_and_module_roots(source_dir, entry, inputs, &[source_dir.to_path_buf()])
}

pub fn load_with_inputs_and_module_roots(
    source_dir: &Path,
    entry: &str,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
) -> Result<LoadedProgram, MagError> {
    let _fuel = eval::fuel::install(EVALUATION_STEP_LIMIT);
    let path = eval::resolve_workspace_path(source_dir, entry)?;
    let source = std::fs::read_to_string(&path)
        .map_err(|e| MagError::Eval(format!("cannot read program {}: {e}", path.display())))?;
    let mut env =
        Env::new_with_stdlib_source_dir_and_module_roots(source_dir, module_roots.to_vec());
    env.define("inputs", json::json_to_value(&inputs));
    let exprs = parser::parse(&lexer::tokenize(&source)?)?;
    let artifact = extract_artifact(eval::eval_program(&mut env, &exprs)?, "top-level program")?;
    let encoded = serde_json::to_vec(&artifact)
        .map_err(|e| MagError::Eval(format!("serialize artifact: {e}")))?;
    let hash = format!("{:x}", Sha256::digest(encoded));
    Ok(LoadedProgram {
        env,
        artifact,
        hash,
    })
}

pub fn eval_fn(
    program: &LoadedProgram,
    name: &str,
    input: serde_json::Value,
) -> Result<Artifact, MagError> {
    let _fuel = eval::fuel::install(EVALUATION_STEP_LIMIT);
    extract_artifact(
        eval::apply_named(&program.env, name, json::json_to_value(&input))?,
        &format!("function '{name}'"),
    )
}

fn extract_artifact(value: Value, source: &str) -> Result<Artifact, MagError> {
    match value {
        Value::Artifact(artifact) => Ok(artifact),
        Value::Typed(inner, types::MagType::Artifact) => extract_artifact(*inner, source),
        other => Err(MagError::Eval(format!(
            "{source} must return Artifact, got {}",
            other.type_name()
        ))),
    }
}
