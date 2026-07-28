use crate::env::Env;
use crate::error::MagError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MagType {
    Artifact,
    JsonValue,
    TypeDescriptor,
    TypeSchema,
    SemanticTypeId,
    PackedValue,
    HostInputs,
    Never,
    Unit,
    Bool,
    Int,
    Float,
    String,
    Var(String),
    Named(String, Vec<MagType>),
    TypeTag(Box<MagType>),
    ForeignEvidence,
    List(Box<MagType>),
    EmptyList,
    Map(Box<MagType>, Box<MagType>),
    Record(BTreeMap<String, MagType>),
    Union(Vec<MagType>),
    Product(Vec<MagType>),
    Function(Vec<MagType>, Box<MagType>),
    Foreign(Box<MagType>, Box<MagType>, Box<MagType>),
}

/// The single runtime-safe semantic type representation. Unlike `MagType`,
/// this cannot contain inference variables, placeholders, or executable types.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConcreteType {
    JsonValue,
    Unit,
    Bool,
    Int,
    Float,
    String,
    Named {
        name: String,
        arguments: Vec<ConcreteType>,
        body: Box<ConcreteType>,
    },
    List {
        item: Box<ConcreteType>,
    },
    Map {
        key: Box<ConcreteType>,
        value: Box<ConcreteType>,
    },
    Record {
        fields: BTreeMap<String, ConcreteType>,
    },
    Sum {
        arms: Vec<ConcreteType>,
    },
    Product {
        items: Vec<ConcreteType>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SemanticTypeId(String);

impl SemanticTypeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SemanticTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ConcreteType {
    pub fn resolve(env: &Env, ty: &MagType) -> Result<Self, MagError> {
        resolve(env, ty, &mut HashSet::new())
    }

    pub fn to_mag_type(&self) -> MagType {
        match self {
            Self::JsonValue => MagType::JsonValue,
            Self::Unit => MagType::Unit,
            Self::Bool => MagType::Bool,
            Self::Int => MagType::Int,
            Self::Float => MagType::Float,
            Self::String => MagType::String,
            Self::Named {
                name, arguments, ..
            } => MagType::Named(
                name.clone(),
                arguments.iter().map(Self::to_mag_type).collect(),
            ),
            Self::List { item } => MagType::List(Box::new(item.to_mag_type())),
            Self::Map { key, value } => {
                MagType::Map(Box::new(key.to_mag_type()), Box::new(value.to_mag_type()))
            }
            Self::Record { fields } => MagType::Record(
                fields
                    .iter()
                    .map(|(name, ty)| (name.clone(), ty.to_mag_type()))
                    .collect(),
            ),
            Self::Sum { arms } => MagType::Union(arms.iter().map(Self::to_mag_type).collect()),
            Self::Product { items } => {
                MagType::Product(items.iter().map(Self::to_mag_type).collect())
            }
        }
    }

    pub fn stable_id(&self) -> SemanticTypeId {
        // serde_json follows enum field order and BTreeMap key order, making
        // these canonical bytes independent of allocation and compilation.
        let bytes = serde_json::to_vec(self)
            .unwrap_or_else(|_| unreachable!("ConcreteType serialization is infallible"));
        SemanticTypeId(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn accepts(&self, actual: &Self) -> bool {
        if self == actual {
            return true;
        }
        if let Self::Sum { arms } = actual {
            return arms.iter().all(|arm| self.accepts(arm));
        }
        if let Self::Sum { arms } = self {
            return arms.iter().any(|arm| arm.accepts(actual));
        }
        match (self, actual) {
            (Self::List { item: expected }, Self::List { item: actual }) => {
                expected.accepts(actual)
            }
            (
                Self::Map {
                    key: expected_key,
                    value: expected_value,
                },
                Self::Map {
                    key: actual_key,
                    value: actual_value,
                },
            ) => expected_key.accepts(actual_key) && expected_value.accepts(actual_value),
            (Self::Map { key, value }, Self::Record { fields })
                if key.as_ref() == &Self::String =>
            {
                fields.values().all(|actual| value.accepts(actual))
            }
            (Self::Record { fields: expected }, Self::Record { fields: actual })
                if expected.len() == actual.len() =>
            {
                expected.iter().all(|(name, expected)| {
                    actual
                        .get(name)
                        .is_some_and(|actual| expected.accepts(actual))
                })
            }
            (Self::Product { items: expected }, Self::Product { items: actual })
                if expected.len() == actual.len() =>
            {
                expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| expected.accepts(actual))
            }
            _ => false,
        }
    }
}

fn resolve(
    env: &Env,
    ty: &MagType,
    resolving: &mut HashSet<String>,
) -> Result<ConcreteType, MagError> {
    Ok(match ty {
        MagType::JsonValue => ConcreteType::JsonValue,
        MagType::Unit => ConcreteType::Unit,
        MagType::Bool => ConcreteType::Bool,
        MagType::Int => ConcreteType::Int,
        MagType::Float => ConcreteType::Float,
        MagType::String => ConcreteType::String,
        MagType::List(item) => ConcreteType::List {
            item: Box::new(resolve(env, item, resolving)?),
        },
        MagType::Map(key, value) => ConcreteType::Map {
            key: Box::new(resolve(env, key, resolving)?),
            value: Box::new(resolve(env, value, resolving)?),
        },
        MagType::Record(fields) => ConcreteType::Record {
            fields: fields
                .iter()
                .map(|(name, ty)| Ok((name.clone(), resolve(env, ty, resolving)?)))
                .collect::<Result<_, MagError>>()?,
        },
        MagType::Product(items) => ConcreteType::Product {
            // Products deliberately retain authored arity, repetition, order,
            // and every nested Product node.
            items: items
                .iter()
                .map(|ty| resolve(env, ty, resolving))
                .collect::<Result<_, _>>()?,
        },
        MagType::Union(items) => {
            let mut arms = BTreeSet::new();
            for item in items {
                match resolve(env, item, resolving)? {
                    ConcreteType::Sum { arms: nested } => arms.extend(nested),
                    arm => {
                        arms.insert(arm);
                    }
                }
            }
            let arms = arms.into_iter().collect::<Vec<_>>();
            match arms.as_slice() {
                [] => return Err(MagError::Type("a sum must have at least one arm".into())),
                [only] => only.clone(),
                _ => ConcreteType::Sum { arms },
            }
        }
        MagType::Named(name, args) => {
            let decl = env
                .type_decl(name)
                .ok_or_else(|| MagError::Type(format!("unknown nominal type {name}")))?;
            if decl.params.len() != args.len() {
                return Err(MagError::Type(format!(
                    "{name} expects {} type arguments, got {}",
                    decl.params.len(),
                    args.len()
                )));
            }
            let arguments = args
                .iter()
                .map(|ty| resolve(env, ty, resolving))
                .collect::<Result<Vec<_>, _>>()?;
            let key = format!("{name}<{arguments:?}>");
            if !resolving.insert(key.clone()) {
                return Err(MagError::Type(format!(
                    "recursive semantic type {name} is unsupported"
                )));
            }
            let substitutions: HashMap<_, _> = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = resolve(
                env,
                &crate::checker::substitute(&decl.body, &substitutions),
                resolving,
            )?;
            resolving.remove(&key);
            // A named declaration whose instantiated body unfolds to a sum is
            // an alias. Every other declaration remains a nominal constructor.
            match body {
                ConcreteType::Sum { .. } => body,
                body => ConcreteType::Named {
                    name: name.clone(),
                    arguments,
                    body: Box::new(body),
                },
            }
        }
        MagType::Var(name) => {
            return Err(MagError::Type(format!(
                "unresolved type variable {name} cannot enter a concrete descriptor"
            )))
        }
        MagType::EmptyList => {
            return Err(MagError::Type(
                "untyped empty-list placeholder cannot enter a concrete descriptor".into(),
            ))
        }
        MagType::Artifact => return unsupported("Artifact"),
        MagType::TypeDescriptor => return unsupported("TypeDescriptor"),
        MagType::TypeSchema => return unsupported("TypeSchema"),
        MagType::SemanticTypeId => return unsupported("SemanticTypeId"),
        MagType::PackedValue => return unsupported("PackedValue"),
        MagType::HostInputs => return unsupported("HostInputs"),
        MagType::Never => return unsupported("Never"),
        MagType::TypeTag(_) => return unsupported("TypeTag"),
        MagType::ForeignEvidence => return unsupported("ForeignEvidence"),
        MagType::Function(_, _) => return unsupported("Fn"),
        MagType::Foreign(_, _, _) => return unsupported("Foreign"),
    })
}

fn unsupported<T>(name: &str) -> Result<T, MagError> {
    Err(MagError::Type(format!(
        "{name} cannot enter a concrete semantic descriptor"
    )))
}

impl fmt::Display for MagType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact => write!(f, "Artifact"),
            Self::JsonValue => write!(f, "JsonValue"),
            Self::TypeDescriptor => write!(f, "TypeDescriptor"),
            Self::TypeSchema => write!(f, "TypeSchema"),
            Self::SemanticTypeId => write!(f, "SemanticTypeId"),
            Self::PackedValue => write!(f, "PackedValue"),
            Self::HostInputs => write!(f, "HostInputs"),
            Self::Never => write!(f, "Never"),
            Self::Unit => write!(f, "Unit"),
            Self::Bool => write!(f, "Bool"),
            Self::Int => write!(f, "Int"),
            Self::Float => write!(f, "Float"),
            Self::String => write!(f, "String"),
            Self::Var(name) => write!(f, "{name}"),
            Self::Named(name, args) if args.is_empty() => write!(f, "{name}"),
            Self::Named(name, args) => write!(f, "({name} {})", join(args)),
            Self::TypeTag(ty) => write!(f, "(TypeTag {ty})"),
            Self::ForeignEvidence => write!(f, "ForeignEvidence"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{TypeDecl, Value};

    fn env_with_types() -> Env {
        let mut env = Env::new();
        for (local, body) in [
            (
                "X",
                MagType::Record(BTreeMap::from([("value".into(), MagType::Int)])),
            ),
            (
                "Y",
                MagType::Record(BTreeMap::from([("value".into(), MagType::Int)])),
            ),
            (
                "Z",
                MagType::Record(BTreeMap::from([("value".into(), MagType::String)])),
            ),
            (
                "A",
                MagType::Union(vec![
                    MagType::Named("main.X".into(), vec![]),
                    MagType::Named("main.Y".into(), vec![]),
                ]),
            ),
            (
                "B",
                MagType::Union(vec![
                    MagType::Named("main.A".into(), vec![]),
                    MagType::Named("main.Z".into(), vec![]),
                ]),
            ),
        ] {
            env.define(
                local,
                Value::TypeDecl(TypeDecl {
                    name: format!("main.{local}"),
                    params: vec![],
                    body,
                }),
            );
        }
        env
    }

    #[test]
    fn sums_are_associative_idempotent_and_canonically_ordered() {
        let env = env_with_types();
        let x = MagType::Named("main.X".into(), vec![]);
        let y = MagType::Named("main.Y".into(), vec![]);
        let z = MagType::Named("main.Z".into(), vec![]);
        let left = MagType::Union(vec![
            x.clone(),
            MagType::Union(vec![y.clone(), z.clone(), x.clone()]),
        ]);
        let right = MagType::Union(vec![z, y, x]);
        assert_eq!(
            ConcreteType::resolve(&env, &left).unwrap(),
            ConcreteType::resolve(&env, &right).unwrap()
        );
    }

    #[test]
    fn sum_aliases_erase_to_distinct_nominal_leaf_constructors() {
        let env = env_with_types();
        let descriptor =
            ConcreteType::resolve(&env, &MagType::Named("main.B".into(), vec![])).unwrap();
        let ConcreteType::Sum { arms } = descriptor else {
            panic!("B must unfold to a sum")
        };
        assert_eq!(arms.len(), 3);
        let names = arms
            .iter()
            .map(|arm| match arm {
                ConcreteType::Named { name, .. } => name.as_str(),
                _ => panic!("sum arm must remain nominal"),
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from(["main.X", "main.Y", "main.Z"]));
        assert_ne!(
            arms.iter()
                .find(|arm| matches!(arm, ConcreteType::Named { name, .. } if name == "main.X")),
            arms.iter()
                .find(|arm| matches!(arm, ConcreteType::Named { name, .. } if name == "main.Y"))
        );
    }

    #[test]
    fn products_preserve_order_grouping_arity_and_repetition() {
        let env = env_with_types();
        let x = MagType::Named("main.X".into(), vec![]);
        let y = MagType::Named("main.Y".into(), vec![]);
        let left = ConcreteType::resolve(
            &env,
            &MagType::Product(vec![
                MagType::Product(vec![x.clone(), y.clone()]),
                x.clone(),
            ]),
        )
        .unwrap();
        let right = ConcreteType::resolve(
            &env,
            &MagType::Product(vec![x.clone(), MagType::Product(vec![y, x.clone()])]),
        )
        .unwrap();
        let flat = ConcreteType::resolve(&env, &MagType::Product(vec![x.clone(), x])).unwrap();
        assert_ne!(left, right);
        let ConcreteType::Product { items } = flat else {
            panic!("product expected")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], items[1]);
    }

    #[test]
    fn stable_ids_match_across_independent_environments() {
        let first =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.B".into(), vec![]))
                .unwrap();
        let second =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.B".into(), vec![]))
                .unwrap();
        assert_eq!(first.stable_id(), second.stable_id());
        assert!(first.stable_id().as_str().starts_with("sha256:"));
    }

    #[test]
    fn open_and_non_runtime_types_cannot_be_descriptors() {
        let env = env_with_types();
        for ty in [
            MagType::Var("T".into()),
            MagType::EmptyList,
            MagType::Artifact,
            MagType::Function(vec![MagType::Int], Box::new(MagType::Int)),
            MagType::Foreign(
                Box::new(MagType::Unit),
                Box::new(MagType::Unit),
                Box::new(MagType::Unit),
            ),
        ] {
            assert!(ConcreteType::resolve(&env, &ty).is_err(), "{ty}");
        }
    }
}
