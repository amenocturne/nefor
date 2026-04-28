//! Wire parsing + dispatch helpers for `combinators.run` (legacy),
//! `combinators.query`, and `combinators.invoke`.
//!
//! Parse once at the wire boundary into closed Rust shapes, then branch on
//! the shape everywhere downstream (D-16). Per-handler correlation is the
//! job of [`PendingMap`]: the main loop stashes a oneshot sender keyed by
//! an internal id, emits the handler-dispatch event, and a spawned task
//! awaits the reply (with a timeout) via the matching oneshot receiver.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::{oneshot, Mutex};

use crate::error::{CombinatorsError, ErrorCode};
use crate::registry::{FullyQualifiedKind, FullyQualifiedType, Identity};

// ---- Legacy `combinators.run` (kept for the mock-plugin Slice 1 path) ------

/// Closed set of legacy `combinators.run` ops the plugin understands.
///
/// Stage 1 keeps these working alongside `combinators.invoke`. Per spec
/// §8: short coexistence period, then `combinators.run` drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Binary combine: `Merge<T> :: (T, T) -> T`.
    Merge,
    /// Conversion: `Into<In, Out> :: In -> Out`.
    Into,
}

impl Op {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "Merge" => Some(Self::Merge),
            "Into" => Some(Self::Into),
            _ => None,
        }
    }

    /// Required number of `inputs` for this op.
    pub fn arity(self) -> usize {
        match self {
            Self::Merge => 2,
            Self::Into => 1,
        }
    }
}

/// A parsed, validated legacy `combinators.run` request.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRequest {
    /// Caller's opaque correlation id, echoed back on reply.
    pub caller_id: String,
    /// Which combinator to invoke.
    pub op: Op,
    /// Fully-qualified input type tag.
    pub type_: FullyQualifiedType,
    /// For `Into`: the target type tag. `None` for other ops.
    pub target_type: Option<FullyQualifiedType>,
    /// Raw JSON inputs — passed through to the handler untouched.
    pub inputs: Vec<Value>,
}

impl RunRequest {
    /// Identity tuple this run targets, derived from op + types.
    pub fn identity(&self) -> Identity {
        match self.op {
            Op::Merge => Identity::new(2, self.type_.clone(), vec![self.type_.clone()]),
            Op::Into => {
                let target = self
                    .target_type
                    .clone()
                    .expect("Into request validated to have target_type");
                Identity::new(1, self.type_.clone(), vec![target])
            }
        }
    }
}

/// Internal id used to correlate a handler reply with the pending oneshot.
pub type InternalId = String;

/// Map of pending handler dispatches. Retained as a public type alias for
/// readability of test code; main.rs uses a richer slot type internally.
#[allow(dead_code)]
pub type PendingMap =
    Arc<Mutex<HashMap<InternalId, oneshot::Sender<Result<Value, CombinatorsError>>>>>;

/// Parse a `combinators.run` event body into a [`RunRequest`].
///
/// Wire shape:
/// ```json
/// { "kind": "combinators.run",
///   "id": "caller-id",
///   "op": "Merge",
///   "type": "mock-plugin.Message",
///   "inputs": [<a>, <b>] }
/// ```
/// For `Into`, an additional `target_type: "<plugin>.<out>"` is required.
pub fn parse_run_body(body: &Map<String, Value>) -> Result<RunRequest, CombinatorsError> {
    let caller_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CombinatorsError::RunRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing `id` (string)".into(),
        })?
        .to_owned();

    let op_raw =
        body.get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| CombinatorsError::RunRejected {
                code: ErrorCode::MalformedEntry,
                message: "missing `op` (string)".into(),
            })?;
    let op = Op::parse(op_raw).ok_or_else(|| CombinatorsError::RunRejected {
        code: ErrorCode::UnknownOp,
        message: format!("unknown op `{op_raw}` (expected `Merge` or `Into`)"),
    })?;

    let type_raw =
        body.get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| CombinatorsError::RunRejected {
                code: ErrorCode::MalformedEntry,
                message: "missing `type` (string)".into(),
            })?;
    let type_ =
        FullyQualifiedType::parse(type_raw).ok_or_else(|| CombinatorsError::RunRejected {
            code: ErrorCode::MalformedEntry,
            message: format!("`type` must be `<plugin>.<Type>`: {type_raw:?}"),
        })?;

    let target_type = match op {
        Op::Into => {
            let raw = body
                .get("target_type")
                .and_then(Value::as_str)
                .ok_or_else(|| CombinatorsError::RunRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Into requires `target_type` (string)".into(),
                })?;
            Some(
                FullyQualifiedType::parse(raw).ok_or_else(|| CombinatorsError::RunRejected {
                    code: ErrorCode::MalformedEntry,
                    message: format!("`target_type` must be `<plugin>.<Type>`: {raw:?}"),
                })?,
            )
        }
        Op::Merge => None,
    };

    let inputs_raw = body
        .get("inputs")
        .ok_or_else(|| CombinatorsError::RunRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing `inputs` (array)".into(),
        })?;
    let inputs = inputs_raw
        .as_array()
        .ok_or_else(|| CombinatorsError::RunRejected {
            code: ErrorCode::MalformedEntry,
            message: "`inputs` must be an array".into(),
        })?
        .clone();

    if inputs.len() != op.arity() {
        return Err(CombinatorsError::RunRejected {
            code: ErrorCode::BadArity,
            message: format!(
                "op `{}` requires {} inputs, got {}",
                match op {
                    Op::Merge => "Merge",
                    Op::Into => "Into",
                },
                op.arity(),
                inputs.len()
            ),
        });
    }

    Ok(RunRequest {
        caller_id,
        op,
        type_,
        target_type,
        inputs,
    })
}

// ---- New `combinators.query` -----------------------------------------------

/// One signature lookup in a `combinators.query`. Both fields are
/// fully-qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySignature {
    /// Input type tag.
    pub in_type: FullyQualifiedType,
    /// Output multiset (order-insignificant; we store sorted).
    pub out_multiset: Vec<FullyQualifiedType>,
    /// Arity hint (1 default; 2 for Merge-shape queries).
    pub arity: u8,
}

/// Parsed `combinators.query` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRequest {
    /// Caller's opaque correlation id.
    pub caller_id: String,
    /// Signatures to resolve.
    pub signatures: Vec<QuerySignature>,
}

/// Parse `combinators.query`. Wire shape per spec §4.2.
pub fn parse_query_body(body: &Map<String, Value>) -> Result<QueryRequest, CombinatorsError> {
    let caller_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CombinatorsError::QueryRejected {
            code: ErrorCode::MalformedQuery,
            message: "missing `id` (string)".into(),
        })?
        .to_owned();

    let sigs_raw = body
        .get("signatures")
        .and_then(Value::as_array)
        .ok_or_else(|| CombinatorsError::QueryRejected {
            code: ErrorCode::MalformedQuery,
            message: "missing `signatures` (array)".into(),
        })?;

    let mut signatures: Vec<QuerySignature> = Vec::with_capacity(sigs_raw.len());
    for entry in sigs_raw {
        let obj = entry
            .as_object()
            .ok_or_else(|| CombinatorsError::QueryRejected {
                code: ErrorCode::MalformedQuery,
                message: "every signature entry must be an object".into(),
            })?;
        let in_raw = obj.get("in").and_then(Value::as_str).ok_or_else(|| {
            CombinatorsError::QueryRejected {
                code: ErrorCode::MalformedQuery,
                message: "signature missing `in` (string)".into(),
            }
        })?;
        let in_type =
            FullyQualifiedType::parse(in_raw).ok_or_else(|| CombinatorsError::QueryRejected {
                code: ErrorCode::MalformedQuery,
                message: format!("signature `in` must be `<plugin>.<Type>`: {in_raw:?}"),
            })?;
        let out_arr = obj.get("out").and_then(Value::as_array).ok_or_else(|| {
            CombinatorsError::QueryRejected {
                code: ErrorCode::MalformedQuery,
                message: "signature missing `out` (array)".into(),
            }
        })?;
        let mut out_multiset: Vec<FullyQualifiedType> = Vec::with_capacity(out_arr.len());
        for v in out_arr {
            let raw = v.as_str().ok_or_else(|| CombinatorsError::QueryRejected {
                code: ErrorCode::MalformedQuery,
                message: "signature `out[]` entries must be strings".into(),
            })?;
            let parsed =
                FullyQualifiedType::parse(raw).ok_or_else(|| CombinatorsError::QueryRejected {
                    code: ErrorCode::MalformedQuery,
                    message: format!("signature `out[]` entry must be `<plugin>.<Type>`: {raw:?}"),
                })?;
            out_multiset.push(parsed);
        }
        out_multiset.sort();
        let arity = obj
            .get("arity")
            .and_then(Value::as_u64)
            .map(|n| n as u8)
            .unwrap_or(1);
        signatures.push(QuerySignature {
            in_type,
            out_multiset,
            arity,
        });
    }

    Ok(QueryRequest {
        caller_id,
        signatures,
    })
}

/// Resolution outcome for a single signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryResolution {
    /// Resolved (exact registration or `pass_through` synthesis).
    Resolved {
        /// Sender plugin namespace currently owning the entry.
        owner: String,
    },
    /// Not registered.
    Missing,
}

/// Build the wire body for `combinators.query.result` from the resolutions.
pub fn query_result_body(
    caller: &str,
    caller_id: &str,
    sigs: &[QuerySignature],
    resolutions: &[QueryResolution],
) -> Map<String, Value> {
    let mut resolved: Vec<Value> = Vec::new();
    let mut missing: Vec<Value> = Vec::new();
    for (sig, resolution) in sigs.iter().zip(resolutions.iter()) {
        let mut entry = Map::new();
        entry.insert("in".into(), Value::String(sig.in_type.to_wire()));
        entry.insert(
            "out".into(),
            Value::Array(
                sig.out_multiset
                    .iter()
                    .map(|t| Value::String(t.to_wire()))
                    .collect(),
            ),
        );
        if sig.arity != 1 {
            entry.insert("arity".into(), Value::Number(sig.arity.into()));
        }
        match resolution {
            QueryResolution::Resolved { owner } => {
                entry.insert("owner".into(), Value::String(owner.clone()));
                resolved.push(Value::Object(entry));
            }
            QueryResolution::Missing => {
                missing.push(Value::Object(entry));
            }
        }
    }
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("combinators.query.result".into()),
    );
    m.insert("to".into(), Value::String(caller.to_owned()));
    m.insert("id".into(), Value::String(caller_id.to_owned()));
    m.insert("resolved".into(), Value::Array(resolved));
    m.insert("missing".into(), Value::Array(missing));
    m
}

// ---- New `combinators.invoke` ---------------------------------------------

/// Parsed `combinators.invoke` request.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeRequest {
    /// Caller's opaque correlation id.
    pub caller_id: String,
    /// Identity to invoke.
    pub identity: Identity,
    /// Input value(s) — `arity` items.
    pub inputs: Vec<Value>,
}

/// Parse `combinators.invoke`. Wire shape per spec §4.3.
pub fn parse_invoke_body(body: &Map<String, Value>) -> Result<InvokeRequest, CombinatorsError> {
    let caller_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing `id` (string)".into(),
        })?
        .to_owned();
    let sig_obj = body
        .get("signature")
        .and_then(Value::as_object)
        .ok_or_else(|| CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing `signature` (object)".into(),
        })?;
    let in_raw = sig_obj.get("in").and_then(Value::as_str).ok_or_else(|| {
        CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: "signature missing `in`".into(),
        }
    })?;
    let in_type =
        FullyQualifiedType::parse(in_raw).ok_or_else(|| CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: format!("signature `in` must be `<plugin>.<Type>`: {in_raw:?}"),
        })?;
    let out_arr = sig_obj
        .get("out")
        .and_then(Value::as_array)
        .ok_or_else(|| CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: "signature missing `out` (array)".into(),
        })?;
    let mut out_multiset: Vec<FullyQualifiedType> = Vec::with_capacity(out_arr.len());
    for v in out_arr {
        let raw = v.as_str().ok_or_else(|| CombinatorsError::InvokeRejected {
            code: ErrorCode::MalformedEntry,
            message: "signature `out[]` entries must be strings".into(),
        })?;
        let parsed =
            FullyQualifiedType::parse(raw).ok_or_else(|| CombinatorsError::InvokeRejected {
                code: ErrorCode::MalformedEntry,
                message: format!("signature `out[]` entry must be `<plugin>.<Type>`: {raw:?}"),
            })?;
        out_multiset.push(parsed);
    }
    let arity = sig_obj
        .get("arity")
        .and_then(Value::as_u64)
        .map(|n| n as u8)
        .unwrap_or(1);
    if arity == 0 {
        return Err(CombinatorsError::InvokeRejected {
            code: ErrorCode::BadArity,
            message: "arity must be >= 1".into(),
        });
    }
    let identity = Identity::new(arity, in_type, out_multiset);

    let inputs: Vec<Value> = match (body.get("input"), body.get("inputs")) {
        (Some(_), Some(_)) => {
            return Err(CombinatorsError::InvokeRejected {
                code: ErrorCode::MalformedEntry,
                message: "specify exactly one of `input` (arity 1) or `inputs[]` (arity 2+)".into(),
            });
        }
        (Some(v), None) => vec![v.clone()],
        (None, Some(arr)) => arr
            .as_array()
            .ok_or_else(|| CombinatorsError::InvokeRejected {
                code: ErrorCode::MalformedEntry,
                message: "`inputs` must be an array".into(),
            })?
            .clone(),
        (None, None) => {
            return Err(CombinatorsError::InvokeRejected {
                code: ErrorCode::MalformedEntry,
                message: "missing `input` (arity 1) or `inputs` (arity 2+)".into(),
            });
        }
    };

    if inputs.len() != arity as usize {
        return Err(CombinatorsError::InvokeRejected {
            code: ErrorCode::BadArity,
            message: format!(
                "signature has arity {}, got {} input(s)",
                arity,
                inputs.len()
            ),
        });
    }

    Ok(InvokeRequest {
        caller_id,
        identity,
        inputs,
    })
}

/// Build the dispatch event sent to a registered owner plugin (new path).
///
/// Emits `signature` so owner plugins can disambiguate when one handler
/// kind serves multiple registrations. Carries a single `input` (arity 1)
/// or `inputs[]` (arity 2+); the owner is expected to mirror the choice.
pub fn invoke_dispatch_body(
    handler: &FullyQualifiedKind,
    internal_id: &str,
    identity: &Identity,
    inputs: &[Value],
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(handler.to_wire()));
    m.insert("id".into(), Value::String(internal_id.to_owned()));
    m.insert("to".into(), Value::String(handler.plugin.clone()));
    let mut sig = Map::new();
    sig.insert("in".into(), Value::String(identity.input_type.to_wire()));
    sig.insert(
        "out".into(),
        Value::Array(
            identity
                .output_multiset
                .iter()
                .map(|t| Value::String(t.to_wire()))
                .collect(),
        ),
    );
    if identity.arity != 1 {
        sig.insert("arity".into(), Value::Number(identity.arity.into()));
    }
    m.insert("signature".into(), Value::Object(sig));
    if identity.arity == 1 {
        // Arity 1: emit `input` (single value); owner pattern matches §4.3.
        m.insert(
            "input".into(),
            inputs.first().cloned().unwrap_or(Value::Null),
        );
    } else {
        m.insert("inputs".into(), Value::Array(inputs.to_vec()));
    }
    m
}

/// Build the legacy dispatch body for `combinators.run` callers. Same shape
/// as Slice 1: `inputs[]`, no `signature`. Kept stable so existing handlers
/// (mock-plugin) don't need to change.
pub fn handler_dispatch_body(
    handler: &FullyQualifiedKind,
    internal_id: &str,
    inputs: &[Value],
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(handler.to_wire()));
    m.insert("id".into(), Value::String(internal_id.to_owned()));
    m.insert("to".into(), Value::String(handler.plugin.clone()));
    m.insert("inputs".into(), Value::Array(inputs.to_vec()));
    m
}

/// Build the legacy `combinators.result` reply for `combinators.run`
/// callers. Single-output shape (`output` field). Kept for the mock-plugin
/// Slice 1 path until callers migrate.
pub fn caller_result_body(caller: &str, caller_id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.result".into()));
    m.insert("to".into(), Value::String(caller.to_owned()));
    m.insert("id".into(), Value::String(caller_id.to_owned()));
    m.insert("output".into(), output);
    m
}

/// Build a `combinators.invoke.result` reply (new shape; multiset outputs).
pub fn invoke_result_body(
    caller: &str,
    caller_id: &str,
    outputs: Vec<TypedOutput>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("combinators.invoke.result".into()),
    );
    m.insert("to".into(), Value::String(caller.to_owned()));
    m.insert("id".into(), Value::String(caller_id.to_owned()));
    let arr: Vec<Value> = outputs
        .into_iter()
        .map(|o| {
            let mut e = Map::new();
            e.insert("type".into(), Value::String(o.type_.to_wire()));
            e.insert("value".into(), o.value);
            Value::Object(e)
        })
        .collect();
    m.insert("outputs".into(), Value::Array(arr));
    m
}

/// One typed output value in an `invoke.result` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedOutput {
    /// Type tag of this output.
    pub type_: FullyQualifiedType,
    /// JSON value, possibly `null` to suppress the edge.
    pub value: Value,
}

/// Parse the `outputs` array from an owner's reply body. Validates the
/// shape (each entry is `{ type, value }`) but not the multiset; that's
/// the caller's job.
pub fn parse_typed_outputs(body: &Map<String, Value>) -> Option<Vec<TypedOutput>> {
    let arr = body.get("outputs")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let obj = v.as_object()?;
        let raw = obj.get("type")?.as_str()?;
        let type_ = FullyQualifiedType::parse(raw)?;
        let value = obj.get("value").cloned().unwrap_or(Value::Null);
        out.push(TypedOutput { type_, value });
    }
    Some(out)
}

/// Compare a reply's output types against a registered output multiset.
/// Both sides are normalised (sorted) before comparison.
pub fn validate_output_multiset(
    expected: &[FullyQualifiedType],
    got: &[TypedOutput],
) -> Result<(), String> {
    let mut got_types: Vec<FullyQualifiedType> = got.iter().map(|o| o.type_.clone()).collect();
    got_types.sort();
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort();
    if got_types == expected_sorted {
        Ok(())
    } else {
        let exp_wire: Vec<String> = expected_sorted.iter().map(|t| t.to_wire()).collect();
        let got_wire: Vec<String> = got_types.iter().map(|t| t.to_wire()).collect();
        Err(format!(
            "expected output multiset {exp_wire:?}, got {got_wire:?}"
        ))
    }
}

// ---- Error helper -----------------------------------------------------------

/// Build a `combinators.error` reply for a known caller.
pub fn caller_error_body(
    caller: &str,
    caller_id: Option<&str>,
    code: ErrorCode,
    message: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.error".into()));
    m.insert("to".into(), Value::String(caller.to_owned()));
    if let Some(id) = caller_id {
        m.insert("id".into(), Value::String(id.to_owned()));
    }
    m.insert("code".into(), Value::String(code.as_wire().to_owned()));
    m.insert("message".into(), Value::String(message.to_owned()));
    m
}

// ---- Reply classification ---------------------------------------------------

/// Classify an inbound event `kind` against a registered handler.
///
/// Handler reply kinds use the convention `<handler-kind>.result` on
/// success and `<handler-kind>.error` on failure.
pub enum HandlerReplyKind {
    /// Success reply.
    Result,
    /// Error reply.
    Error,
}

/// Return the reply shape if `kind` looks like `<anything>.result` or
/// `<anything>.error`. Purely syntactic; the actual pending-map lookup
/// confirms whether the reply is for us.
pub fn classify_reply_kind(kind: &str) -> Option<HandlerReplyKind> {
    if let Some(stem) = kind.strip_suffix(".result") {
        if !stem.is_empty() {
            return Some(HandlerReplyKind::Result);
        }
    }
    if let Some(stem) = kind.strip_suffix(".error") {
        if !stem.is_empty() {
            return Some(HandlerReplyKind::Error);
        }
    }
    None
}

/// What the reader should hand the per-invoke task: either a single
/// scalar `output` (legacy `combinators.run` shape, used by mock-plugin),
/// or a typed multiset (new `invoke` shape).
#[derive(Debug, Clone)]
pub enum HandlerOutcome {
    /// Successful single-value reply (legacy shape).
    Single(Value),
    /// Successful typed-multiset reply (new shape).
    Multi(Vec<TypedOutput>),
    /// Owner replied on the `.error` channel.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fqt(plugin: &str, name: &str) -> FullyQualifiedType {
        FullyQualifiedType {
            plugin: plugin.into(),
            name: name.into(),
        }
    }

    fn fqk(plugin: &str, bare: &str) -> FullyQualifiedKind {
        FullyQualifiedKind {
            plugin: plugin.into(),
            bare: bare.into(),
        }
    }

    // ---- Legacy run path -------------------------------------------------

    #[test]
    fn parses_valid_merge_run() {
        let body = json!({
            "id": "caller-1",
            "op": "Merge",
            "type": "mock-plugin.Message",
            "inputs": [{"text": "hi"}, {"text": "there"}]
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_run_body(&obj).expect("parse");
        assert_eq!(req.caller_id, "caller-1");
        assert_eq!(req.op, Op::Merge);
        assert_eq!(req.type_.to_wire(), "mock-plugin.Message");
        assert!(req.target_type.is_none());
        assert_eq!(req.inputs.len(), 2);
        assert_eq!(req.identity().arity, 2);
    }

    #[test]
    fn parses_valid_into_run_with_target_type() {
        let body = json!({
            "id": "caller-2",
            "op": "Into",
            "type": "mock-plugin.Message",
            "target_type": "mock-plugin.Context",
            "inputs": [{"text": "hi"}]
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_run_body(&obj).expect("parse");
        assert_eq!(req.op, Op::Into);
        assert_eq!(
            req.target_type.as_ref().expect("target_type set").to_wire(),
            "mock-plugin.Context"
        );
        assert_eq!(req.identity().arity, 1);
    }

    #[test]
    fn rejects_wrong_arity_for_merge() {
        let body = json!({
            "id": "c",
            "op": "Merge",
            "type": "mock-plugin.Message",
            "inputs": [{"text": "solo"}]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_run_body(&obj).unwrap_err();
        match err {
            CombinatorsError::RunRejected { code, .. } => assert_eq!(code, ErrorCode::BadArity),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_op() {
        let body = json!({
            "id": "c",
            "op": "Frobnicate",
            "type": "mock-plugin.Message",
            "inputs": []
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_run_body(&obj).unwrap_err();
        match err {
            CombinatorsError::RunRejected { code, .. } => assert_eq!(code, ErrorCode::UnknownOp),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_into_without_target_type() {
        let body = json!({
            "id": "c",
            "op": "Into",
            "type": "mock-plugin.Message",
            "inputs": [{"text": "hi"}]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_run_body(&obj).unwrap_err();
        match err {
            CombinatorsError::RunRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry)
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_type() {
        let body = json!({
            "id": "c",
            "op": "Merge",
            "type": "MessageWithoutPlugin",
            "inputs": [{}, {}]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_run_body(&obj).unwrap_err();
        match err {
            CombinatorsError::RunRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry)
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn handler_dispatch_body_has_full_kind_and_to() {
        let body = handler_dispatch_body(
            &fqk("mock-plugin", "message.concat"),
            "internal-1",
            &[json!({"text": "a"}), json!({"text": "b"})],
        );
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("mock-plugin.message.concat")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("internal-1"));
        assert_eq!(body.get("to").and_then(Value::as_str), Some("mock-plugin"));
        assert_eq!(
            body.get("inputs")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(2)
        );
    }

    #[test]
    fn classify_reply_kind_covers_suffixes() {
        assert!(matches!(
            classify_reply_kind("mock-plugin.message.concat.result"),
            Some(HandlerReplyKind::Result)
        ));
        assert!(matches!(
            classify_reply_kind("mock-plugin.message.concat.error"),
            Some(HandlerReplyKind::Error)
        ));
        assert!(classify_reply_kind("combinators.run").is_none());
        assert!(classify_reply_kind(".result").is_none());
    }

    // ---- Query path ------------------------------------------------------

    #[test]
    fn parses_query_with_mixed_signatures() {
        let body = json!({
            "id": "q-1",
            "signatures": [
                { "in": "p.A", "out": ["p.B"] },
                { "in": "p.X", "out": ["p.Y", "p.Z"] }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_query_body(&obj).expect("parse");
        assert_eq!(req.caller_id, "q-1");
        assert_eq!(req.signatures.len(), 2);
        assert_eq!(req.signatures[0].in_type.to_wire(), "p.A");
    }

    #[test]
    fn query_signature_out_is_normalised() {
        let body = json!({
            "id": "q-1",
            "signatures": [{ "in": "p.X", "out": ["p.Z", "p.Y"] }]
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_query_body(&obj).expect("parse");
        let outs: Vec<String> = req.signatures[0]
            .out_multiset
            .iter()
            .map(|t| t.to_wire())
            .collect();
        assert_eq!(outs, vec!["p.Y".to_owned(), "p.Z".to_owned()]);
    }

    #[test]
    fn query_rejects_missing_signatures() {
        let body = json!({ "id": "q-1" });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_query_body(&obj).unwrap_err();
        match err {
            CombinatorsError::QueryRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedQuery);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn query_result_body_partitions_resolved_and_missing() {
        let sigs = vec![
            QuerySignature {
                in_type: fqt("p", "A"),
                out_multiset: vec![fqt("p", "B")],
                arity: 1,
            },
            QuerySignature {
                in_type: fqt("p", "X"),
                out_multiset: vec![fqt("p", "Y")],
                arity: 1,
            },
        ];
        let resolutions = vec![
            QueryResolution::Resolved {
                owner: "p".to_owned(),
            },
            QueryResolution::Missing,
        ];
        let body = query_result_body("scheduler", "q-1", &sigs, &resolutions);
        let resolved = body.get("resolved").and_then(Value::as_array).expect("arr");
        let missing = body.get("missing").and_then(Value::as_array).expect("arr");
        assert_eq!(resolved.len(), 1);
        assert_eq!(missing.len(), 1);
        assert_eq!(resolved[0].get("owner").and_then(Value::as_str), Some("p"));
    }

    // ---- Invoke path -----------------------------------------------------

    #[test]
    fn parses_invoke_with_single_input() {
        let body = json!({
            "id": "inv-1",
            "signature": { "in": "p.A", "out": ["p.B"] },
            "input": { "x": 1 }
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_invoke_body(&obj).expect("parse");
        assert_eq!(req.caller_id, "inv-1");
        assert_eq!(req.identity.arity, 1);
        assert_eq!(req.inputs.len(), 1);
    }

    #[test]
    fn parses_invoke_with_inputs_array_for_arity_two() {
        let body = json!({
            "id": "inv-2",
            "signature": { "in": "mock-plugin.Message", "out": ["mock-plugin.Message"], "arity": 2 },
            "inputs": [{"text": "a"}, {"text": "b"}]
        });
        let obj = body.as_object().expect("obj").clone();
        let req = parse_invoke_body(&obj).expect("parse");
        assert_eq!(req.identity.arity, 2);
        assert_eq!(req.inputs.len(), 2);
    }

    #[test]
    fn invoke_rejects_input_arity_mismatch() {
        let body = json!({
            "id": "inv-3",
            "signature": { "in": "p.A", "out": ["p.B"], "arity": 2 },
            "input": { "x": 1 }
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_invoke_body(&obj).unwrap_err();
        match err {
            CombinatorsError::InvokeRejected { code, .. } => {
                assert_eq!(code, ErrorCode::BadArity);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn invoke_rejects_both_input_and_inputs() {
        let body = json!({
            "id": "inv-4",
            "signature": { "in": "p.A", "out": ["p.B"] },
            "input": 1,
            "inputs": [1]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_invoke_body(&obj).unwrap_err();
        match err {
            CombinatorsError::InvokeRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn invoke_dispatch_body_emits_arity_aware_inputs() {
        let id1 = Identity::new(1, fqt("p", "A"), vec![fqt("p", "B")]);
        let body1 = invoke_dispatch_body(&fqk("p", "h"), "i-1", &id1, &[json!({"x": 1})]);
        assert!(body1.get("input").is_some());
        assert!(body1.get("inputs").is_none());

        let id2 = Identity::new(2, fqt("p", "T"), vec![fqt("p", "T")]);
        let body2 = invoke_dispatch_body(&fqk("p", "h"), "i-2", &id2, &[json!(1), json!(2)]);
        assert!(body2.get("input").is_none());
        assert_eq!(
            body2
                .get("inputs")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(2)
        );
        // arity field surfaces only on >1 case in dispatch sig
        let sig = body2
            .get("signature")
            .and_then(Value::as_object)
            .expect("sig");
        assert_eq!(sig.get("arity").and_then(Value::as_u64), Some(2));
    }

    #[test]
    fn invoke_result_body_round_trip_typed() {
        let outputs = vec![
            TypedOutput {
                type_: fqt("p", "A"),
                value: json!({"a": 1}),
            },
            TypedOutput {
                type_: fqt("p", "B"),
                value: Value::Null,
            },
        ];
        let body = invoke_result_body("scheduler", "inv-1", outputs);
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("combinators.invoke.result")
        );
        let arr = body.get("outputs").and_then(Value::as_array).expect("arr");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].get("type").and_then(Value::as_str), Some("p.A"));
    }

    #[test]
    fn parse_typed_outputs_accepts_value_null() {
        let body_owner = json!({
            "outputs": [
                { "type": "p.A", "value": {"x": 1} },
                { "type": "p.B", "value": null }
            ]
        });
        let obj = body_owner.as_object().expect("obj").clone();
        let outs = parse_typed_outputs(&obj).expect("ok");
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[1].value, Value::Null);
    }

    #[test]
    fn validate_output_multiset_succeeds_for_reordered() {
        let expected = vec![fqt("p", "A"), fqt("p", "B")];
        let got = vec![
            TypedOutput {
                type_: fqt("p", "B"),
                value: Value::Null,
            },
            TypedOutput {
                type_: fqt("p", "A"),
                value: json!(1),
            },
        ];
        assert!(validate_output_multiset(&expected, &got).is_ok());
    }

    #[test]
    fn validate_output_multiset_fails_for_extra_type() {
        let expected = vec![fqt("p", "A")];
        let got = vec![
            TypedOutput {
                type_: fqt("p", "A"),
                value: json!(1),
            },
            TypedOutput {
                type_: fqt("p", "B"),
                value: json!(2),
            },
        ];
        assert!(validate_output_multiset(&expected, &got).is_err());
    }

    #[tokio::test]
    async fn pending_map_round_trip() {
        let (tx, rx) = oneshot::channel::<Result<Value, CombinatorsError>>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        pending.lock().await.insert("internal-1".to_string(), tx);
        let taken = pending.lock().await.remove("internal-1").expect("pending");
        let _ = taken.send(Ok(json!({"text": "ok"})));
        let received = rx.await.expect("oneshot delivered").expect("handler ok");
        assert_eq!(received, json!({"text": "ok"}));
    }
}
