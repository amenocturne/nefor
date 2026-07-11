use crate::checker::substitute;
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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
    Data,
    Unit,
    Bool,
    Int,
    Float,
    String,
    List { item: Box<SchemaType> },
    Map { value: Box<SchemaType> },
    Record { fields: Vec<SchemaField> },
    Union { variants: Vec<SchemaType> },
    Product { components: Vec<SchemaType> },
    Named { name: String, body: Box<SchemaType> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaField {
    pub name: String,
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
        let mut resolving = HashSet::new();
        Ok(Self {
            version: SCHEMA_VERSION,
            root: reify_type(env, ty, &mut resolving)?,
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
}

fn reify_type(
    env: &Env,
    ty: &MagType,
    resolving: &mut HashSet<String>,
) -> Result<SchemaType, MagError> {
    Ok(match ty {
        MagType::Data => SchemaType::Data,
        MagType::Unit => SchemaType::Unit,
        MagType::Bool => SchemaType::Bool,
        MagType::Int => SchemaType::Int,
        MagType::Float => SchemaType::Float,
        MagType::String => SchemaType::String,
        MagType::List(item) => SchemaType::List {
            item: Box::new(reify_type(env, item, resolving)?),
        },
        MagType::Map(key, value) if key.as_ref() == &MagType::String => SchemaType::Map {
            value: Box::new(reify_type(env, value, resolving)?),
        },
        MagType::Map(key, _) => {
            return Err(MagError::Type(format!(
                "cannot reify (Map {key} _): JSON object keys must be String"
            )))
        }
        MagType::Record(fields) => SchemaType::Record {
            fields: fields
                .iter()
                .map(|(name, ty)| {
                    Ok(SchemaField {
                        name: name.clone(),
                        schema: reify_type(env, ty, resolving)?,
                    })
                })
                .collect::<Result<_, MagError>>()?,
        },
        MagType::Union(variants) => SchemaType::Union {
            variants: variants
                .iter()
                .map(|ty| reify_type(env, ty, resolving))
                .collect::<Result<_, _>>()?,
        },
        MagType::Product(components) => SchemaType::Product {
            components: components
                .iter()
                .map(|ty| reify_type(env, ty, resolving))
                .collect::<Result<_, _>>()?,
        },
        MagType::Named(name, args) => {
            let decl = env.type_decl(name).ok_or_else(|| {
                MagError::Type(format!("cannot reify unknown nominal type {name}"))
            })?;
            if decl.params.len() != args.len() {
                return Err(MagError::Type(format!(
                    "cannot reify {name}: expected {} type arguments, got {}",
                    decl.params.len(),
                    args.len()
                )));
            }
            let key = format!("{name}<{args:?}>");
            if !resolving.insert(key.clone()) {
                return Err(MagError::Type(format!(
                    "cannot reify recursive type {name}"
                )));
            }
            let substitutions: HashMap<_, _> = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = reify_type(env, &substitute(&decl.body, &substitutions), resolving)?;
            resolving.remove(&key);
            SchemaType::Named {
                name: name.clone(),
                body: Box::new(body),
            }
        }
        MagType::EmptyList => {
            return Err(MagError::Type(
                "cannot reify an untyped empty-list placeholder".into(),
            ))
        }
        MagType::Artifact => return unsupported("Artifact"),
        MagType::TypeTag(_) => return unsupported("TypeTag"),
        MagType::Function(_, _) => return unsupported("Fn"),
        MagType::Foreign(_, _, _) => return unsupported("Foreign"),
        MagType::Var(name) => {
            return Err(MagError::Type(format!(
                "cannot reify unresolved type variable {name}"
            )))
        }
    })
}

fn unsupported<T>(name: &str) -> Result<T, MagError> {
    Err(MagError::Type(format!(
        "{name} is not representable as structural JSON data"
    )))
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
        SchemaType::Data => {}
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
        SchemaType::Union { variants } => {
            let mut candidates = variants
                .iter()
                .map(|variant| {
                    let mut violations = vec![];
                    validate_at(variant, value, path, &mut violations);
                    violations
                })
                .collect::<Vec<_>>();
            if candidates.iter().any(Vec::is_empty) {
                return;
            }
            candidates.sort_by_key(Vec::len);
            out.push(Violation {
                path: path.into(),
                code: "no_union_variant".into(),
                expected: describe(schema),
                actual: json_kind(value).into(),
                message: "value does not conform to any union variant".into(),
            });
            if let Some(best) = candidates.into_iter().next() {
                out.extend(best);
            }
        }
        SchemaType::Product { components } => {
            for component in components {
                validate_at(component, value, path, out);
            }
        }
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
        SchemaType::Data => "Data".into(),
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
            .map(describe)
            .collect::<Vec<_>>()
            .join(" | "),
        SchemaType::Product { components } => components
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(" & "),
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
                            variants: vec![SchemaType::String, SchemaType::Bool],
                        },
                    },
                ],
            },
        };
        assert!(
            schema
                .validate_json(r#"{"labels":{"a":1},"choice":true}"#)
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
    fn products_require_every_component() {
        let schema = TypeSchema {
            version: SCHEMA_VERSION,
            root: SchemaType::Product {
                components: vec![
                    SchemaType::Record {
                        fields: vec![SchemaField {
                            name: "name".into(),
                            schema: SchemaType::String,
                        }],
                    },
                    SchemaType::Record {
                        fields: vec![SchemaField {
                            name: "count".into(),
                            schema: SchemaType::Int,
                        }],
                    },
                ],
            },
        };
        // Products are intersections; strict records make two disjoint record
        // components unsatisfiable, which is the existing MAG product meaning.
        let invalid = schema.validate_json(r#"{"name":"x","count":1}"#);
        assert!(!invalid.ok);
        assert!(invalid.violations.iter().any(|v| v.code == "extra_field"));
    }
}
