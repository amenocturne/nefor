//! `combinators.run` parsing and dispatch helpers.
//!
//! Parse once at the wire boundary into closed Rust enums, then branch on
//! the enum everywhere downstream (D-16). Per-handler correlation is the
//! job of [`PendingMap`]: the main loop stashes a oneshot sender keyed by
//! an internal id, emits the handler-dispatch event, and a spawned task
//! awaits the reply (with a timeout) via the matching oneshot receiver.
//!
//! Slice 1 note: `Into` is accepted on the wire but dispatch is stubbed —
//! we reply with `no_handler_registered`. Type-agnostic combinators
//! (`Chain`, `Identity`, `Map<Option<T>>`, …) are intentionally out of
//! scope and land when a consumer needs them.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::{oneshot, Mutex};

use crate::error::{CombinatorsError, ErrorCode};
use crate::registry::{FullyQualifiedKind, FullyQualifiedType};

/// Closed set of combinator ops this plugin understands on the wire (v1).
///
/// Adding a new op is a protocol change — keep this enum authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Binary combine: `Merge<T>(T, T) -> T`.
    Merge,
    /// Conversion: `Into<In, Out>(In) -> Out`.
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

/// A parsed, validated `combinators.run` request.
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

/// Internal id used to correlate a handler reply with the pending oneshot.
pub type InternalId = String;

/// Map of pending handler dispatches.
pub type PendingMap =
    Arc<Mutex<HashMap<InternalId, oneshot::Sender<Result<Value, CombinatorsError>>>>>;

/// Parse a `combinators.run` event body into a [`RunRequest`].
///
/// Wire shape (per architecture doc):
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

/// Build the event body this plugin emits to invoke a handler.
///
/// The handler's plugin owns the `kind` namespace, so the dispatch event's
/// `kind` is the fully-qualified handler kind. `to` is a hint — the bus is
/// broadcast — so handlers can filter quickly.
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

/// Build the reply event body sent to the original caller on success.
pub fn caller_result_body(caller: &str, caller_id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.result".into()));
    m.insert("to".into(), Value::String(caller.to_owned()));
    m.insert("id".into(), Value::String(caller_id.to_owned()));
    m.insert("output".into(), output);
    m
}

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

/// Classify an inbound event `kind` against a registered handler.
///
/// Handler reply kinds use the convention `<handler-kind>.result` on success
/// and `<handler-kind>.error` on failure. Returns `None` for kinds that
/// aren't a handler reply shape.
pub enum HandlerReplyKind {
    /// Success reply — body should carry an `output` field.
    Result,
    /// Error reply — body should carry `code` + `message`.
    Error,
}

/// Return the reply shape if `kind` looks like `<anything>.result` or
/// `<anything>.error`. Purely syntactic; the actual pending-map lookup on
/// the `id` field confirms whether the reply is for us.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, TraitImpl};
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
            req.target_type.expect("target_type set").to_wire(),
            "mock-plugin.Context"
        );
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

    /// Synchronous end-to-end dispatch check: a `combinators.run` parses,
    /// looks up its handler, produces a dispatch body, a fake handler
    /// "replies" via the oneshot, and we synthesize the caller-facing
    /// `combinators.result` body.
    #[tokio::test]
    async fn dispatch_merge_synthesises_result() {
        let mut registry = Registry::new();
        registry
            .install(
                "mock-plugin",
                vec![fqt("mock-plugin", "Message")],
                vec![TraitImpl::Merge {
                    type_: fqt("mock-plugin", "Message"),
                    handler: fqk("mock-plugin", "message.concat"),
                }],
            )
            .expect("install");

        let run_body = json!({
            "id": "caller-1",
            "op": "Merge",
            "type": "mock-plugin.Message",
            "inputs": [{"text": "hi "}, {"text": "there"}]
        });
        let req = parse_run_body(run_body.as_object().expect("obj")).expect("parse");
        let handler = registry
            .merge_handler(&req.type_)
            .expect("handler registered")
            .clone();

        // Simulate: main loop stashes oneshot; spawned task awaits it.
        let (tx, rx) = oneshot::channel::<Result<Value, CombinatorsError>>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        pending.lock().await.insert("internal-1".to_string(), tx);

        // Emit the dispatch body (we only check its shape here).
        let dispatch = handler_dispatch_body(&handler, "internal-1", &req.inputs);
        assert_eq!(
            dispatch.get("kind").and_then(Value::as_str),
            Some("mock-plugin.message.concat")
        );

        // Fake handler "replies": the reader-side code would pop the
        // pending entry and fulfil the oneshot. Do that inline.
        let reply_output = json!({"text": "hi there"});
        let taken = pending.lock().await.remove("internal-1").expect("pending");
        let _ = taken.send(Ok(reply_output.clone()));

        let received = rx.await.expect("oneshot delivered").expect("handler ok");
        assert_eq!(received, reply_output);

        // Finally, the caller-facing result body.
        let caller_reply = caller_result_body("some-caller", &req.caller_id, received);
        assert_eq!(
            caller_reply.get("kind").and_then(Value::as_str),
            Some("combinators.result")
        );
        assert_eq!(
            caller_reply.get("id").and_then(Value::as_str),
            Some("caller-1")
        );
        assert_eq!(
            caller_reply.get("output").expect("output"),
            &json!({"text": "hi there"})
        );
    }
}
