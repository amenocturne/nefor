use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const SCHEMA_VERSION: u32 = 1;

/// A stable, data-only description of the JSON-representable subset of MAG.
/// It deliberately contains no runtime behavior and can cross the MAG/Lua
/// boundary as ordinary artifact data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeSchema {
    pub version: u32,
    pub root: SchemaType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSchema {
    pub schema: Value,
    pub wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaType {
    JsonValue,
    Unit,
    Bool,
    Int,
    Float,
    String,
    List { item: Box<SchemaType> },
    Map { value: Box<SchemaType> },
    Record { fields: Vec<SchemaField> },
    Union { variants: Vec<SchemaVariant> },
    Product { components: Vec<SchemaType> },
    Named { name: String, body: Box<SchemaType> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaField {
    pub name: String,
    pub schema: SchemaType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaVariant {
    pub tag: String,
    pub schema: SchemaType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub path: String,
    pub code: String,
    pub expected: String,
    pub actual: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonValidation {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonValidationError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonValidationError {
    pub kind: String,
    pub message: String,
}

impl TypeSchema {
    pub fn reify(env: &Env, ty: &MagType) -> Result<Self, MagError> {
        Self::from_concrete(&crate::types::ConcreteType::resolve(env, ty)?)
    }

    pub fn from_concrete(ty: &crate::types::ConcreteType) -> Result<Self, MagError> {
        Ok(Self {
            version: SCHEMA_VERSION,
            root: reify_concrete(ty)?,
        })
    }

    pub fn validate_json(&self, source: &str) -> JsonValidation {
        if self.version != SCHEMA_VERSION {
            return JsonValidation {
                ok: false,
                value: None,
                error: Some(JsonValidationError {
                    kind: "unsupported_schema_version".into(),
                    message: format!(
                        "schema version {} is unsupported; expected {}",
                        self.version, SCHEMA_VERSION
                    ),
                }),
                violations: vec![],
            };
        }
        let value: Value = match serde_json::from_str(source) {
            Ok(value) => value,
            Err(error) => {
                return JsonValidation {
                    ok: false,
                    value: None,
                    error: Some(JsonValidationError {
                        kind: "malformed_json".into(),
                        message: error.to_string(),
                    }),
                    violations: vec![],
                };
            }
        };
        self.validate_value(value)
    }

    pub fn validate_value(&self, value: Value) -> JsonValidation {
        if self.version != SCHEMA_VERSION {
            return JsonValidation {
                ok: false,
                value: None,
                error: Some(JsonValidationError {
                    kind: "unsupported_schema_version".into(),
                    message: format!(
                        "schema version {} is unsupported; expected {}",
                        self.version, SCHEMA_VERSION
                    ),
                }),
                violations: vec![],
            };
        }
        let mut violations = vec![];
        validate_at(&self.root, &value, "$", &mut violations);
        JsonValidation {
            ok: violations.is_empty(),
            value: violations.is_empty().then_some(value),
            error: None,
            violations,
        }
    }

    /// Convert the compiler-owned descriptor into provider-neutral JSON Schema.
    pub fn to_json_schema(&self) -> Value {
        schema_type_to_json_schema(&self.root)
    }

    /// Lower the semantic type into OpenAI's strict Structured Outputs subset.
    /// Representations which differ from MAG JSON are decoded again by
    /// `validate_provider_json`; types without a faithful representation fail
    /// before a provider request is made.
    pub fn to_provider_schema(&self) -> Result<ProviderSchema, MagError> {
        let schema = provider_schema_at(&self.root)?;
        if schema_root_is_record(&self.root) {
            Ok(ProviderSchema {
                schema,
                wrapped: false,
            })
        } else {
            Ok(ProviderSchema {
                schema: serde_json::json!({
                    "type": "object",
                    "properties": { "value": schema },
                    "required": ["value"],
                    "additionalProperties": false,
                }),
                wrapped: true,
            })
        }
    }

    pub fn validate_provider_json(&self, source: &str) -> JsonValidation {
        let value: Value = match serde_json::from_str(source) {
            Ok(value) => value,
            Err(error) => return validation_error("malformed_json", error.to_string()),
        };
        let provider = match self.to_provider_schema() {
            Ok(provider) => provider,
            Err(error) => {
                return validation_error("unsupported_provider_schema", error.to_string())
            }
        };
        let encoded = if provider.wrapped {
            match value {
                Value::Object(mut fields) if fields.len() == 1 && fields.contains_key("value") => {
                    fields.remove("value").unwrap_or(Value::Null)
                }
                value => {
                    return JsonValidation {
                        ok: false,
                        value: None,
                        error: None,
                        violations: vec![Violation {
                            path: "$".into(),
                            code: "invalid_provider_envelope".into(),
                            expected: "object containing only required field 'value'".into(),
                            actual: json_kind(&value).into(),
                            message: "provider response did not match the structured-output root envelope".into(),
                        }],
                    };
                }
            }
        } else {
            value
        };
        match decode_provider_value(&self.root, encoded, "$") {
            Ok(value) => self.validate_value(value),
            Err(violation) => JsonValidation {
                ok: false,
                value: None,
                error: None,
                violations: vec![violation],
            },
        }
    }
}

fn validation_error(kind: &str, message: String) -> JsonValidation {
    JsonValidation {
        ok: false,
        value: None,
        error: Some(JsonValidationError {
            kind: kind.into(),
            message,
        }),
        violations: vec![],
    }
}

fn schema_root_is_record(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Record { .. } => true,
        SchemaType::Named { body, .. } => schema_root_is_record(body),
        _ => false,
    }
}

fn provider_schema_at(schema: &SchemaType) -> Result<Value, MagError> {
    Ok(match schema {
        SchemaType::JsonValue => {
            return Err(MagError::Type(
                "JsonValue has no faithful OpenAI strict structured-output representation".into(),
            ));
        }
        SchemaType::Unit => serde_json::json!({"type": "null"}),
        SchemaType::Bool => serde_json::json!({"type": "boolean"}),
        SchemaType::Int => serde_json::json!({
            "type": "integer",
            "minimum": i64::MIN,
            "maximum": i64::MAX,
        }),
        SchemaType::Float => serde_json::json!({
            "type": "number",
            "minimum": f64::MIN,
            "maximum": f64::MAX,
        }),
        SchemaType::String => serde_json::json!({"type": "string"}),
        SchemaType::List { item } => serde_json::json!({
            "type": "array",
            "items": provider_schema_at(item)?,
        }),
        SchemaType::Map { value } => serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "value": provider_schema_at(value)?,
                },
                "required": ["key", "value"],
                "additionalProperties": false,
            },
        }),
        SchemaType::Record { fields } => {
            let properties = fields
                .iter()
                .map(|field| Ok((field.name.clone(), provider_schema_at(&field.schema)?)))
                .collect::<Result<serde_json::Map<_, _>, MagError>>()?;
            let required = fields
                .iter()
                .map(|field| Value::String(field.name.clone()))
                .collect::<Vec<_>>();
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        }
        SchemaType::Union { variants } => serde_json::json!({
            "anyOf": variants.iter().map(|variant| Ok(serde_json::json!({
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": [variant.tag.clone()] },
                    "value": provider_schema_at(&variant.schema)?,
                },
                "required": ["type", "value"],
                "additionalProperties": false,
            }))).collect::<Result<Vec<_>, MagError>>()?,
        }),
        SchemaType::Product { components } => {
            let properties = components
                .iter()
                .enumerate()
                .map(|(index, component)| Ok((index.to_string(), provider_schema_at(component)?)))
                .collect::<Result<serde_json::Map<_, _>, MagError>>()?;
            let required = (0..components.len())
                .map(|index| Value::String(index.to_string()))
                .collect::<Vec<_>>();
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        }
        SchemaType::Named { name, body } => {
            let mut value = provider_schema_at(body)?;
            if let Some(object) = value.as_object_mut() {
                object.insert("title".into(), Value::String(name.clone()));
            }
            value
        }
    })
}

fn decode_provider_value(
    schema: &SchemaType,
    value: Value,
    path: &str,
) -> Result<Value, Violation> {
    match schema {
        SchemaType::Named { body, .. } => decode_provider_value(body, value, path),
        SchemaType::Map { value: item } => {
            let Value::Array(entries) = value else {
                return Err(provider_decode_violation(
                    path,
                    "array of map entries",
                    &value,
                ));
            };
            let mut decoded = serde_json::Map::new();
            for (index, entry) in entries.into_iter().enumerate() {
                let entry_path = format!("{path}[{index}]");
                let Value::Object(mut fields) = entry else {
                    return Err(provider_decode_violation(
                        &entry_path,
                        "map entry object",
                        &entry,
                    ));
                };
                if fields.len() != 2 || !fields.contains_key("key") || !fields.contains_key("value")
                {
                    return Err(provider_decode_violation(
                        &entry_path,
                        "object containing only key and value",
                        &Value::Object(fields),
                    ));
                }
                let key_value = fields.remove("key").unwrap_or(Value::Null);
                let Some(key) = key_value.as_str() else {
                    return Err(provider_decode_violation(
                        &format!("{entry_path}.key"),
                        "string",
                        &key_value,
                    ));
                };
                if decoded.contains_key(key) {
                    return Err(Violation {
                        path: format!("{entry_path}.key"),
                        code: "duplicate_map_key".into(),
                        expected: "unique map key".into(),
                        actual: key.into(),
                        message: format!("map key '{key}' appears more than once"),
                    });
                }
                let item_value = fields.remove("value").unwrap_or(Value::Null);
                decoded.insert(
                    key.into(),
                    decode_provider_value(item, item_value, &field_path(path, key))?,
                );
            }
            Ok(Value::Object(decoded))
        }
        SchemaType::Product { components } => {
            let Value::Object(mut fields) = value else {
                return Err(provider_decode_violation(path, "positional object", &value));
            };
            let mut decoded = Vec::with_capacity(components.len());
            for (index, component) in components.iter().enumerate() {
                let key = index.to_string();
                let position = fields.remove(&key).ok_or_else(|| {
                    provider_decode_violation(
                        path,
                        "complete positional object",
                        &Value::Object(fields.clone()),
                    )
                })?;
                decoded.push(decode_provider_value(
                    component,
                    position,
                    &format!("{path}.{key}"),
                )?);
            }
            if !fields.is_empty() {
                return Err(provider_decode_violation(
                    path,
                    "exact positional object",
                    &Value::Object(fields),
                ));
            }
            Ok(Value::Array(decoded))
        }
        SchemaType::Record { fields } => {
            let Value::Object(mut values) = value else {
                return Err(provider_decode_violation(path, "object", &value));
            };
            for field in fields {
                if let Some(field_value) = values.remove(&field.name) {
                    values.insert(
                        field.name.clone(),
                        decode_provider_value(
                            &field.schema,
                            field_value,
                            &field_path(path, &field.name),
                        )?,
                    );
                }
            }
            Ok(Value::Object(values))
        }
        SchemaType::List { item } => {
            let Value::Array(values) = value else {
                return Err(provider_decode_violation(path, "array", &value));
            };
            values
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    decode_provider_value(item, value, &format!("{path}[{index}]"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        SchemaType::Int => match value {
            Value::Number(number) if number.as_i64().is_some() => Ok(Value::Number(number)),
            Value::Number(number) => crate::json::json_number_to_i64(&number)
                .map(|number| Value::Number(serde_json::Number::from(number)))
                .ok_or_else(|| provider_decode_violation(path, "Int", &Value::Number(number))),
            value => Err(provider_decode_violation(path, "Int", &value)),
        },
        SchemaType::Union { variants } => {
            let Value::Object(mut fields) = value else {
                return Err(provider_decode_violation(path, "tagged sum object", &value));
            };
            let tag = fields
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    provider_decode_violation(
                        path,
                        "tagged sum object",
                        &Value::Object(fields.clone()),
                    )
                })?
                .to_owned();
            if let Some(variant) = variants.iter().find(|variant| variant.tag == tag) {
                if let Some(payload) = fields.remove("value") {
                    fields.insert(
                        "value".into(),
                        decode_provider_value(
                            &variant.schema,
                            payload,
                            &field_path(path, "value"),
                        )?,
                    );
                }
            }
            Ok(Value::Object(fields))
        }
        _ => Ok(value),
    }
}

fn provider_decode_violation(path: &str, expected: &str, actual: &Value) -> Violation {
    Violation {
        path: path.into(),
        code: "invalid_provider_encoding".into(),
        expected: expected.into(),
        actual: json_kind(actual).into(),
        message: format!("provider response did not use the required {expected} encoding"),
    }
}

fn schema_type_to_json_schema(schema: &SchemaType) -> Value {
    match schema {
        SchemaType::JsonValue => serde_json::json!({}),
        SchemaType::Unit => serde_json::json!({"type": "null"}),
        SchemaType::Bool => serde_json::json!({"type": "boolean"}),
        SchemaType::Int => serde_json::json!({"type": "integer"}),
        SchemaType::Float => serde_json::json!({"type": "number"}),
        SchemaType::String => serde_json::json!({"type": "string"}),
        SchemaType::List { item } => serde_json::json!({
            "type": "array",
            "items": schema_type_to_json_schema(item),
        }),
        SchemaType::Map { value } => serde_json::json!({
            "type": "object",
            "additionalProperties": schema_type_to_json_schema(value),
        }),
        SchemaType::Record { fields } => {
            let properties = fields
                .iter()
                .map(|field| {
                    (
                        field.name.clone(),
                        schema_type_to_json_schema(&field.schema),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let required = fields
                .iter()
                .map(|field| Value::String(field.name.clone()))
                .collect::<Vec<_>>();
            serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        }
        SchemaType::Union { variants } => serde_json::json!({
            "oneOf": variants.iter().map(|variant| serde_json::json!({
                "type": "object",
                "properties": {
                    "type": { "const": variant.tag },
                    "value": schema_type_to_json_schema(&variant.schema),
                },
                "required": ["type", "value"],
                "additionalProperties": false,
            })).collect::<Vec<_>>(),
        }),
        SchemaType::Product { components } => serde_json::json!({
            "type": "array",
            "prefixItems": components.iter().map(schema_type_to_json_schema).collect::<Vec<_>>(),
            "minItems": components.len(),
            "maxItems": components.len(),
        }),
        SchemaType::Named { name, body } => {
            let mut value = schema_type_to_json_schema(body);
            if let Some(object) = value.as_object_mut() {
                object.insert("title".into(), Value::String(name.clone()));
            }
            value
        }
    }
}

fn reify_concrete(ty: &crate::types::ConcreteType) -> Result<SchemaType, MagError> {
    use crate::types::ConcreteType;
    Ok(match ty {
        ConcreteType::JsonValue => SchemaType::JsonValue,
        ConcreteType::Unit => SchemaType::Unit,
        ConcreteType::Bool => SchemaType::Bool,
        ConcreteType::Int => SchemaType::Int,
        ConcreteType::Float => SchemaType::Float,
        ConcreteType::String => SchemaType::String,
        ConcreteType::List { item } => SchemaType::List {
            item: Box::new(reify_concrete(item)?),
        },
        ConcreteType::Map { key, value } if key.as_ref() == &ConcreteType::String => {
            SchemaType::Map {
                value: Box::new(reify_concrete(value)?),
            }
        }
        ConcreteType::Map { key, .. } => {
            return Err(MagError::Type(format!(
                "cannot reify Map<{key:?}, _>: JSON object keys must be String"
            )))
        }
        ConcreteType::Record { fields } => SchemaType::Record {
            fields: fields
                .iter()
                .map(|(name, ty)| {
                    Ok(SchemaField {
                        name: name.clone(),
                        schema: reify_concrete(ty)?,
                    })
                })
                .collect::<Result<_, MagError>>()?,
        },
        ConcreteType::Sum { arms } => SchemaType::Union {
            variants: arms
                .iter()
                .map(|arm| {
                    ensure_nominal_constructor(arm)?;
                    Ok(SchemaVariant {
                        tag: arm.stable_id().to_string(),
                        schema: reify_concrete(arm)?,
                    })
                })
                .collect::<Result<_, MagError>>()?,
        },
        ConcreteType::Product { items } => SchemaType::Product {
            components: items.iter().map(reify_concrete).collect::<Result<_, _>>()?,
        },
        ConcreteType::Named { name, body, .. } => SchemaType::Named {
            name: name.clone(),
            body: Box::new(reify_concrete(body)?),
        },
    })
}

fn ensure_nominal_constructor(ty: &crate::types::ConcreteType) -> Result<(), MagError> {
    match ty {
        crate::types::ConcreteType::Named { .. } => Ok(()),
        other => Err(MagError::Type(format!(
            "sum arm {other:?} has no stable nominal constructor identity"
        ))),
    }
}

fn validate_at(schema: &SchemaType, value: &Value, path: &str, out: &mut Vec<Violation>) {
    match schema {
        SchemaType::Named { name, body } => {
            let start = out.len();
            validate_at(body, value, path, out);
            for violation in &mut out[start..] {
                if violation.path == path && violation.code == "wrong_type" {
                    violation.expected = name.clone();
                    violation.message = format!("expected {name}, got {}", violation.actual);
                }
            }
        }
        SchemaType::JsonValue => {}
        SchemaType::Unit => expect(value.is_null(), path, "Unit", value, out),
        SchemaType::Bool => expect(value.is_boolean(), path, "Bool", value, out),
        SchemaType::Int => expect(value.as_i64().is_some(), path, "Int", value, out),
        SchemaType::Float => expect(value.as_f64().is_some(), path, "Float", value, out),
        SchemaType::String => expect(value.is_string(), path, "String", value, out),
        SchemaType::List { item } => match value {
            Value::Array(items) => {
                for (index, item_value) in items.iter().enumerate() {
                    validate_at(item, item_value, &format!("{path}[{index}]"), out);
                }
            }
            _ => expect(false, path, "List", value, out),
        },
        SchemaType::Map { value: item } => match value {
            Value::Object(entries) => {
                for (key, item_value) in entries {
                    validate_at(item, item_value, &field_path(path, key), out);
                }
            }
            _ => expect(false, path, "Map String", value, out),
        },
        SchemaType::Record { fields } => match value {
            Value::Object(entries) => {
                let declared = fields
                    .iter()
                    .map(|field| field.name.as_str())
                    .collect::<HashSet<_>>();
                for field in fields {
                    let child_path = field_path(path, &field.name);
                    match entries.get(&field.name) {
                        Some(field_value) => {
                            validate_at(&field.schema, field_value, &child_path, out)
                        }
                        None => out.push(Violation {
                            path: child_path,
                            code: "missing_field".into(),
                            expected: describe(&field.schema),
                            actual: "missing".into(),
                            message: format!("required field '{}' is missing", field.name),
                        }),
                    }
                }
                for (key, extra) in entries {
                    if !declared.contains(key.as_str()) {
                        out.push(Violation {
                            path: field_path(path, key),
                            code: "extra_field".into(),
                            expected: "no additional field".into(),
                            actual: json_kind(extra).into(),
                            message: format!("field '{key}' is not declared"),
                        });
                    }
                }
            }
            _ => expect(false, path, "record", value, out),
        },
        SchemaType::Union { variants } => match value {
            Value::Object(entries) => {
                for (key, extra) in entries {
                    if key != "type" && key != "value" {
                        out.push(Violation {
                            path: field_path(path, key),
                            code: "extra_field".into(),
                            expected: "no additional field".into(),
                            actual: json_kind(extra).into(),
                            message: format!("field '{key}' is not declared"),
                        });
                    }
                }
                let Some(tag) = entries.get("type") else {
                    out.push(Violation {
                        path: field_path(path, "type"),
                        code: "missing_field".into(),
                        expected: "constructor tag".into(),
                        actual: "missing".into(),
                        message: "required field 'type' is missing".into(),
                    });
                    return;
                };
                let Some(tag) = tag.as_str() else {
                    expect(
                        false,
                        &field_path(path, "type"),
                        "constructor tag",
                        tag,
                        out,
                    );
                    return;
                };
                let Some(variant) = variants.iter().find(|variant| variant.tag == tag) else {
                    out.push(Violation {
                        path: field_path(path, "type"),
                        code: "unknown_union_tag".into(),
                        expected: variants
                            .iter()
                            .map(|variant| variant.tag.as_str())
                            .collect::<Vec<_>>()
                            .join(" | "),
                        actual: tag.into(),
                        message: format!("constructor '{tag}' is not a member of this sum"),
                    });
                    return;
                };
                match entries.get("value") {
                    Some(payload) => {
                        validate_at(&variant.schema, payload, &field_path(path, "value"), out)
                    }
                    None => out.push(Violation {
                        path: field_path(path, "value"),
                        code: "missing_field".into(),
                        expected: describe(&variant.schema),
                        actual: "missing".into(),
                        message: "required field 'value' is missing".into(),
                    }),
                }
            }
            _ => expect(false, path, "tagged sum envelope", value, out),
        },
        SchemaType::Product { components } => match value {
            Value::Array(values) => {
                if values.len() != components.len() {
                    out.push(Violation {
                        path: path.into(),
                        code: "wrong_arity".into(),
                        expected: format!("{} tuple positions", components.len()),
                        actual: format!("{} tuple positions", values.len()),
                        message: format!(
                            "expected {} tuple positions, got {}",
                            components.len(),
                            values.len()
                        ),
                    });
                }
                for (index, (component, position)) in components.iter().zip(values).enumerate() {
                    validate_at(component, position, &format!("{path}[{index}]"), out);
                }
            }
            _ => expect(false, path, "array", value, out),
        },
    }
}

fn expect(valid: bool, path: &str, expected: &str, value: &Value, out: &mut Vec<Violation>) {
    if !valid {
        let actual = json_kind(value);
        out.push(Violation {
            path: path.into(),
            code: "wrong_type".into(),
            expected: expected.into(),
            actual: actual.into(),
            message: format!("expected {expected}, got {actual}"),
        });
    }
}

fn describe(schema: &SchemaType) -> String {
    match schema {
        SchemaType::JsonValue => "JsonValue".into(),
        SchemaType::Unit => "Unit".into(),
        SchemaType::Bool => "Bool".into(),
        SchemaType::Int => "Int".into(),
        SchemaType::Float => "Float".into(),
        SchemaType::String => "String".into(),
        SchemaType::List { item } => format!("List<{}>", describe(item)),
        SchemaType::Map { value } => format!("Map<String, {}>", describe(value)),
        SchemaType::Record { .. } => "record".into(),
        SchemaType::Union { variants } => variants
            .iter()
            .map(|variant| variant.tag.clone())
            .collect::<Vec<_>>()
            .join(" | "),
        SchemaType::Product { components } => format!(
            "({})",
            components
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        SchemaType::Named { name, .. } => name.clone(),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn field_path(parent: &str, field: &str) -> String {
    if field
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        format!("{parent}.{field}")
    } else {
        format!(
            "{parent}[{}]",
            serde_json::to_string(field).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_nested_validation_reports_precise_paths() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Record {
                fields: vec![SchemaField {
                    name: "tasks".into(),
                    schema: SchemaType::List {
                        item: Box::new(SchemaType::Record {
                            fields: vec![SchemaField {
                                name: "description".into(),
                                schema: SchemaType::String,
                            }],
                        }),
                    },
                }],
            },
        };
        let result = schema.validate_json(r#"{"tasks":[{"description":4,"extra":true}]}"#);
        assert!(!result.ok);
        assert!(result
            .violations
            .iter()
            .any(|v| v.path == "$.tasks[0].description"));
        assert!(result
            .violations
            .iter()
            .any(|v| v.path == "$.tasks[0].extra"));
    }

    #[test]
    fn fenced_json_is_not_bare_json() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::String,
        };
        let result = schema.validate_json("```json\n\"x\"\n```");
        assert_eq!(result.error.unwrap().kind, "malformed_json");
    }

    #[test]
    fn validates_maps_unions_and_missing_fields() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Record {
                fields: vec![
                    SchemaField {
                        name: "labels".into(),
                        schema: SchemaType::Map {
                            value: Box::new(SchemaType::Int),
                        },
                    },
                    SchemaField {
                        name: "choice".into(),
                        schema: SchemaType::Union {
                            variants: vec![
                                SchemaVariant {
                                    tag: "sha256:text".into(),
                                    schema: SchemaType::String,
                                },
                                SchemaVariant {
                                    tag: "sha256:flag".into(),
                                    schema: SchemaType::Bool,
                                },
                            ],
                        },
                    },
                ],
            },
        };
        assert!(
            schema
                .validate_json(r#"{"labels":{"a":1},"choice":{"type":"sha256:flag","value":true}}"#)
                .ok
        );
        let invalid = schema.validate_json(r#"{"labels":{"a":"one"}}"#);
        assert!(invalid.violations.iter().any(|v| v.path == "$.labels.a"));
        assert!(invalid
            .violations
            .iter()
            .any(|v| v.path == "$.choice" && v.code == "missing_field"));
    }

    #[test]
    fn unresolved_variables_cannot_cross_the_schema_boundary() {
        let error = TypeSchema::reify(&Env::new(), &MagType::Var("T".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unresolved type variable T"), "{error}");
    }

    #[test]
    fn provider_json_schema_preserves_records_lists_and_nominal_identity() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Named {
                name: "TaskPlan".into(),
                body: Box::new(SchemaType::Record {
                    fields: vec![SchemaField {
                        name: "steps".into(),
                        schema: SchemaType::List {
                            item: Box::new(SchemaType::String),
                        },
                    }],
                }),
            },
        };
        let json = schema.to_json_schema();
        assert_eq!(json["title"], "TaskPlan");
        assert_eq!(json["properties"]["steps"]["type"], "array");
        assert_eq!(json["properties"]["steps"]["items"]["type"], "string");
        assert_eq!(json["required"], serde_json::json!(["steps"]));
        assert_eq!(json["additionalProperties"], false);
    }

    #[test]
    fn unit_empty_collections_and_float_are_strict() {
        let unit = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Unit,
        };
        assert!(unit.validate_json("null").ok);
        assert!(!unit.validate_json("false").ok);

        let empty_record = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Record { fields: vec![] },
        };
        assert!(empty_record.validate_json("{}").ok);
        assert!(!empty_record.validate_json(r#"{"extra":1}"#).ok);

        let list = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::List {
                item: Box::new(SchemaType::String),
            },
        };
        assert!(list.validate_json("[]").ok);

        let float = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Float,
        };
        assert!(float.validate_json("1.5").ok);
        assert!(float.validate_json("1").ok);
    }

    #[test]
    fn tagged_sums_require_exact_envelopes_and_selected_payloads() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Union {
                variants: vec![
                    SchemaVariant {
                        tag: "sha256:x".into(),
                        schema: SchemaType::Record {
                            fields: vec![SchemaField {
                                name: "value".into(),
                                schema: SchemaType::Int,
                            }],
                        },
                    },
                    SchemaVariant {
                        tag: "sha256:y".into(),
                        schema: SchemaType::Record {
                            fields: vec![SchemaField {
                                name: "value".into(),
                                schema: SchemaType::Int,
                            }],
                        },
                    },
                ],
            },
        };
        assert!(
            schema
                .validate_json(r#"{"type":"sha256:y","value":{"value":1}}"#)
                .ok
        );
        for invalid in [
            r#"{"value":{"value":1}}"#,
            r#"{"type":"sha256:y"}"#,
            r#"{"type":"sha256:y","value":{"value":1},"extra":true}"#,
            r#"{"type":"sha256:forged","value":{"value":1}}"#,
            r#"{"type":"sha256:y","value":{"value":"wrong"}}"#,
            r#"{"value":1}"#,
        ] {
            assert!(!schema.validate_json(invalid).ok, "{invalid}");
        }
        let provider = schema.to_json_schema();
        assert_eq!(
            provider["oneOf"][1]["properties"]["type"]["const"],
            "sha256:y"
        );
        assert_eq!(provider["oneOf"][1]["additionalProperties"], false);
    }

    #[test]
    fn provider_schema_wraps_non_object_roots_and_decodes_without_semantic_loss() {
        let union = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Union {
                variants: vec![SchemaVariant {
                    tag: "sha256:answer".into(),
                    schema: SchemaType::Named {
                        name: "FinalAnswer".into(),
                        body: Box::new(SchemaType::Record {
                            fields: vec![SchemaField {
                                name: "content".into(),
                                schema: SchemaType::String,
                            }],
                        }),
                    },
                }],
            },
        };
        let provider = union.to_provider_schema().unwrap();
        assert!(provider.wrapped);
        assert_eq!(provider.schema["type"], "object");
        assert_eq!(provider.schema["required"], serde_json::json!(["value"]));
        assert_eq!(provider.schema["additionalProperties"], false);
        assert_eq!(
            provider.schema["properties"]["value"]["anyOf"][0]["properties"]["type"]["enum"],
            serde_json::json!(["sha256:answer"])
        );
        let decoded = union.validate_provider_json(
            r#"{"value":{"type":"sha256:answer","value":{"content":"done"}}}"#,
        );
        assert!(decoded.ok, "{:?}", decoded.violations);
        assert_eq!(decoded.value.unwrap()["value"]["content"], "done");
    }

    #[test]
    fn provider_schema_covers_nested_records_products_lists_and_option_sums() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Named {
                name: "Envelope".into(),
                body: Box::new(SchemaType::Record {
                    fields: vec![
                        SchemaField {
                            name: "items".into(),
                            schema: SchemaType::List {
                                item: Box::new(SchemaType::Product {
                                    components: vec![SchemaType::String, SchemaType::Int],
                                }),
                            },
                        },
                        SchemaField {
                            name: "optional".into(),
                            schema: SchemaType::Union {
                                variants: vec![
                                    SchemaVariant {
                                        tag: "Some".into(),
                                        schema: SchemaType::Named {
                                            name: "Some".into(),
                                            body: Box::new(SchemaType::String),
                                        },
                                    },
                                    SchemaVariant {
                                        tag: "None".into(),
                                        schema: SchemaType::Named {
                                            name: "None".into(),
                                            body: Box::new(SchemaType::Unit),
                                        },
                                    },
                                ],
                            },
                        },
                    ],
                }),
            },
        };
        let provider = schema.to_provider_schema().unwrap();
        assert!(!provider.wrapped);
        assert_eq!(provider.schema["type"], "object");
        assert_eq!(provider.schema["additionalProperties"], false);
        assert_eq!(
            provider.schema["required"],
            serde_json::json!(["items", "optional"])
        );
        assert_eq!(
            provider.schema["properties"]["items"]["items"]["properties"]["1"]["type"],
            "integer"
        );
        assert_eq!(
            provider.schema["properties"]["optional"]["anyOf"][1]["properties"]["value"]["type"],
            "null"
        );
    }

    #[test]
    fn provider_schema_lowers_maps_products_and_rejects_unconstrained_json() {
        let map = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Map {
                value: Box::new(SchemaType::Int),
            },
        };
        let provider = map.to_provider_schema().unwrap();
        assert!(provider.wrapped);
        assert_eq!(provider.schema["properties"]["value"]["type"], "array");
        assert_eq!(
            provider.schema["properties"]["value"]["items"]["additionalProperties"],
            false
        );
        let decoded = map.validate_provider_json(
            r#"{"value":[{"key":"one","value":1},{"key":"two","value":2}]}"#,
        );
        assert!(decoded.ok, "{:?}", decoded.violations);
        assert_eq!(
            decoded.value.unwrap(),
            serde_json::json!({"one": 1, "two": 2})
        );
        assert!(
            !map.validate_provider_json(
                r#"{"value":[{"key":"same","value":1},{"key":"same","value":2}]}"#,
            )
            .ok
        );

        let product = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Product {
                components: vec![SchemaType::String, SchemaType::Bool],
            },
        };
        let provider = product.to_provider_schema().unwrap();
        assert_eq!(provider.schema["properties"]["value"]["type"], "object");
        assert_eq!(
            provider.schema["properties"]["value"]["required"],
            serde_json::json!(["0", "1"])
        );
        let decoded = product.validate_provider_json(r#"{"value":{"0":"x","1":true}}"#);
        assert_eq!(decoded.value.unwrap(), serde_json::json!(["x", true]));

        let json = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::JsonValue,
        };
        assert!(json
            .to_provider_schema()
            .unwrap_err()
            .to_string()
            .contains("no faithful"));
    }

    #[test]
    fn provider_schema_int_bounds_and_float_validation_match_decoder() {
        let int = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Int,
        };
        let provider = int.to_provider_schema().unwrap();
        assert_eq!(provider.schema["properties"]["value"]["minimum"], i64::MIN);
        assert_eq!(provider.schema["properties"]["value"]["maximum"], i64::MAX);
        for (source, expected) in [
            (r#"{"value":1.0}"#, Some(serde_json::json!(1))),
            (
                r#"{"value":-9223372036854775808}"#,
                Some(serde_json::json!(i64::MIN)),
            ),
            (
                r#"{"value":9223372036854775807}"#,
                Some(serde_json::json!(i64::MAX)),
            ),
            (r#"{"value":1.5}"#, None),
            (r#"{"value":9223372036854775808}"#, None),
        ] {
            let validation = int.validate_provider_json(source);
            assert_eq!(validation.value, expected, "{source}: {validation:?}");
        }

        let float = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Float,
        };
        let provider = float.to_provider_schema().unwrap();
        assert_eq!(provider.schema["properties"]["value"]["minimum"], f64::MIN);
        assert_eq!(provider.schema["properties"]["value"]["maximum"], f64::MAX);
        for value in [0.0, 1.0, f64::MIN, f64::MAX] {
            let source = serde_json::json!({"value": value}).to_string();
            assert!(
                float.validate_provider_json(&source).ok,
                "provider-valid boundary must decode: {source}"
            );
        }
    }

    #[test]
    fn recursive_types_are_rejected_before_schema_reification() {
        let mut env = Env::new();
        env.define(
            "main.Node",
            crate::ast::Value::TypeDecl(crate::ast::TypeDecl {
                name: "main.Node".into(),
                params: vec![],
                body: MagType::Named("main.Node".into(), vec![]),
            }),
        );
        let error = TypeSchema::reify(&env, &MagType::Named("main.Node".into(), vec![]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("recursive semantic type main.Node is unsupported"));
    }

    #[test]
    fn products_are_exact_positional_tuples() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Product {
                components: vec![SchemaType::String, SchemaType::Int, SchemaType::String],
            },
        };
        assert!(schema.validate_json(r#"["left",1,"right"]"#).ok);

        let short = schema.validate_json(r#"["left",1]"#);
        assert!(short.violations.iter().any(|v| v.code == "wrong_arity"));

        let swapped = schema.validate_json(r#"[1,"left","right"]"#);
        assert!(swapped.violations.iter().any(|v| v.path == "$[0]"));
        assert!(swapped.violations.iter().any(|v| v.path == "$[1]"));

        let old_intersection = schema.validate_json(r#"{"name":"x","count":1}"#);
        assert!(!old_intersection.ok);
        assert!(old_intersection
            .violations
            .iter()
            .any(|v| v.code == "wrong_type"));
    }
}
