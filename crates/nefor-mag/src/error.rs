#[derive(Debug, thiserror::Error)]
pub enum MagError {
    #[error("lexer: {0}")]
    Lex(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("eval: {0}")]
    Eval(String),
    #[error("evaluation budget exceeded: {0}")]
    Budget(String),
    #[error("type error: {0}")]
    Type(String),
    #[error("unresolved symbol: {0}")]
    Unresolved(String),
    #[error("arity: expected {expected}, got {got}")]
    Arity { expected: usize, got: usize },
}
