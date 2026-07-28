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

pub fn concrete_type_to_json(
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
            name,
            arguments,
            body,
        } => serde_json::json!({
            "kind": "named",
            "name": name,
            "arguments": arguments.iter().map(concrete_type_to_json).collect::<Result<Vec<_>, _>>()?,
            "body": concrete_type_to_json(body)?,
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

/// Decode the canonical descriptor representation emitted into MAG artifacts.
/// Runtime callers use this before applying the compiler's compatibility
/// relation, so arbitrary Lua tables never become semantic authority.
pub fn concrete_type_from_json(
    value: &serde_json::Value,
) -> Result<crate::types::ConcreteType, MagError> {
    use crate::types::ConcreteType;

    let object = value
        .as_object()
        .ok_or_else(|| MagError::Type("semantic descriptor must be an object".into()))?;
    let kind = string_field(object, "kind")?;
    let descriptor = match kind {
        "primitive" => match string_field(object, "name")? {
            "JsonValue" => ConcreteType::JsonValue,
            "Unit" => ConcreteType::Unit,
            "Bool" => ConcreteType::Bool,
            "Int" => ConcreteType::Int,
            "Float" => ConcreteType::Float,
            "String" => ConcreteType::String,
            name => {
                return Err(MagError::Type(format!(
                    "unknown semantic primitive {name:?}"
                )))
            }
        },
        "named" => ConcreteType::Named {
            name: string_field(object, "name")?.to_owned(),
            arguments: descriptor_list(object, "arguments")?,
            // Compiler artifacts include the body. Separately-owned Lua
            // declarations and direct kernel fixtures name nominal
            // constructors without embedding MAG definitions; those nodes
            // are usable for nominal compatibility but not stable identity.
            body: Box::new(match object.get("body") {
                Some(body) => concrete_type_from_json(body)?,
                None => ConcreteType::Unit,
            }),
        },
        "list" => ConcreteType::List {
            item: Box::new(concrete_type_from_json(object.get("item").ok_or_else(
                || MagError::Type("list semantic descriptor needs item".into()),
            )?)?),
        },
        "map" => ConcreteType::Map {
            key: Box::new(concrete_type_from_json(object.get("key").ok_or_else(
                || MagError::Type("map semantic descriptor needs key".into()),
            )?)?),
            value: Box::new(concrete_type_from_json(object.get("value").ok_or_else(
                || MagError::Type("map semantic descriptor needs value".into()),
            )?)?),
        },
        "record" => {
            let fields = object
                .get("fields")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| MagError::Type("record semantic descriptor needs fields".into()))?;
            let mut decoded = BTreeMap::new();
            let mut previous: Option<&str> = None;
            for field in fields {
                let field = field.as_object().ok_or_else(|| {
                    MagError::Type("record descriptor field must be an object".into())
                })?;
                let name = string_field(field, "name")?;
                if previous.is_some_and(|previous| previous >= name) {
                    return Err(MagError::Type(
                        "record descriptor fields must be uniquely sorted".into(),
                    ));
                }
                previous = Some(name);
                let ty =
                    concrete_type_from_json(field.get("type").ok_or_else(|| {
                        MagError::Type("record descriptor field needs type".into())
                    })?)?;
                decoded.insert(name.to_owned(), ty);
            }
            ConcreteType::Record { fields: decoded }
        }
        "union" => {
            let arms = descriptor_list(object, "items")?;
            if arms.len() < 2
                || arms
                    != arms
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
            {
                return Err(MagError::Type(
                    "sum descriptor arms must be canonical, unique, and sorted".into(),
                ));
            }
            ConcreteType::Sum { arms }
        }
        "product" => {
            let items = descriptor_list(object, "items")?;
            if items.len() < 2 {
                return Err(MagError::Type(
                    "product semantic descriptor needs at least two items".into(),
                ));
            }
            ConcreteType::Product { items }
        }
        other => {
            return Err(MagError::Type(format!(
                "unknown semantic descriptor kind {other:?}"
            )))
        }
    };
    let canonical = concrete_type_to_json(&descriptor)?;
    if canonical != fill_missing_named_bodies(value) {
        return Err(MagError::Type(
            "semantic descriptor is not in canonical compiler form".into(),
        ));
    }
    Ok(descriptor)
}

fn fill_missing_named_bodies(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(fill_missing_named_bodies).collect())
        }
        serde_json::Value::Object(object) => {
            let mut object = object
                .iter()
                .map(|(key, value)| (key.clone(), fill_missing_named_bodies(value)))
                .collect::<serde_json::Map<_, _>>();
            if object.get("kind").and_then(serde_json::Value::as_str) == Some("named")
                && !object.contains_key("body")
            {
                object.insert(
                    "body".into(),
                    serde_json::json!({"kind":"primitive","name":"Unit"}),
                );
            }
            serde_json::Value::Object(object)
        }
        scalar => scalar.clone(),
    }
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a str, MagError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MagError::Type(format!("semantic descriptor needs string {field}")))
}

fn descriptor_list(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Vec<crate::types::ConcreteType>, MagError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| MagError::Type(format!("semantic descriptor needs list {field}")))?
        .iter()
        .map(concrete_type_from_json)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConcreteType;

    #[test]
    fn canonical_semantic_descriptors_round_trip_and_reject_forged_shape() {
        let descriptor = ConcreteType::Named {
            name: "main.Payload".into(),
            arguments: vec![],
            body: Box::new(ConcreteType::Record {
                fields: BTreeMap::from([("value".into(), ConcreteType::Int)]),
            }),
        };
        let encoded = concrete_type_to_json(&descriptor).unwrap();
        assert_eq!(concrete_type_from_json(&encoded).unwrap(), descriptor);

        let mut forged = encoded;
        forged
            .as_object_mut()
            .unwrap()
            .insert("wire".into(), serde_json::json!("not-semantic"));
        assert!(concrete_type_from_json(&forged).is_err());
    }
}
