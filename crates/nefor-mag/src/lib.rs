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
    let function = match program.env.lookup(name)? {
        Value::Fn(function) if function.param_types.len() == 1 => function,
        Value::Fn(function) => {
            return Err(MagError::Type(format!(
                "function '{name}' must be unary, got {} parameters",
                function.param_types.len()
            )))
        }
        _ => return Err(MagError::Type(format!("'{name}' is not a function"))),
    };
    let input_type = function.param_types[0].clone();
    let schema = schema::TypeSchema::reify(&program.env, &input_type)?;
    let encoded = serde_json::to_string(&input)
        .map_err(|error| MagError::Eval(format!("serialize rule input: {error}")))?;
    let validation = schema.validate_json(&encoded);
    if !validation.ok {
        let detail = validation
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| {
                validation
                    .violations
                    .iter()
                    .map(|violation| format!("{}: {}", violation.path, violation.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            });
        return Err(MagError::Type(format!(
            "rule function '{name}' input does not conform to {input_type}: {detail}"
        )));
    }
    let argument = Value::Typed(
        std::sync::Arc::new(json::json_to_typed_value(
            &program.env,
            &input,
            &input_type,
        )?),
        input_type,
    );
    extract_artifact(
        eval::apply_named(&program.env, name, argument)?,
        &format!("function '{name}'"),
    )
}

/// Validate that a resident rule binding names a concrete unary MAG function
/// whose declared result crosses the host boundary as an Artifact.
pub fn validate_rule_fn(program: &LoadedProgram, name: &str) -> Result<(), MagError> {
    let value = program.env.lookup(name)?;
    let ast::Value::Fn(function) = value else {
        return Err(MagError::Type(format!(
            "rule function '{name}' is not a function"
        )));
    };
    if function.params.len() != 1 {
        return Err(MagError::Type(format!(
            "rule function '{name}' must be unary, got {} parameters",
            function.params.len()
        )));
    }
    if function.return_type != types::MagType::Artifact {
        return Err(MagError::Type(format!(
            "rule function '{name}' must return Artifact, got {}",
            function.return_type
        )));
    }
    Ok(())
}

pub fn validate_rule_fn_input(
    program: &LoadedProgram,
    name: &str,
    expected_input: &serde_json::Value,
) -> Result<(), MagError> {
    validate_rule_fn(program, name)?;
    let ast::Value::Fn(function) = program.env.lookup(name)? else {
        unreachable!("validate_rule_fn accepted a non-function")
    };
    let actual = json::type_evidence_to_json(&function.param_types[0])?;
    if actual != *expected_input {
        return Err(MagError::Type(format!(
            "rule function '{name}' input does not match its structural source type"
        )));
    }
    Ok(())
}

fn extract_artifact(value: Value, source: &str) -> Result<Artifact, MagError> {
    match value {
        Value::Artifact(artifact) => Ok(artifact),
        Value::Typed(inner, types::MagType::Artifact) => {
            extract_artifact(inner.as_ref().clone(), source)
        }
        other => Err(MagError::Eval(format!(
            "{source} must return Artifact, got {}",
            other.type_name()
        ))),
    }
}
