use crate::ast::Value;
use crate::checker::substitute;
use crate::env::Env;
use crate::error::MagError;
use crate::schema::TypeSchema;
use crate::types::MagType;
use std::collections::{BTreeMap, HashMap};

pub fn value_to_json(value: &Value) -> Result<serde_json::Value, MagError> {
    match value {
        Value::Unit => Ok(serde_json::Value::Null),
        Value::Str(v) => Ok(v.clone().into()),
        Value::Int(v) => Ok((*v).into()),
        Value::Float(v) => Ok(serde_json::json!(v)),
        Value::Bool(v) => Ok((*v).into()),
        Value::Keyword(v) => Ok(format!(":{v}").into()),
        Value::Symbol(v) => Ok(v.clone().into()),
        Value::TypeTag(ty) => concrete_type_to_json(ty),
        Value::ForeignEvidence(evidence) => Ok(serde_json::json!({
            "version": 2,
            "identity": evidence.identity,
            "arguments": evidence.arguments.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
            "input": concrete_type_to_json(&evidence.input)?,
            "output": concrete_type_to_json(&evidence.output)?,
        })),
        Value::List(v) | Value::Vector(v) | Value::Product(v) => Ok(serde_json::Value::Array(
            v.iter().map(value_to_json).collect::<Result<_, _>>()?,
        )),
        Value::Map(v) => Ok(serde_json::Value::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), value_to_json(v)?)))
                .collect::<Result<_, MagError>>()?,
        )),
        Value::Artifact(v) => {
            serde_json::to_value(v).map_err(|e| MagError::Eval(format!("serialize artifact: {e}")))
        }
        Value::Typed(value, _) => value_to_json(value),
        other => Err(MagError::Eval(format!(
            "cannot serialize {} to JSON",
            other.type_name()
        ))),
    }
}

pub(crate) fn concrete_type_to_json(
    ty: &crate::types::ConcreteType,
) -> Result<serde_json::Value, MagError> {
    use crate::types::ConcreteType;
    let primitive = |name: &str| serde_json::json!({ "kind": "primitive", "name": name });
    Ok(match ty {
        ConcreteType::Data => primitive("Data"),
        ConcreteType::Unit => primitive("Unit"),
        ConcreteType::Bool => primitive("Bool"),
        ConcreteType::Int => primitive("Int"),
        ConcreteType::Float => primitive("Float"),
        ConcreteType::String => primitive("String"),
        ConcreteType::Named {
            name, arguments, ..
        } => serde_json::json!({
            "kind": "named",
            "name": name,
            "arguments": arguments.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
        ConcreteType::List { item } => serde_json::json!({
            "kind": "list", "item": concrete_type_to_json(item)?
        }),
        ConcreteType::Map { key, value } => serde_json::json!({
            "kind": "map",
            "key": concrete_type_to_json(key)?,
            "value": concrete_type_to_json(value)?,
        }),
        ConcreteType::Record { fields } => serde_json::json!({
            "kind": "record",
            "fields": fields.iter().map(|(name, ty)| Ok(serde_json::json!({
                "name": name, "type": concrete_type_to_json(ty)?
            }))).collect::<Result<Vec<_>, MagError>>()?,
        }),
        ConcreteType::Sum { arms } => serde_json::json!({
            "kind": "union",
            "items": arms.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
        ConcreteType::Product { items } => serde_json::json!({
            "kind": "product",
            "items": items.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
    })
}

pub(crate) fn type_evidence_to_json(ty: &MagType) -> Result<serde_json::Value, MagError> {
    let primitive = |name: &str| serde_json::json!({ "kind": "primitive", "name": name });
    Ok(match ty {
        MagType::Data => primitive("Data"),
        MagType::Unit => primitive("Unit"),
        MagType::Bool => primitive("Bool"),
        MagType::Int => primitive("Int"),
        MagType::Float => primitive("Float"),
        MagType::String => primitive("String"),
        MagType::Named(name, arguments) => serde_json::json!({
            "kind": "named",
            "name": name,
            "arguments": arguments.iter().map(type_evidence_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
        MagType::List(item) => serde_json::json!({
            "kind": "list", "item": type_evidence_to_json(item)?
        }),
        MagType::Map(key, value) => serde_json::json!({
            "kind": "map",
            "key": type_evidence_to_json(key)?,
            "value": type_evidence_to_json(value)?,
        }),
        MagType::Record(fields) => serde_json::json!({
            "kind": "record",
            "fields": fields.iter().map(|(name, ty)| Ok(serde_json::json!({
                "name": name, "type": type_evidence_to_json(ty)?
            }))).collect::<Result<Vec<_>, MagError>>()?,
        }),
        MagType::Union(items) => serde_json::json!({
            "kind": "union",
            "items": items.iter().map(type_evidence_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
        MagType::Product(items) => serde_json::json!({
            "kind": "product",
            "items": items.iter().map(type_evidence_to_json).collect::<Result<Vec<_>, _>>()?,
        }),
        MagType::Artifact
        | MagType::Var(_)
        | MagType::TypeTag(_)
        | MagType::ForeignEvidence
        | MagType::EmptyList
        | MagType::Function(_, _)
        | MagType::Foreign(_, _, _) => {
            return Err(MagError::Type(format!(
                "{ty} is not legal in runtime foreign evidence"
            )))
        }
    })
}

pub(crate) fn type_evidence_from_json(value: &serde_json::Value) -> Result<MagType, MagError> {
    let object = value
        .as_object()
        .ok_or_else(|| MagError::Type("type evidence must be an object".into()))?;
    let kind = object
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MagError::Type("type evidence needs a string kind".into()))?;
    let string = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| MagError::Type(format!("type evidence needs string field {name}")))
    };
    let nested = |name: &str| {
        object
            .get(name)
            .ok_or_else(|| MagError::Type(format!("type evidence needs field {name}")))
            .and_then(type_evidence_from_json)
    };
    let items = |name: &str| -> Result<Vec<MagType>, MagError> {
        object
            .get(name)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| MagError::Type(format!("type evidence needs list field {name}")))?
            .iter()
            .map(type_evidence_from_json)
            .collect()
    };
    Ok(match kind {
        "primitive" => match string("name")?.as_str() {
            "Data" => MagType::Data,
            "Unit" => MagType::Unit,
            "Bool" => MagType::Bool,
            "Int" => MagType::Int,
            "Float" => MagType::Float,
            "String" => MagType::String,
            name => {
                return Err(MagError::Type(format!(
                    "unknown primitive type evidence {name}"
                )))
            }
        },
        "named" => MagType::Named(string("name")?, items("arguments")?),
        "list" => MagType::List(Box::new(nested("item")?)),
        "map" => MagType::Map(Box::new(nested("key")?), Box::new(nested("value")?)),
        "record" => {
            let fields = object
                .get("fields")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| MagError::Type("record type evidence needs fields".into()))?
                .iter()
                .map(|field| {
                    let field = field.as_object().ok_or_else(|| {
                        MagError::Type("record type evidence field must be an object".into())
                    })?;
                    let name = field
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            MagError::Type("record type evidence field needs a name".into())
                        })?;
                    let ty = field.get("type").ok_or_else(|| {
                        MagError::Type("record type evidence field needs a type".into())
                    })?;
                    Ok((name.to_owned(), type_evidence_from_json(ty)?))
                })
                .collect::<Result<_, MagError>>()?;
            MagType::Record(fields)
        }
        "union" => MagType::Union(items("items")?),
        "product" => MagType::Product(items("items")?),
        unknown => {
            return Err(MagError::Type(format!(
                "unknown type evidence kind {unknown}"
            )))
        }
    })
}

pub fn json_to_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Unit,
        serde_json::Value::Bool(v) => Value::Bool(*v),
        serde_json::Value::Number(v) => v
            .as_i64()
            .map(Value::Int)
            .or_else(|| v.as_f64().map(Value::Float))
            .unwrap_or_else(|| Value::Str(v.to_string())),
        serde_json::Value::String(v) => Value::Str(v.clone()),
        serde_json::Value::Array(v) => {
            Value::Vector(std::sync::Arc::new(v.iter().map(json_to_value).collect()))
        }
        serde_json::Value::Object(v) => Value::Map(std::sync::Arc::new(
            v.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        )),
    }
}

pub fn json_to_typed_value(
    env: &Env,
    value: &serde_json::Value,
    ty: &MagType,
) -> Result<Value, MagError> {
    Ok(match ty {
        MagType::Named(name, args) => {
            let decl = env
                .type_decl(name)
                .ok_or_else(|| MagError::Type(format!("unknown nominal type {name}")))?;
            let substitutions: HashMap<_, _> = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = substitute(&decl.body, &substitutions);
            Value::Typed(
                std::sync::Arc::new(json_to_typed_value(env, value, &body)?),
                ty.clone(),
            )
        }
        MagType::List(item) => Value::Vector(std::sync::Arc::new(
            value
                .as_array()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?
                .iter()
                .map(|entry| json_to_typed_value(env, entry, item))
                .collect::<Result<_, _>>()?,
        )),
        MagType::Map(_, item) => Value::Map(std::sync::Arc::new(
            value
                .as_object()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?
                .iter()
                .map(|(key, entry)| Ok((key.clone(), json_to_typed_value(env, entry, item)?)))
                .collect::<Result<BTreeMap<_, _>, MagError>>()?,
        )),
        MagType::Record(fields) => {
            let object = value
                .as_object()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?;
            Value::Map(std::sync::Arc::new(
                fields
                    .iter()
                    .map(|(key, field_type)| {
                        let field = object.get(key).ok_or_else(|| {
                            MagError::Type(format!("missing field {key} for {ty}"))
                        })?;
                        Ok((key.clone(), json_to_typed_value(env, field, field_type)?))
                    })
                    .collect::<Result<BTreeMap<_, _>, MagError>>()?,
            ))
        }
        MagType::Union(variants) => {
            let selected = variants
                .iter()
                .find(|variant| {
                    TypeSchema::reify(env, variant).is_ok_and(|schema| {
                        serde_json::to_string(value)
                            .is_ok_and(|encoded| schema.validate_json(&encoded).ok)
                    })
                })
                .ok_or_else(|| MagError::Type(format!("value does not conform to {ty}")))?;
            Value::Typed(
                std::sync::Arc::new(json_to_typed_value(env, value, selected)?),
                ty.clone(),
            )
        }
        MagType::Product(components) => {
            let values = value
                .as_array()
                .ok_or_else(|| MagError::Type(format!("expected ordered tuple {ty}")))?;
            if values.len() != components.len() {
                return Err(MagError::Type(format!(
                    "expected {} tuple positions for {ty}, got {}",
                    components.len(),
                    values.len()
                )));
            }
            Value::Product(std::sync::Arc::new(
                values
                    .iter()
                    .zip(components)
                    .map(|(position, component)| json_to_typed_value(env, position, component))
                    .collect::<Result<_, _>>()?,
            ))
        }
        MagType::Data => json_to_value(value),
        MagType::Unit | MagType::Bool | MagType::Int | MagType::Float | MagType::String => {
            json_to_value(value)
        }
        unsupported => {
            return Err(MagError::Type(format!(
                "{unsupported} is not representable as rule input JSON"
            )))
        }
    })
}
