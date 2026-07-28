use crate::types::MagType;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

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
        if let Self::Symbol(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_keyword(&self) -> Option<&str> {
        if let Self::Keyword(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_list(&self) -> Option<&[Expr]> {
        if let Self::List(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct FnValue {
    pub name: Option<String>,
    pub type_params: Vec<String>,
    pub params: Vec<String>,
    pub param_types: Vec<MagType>,
    pub return_type: MagType,
    pub body: Vec<Expr>,
    pub closure: Vec<(String, Value)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeDecl {
    pub name: String,
    pub params: Vec<String>,
    pub body: MagType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForeignDecl {
    pub name: String,
    pub type_params: Vec<String>,
    pub specialization: Vec<MagType>,
    pub params: MagType,
    pub input: MagType,
    pub output: MagType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForeignEvidence {
    pub identity: String,
    pub arguments: Vec<crate::types::ConcreteType>,
    pub input: crate::types::ConcreteType,
    pub output: crate::types::ConcreteType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub format: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Keyword(String),
    Symbol(String),
    List(Arc<Vec<Value>>),
    Vector(Arc<Vec<Value>>),
    /// A checked ordered product. Unlike a Vector, every position retains its
    /// own trusted type and constructor evidence.
    Product(Arc<Vec<Value>>),
    Map(Arc<BTreeMap<String, Value>>),
    Fn(Arc<FnValue>),
    BuiltinFn(String),
    Type(MagType),
    TypeDecl(TypeDecl),
    TypeTag(crate::types::ConcreteType),
    Foreign(ForeignDecl),
    ForeignEvidence(ForeignEvidence),
    TypeDescriptor(crate::types::ConcreteType),
    TypeSchema(crate::schema::TypeSchema),
    SemanticTypeId(crate::types::SemanticTypeId),
    PackedValue(Arc<Value>),
    JsonValue(serde_json::Value),
    HostInputs(serde_json::Value),
    Artifact(Artifact),
    Typed(Arc<Value>, MagType),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Typed(value, _) => value.as_str(),
            _ => None,
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Unit => "unit",
            Self::Str(_) => "string",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Bool(_) => "bool",
            Self::Keyword(_) => "keyword",
            Self::Symbol(_) => "symbol",
            Self::List(_) => "list",
            Self::Vector(_) => "vector",
            Self::Product(_) => "product",
            Self::Map(_) => "map",
            Self::Fn(_) => "fn",
            Self::BuiltinFn(_) => "builtin-fn",
            Self::Type(_) | Self::TypeDecl(_) => "type",
            Self::TypeTag(_) => "type-tag",
            Self::Foreign(_) => "foreign",
            Self::ForeignEvidence(_) => "foreign-evidence",
            Self::TypeDescriptor(_) => "type-descriptor",
            Self::TypeSchema(_) => "type-schema",
            Self::SemanticTypeId(_) => "semantic-type-id",
            Self::PackedValue(_) => "packed-value",
            Self::JsonValue(_) => "json-value",
            Self::HostInputs(_) => "host-inputs",
            Self::Artifact(_) => "artifact",
            Self::Typed(value, _) => value.type_name(),
        }
    }
}
