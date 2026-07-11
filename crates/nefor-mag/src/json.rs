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
        Value::TypeTag(ty) => Ok(ty.to_string().into()),
        Value::List(v) | Value::Vector(v) => Ok(serde_json::Value::Array(
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
        serde_json::Value::Array(v) => Value::Vector(v.iter().map(json_to_value).collect()),
        serde_json::Value::Object(v) => Value::Map(
            v.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
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
                Box::new(json_to_typed_value(env, value, &body)?),
                ty.clone(),
            )
        }
        MagType::List(item) => Value::Vector(
            value
                .as_array()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?
                .iter()
                .map(|entry| json_to_typed_value(env, entry, item))
                .collect::<Result<_, _>>()?,
        ),
        MagType::Map(_, item) => Value::Map(
            value
                .as_object()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?
                .iter()
                .map(|(key, entry)| Ok((key.clone(), json_to_typed_value(env, entry, item)?)))
                .collect::<Result<BTreeMap<_, _>, MagError>>()?,
        ),
        MagType::Record(fields) => {
            let object = value
                .as_object()
                .ok_or_else(|| MagError::Type(format!("expected {ty}")))?;
            Value::Map(
                fields
                    .iter()
                    .map(|(key, field_type)| {
                        let field = object.get(key).ok_or_else(|| {
                            MagError::Type(format!("missing field {key} for {ty}"))
                        })?;
                        Ok((key.clone(), json_to_typed_value(env, field, field_type)?))
                    })
                    .collect::<Result<BTreeMap<_, _>, MagError>>()?,
            )
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
                Box::new(json_to_typed_value(env, value, selected)?),
                ty.clone(),
            )
        }
        MagType::Product(_) => Value::Typed(Box::new(json_to_value(value)), ty.clone()),
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
