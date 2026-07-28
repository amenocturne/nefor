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
        Ok(Self {
            version: SCHEMA_VERSION,
            root: reify_concrete(&crate::types::ConcreteType::resolve(env, ty)?)?,
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
        let mut violations = vec![];
        validate_at(&self.root, &value, "$", &mut violations);
        JsonValidation {
            ok: violations.is_empty(),
            value: violations.is_empty().then_some(value),
            error: None,
            violations,
        }
    }

    /// Convert the compiler-owned MAG descriptor into provider-neutral JSON
    /// Schema. Providers decide whether to use this as a native response
    /// format, a terminal tool schema, constrained decoding, or a prompt aid.
    pub fn to_json_schema(&self) -> Value {
        schema_type_to_json_schema(&self.root)
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
        SchemaType::Float => expect(
            matches!(value, Value::Number(number) if number.is_f64()),
            path,
            "Float",
            value,
            out,
        ),
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
        assert!(!float.validate_json("1").ok);
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
