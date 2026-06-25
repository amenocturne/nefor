use crate::types::MagType;

#[derive(Debug, thiserror::Error)]
pub enum MagError {
    #[error("lexer: {0}")]
    Lex(String),

    #[error("parse: {0}")]
    Parse(String),

    #[error("eval: {0}")]
    Eval(String),

    #[error("type error: {0}")]
    Type(String),

    #[error("graph: {0}")]
    Graph(String),

    #[error("unresolved symbol: {0}")]
    Unresolved(String),

    #[error("arity: expected {expected}, got {got}")]
    Arity { expected: usize, got: usize },

    #[error("dead branch: variant {variant} of {source_type} from node '{node}' has no destination edge")]
    DeadBranch {
        node: String,
        variant: MagType,
        source_type: MagType,
    },

    #[error("unbounded loop: cycle through [{nodes}] has no loop-counter node")]
    UnboundedLoop { nodes: String },

    #[error("disconnected node: '{node}' has no path to any terminal")]
    Disconnected { node: String },

    #[error(
        "type mismatch on edge {from} -> {to}: output {output} incompatible with input {input}"
    )]
    EdgeTypeMismatch {
        from: String,
        to: String,
        output: MagType,
        input: MagType,
    },

    #[error("no terminal node declared")]
    NoTerminal,
}
