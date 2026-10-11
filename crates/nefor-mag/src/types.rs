use crate::env::Env;
use crate::error::MagError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
    List(Box<MagType>),
    Set(Box<MagType>),
    EmptyList,
    Map(Box<MagType>, Box<MagType>),
    Product(Vec<MagType>),
    Function(Vec<MagType>, Box<MagType>),
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
        body: ConcreteNamedBody,
    },
    Adt {
        name: String,
        arguments: Vec<ConcreteType>,
        constructors: Vec<ConcreteConstructor>,
    },
    List {
        item: Box<ConcreteType>,
    },
    Set {
        item: Box<ConcreteType>,
    },
    Map {
        key: Box<ConcreteType>,
        value: Box<ConcreteType>,
    },
    Product {
        items: Vec<ConcreteType>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConcreteNamedBody {
    Fields {
        fields: BTreeMap<String, ConcreteType>,
    },
    Alias {
        ty: Box<ConcreteType>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConcreteConstructor {
    pub name: String,
    pub payload: ConcreteType,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SemanticTypeId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SemanticConstructorId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAssignmentError {
    IncompleteCoverage,
    AmbiguousCoverage,
}

impl fmt::Display for InputAssignmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteCoverage => {
                write!(f, "incoming edge types do not completely cover the input")
            }
            Self::AmbiguousCoverage => {
                write!(
                    f,
                    "incoming edge types ambiguously cover distinct input positions"
                )
            }
        }
    }
}

impl SemanticTypeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SemanticConstructorId {
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
        env.profile_counters(|counters| {
            counters.concrete_type_resolution_requests =
                counters.concrete_type_resolution_requests.saturating_add(1);
        });
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
            }
            | Self::Adt {
                name, arguments, ..
            } => MagType::Named(
                name.clone(),
                arguments.iter().map(Self::to_mag_type).collect(),
            ),
            Self::List { item } => MagType::List(Box::new(item.to_mag_type())),
            Self::Set { item } => MagType::Set(Box::new(item.to_mag_type())),
            Self::Map { key, value } => {
                MagType::Map(Box::new(key.to_mag_type()), Box::new(value.to_mag_type()))
            }
            Self::Product { items } => {
                MagType::Product(items.iter().map(Self::to_mag_type).collect())
            }
        }
    }

    pub fn stable_id(&self) -> SemanticTypeId {
        // Keep the established bytes for descriptors expressible before the v2
        // algebra; only identities using new forms enter the namespaced format.
        let identity = legacy_identity_json(self).unwrap_or_else(|| {
            let descriptor = crate::json::concrete_type_to_json(self)
                .unwrap_or_else(|_| unreachable!("ConcreteType serialization is infallible"));
            json_object([
                ("domain", "mag.type.v2".into()),
                ("version", 2.into()),
                ("descriptor", descriptor),
            ])
        });
        let bytes = serde_json::to_vec(&identity)
            .unwrap_or_else(|_| unreachable!("semantic descriptor serialization is infallible"));
        SemanticTypeId(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn constructor_id(&self, name: &str) -> Result<SemanticConstructorId, MagError> {
        let Self::Adt { constructors, .. } = self else {
            return Err(MagError::Type(
                "constructor identity requires an ADT owner".into(),
            ));
        };
        if !constructors
            .iter()
            .any(|constructor| constructor.name == name)
        {
            return Err(MagError::Type(format!(
                "constructor {name} is not a member of this ADT"
            )));
        }
        let bytes = serde_json::to_vec(&serde_json::json!({
            "domain": "mag.constructor.v2",
            "owner": self.stable_id().as_str(),
            "name": name,
        }))
        .unwrap_or_else(|_| unreachable!("constructor identity serialization is infallible"));
        Ok(SemanticConstructorId(format!(
            "sha256:{:x}",
            Sha256::digest(bytes)
        )))
    }

    pub fn declarations(&self) -> Result<BTreeMap<String, ConcreteType>, MagError> {
        let mut declarations = BTreeMap::new();
        self.collect_declarations(&mut declarations)?;
        Ok(declarations)
    }

    fn collect_declarations(
        &self,
        declarations: &mut BTreeMap<String, ConcreteType>,
    ) -> Result<(), MagError> {
        let id = self.stable_id().to_string();
        if let Some(existing) = declarations.get(&id) {
            if existing != self {
                return Err(MagError::Type(format!(
                    "semantic type identity collision at {id}"
                )));
            }
            return Ok(());
        }
        declarations.insert(id, self.clone());
        match self {
            Self::Named {
                arguments, body, ..
            } => {
                for argument in arguments {
                    argument.collect_declarations(declarations)?;
                }
                match body {
                    ConcreteNamedBody::Fields { fields } => {
                        for field in fields.values() {
                            field.collect_declarations(declarations)?;
                        }
                    }
                    ConcreteNamedBody::Alias { ty } => ty.collect_declarations(declarations)?,
                }
            }
            Self::Adt {
                arguments,
                constructors,
                ..
            } => {
                for argument in arguments {
                    argument.collect_declarations(declarations)?;
                }
                for constructor in constructors {
                    constructor.payload.collect_declarations(declarations)?;
                }
            }
            Self::List { item } | Self::Set { item } => item.collect_declarations(declarations)?,
            Self::Map { key, value } => {
                key.collect_declarations(declarations)?;
                value.collect_declarations(declarations)?;
            }
            Self::Product { items } => {
                for item in items {
                    item.collect_declarations(declarations)?;
                }
            }
            Self::JsonValue | Self::Unit | Self::Bool | Self::Int | Self::Float | Self::String => {}
        }
        Ok(())
    }

    pub fn accepts(&self, actual: &Self) -> bool {
        if self == actual {
            return true;
        }
        match (self, actual) {
            (Self::List { item: expected }, Self::List { item: actual })
            | (Self::Set { item: expected }, Self::Set { item: actual }) => {
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

    /// Compatibility for one graph edge. A product consumer may receive the
    /// complete tuple or one source statically assigned to a component
    /// occurrence. A sum source may route the alternatives accepted by this
    /// destination; exhaustive handling of every source arm is checked across
    /// all outgoing edges separately.
    pub fn accepts_edge_source(&self, actual: &Self) -> bool {
        self.accepts(actual)
            || matches!(self, Self::Product { items } if items.iter().any(|item| item.accepts_edge_source(actual)))
    }

    /// Whether incoming edge types completely supply this input. Ordinary
    /// inputs need no aggregate coverage check. Product inputs accept either
    /// one whole-product edge or one compatible edge per ordered occurrence.
    ///
    /// Retaining the successful occurrence assignment is a lowering concern;
    /// this predicate owns only the compatibility-backed coverage decision.
    pub fn input_is_covered_by(&self, sources: &[Self]) -> bool {
        self.assign_input_sources(sources).is_ok()
    }

    /// Assign every component edge to one ordered product occurrence. `None`
    /// identifies a whole-value edge; `Some(position)` identifies a component
    /// queue. Equal repeated components are assigned by source order, while
    /// more than one assignment across distinct component descriptors is an
    /// ambiguity.
    pub fn assign_input_sources(
        &self,
        sources: &[Self],
    ) -> Result<Vec<Option<usize>>, InputAssignmentError> {
        let Self::Product { items } = self else {
            return (!sources.is_empty()
                && sources
                    .iter()
                    .all(|source| self.accepts_edge_source(source)))
            .then(|| vec![None; sources.len()])
            .ok_or(InputAssignmentError::IncompleteCoverage);
        };

        let mut result = vec![None; sources.len()];
        let component_sources = sources
            .iter()
            .enumerate()
            .filter(|(_, source)| !self.accepts(source))
            .collect::<Vec<_>>();
        if component_sources.is_empty() {
            return sources
                .iter()
                .all(|source| self.accepts(source))
                .then_some(result)
                .ok_or(InputAssignmentError::IncompleteCoverage);
        }
        if component_sources.len() != items.len() {
            return Err(InputAssignmentError::IncompleteCoverage);
        }

        let mut capacities = BTreeMap::<ConcreteType, usize>::new();
        for item in items {
            *capacities.entry(item.clone()).or_default() += 1;
        }
        let mut semantic_assignments = BTreeSet::new();
        collect_component_assignments(
            &component_sources,
            0,
            &mut capacities,
            &mut Vec::new(),
            &mut semantic_assignments,
        );
        let mut assignments = semantic_assignments.into_iter();
        let assignment = assignments
            .next()
            .ok_or(InputAssignmentError::IncompleteCoverage)?;
        if assignments.next().is_some() {
            return Err(InputAssignmentError::AmbiguousCoverage);
        }

        let mut positions = BTreeMap::<ConcreteType, VecDeque<usize>>::new();
        for (position, item) in items.iter().enumerate() {
            positions
                .entry(item.clone())
                .or_default()
                .push_back(position);
        }
        for ((source_index, _), component) in component_sources.into_iter().zip(assignment) {
            result[source_index] = positions
                .get_mut(&component)
                .and_then(VecDeque::pop_front)
                .map(Some)
                .ok_or(InputAssignmentError::IncompleteCoverage)?;
        }
        Ok(result)
    }

    pub fn output_is_covered_by(&self, handlers: &[Self]) -> bool {
        handlers
            .iter()
            .any(|handler| handler.accepts_edge_source(self))
    }
}

fn json_object<const N: usize>(fields: [(&str, serde_json::Value); N]) -> serde_json::Value {
    serde_json::Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn legacy_identity_json(ty: &ConcreteType) -> Option<serde_json::Value> {
    let primitive = |kind: &str| json_object([("kind", kind.to_owned().into())]);
    Some(match ty {
        ConcreteType::JsonValue => primitive("json_value"),
        ConcreteType::Unit => primitive("unit"),
        ConcreteType::Bool => primitive("bool"),
        ConcreteType::Int => primitive("int"),
        ConcreteType::Float => primitive("float"),
        ConcreteType::String => primitive("string"),
        ConcreteType::Named {
            name,
            arguments,
            body,
        } => {
            let arguments = arguments
                .iter()
                .map(legacy_identity_json)
                .collect::<Option<Vec<_>>>()?;
            let body = match body {
                ConcreteNamedBody::Fields { fields } => json_object([
                    ("kind", "record".into()),
                    (
                        "fields",
                        serde_json::Value::Object(
                            fields
                                .iter()
                                .map(|(name, ty)| Some((name.clone(), legacy_identity_json(ty)?)))
                                .collect::<Option<_>>()?,
                        ),
                    ),
                ]),
                ConcreteNamedBody::Alias { ty } => legacy_identity_json(ty)?,
            };
            json_object([
                ("kind", "named".into()),
                ("name", name.clone().into()),
                ("arguments", serde_json::Value::Array(arguments)),
                ("body", body),
            ])
        }
        ConcreteType::Adt { .. } | ConcreteType::Set { .. } => return None,
        ConcreteType::List { item } => json_object([
            ("kind", "list".into()),
            ("item", legacy_identity_json(item)?),
        ]),
        ConcreteType::Map { key, value } => json_object([
            ("kind", "map".into()),
            ("key", legacy_identity_json(key)?),
            ("value", legacy_identity_json(value)?),
        ]),
        ConcreteType::Product { items } => json_object([
            ("kind", "product".into()),
            (
                "items",
                serde_json::Value::Array(
                    items
                        .iter()
                        .map(legacy_identity_json)
                        .collect::<Option<_>>()?,
                ),
            ),
        ]),
    })
}

fn collect_component_assignments(
    sources: &[(usize, &ConcreteType)],
    source_index: usize,
    capacities: &mut BTreeMap<ConcreteType, usize>,
    current: &mut Vec<ConcreteType>,
    assignments: &mut BTreeSet<Vec<ConcreteType>>,
) {
    if assignments.len() > 1 {
        return;
    }
    let Some((_, source)) = sources.get(source_index) else {
        assignments.insert(current.clone());
        return;
    };
    let candidates = capacities
        .iter()
        .filter(|(component, remaining)| **remaining > 0 && component.accepts(source))
        .map(|(component, _)| component.clone())
        .collect::<Vec<_>>();
    for component in candidates {
        let Some(remaining) = capacities.get_mut(&component) else {
            continue;
        };
        *remaining -= 1;
        current.push(component.clone());
        collect_component_assignments(sources, source_index + 1, capacities, current, assignments);
        current.pop();
        if let Some(remaining) = capacities.get_mut(&component) {
            *remaining += 1;
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
        MagType::Set(item) => ConcreteType::Set {
            item: Box::new(resolve(env, item, resolving)?),
        },
        MagType::Map(key, value) => ConcreteType::Map {
            key: Box::new(resolve(env, key, resolving)?),
            value: Box::new(resolve(env, value, resolving)?),
        },
        MagType::Product(items) => ConcreteType::Product {
            // Products deliberately retain authored arity, repetition, order,
            // and every nested Product node.
            items: items
                .iter()
                .map(|ty| resolve(env, ty, resolving))
                .collect::<Result<_, _>>()?,
        },
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
            let concrete = match decl.body {
                crate::ast::TypeDeclBody::Fields(crate::ast::FieldTypes {
                    types: fields, ..
                }) => ConcreteType::Named {
                    name: name.clone(),
                    arguments,
                    body: ConcreteNamedBody::Fields {
                        fields: fields
                            .iter()
                            .map(|(field, ty)| {
                                Ok((
                                    field.clone(),
                                    resolve(
                                        env,
                                        &crate::checker::substitute(ty, &substitutions),
                                        resolving,
                                    )?,
                                ))
                            })
                            .collect::<Result<_, MagError>>()?,
                    },
                },
                crate::ast::TypeDeclBody::TransparentAlias(body) => resolve(
                    env,
                    &crate::checker::substitute(&body, &substitutions),
                    resolving,
                )?,
                crate::ast::TypeDeclBody::Alias(body) => ConcreteType::Named {
                    name: name.clone(),
                    arguments,
                    body: ConcreteNamedBody::Alias {
                        ty: Box::new(resolve(
                            env,
                            &crate::checker::substitute(&body, &substitutions),
                            resolving,
                        )?),
                    },
                },
                crate::ast::TypeDeclBody::Adt(mut constructors) => {
                    constructors.sort_by(|left, right| left.id.name.cmp(&right.id.name));
                    ConcreteType::Adt {
                        name: name.clone(),
                        arguments,
                        constructors: constructors
                            .into_iter()
                            .map(|constructor| {
                                Ok(ConcreteConstructor {
                                    name: constructor.id.name,
                                    payload: resolve(
                                        env,
                                        &crate::checker::substitute(
                                            &constructor.payload,
                                            &substitutions,
                                        ),
                                        resolving,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>, MagError>>()?,
                    }
                }
                crate::ast::TypeDeclBody::Native => {
                    return Err(MagError::Type(format!(
                        "native type {} did not lower to its concrete representation",
                        decl.name
                    )))
                }
            };
            resolving.remove(&key);
            concrete
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
        MagType::Function(_, _) => return unsupported("Fn"),
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
            Self::List(item) => write!(f, "(List {item})"),
            Self::Set(item) => write!(f, "(Set {item})"),
            Self::EmptyList => write!(f, "(List _)"),
            Self::Map(key, value) => write!(f, "(Map {key} {value})"),
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
        for (local, fields) in [
            ("X", BTreeMap::from([("value".into(), MagType::Int)])),
            ("Y", BTreeMap::from([("value".into(), MagType::Int)])),
            ("Z", BTreeMap::from([("value".into(), MagType::String)])),
        ] {
            env.define(
                local,
                Value::TypeDecl(TypeDecl {
                    name: format!("main.{local}"),
                    params: vec![],
                    body: crate::ast::TypeDeclBody::Fields(crate::ast::FieldTypes::sorted(fields)),
                }),
            );
        }
        env
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
    fn graph_edges_use_central_compatibility_and_products_require_occurrences() {
        let text =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.Z".into(), vec![]))
                .unwrap();
        let int =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.X".into(), vec![]))
                .unwrap();
        let pair = ConcreteType::Product {
            items: vec![text.clone(), text.clone()],
        };
        assert!(pair.accepts_edge_source(&text));
        assert!(!pair.accepts_edge_source(&int));
        assert!(pair.input_is_covered_by(std::slice::from_ref(&pair)));
        assert!(pair.input_is_covered_by(&[text.clone(), text.clone()]));
        assert!(!pair.input_is_covered_by(std::slice::from_ref(&text)));
        assert!(!pair.input_is_covered_by(&[text, int]));
    }

    #[test]
    fn product_assignments_preserve_wholes_and_order_equal_occurrences() {
        let text =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.Z".into(), vec![]))
                .unwrap();
        let pair = ConcreteType::Product {
            items: vec![text.clone(), text.clone()],
        };
        assert_eq!(
            pair.assign_input_sources(&[text.clone(), pair.clone(), text])
                .unwrap(),
            vec![Some(0), None, Some(1)]
        );
    }

    #[test]
    fn stable_ids_match_across_independent_environments() {
        let first =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.X".into(), vec![]))
                .unwrap();
        let second =
            ConcreteType::resolve(&env_with_types(), &MagType::Named("main.X".into(), vec![]))
                .unwrap();
        assert_eq!(first.stable_id(), second.stable_id());
        assert!(first.stable_id().as_str().starts_with("sha256:"));
    }

    #[test]
    fn adt_and_constructor_ids_are_nominal_and_instantiation_sensitive() {
        let constructors = vec![
            ConcreteConstructor {
                name: "Err".into(),
                payload: ConcreteType::String,
            },
            ConcreteConstructor {
                name: "Ok".into(),
                payload: ConcreteType::Int,
            },
        ];
        let first = ConcreteType::Adt {
            name: "main.Result".into(),
            arguments: vec![ConcreteType::String, ConcreteType::Int],
            constructors: constructors.clone(),
        };
        let phantom = ConcreteType::Adt {
            name: "main.Result".into(),
            arguments: vec![ConcreteType::Bool, ConcreteType::Int],
            constructors: constructors.clone(),
        };
        let other = ConcreteType::Adt {
            name: "main.Other".into(),
            arguments: vec![ConcreteType::String, ConcreteType::Int],
            constructors,
        };

        assert_eq!(
            first.stable_id().as_str(),
            "sha256:b0fb26fdf5d68d7852b5cded5f51f6f226f9351809f8128112241b724dd82176"
        );
        assert_eq!(
            first.constructor_id("Ok").unwrap().as_str(),
            "sha256:5945fa2b4a925d868c3e575bbb4326689bd890a7458b8b590cf8d071aa99aa98"
        );
        assert_ne!(first.stable_id(), phantom.stable_id());
        assert_ne!(first.stable_id(), other.stable_id());
        assert_ne!(
            first.constructor_id("Ok").unwrap(),
            phantom.constructor_id("Ok").unwrap()
        );
        assert_ne!(
            first.constructor_id("Ok").unwrap(),
            other.constructor_id("Ok").unwrap()
        );
    }

    #[test]
    fn sets_resolve_round_trip_and_use_item_compatibility() {
        let env = Env::new();
        let concrete = ConcreteType::resolve(&env, &MagType::Set(Box::new(MagType::Int))).unwrap();
        assert_eq!(
            concrete,
            ConcreteType::Set {
                item: Box::new(ConcreteType::Int)
            }
        );
        assert_eq!(concrete.to_mag_type(), MagType::Set(Box::new(MagType::Int)));
        assert!(concrete.accepts(&ConcreteType::Set {
            item: Box::new(ConcreteType::Int)
        }));
        assert!(!concrete.accepts(&ConcreteType::Set {
            item: Box::new(ConcreteType::String)
        }));
        assert_eq!(concrete.stable_id(), concrete.clone().stable_id());
    }

    #[test]
    fn open_and_non_runtime_types_cannot_be_descriptors() {
        let env = env_with_types();
        for ty in [
            MagType::Var("T".into()),
            MagType::EmptyList,
            MagType::Artifact,
            MagType::Function(vec![MagType::Int], Box::new(MagType::Int)),
        ] {
            assert!(ConcreteType::resolve(&env, &ty).is_err(), "{ty}");
        }
    }
}
