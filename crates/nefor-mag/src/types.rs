use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MagType {
    Data,
    Artifact,
    Unit,
    Bool,
    Int,
    Float,
    String,
    Var(String),
    Named(String, Vec<MagType>),
    TypeTag(Box<MagType>),
    List(Box<MagType>),
    EmptyList,
    Map(Box<MagType>, Box<MagType>),
    Record(BTreeMap<String, MagType>),
    Union(Vec<MagType>),
    Product(Vec<MagType>),
    Function(Vec<MagType>, Box<MagType>),
    Foreign(Box<MagType>, Box<MagType>, Box<MagType>),
}

impl fmt::Display for MagType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data => write!(f, "Data"),
            Self::Artifact => write!(f, "Artifact"),
            Self::Unit => write!(f, "Unit"),
            Self::Bool => write!(f, "Bool"),
            Self::Int => write!(f, "Int"),
            Self::Float => write!(f, "Float"),
            Self::String => write!(f, "String"),
            Self::Var(name) => write!(f, "{name}"),
            Self::Named(name, args) if args.is_empty() => write!(f, "{name}"),
            Self::Named(name, args) => write!(f, "({name} {})", join(args)),
            Self::TypeTag(ty) => write!(f, "(TypeTag {ty})"),
            Self::List(item) => write!(f, "(List {item})"),
            Self::EmptyList => write!(f, "(List _)"),
            Self::Map(key, value) => write!(f, "(Map {key} {value})"),
            Self::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|(k, v)| format!(":{k} {v}"))
                    .collect::<Vec<_>>();
                write!(f, "{{{}}}", fields.join(" "))
            }
            Self::Union(types) => write!(
                f,
                "({})",
                types
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            Self::Product(types) => write!(
                f,
                "({})",
                types
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" + ")
            ),
            Self::Function(params, result) => {
                let mut parts = params.iter().map(ToString::to_string).collect::<Vec<_>>();
                parts.push(result.to_string());
                write!(f, "(Fn {})", parts.join(" "))
            }
            Self::Foreign(params, input, output) => {
                write!(f, "(Foreign {params} {input} {output})")
            }
        }
    }
}

fn join(types: &[MagType]) -> String {
    types
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}
