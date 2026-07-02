pub mod ast;
pub mod env;
pub mod error;
pub mod eval;
pub mod graph;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod types;

use error::MagError;
use ir::ModificationIr;
use std::path::Path;

pub fn compile(source: &str, source_dir: &Path) -> Result<ModificationIr, MagError> {
    let tokens = lexer::tokenize(source)?;
    let exprs = parser::parse(&tokens)?;
    let mut env = env::Env::new_with_stdlib_and_source_dir(source_dir);
    let value = eval::eval_program(&mut env, &exprs)?;
    let graph = graph::extract_graph(value)?;
    // Authoring-level graph passes stay meaningful over the pre-lowering
    // representation: single sink, connectivity, dead branches, bounded loops,
    // edge-type compatibility.
    graph::validate(&graph)?;
    let modification = ir::lower(graph)?;
    ir::validate_modification(&modification, &env)?;
    Ok(modification)
}
