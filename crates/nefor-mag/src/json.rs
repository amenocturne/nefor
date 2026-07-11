use crate::ast::Value;
use crate::error::MagError;

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
