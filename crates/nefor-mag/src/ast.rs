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
    pub terminals: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EdgeValue {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct FnValue {
    pub params: Vec<String>,
    pub body: Vec<Expr>,
    pub closure: Vec<(String, Value)>,
}
