use crate::ast::Value;
use crate::checker::substitute;
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use std::collections::{BTreeMap, HashMap};

pub fn value_to_json(env: &Env, value: &Value) -> Result<serde_json::Value, MagError> {
    match value {
        Value::Unit => Ok(serde_json::Value::Null),
        Value::Str(v) => Ok(v.clone().into()),
        Value::Int(v) => Ok((*v).into()),
        Value::Float(v) => Ok(serde_json::json!(v)),
        Value::Bool(v) => Ok((*v).into()),
        Value::Keyword(v) => Ok(format!(":{v}").into()),
        Value::Symbol(v) => Ok(v.clone().into()),
        Value::TypeTag(ty) => concrete_type_to_json(ty),
        Value::TypeDescriptor(ty) => concrete_type_to_json(ty),
        Value::TypeSchema(schema) => serde_json::to_value(schema)
            .map_err(|e| MagError::Eval(format!("serialize type schema: {e}"))),
        Value::SemanticTypeId(id) => Ok(id.as_str().into()),
        Value::PackedValue(value) => value_to_json(env, value),
        Value::JsonValue(value) => Ok(value.clone()),
        Value::HostInputs(_) => Err(MagError::Eval(
            "compiler host inputs cannot enter an artifact".into(),
        )),
        Value::ForeignEvidence(evidence) => Ok(serde_json::json!({
            "version": 2,
            "identity": evidence.identity,
            "arguments": evidence.arguments.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
            "input": concrete_type_to_json(&evidence.input)?,
            "output": concrete_type_to_json(&evidence.output)?,
        })),
        Value::List(v) | Value::Vector(v) | Value::Product(v) => Ok(serde_json::Value::Array(
            v.iter()
                .map(|value| value_to_json(env, value))
                .collect::<Result<_, _>>()?,
        )),
        Value::Map(v) => Ok(serde_json::Value::Object(
            v.iter()
                .map(|(key, value)| Ok((key.clone(), value_to_json(env, value)?)))
                .collect::<Result<_, MagError>>()?,
        )),
        Value::Artifact(v) => {
            serde_json::to_value(v).map_err(|e| MagError::Eval(format!("serialize artifact: {e}")))
        }
        Value::Typed(value, MagType::Union(_)) => {
            let (selected, payload) = selected_sum_payload(value).ok_or_else(|| {
                MagError::Eval(
                    "cannot serialize a sum without selected constructor evidence".into(),
                )
            })?;
            let tag = crate::types::ConcreteType::resolve(env, selected)?.stable_id();
            Ok(serde_json::json!({
                "type": tag.as_str(),
                "value": value_to_json(env, payload)?,
            }))
        }
        Value::Typed(value, _) => value_to_json(env, value),
        other => Err(MagError::Eval(format!(
            "cannot serialize {} to JSON",
            other.type_name()
        ))),
    }
}

fn selected_sum_payload(value: &Value) -> Option<(&MagType, &Value)> {
    match value {
        Value::Typed(inner, selected @ MagType::Named(_, _)) => Some((selected, inner)),
        Value::Typed(inner, MagType::Union(_)) => selected_sum_payload(inner),
        _ => None,
    }
}

pub(crate) fn concrete_type_to_json(
    ty: &crate::types::ConcreteType,
) -> Result<serde_json::Value, MagError> {
    use crate::types::ConcreteType;
    let primitive = |name: &str| serde_json::json!({ "kind": "primitive", "name": name });
    Ok(match ty {
        ConcreteType::JsonValue => primitive("JsonValue"),
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
        MagType::Union(_) => {
            let object = value
                .as_object()
                .ok_or_else(|| MagError::Type(format!("expected tagged sum envelope for {ty}")))?;
            if object.len() != 2 || !object.contains_key("type") || !object.contains_key("value") {
                return Err(MagError::Type(format!(
                    "expected exact tagged sum envelope {{type, value}} for {ty}"
                )));
            }
            let tag = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    MagError::Type(format!("sum constructor tag for {ty} must be a string"))
                })?;
            let accepted = crate::types::ConcreteType::resolve(env, ty)?;
            let crate::types::ConcreteType::Sum { arms } = accepted else {
                return Err(MagError::Type(format!("{ty} did not normalize to a sum")));
            };
            let selected = arms
                .iter()
                .find(|arm| arm.stable_id().as_str() == tag)
                .ok_or_else(|| {
                    MagError::Type(format!("constructor {tag} is not a member of {ty}"))
                })?;
            let selected_type = selected.to_mag_type();
            let payload = object
                .get("value")
                .ok_or_else(|| MagError::Type(format!("sum envelope for {ty} needs value")))?;
            Value::Typed(
                std::sync::Arc::new(json_to_typed_value(env, payload, &selected_type)?),
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
        MagType::JsonValue => Value::JsonValue(value.clone()),
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
