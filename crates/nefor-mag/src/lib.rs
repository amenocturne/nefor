pub mod ast;
mod checker;
pub mod diagnostic;
pub mod env;
pub mod error;
pub mod eval;
pub mod json;
pub mod lexer;
pub mod parser;
pub mod profile;
pub mod registry;
pub mod schema;
pub mod types;

use ast::{Artifact, Value};
use env::Env;
use error::MagError;
use profile::{CompileProfile, CompileProfiler, Phase};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Instant;

pub(crate) const EVALUATION_STEP_LIMIT: u64 = 500_000;

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
    compile_impl(source, source_dir, inputs, module_roots, None)
}

pub fn compile_profiled(
    source: &str,
    source_dir: &Path,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
) -> Result<(Artifact, CompileProfile), MagError> {
    let profiler = CompileProfiler::new();
    let artifact = compile_impl(source, source_dir, inputs, module_roots, Some(&profiler))?;
    Ok((artifact, profiler.snapshot()))
}

fn compile_impl(
    source: &str,
    source_dir: &Path,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
    profiler: Option<&CompileProfiler>,
) -> Result<Artifact, MagError> {
    let _fuel = eval::fuel::install(EVALUATION_STEP_LIMIT);
    let mut env = Env::new_with_stdlib_source_dir_module_roots_and_profiler(
        source_dir,
        module_roots.to_vec(),
        profiler.cloned(),
    );
    env.define("inputs", Value::HostInputs(inputs));
    let started = phase_started(profiler);
    let source_snapshot = diagnostic::SourceSnapshot::named("<memory>", source);
    let tokens = lexer::tokenize_source(&source_snapshot)?;
    record_phase(profiler, Phase::EntryLex, started);
    let started = phase_started(profiler);
    let exprs = parser::parse_source(&tokens, &source_snapshot)?;
    record_phase(profiler, Phase::EntryParse, started);
    let started = phase_started(profiler);
    let value = eval::eval_program(&mut env, &exprs)?;
    record_phase(profiler, Phase::EntryEvaluate, started);
    let started = phase_started(profiler);
    let artifact = extract_artifact(value, "top-level program")?;
    record_phase(profiler, Phase::ArtifactConversion, started);
    Ok(artifact)
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
    load_impl(source_dir, entry, inputs, module_roots, None)
}

pub fn load_profiled(
    source_dir: &Path,
    entry: &str,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
) -> Result<(LoadedProgram, CompileProfile), MagError> {
    let profiler = CompileProfiler::new();
    let program = load_with_profiler(source_dir, entry, inputs, module_roots, &profiler)?;
    Ok((program, profiler.snapshot()))
}

pub fn load_with_profiler(
    source_dir: &Path,
    entry: &str,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
    profiler: &CompileProfiler,
) -> Result<LoadedProgram, MagError> {
    load_impl(source_dir, entry, inputs, module_roots, Some(profiler))
}

fn load_impl(
    source_dir: &Path,
    entry: &str,
    inputs: serde_json::Value,
    module_roots: &[std::path::PathBuf],
    profiler: Option<&CompileProfiler>,
) -> Result<LoadedProgram, MagError> {
    let _fuel = eval::fuel::install(EVALUATION_STEP_LIMIT);
    let path = eval::resolve_workspace_path(source_dir, entry)?;
    let started = phase_started(profiler);
    let source = std::fs::read_to_string(&path)
        .map_err(|e| MagError::Eval(format!("cannot read program {}: {e}", path.display())))?;
    record_phase(profiler, Phase::EntryRead, started);
    let mut env = Env::new_with_stdlib_source_dir_module_roots_and_profiler(
        source_dir,
        module_roots.to_vec(),
        profiler.cloned(),
    );
    env.define("inputs", Value::HostInputs(inputs));
    let started = phase_started(profiler);
    let source_snapshot = diagnostic::SourceSnapshot::file(&path, &source);
    let tokens = lexer::tokenize_source(&source_snapshot)?;
    record_phase(profiler, Phase::EntryLex, started);
    let started = phase_started(profiler);
    let exprs = parser::parse_source(&tokens, &source_snapshot)?;
    record_phase(profiler, Phase::EntryParse, started);
    let started = phase_started(profiler);
    let value = eval::eval_program(&mut env, &exprs)?;
    record_phase(profiler, Phase::EntryEvaluate, started);
    let started = phase_started(profiler);
    let artifact = extract_artifact(value, "top-level program")?;
    record_phase(profiler, Phase::ArtifactConversion, started);
    let started = phase_started(profiler);
    let encoded = serde_json::to_vec(&artifact)
        .map_err(|e| MagError::Eval(format!("serialize artifact: {e}")))?;
    let hash = format!("{:x}", Sha256::digest(encoded));
    record_phase(profiler, Phase::ArtifactSerializeHash, started);
    Ok(LoadedProgram {
        env,
        artifact,
        hash,
    })
}

fn phase_started(profiler: Option<&CompileProfiler>) -> Option<Instant> {
    profiler.map(|_| Instant::now())
}

fn record_phase(profiler: Option<&CompileProfiler>, phase: Phase, started: Option<Instant>) {
    if let (Some(profiler), Some(started)) = (profiler, started) {
        profiler.add_phase(phase, started.elapsed());
    }
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
    let actual = json::concrete_type_to_json(&types::ConcreteType::resolve(
        &program.env,
        &function.param_types[0],
    )?)?;
    if actual != *expected_input {
        return Err(MagError::Type(format!(
            "rule function '{name}' input does not match its structural source type"
        )));
    }
    Ok(())
}

/// Validate every resident rule referenced by a graph-modification artifact.
/// This is shared by `mag compile` and the runtime's `mag.load` path.
pub fn validate_loaded_rules(program: &LoadedProgram) -> Result<(), MagError> {
    validate_loaded_rules_impl(program, None)
}

pub fn validate_loaded_rules_profiled(
    program: &LoadedProgram,
    profiler: &CompileProfiler,
) -> Result<(), MagError> {
    validate_loaded_rules_impl(program, Some(profiler))
}

fn validate_loaded_rules_impl(
    program: &LoadedProgram,
    profiler: Option<&CompileProfiler>,
) -> Result<(), MagError> {
    let started = phase_started(profiler);
    if program.artifact.format != "nefor.graph-modification/v1" {
        record_phase(profiler, Phase::ResidentRuleValidation, started);
        return Ok(());
    }
    let rules = program
        .artifact
        .data
        .get("rules")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for rule in rules {
        if let Some(profiler) = profiler {
            profiler.update_counters(|counters| {
                counters.resident_rules_validated =
                    counters.resident_rules_validated.saturating_add(1);
            });
        }
        let id = rule
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<malformed>");
        let function = rule
            .get("fn")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| MagError::Type(format!("rule {id:?} missing function name")))?;
        let input = rule
            .get("on")
            .and_then(|on| on.get("type"))
            .ok_or_else(|| MagError::Type(format!("rule {id:?} missing source semantic type")))?;
        validate_rule_fn_input(program, function, input)
            .map_err(|error| MagError::Type(format!("rule {id:?}: {error}")))?;
    }
    record_phase(profiler, Phase::ResidentRuleValidation, started);
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
