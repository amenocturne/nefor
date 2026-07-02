use crate::types::MagType;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Symbol(String),
    Keyword(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    List(Vec<Expr>),
    Vector(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
}

impl Expr {
    pub fn as_symbol(&self) -> Option<&str> {
        match self {
            Expr::Symbol(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_keyword(&self) -> Option<&str> {
        match self {
            Expr::Keyword(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Expr::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Expr]> {
        match self {
            Expr::List(items) => Some(items),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Keyword(String),
    Symbol(String),
    List(Vec<Value>),
    Vector(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(NodeValue),
    Graph(GraphValue),
    Subgraph(SubgraphValue),
    Fn(FnValue),
    BuiltinFn(String),
    TypeDecl(String),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_node(&self) -> Option<&NodeValue> {
        match self {
            Value::Node(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_graph(&self) -> Option<&GraphValue> {
        match self {
            Value::Graph(g) => Some(g),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Nil => "nil",
            Value::Keyword(_) => "keyword",
            Value::Symbol(_) => "symbol",
            Value::List(_) => "list",
            Value::Vector(_) => "vector",
            Value::Map(_) => "map",
            Value::Node(_) => "node",
            Value::Graph(_) => "graph",
            Value::Subgraph(_) => "subgraph",
            Value::Fn(_) => "fn",
            Value::BuiltinFn(_) => "builtin-fn",
            Value::TypeDecl(_) => "type",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeValue {
    pub id: String,
    pub node_type: String,
    pub args: BTreeMap<String, Value>,
    pub input_type: MagType,
    pub output_type: MagType,
}

#[derive(Debug, Clone)]
pub struct GraphValue {
    pub nodes: Vec<NodeValue>,
    pub edges: Vec<EdgeValue>,
    pub terminal: String,
}

#[derive(Debug, Clone)]
pub struct EdgeValue {
    pub from: String,
    pub to: String,
}

/// A boundary handle on a subgraph: the concrete internal actor that receives
/// the boundary input (`input`) or emits the boundary output type (`outputs`),
/// paired with the type crossing that boundary.
#[derive(Debug, Clone)]
pub struct Port {
    pub actor: String,
    pub ty: MagType,
}

/// A composable subgraph value returned by template functions like `agent`.
/// Its internal actor ids are already namespaced; `input`/`outputs` are the
/// boundary ports the enclosing `graph` wires its edges against.
#[derive(Debug, Clone)]
pub struct SubgraphValue {
    pub nodes: Vec<NodeValue>,
    pub edges: Vec<EdgeValue>,
    pub input: Port,
    pub outputs: Vec<Port>,
}

#[derive(Debug, Clone)]
pub struct FnValue {
    pub params: Vec<String>,
    pub body: Vec<Expr>,
    pub closure: Vec<(String, Value)>,
}
