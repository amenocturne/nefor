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
use ir::GraphIr;

pub fn compile(source: &str) -> Result<GraphIr, MagError> {
    let tokens = lexer::tokenize(source)?;
    let exprs = parser::parse(&tokens)?;
    let mut env = env::Env::new_with_stdlib();
    let value = eval::eval_program(&mut env, &exprs)?;
    let graph = graph::extract_graph(value)?;
    graph::validate(&graph)?;
    Ok(ir::normalize(graph))
}
