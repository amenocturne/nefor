pub mod ast;
pub mod env;
pub mod error;
pub mod eval;
pub mod graph;
pub mod ir;
pub mod json;
pub mod lexer;
pub mod parser;
pub mod types;

use env::Env;
use error::MagError;
use ir::ModificationIr;
use std::path::Path;

/// Batch compile: parse → evaluate → validate → lower → validate the
/// modification, returning just the modification. The `mag` CLI dev tool. Runs
/// unbounded on the caller's stack; the resident, session-bounded path is
/// [`load`].
pub fn compile(source: &str, source_dir: &Path) -> Result<ModificationIr, MagError> {
    let (modification, _env) = compile_resident(source, source_dir)?;
    Ok(modification)
}

/// A loaded program whose environment stays resident for the session. The env
/// is evaluated exactly once here; because MAG is pure, it is an exact function
/// of the source snapshot — two loads of the same snapshot produce identical
/// environments and therefore an identical modification `hash`. Rule functions
/// are entry points into this retained env, applied by [`eval_fn`].
pub struct LoadedProgram {
    pub env: Env,
    pub modification: ModificationIr,
    pub hash: String,
}

/// Step budget for the whole load pipeline. Generous: loading expands the full
/// program (agent constellations, module `require`s) once. Shares the fuel
/// mechanism with rule evaluation; the recursion-depth rail (eval.rs) guards
/// the stack regardless of this figure.
const LOAD_BUDGET: u64 = 10_000_000;

/// Step budget for a single rule-function evaluation. A rule maps a node output
/// into a modification — bounded work. Exceeding it rejects the modification
/// (see ir.md "Bounded evaluation") rather than hanging the run.
const EVAL_FN_BUDGET: u64 = 1_000_000;

/// Stack for the dedicated evaluation thread. The interpreter recurses natively
/// per MAG call; a roomy stack keeps the depth cap (4096) the binding limit and
/// makes evaluation independent of the caller's stack (a 2 MiB tokio worker in
/// the plugin, or a test harness thread).
const EVAL_STACK: usize = 64 * 1024 * 1024;

/// Load a program from `source_dir/entry`, retaining its environment.
///
/// The whole pipeline runs on a dedicated large-stack thread under
/// [`LOAD_BUDGET`], so a pathological (non-terminating) program errors cleanly
/// instead of overflowing the stack or hanging.
pub fn load(source_dir: &Path, entry: &str) -> Result<LoadedProgram, MagError> {
    // Same containment check as `read`/`require`: the entry must stay inside the
    // workspace (an absolute or `..`-escaping entry, or one reached through a
    // symlink out of the tree, is rejected before any read).
    let path = eval::resolve_workspace_path(source_dir, entry)?;
    let source = std::fs::read_to_string(&path)
        .map_err(|e| MagError::Eval(format!("cannot read program {}: {e}", path.display())))?;
    run_on_eval_thread(|| {
        let _budget = eval::fuel::install(LOAD_BUDGET);
        let (modification, env) = compile_resident(&source, source_dir)?;
        let hash = modification.hash.clone();
        Ok(LoadedProgram {
            env,
            modification,
            hash,
        })
    })
}

/// Apply the named unary rule function from a loaded program's cached
/// environment to a JSON node output, returning the modification it produces.
///
/// Always runs under [`EVAL_FN_BUDGET`] on the large-stack thread. The input
/// JSON is lifted to a MAG value, the fn applied, and its returned value lowered
/// back to a modification, hashed, and validated against the resident env (the
/// same validator load uses).
pub fn eval_fn(
    program: &LoadedProgram,
    name: &str,
    input: serde_json::Value,
) -> Result<ModificationIr, MagError> {
    run_on_eval_thread(|| {
        let _budget = eval::fuel::install(EVAL_FN_BUDGET);
        let arg = json::json_to_value(&input);
        let result = eval::apply_named(&program.env, name, arg)?;
        let produced = json::value_to_json(&result)?;
        let mut modification: ModificationIr = serde_json::from_value(produced).map_err(|e| {
            MagError::Eval(format!(
                "rule fn '{name}' did not return a graph modification: {e}"
            ))
        })?;
        ir::finalize_hash(&mut modification);
        ir::validate_modification(&modification, &program.env)?;
        Ok(modification)
    })
}

/// Shared load pipeline that also returns the resident environment. `compile`
/// discards the env; `load` keeps it.
fn compile_resident(source: &str, source_dir: &Path) -> Result<(ModificationIr, Env), MagError> {
    let tokens = lexer::tokenize(source)?;
    let exprs = parser::parse(&tokens)?;
    let mut env = Env::new_with_stdlib_and_source_dir(source_dir);
    let value = eval::eval_program(&mut env, &exprs)?;
    let graph = graph::extract_graph(value)?;
    // Authoring-level graph passes stay meaningful over the pre-lowering
    // representation: single sink, connectivity, dead branches, bounded loops,
    // edge-type compatibility.
    graph::validate(&graph)?;
    let modification = ir::lower(graph)?;
    ir::validate_modification(&modification, &env)?;
    Ok((modification, env))
}

/// Run `f` on a dedicated large-stack thread and return its result. Thread-local
/// fuel is per-thread, so the budget must be installed *inside* `f` (on the eval
/// thread), which the callers do.
fn run_on_eval_thread<T, F>(f: F) -> Result<T, MagError>
where
    F: FnOnce() -> Result<T, MagError> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .stack_size(EVAL_STACK)
            .spawn_scoped(scope, f)
            .map_err(|e| MagError::Eval(format!("spawn evaluation thread: {e}")))?;
        handle
            .join()
            .map_err(|_| MagError::Eval("evaluation thread panicked".into()))?
    })
}
