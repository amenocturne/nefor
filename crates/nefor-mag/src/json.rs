//! JSON ↔ MAG `Value` bridge.
//!
//! Two directions, both used by the resident evaluator:
//! - `value_to_json` serializes a MAG value into the IR/wire JSON shape (the
//!   representation node args and rule-produced modifications travel in). It is
//!   the same serialization the lowered IR uses for `params`.
//! - `json_to_value` lifts a node's JSON output back into a MAG value so a rule
//!   function can consume it. It is the data-only inverse: node outputs are
//!   plain data (strings, numbers, arrays, objects), never MAG-authoring
//!   constructs (functions, nodes, subgraphs, keywords, symbols).
//!
//! The pair is deliberately asymmetric. `value_to_json` is lossy on the
//! authoring-only variants (`Keyword` → `":k"`, `Symbol` → `"s"`), so a JSON
//! string cannot be reliably lifted back to a keyword or symbol; every JSON
//! string lifts to `Value::Str`. That is correct for the one job `json_to_value`
//! has — feeding runtime node data to rule fns — where keywords/symbols never
//! appear.

use crate::ast::Value;
use crate::error::MagError;

/// Serialize a MAG value into JSON. Authoring-only variants (node, graph,
/// subgraph, function, builtin, type declaration) have no data representation
/// and are rejected — they can never legitimately appear in node args or a
/// modification a rule returns.
pub fn value_to_json(val: &Value) -> Result<serde_json::Value, MagError> {
    match val {
        Value::Str(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Int(n) => Ok(serde_json::json!(n)),
        Value::Float(n) => Ok(serde_json::json!(n)),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Keyword(k) => Ok(serde_json::Value::String(format!(":{k}"))),
        Value::Symbol(s) => Ok(serde_json::Value::String(s.clone())),
        Value::List(items) | Value::Vector(items) => {
            let arr: Result<Vec<_>, _> = items.iter().map(value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Map(map) => {
            let obj: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        Value::Node(_)
        | Value::Graph(_)
        | Value::Subgraph(_)
        | Value::Fn(_)
        | Value::BuiltinFn(_)
        | Value::TypeDecl(_) => Err(MagError::Eval(format!(
            "cannot serialize {} to JSON",
            val.type_name()
        ))),
    }
}

/// Lift JSON node output into a MAG value. Total (never fails): JSON is a strict
/// subset of MAG's data shapes. Arrays lift to `Vector` (accepted everywhere a
/// list is, via `extract_list`); objects lift to `Map`; integral numbers to
/// `Int`, fractional to `Float`. An integer outside `i64` range widens to
/// `Float` rather than failing — a lossy edge that only bites on out-of-range
/// magnitudes node outputs do not realistically carry.
pub fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                // Unrepresentable (e.g. u64 > i64::MAX and not exactly f64):
                // fall back to the string form to avoid silent data loss.
                Value::Str(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::Vector(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Map(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::FnValue;

    #[test]
    fn scalars_roundtrip() {
        for v in [
            serde_json::json!(42),
            serde_json::json!(-7),
            serde_json::json!(3.5),
            serde_json::json!("hello"),
            serde_json::json!(true),
            serde_json::json!(null),
        ] {
            let back = value_to_json(&json_to_value(&v)).unwrap();
            assert_eq!(back, v, "roundtrip diverged for {v}");
        }
    }

    #[test]
    fn nested_object_and_array_roundtrip() {
        let v = serde_json::json!({
            "kind": "final",
            "tools": ["fs/read", "grep"],
            "meta": { "steps": 12, "ok": true, "score": 0.5 },
            "empty": []
        });
        let back = value_to_json(&json_to_value(&v)).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn json_array_lifts_to_vector() {
        let v = json_to_value(&serde_json::json!([1, 2, 3]));
        assert!(matches!(v, Value::Vector(_)));
    }

    #[test]
    fn json_object_lifts_to_map() {
        let v = json_to_value(&serde_json::json!({"a": 1}));
        match v {
            Value::Map(m) => assert!(matches!(m.get("a"), Some(Value::Int(1)))),
            other => panic!("expected map, got {}", other.type_name()),
        }
    }

    #[test]
    fn keyword_serializes_but_does_not_lift_back() {
        // Documents the deliberate asymmetry: keyword -> ":k", but ":k" lifts
        // to a plain string, not a keyword.
        let j = value_to_json(&Value::Keyword("id".into())).unwrap();
        assert_eq!(j, serde_json::json!(":id"));
        assert!(matches!(json_to_value(&j), Value::Str(s) if s == ":id"));
    }

    #[test]
    fn value_to_json_rejects_function() {
        let val = Value::Fn(FnValue {
            params: vec![],
            body: vec![],
            closure: vec![],
        });
        assert!(value_to_json(&val).is_err());
    }

    #[test]
    fn integral_float_stays_float() {
        // A MAG float with integral value must not collapse to Int on roundtrip.
        let j = value_to_json(&Value::Float(3.0)).unwrap();
        assert!(matches!(json_to_value(&j), Value::Float(_)));
    }
}
