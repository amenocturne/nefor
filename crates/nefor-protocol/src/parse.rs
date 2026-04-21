//! JSON Lines parser for NCP envelopes.
//!
//! The engine's broker calls [`Envelope::parse_line`](crate::Envelope) or
//! [`PluginOutgoing::parse_line`](crate::PluginOutgoing) (re-exported from
//! this module via the inherent impls) and maps the returned [`ParseError`]
//! to a [`SystemBody::Error`](crate::SystemBody::Error) error code per §8.

use serde_json::{Map, Value};

use crate::envelope::{Body, Envelope, MessageKind, PluginOutgoing};
use crate::newtypes::{PluginName, Timestamp};
use crate::system::{ErrorCode, Offending, SystemBody};

/// Failure modes from parsing a wire line into an envelope.
///
/// Each variant maps to a specific §8 `ErrorCode` at the engine layer —
/// the mapping lives in the engine, not here, to keep this crate focused
/// on decoding. The intent is that callers can `match` exhaustively on
/// the variants without inspecting string messages.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The line was not valid JSON at all. Maps to `malformed_envelope`
    /// (connection-level framing error — §8 footnote says the engine
    /// closes after a single emission).
    #[error("invalid JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),

    /// The top-level JSON was not an object.
    #[error("envelope must be a JSON object")]
    NotAnObject,

    /// A required envelope field was missing.
    #[error("missing required envelope field `{0}`")]
    MissingField(&'static str),

    /// An envelope field had the wrong JSON type.
    #[error("envelope field `{field}` has wrong JSON type (expected {expected})")]
    WrongType {
        /// Offending field name (`type`, `from`, `ts`, or `body`).
        field: &'static str,
        /// Human-readable description of the expected type.
        expected: &'static str,
    },

    /// Envelope contained fields beyond the spec's exact set. The payload
    /// lists the unexpected field names, in document order.
    #[error("envelope has forbidden extra fields: {0:?}")]
    ExtraFields(Vec<String>),

    /// `body` was present but not a JSON object. Maps to `body_not_object`.
    #[error("envelope `body` is not a JSON object")]
    BodyNotObject,

    /// `type` was a string, but not `"system"` or `"event"`.
    #[error("envelope `type` must be \"system\" or \"event\", got {0:?}")]
    InvalidType(String),

    /// `type` was `"system"` but the body's `kind` was unrecognized.
    /// Maps to `unknown_kind`.
    #[error("unknown system message kind: {0:?}")]
    UnknownKind(String),

    /// `type` was `"system"` but the body had no `kind` field at all.
    /// Maps to `malformed_envelope`.
    #[error("system body is missing required field `kind`")]
    SystemBodyMissingKind,

    /// `type` was `"system"` and `kind` was present but not a string.
    /// Maps to `malformed_envelope`.
    #[error("system body field `kind` has wrong JSON type (expected string)")]
    SystemBodyKindNotString,

    /// Semantic or structural validation failure inside a `ready` body.
    /// Per NCP §8 this maps to `invalid_ready` and closes the connection.
    #[error("invalid ready: {0}")]
    InvalidReadyBody(#[source] InvalidReadyReason),

    /// Structural failure inside a non-ready system body. Per NCP §8 this
    /// maps to `malformed_envelope` and keeps the connection open.
    #[error("invalid system body for kind `{kind}`: {reason}")]
    InvalidSystemBody {
        /// Which system kind was being parsed when the body-level error
        /// was produced.
        kind: SystemBodyKind,
        /// Structural reason the body was rejected.
        #[source]
        reason: InvalidBodyReason,
    },

    /// A plugin-outgoing envelope included `from` or `ts`, which only the
    /// engine may stamp (§3). Maps to `malformed_envelope`.
    #[error("plugin-outgoing envelope must not contain `{0}`")]
    OutgoingHasStampedField(&'static str),

    /// `ts` was a string but not a valid ISO-8601 UTC millisecond stamp.
    #[error("envelope `ts` is not a valid ISO-8601 UTC timestamp: {0}")]
    InvalidTimestamp(String),

    /// `from` was the empty string.
    #[error("envelope `from` must not be empty")]
    EmptyFrom,
}

/// Why a `ready` body was rejected.
///
/// The ready body carries only `protocol_version`; every rejection — bad
/// semver, missing field, wrong type, extra field — surfaces as this one
/// variant. Per §8 the engine maps every ready-body fault to
/// `invalid_ready` and closes the connection.
#[derive(Debug, thiserror::Error)]
pub enum InvalidReadyReason {
    /// `protocol_version` was missing, not a string, the wrong shape, or
    /// the body carried unexpected extra fields.
    #[error("`protocol_version` must be SemVer 2.0.0 or MAJOR.MINOR shorthand: {raw:?}")]
    InvalidProtocolVersion {
        /// The offending raw string (or the name of the extra field, for
        /// structural faults). Diagnostic only — consumers branch on the
        /// variant, not on `raw`.
        raw: String,
    },
}

/// Identifies which system message kind a body-level structural error was
/// produced for. Printed verbatim in error messages and used for
/// broker-side diagnostics.
///
/// `Ready` is intentionally excluded — ready errors flow through
/// [`ParseError::InvalidReadyBody`] / [`InvalidReadyReason`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemBodyKind {
    /// `ready_ok` (§5.2).
    ReadyOk,
    /// `shutdown` (§5.3).
    Shutdown,
    /// `error` (§5.4).
    Error,
}

impl SystemBodyKind {
    /// Wire-format name (snake_case, matches the `kind` discriminant).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ReadyOk => "ready_ok",
            Self::Shutdown => "shutdown",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for SystemBodyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Structural reason a system body was rejected. Kept deliberately small:
/// extend with new variants if call sites demand, but never fall back to a
/// `Custom(String)` escape hatch.
#[derive(Debug, thiserror::Error)]
pub enum InvalidBodyReason {
    /// A required body field was absent.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A body field was present but had the wrong JSON type.
    #[error("field `{field}` has wrong JSON type (expected {expected})")]
    WrongFieldType {
        /// Offending field name.
        field: &'static str,
        /// Human-readable description of the expected type.
        expected: &'static str,
    },
    /// A body carried a field outside the closed set for its kind.
    #[error("unexpected field `{0}`")]
    ExtraField(String),
    /// An enum-valued field held a string outside its closed vocabulary
    /// (e.g. `error.code`).
    #[error("field `{field}` has invalid value {value:?} (expected one of {expected})")]
    InvalidEnumValue {
        /// Offending field name.
        field: &'static str,
        /// The raw value received.
        value: String,
        /// Human-readable list of accepted values.
        expected: &'static str,
    },
}

impl Envelope {
    /// Parse a single JSON line (no trailing newline required) into a
    /// fully-stamped envelope. See [`ParseError`] for the failure modes.
    pub fn parse_line(line: &str) -> Result<Envelope, ParseError> {
        let value: Value = serde_json::from_str(line).map_err(ParseError::InvalidJson)?;
        let obj = match value {
            Value::Object(map) => map,
            _ => return Err(ParseError::NotAnObject),
        };

        let EnvelopeParts {
            kind,
            from,
            ts,
            body,
        } = extract_envelope_parts(obj, /* require_stamps = */ true)?;

        Ok(Envelope {
            kind,
            from: from.expect("require_stamps => from is Some"),
            ts: ts.expect("require_stamps => ts is Some"),
            body,
        })
    }
}

impl PluginOutgoing {
    /// Parse a plugin-originated JSON line. Rejects any envelope carrying
    /// `from` or `ts` — those are engine-stamped per §3.
    pub fn parse_line(line: &str) -> Result<PluginOutgoing, ParseError> {
        let value: Value = serde_json::from_str(line).map_err(ParseError::InvalidJson)?;
        let obj = match value {
            Value::Object(map) => map,
            _ => return Err(ParseError::NotAnObject),
        };

        let EnvelopeParts {
            kind,
            from: _,
            ts: _,
            body,
        } = extract_envelope_parts(obj, /* require_stamps = */ false)?;

        Ok(PluginOutgoing { kind, body })
    }
}

struct EnvelopeParts {
    kind: MessageKind,
    from: Option<PluginName>,
    ts: Option<Timestamp>,
    body: Body,
}

fn extract_envelope_parts(
    mut obj: Map<String, Value>,
    require_stamps: bool,
) -> Result<EnvelopeParts, ParseError> {
    // Step 1: detect extras. Spec §3 forbids any envelope field beyond the
    // allowed set. For plugin-outgoing, `from`/`ts` are also forbidden
    // (handled below, as a distinct error code so the engine can tell the
    // plugin exactly what it did wrong).
    let allowed: &[&str] = if require_stamps {
        &["type", "from", "ts", "body"]
    } else {
        &["type", "body"]
    };
    // Collect extras in document order so diagnostics are stable.
    let mut extras: Vec<String> = Vec::new();
    let mut outgoing_stamped: Option<&'static str> = None;
    for key in obj.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        if !require_stamps && key == "from" {
            outgoing_stamped = Some("from");
            break;
        }
        if !require_stamps && key == "ts" {
            outgoing_stamped = Some("ts");
            break;
        }
        extras.push(key.clone());
    }
    if let Some(field) = outgoing_stamped {
        return Err(ParseError::OutgoingHasStampedField(field));
    }
    if !extras.is_empty() {
        return Err(ParseError::ExtraFields(extras));
    }

    // Step 2: `type`.
    let kind_raw = obj.remove("type").ok_or(ParseError::MissingField("type"))?;
    let kind_str = match kind_raw {
        Value::String(s) => s,
        _ => {
            return Err(ParseError::WrongType {
                field: "type",
                expected: "string",
            })
        }
    };
    let kind = match kind_str.as_str() {
        "system" => MessageKind::System,
        "event" => MessageKind::Event,
        _ => return Err(ParseError::InvalidType(kind_str)),
    };

    // Step 3: `from` / `ts` — only when required.
    let from = if require_stamps {
        let v = obj.remove("from").ok_or(ParseError::MissingField("from"))?;
        let s = match v {
            Value::String(s) => s,
            _ => {
                return Err(ParseError::WrongType {
                    field: "from",
                    expected: "string",
                })
            }
        };
        if s.is_empty() {
            return Err(ParseError::EmptyFrom);
        }
        // Route through the deserializer so "engine" is accepted on the
        // wire (the engine stamps from:"engine" on its own messages).
        // The deserializer only rejects empty strings, which we've already
        // screened out above — so any error here is a defensive fallback
        // and maps cleanly to EmptyFrom.
        Some(
            serde_json::from_value::<PluginName>(Value::String(s))
                .map_err(|_| ParseError::EmptyFrom)?,
        )
    } else {
        None
    };

    let ts = if require_stamps {
        let v = obj.remove("ts").ok_or(ParseError::MissingField("ts"))?;
        let s = match v {
            Value::String(s) => s,
            _ => {
                return Err(ParseError::WrongType {
                    field: "ts",
                    expected: "string",
                })
            }
        };
        Some(Timestamp::parse(&s).map_err(|e| ParseError::InvalidTimestamp(e.to_string()))?)
    } else {
        None
    };

    // Step 4: `body` must exist and be an object (§3).
    let body_raw = obj.remove("body").ok_or(ParseError::MissingField("body"))?;
    let body_obj = match body_raw {
        Value::Object(m) => m,
        _ => return Err(ParseError::BodyNotObject),
    };

    // Step 5: dispatch on `type` to build the typed body.
    let body = match kind {
        MessageKind::System => Body::System(parse_system_body(body_obj)?),
        MessageKind::Event => Body::Event(body_obj),
    };

    Ok(EnvelopeParts {
        kind,
        from,
        ts,
        body,
    })
}

fn parse_system_body(mut obj: Map<String, Value>) -> Result<SystemBody, ParseError> {
    // Peek `kind` first so we can emit the distinct UnknownKind error
    // (or MissingKind / KindNotString) instead of a generic body error.
    let kind = match obj.remove("kind") {
        Some(Value::String(s)) => s,
        Some(_) => return Err(ParseError::SystemBodyKindNotString),
        None => return Err(ParseError::SystemBodyMissingKind),
    };

    match kind.as_str() {
        "ready" => parse_ready(obj).map_err(ParseError::InvalidReadyBody),
        "ready_ok" => parse_ready_ok(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::ReadyOk,
            reason,
        }),
        "shutdown" => parse_shutdown(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::Shutdown,
            reason,
        }),
        "error" => parse_error_body(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::Error,
            reason,
        }),
        _ => Err(ParseError::UnknownKind(kind)),
    }
}

// ---- per-kind body parsers ---------------------------------------------
//
// Each of these takes the body map with `kind` already removed, validates
// structure (no extras, required fields present, types correct), then
// applies any per-kind semantic invariants.

fn parse_ready(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidReadyReason> {
    // The ready body is tiny (just `protocol_version`). Every structural
    // fault — missing field, wrong type, extra field — collapses to the
    // single `invalid_ready` error code per §8, so we funnel them all
    // through `InvalidProtocolVersion`. The `raw` payload is diagnostic
    // only; callers should branch on the variant, not on `raw`.
    let protocol_version = match obj.remove("protocol_version") {
        Some(Value::String(s)) => s,
        Some(Value::Null) | None => {
            return Err(InvalidReadyReason::InvalidProtocolVersion { raw: String::new() })
        }
        Some(other) => {
            return Err(InvalidReadyReason::InvalidProtocolVersion {
                raw: other.to_string(),
            })
        }
    };
    if let Some((k, _)) = obj.into_iter().next() {
        return Err(InvalidReadyReason::InvalidProtocolVersion { raw: k });
    }
    if !is_valid_protocol_version(&protocol_version) {
        return Err(InvalidReadyReason::InvalidProtocolVersion {
            raw: protocol_version,
        });
    }
    Ok(SystemBody::Ready { protocol_version })
}

fn parse_ready_ok(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let engine_version = take_string(&mut obj, "engine_version")?
        .ok_or(InvalidBodyReason::MissingField("engine_version"))?;
    reject_extras(obj)?;
    Ok(SystemBody::ReadyOk { engine_version })
}

fn parse_shutdown(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let reason = take_string(&mut obj, "reason")?;
    let grace_ms = match obj.remove("grace_ms") {
        None => None,
        Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) => Some(v),
            None => {
                return Err(InvalidBodyReason::WrongFieldType {
                    field: "grace_ms",
                    expected: "non-negative integer",
                })
            }
        },
        Some(_) => {
            return Err(InvalidBodyReason::WrongFieldType {
                field: "grace_ms",
                expected: "non-negative integer",
            })
        }
    };
    reject_extras(obj)?;
    Ok(SystemBody::Shutdown { reason, grace_ms })
}

fn parse_error_body(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let code_raw = take_string(&mut obj, "code")?.ok_or(InvalidBodyReason::MissingField("code"))?;
    let code = match code_raw.as_str() {
        "protocol_version_mismatch" => ErrorCode::ProtocolVersionMismatch,
        "invalid_ready" => ErrorCode::InvalidReady,
        "malformed_envelope" => ErrorCode::MalformedEnvelope,
        "body_not_object" => ErrorCode::BodyNotObject,
        "unknown_kind" => ErrorCode::UnknownKind,
        "queue_overflow" => ErrorCode::QueueOverflow,
        "rate_limited" => ErrorCode::RateLimited,
        _ => {
            return Err(InvalidBodyReason::InvalidEnumValue {
                field: "code",
                value: code_raw,
                expected: "a §8 error code (e.g. \"malformed_envelope\", \"invalid_ready\")",
            })
        }
    };
    let message =
        take_string(&mut obj, "message")?.ok_or(InvalidBodyReason::MissingField("message"))?;
    let offending = match obj.remove("offending") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(parse_offending(map)?),
        Some(_) => {
            return Err(InvalidBodyReason::WrongFieldType {
                field: "offending",
                expected: "object",
            })
        }
    };
    reject_extras(obj)?;
    Ok(SystemBody::Error {
        code,
        message,
        offending,
    })
}

fn parse_offending(mut obj: Map<String, Value>) -> Result<Offending, InvalidBodyReason> {
    let from_raw = take_string(&mut obj, "from")?.ok_or(InvalidBodyReason::MissingField("from"))?;
    let ts_raw = take_string(&mut obj, "ts")?.ok_or(InvalidBodyReason::MissingField("ts"))?;
    reject_extras(obj)?;
    // Offending.from uses the same wire-level `PluginName` deserializer as
    // envelope.from: accepts "engine", rejects empty. An invalid shape
    // here surfaces as a wrong-field-type body error; that's the right
    // fit because `offending` is structural diagnostics rather than a
    // real identity check.
    let from = serde_json::from_value::<PluginName>(Value::String(from_raw)).map_err(|_| {
        InvalidBodyReason::WrongFieldType {
            field: "from",
            expected: "non-empty string",
        }
    })?;
    let ts = Timestamp::parse(&ts_raw).map_err(|_| InvalidBodyReason::WrongFieldType {
        field: "ts",
        expected: "ISO-8601 UTC timestamp",
    })?;
    Ok(Offending { from, ts })
}

// ---- small structural helpers ------------------------------------------

fn take_string(
    obj: &mut Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, InvalidBodyReason> {
    match obj.remove(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(InvalidBodyReason::WrongFieldType {
            field,
            expected: "string",
        }),
    }
}

fn reject_extras(obj: Map<String, Value>) -> Result<(), InvalidBodyReason> {
    // Document order is preserved by serde_json's preserve-order-by-default
    // object representation; return the first extra so the diagnostic is
    // stable and small.
    if let Some(name) = obj.into_iter().next().map(|(k, _)| k) {
        return Err(InvalidBodyReason::ExtraField(name));
    }
    Ok(())
}

fn is_valid_protocol_version(s: &str) -> bool {
    if semver::Version::parse(s).is_ok() {
        return true;
    }
    // Accept MAJOR.MINOR shorthand — e.g. "0.1" — since that's the form
    // §5.1's example and §9 both use.
    let mut parts = s.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|c| c.is_ascii_digit())
        && minor.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::newtypes::PluginName;
    use crate::system::{ErrorCode, Offending};
    use serde_json::json;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-21T12:34:56.789Z").expect("valid")
    }

    fn pn(s: &str) -> PluginName {
        PluginName::new(s).expect("valid plugin name")
    }

    // ---- round-trip every SystemBody variant -------------------------------

    fn roundtrip(body: SystemBody) {
        let env = Envelope::system(pn("p"), ts(), body);
        let line = env.to_line();
        let parsed = Envelope::parse_line(&line).expect("round-trip parse");
        assert_eq!(parsed, env, "round trip failed for line: {line}");
    }

    #[test]
    fn round_trip_ready() {
        roundtrip(SystemBody::Ready {
            protocol_version: "0.1".into(),
        });
    }

    #[test]
    fn round_trip_ready_ok() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::ReadyOk {
                engine_version: "0.1.0".into(),
            },
        );
        let line = env.to_line();
        let parsed = Envelope::parse_line(&line).expect("round-trip");
        assert_eq!(parsed, env);
    }

    #[test]
    fn round_trip_shutdown() {
        roundtrip(SystemBody::Shutdown {
            reason: Some("quit".into()),
            grace_ms: Some(2000),
        });
        roundtrip(SystemBody::Shutdown {
            reason: None,
            grace_ms: None,
        });
    }

    #[test]
    fn round_trip_error() {
        roundtrip(SystemBody::Error {
            code: ErrorCode::MalformedEnvelope,
            message: "body is not an object".into(),
            offending: Some(Offending {
                from: pn("bad"),
                ts: ts(),
            }),
        });
        roundtrip(SystemBody::Error {
            code: ErrorCode::RateLimited,
            message: "slow down".into(),
            offending: None,
        });
    }

    #[test]
    fn round_trip_event() {
        let mut body = Map::new();
        body.insert(String::from("custom"), json!("payload"));
        let env = Envelope::event(pn("p"), ts(), body);
        let line = env.to_line();
        let parsed = Envelope::parse_line(&line).expect("round-trip event");
        assert_eq!(parsed, env);
    }

    // ---- rejection cases ---------------------------------------------------

    #[test]
    fn rejects_extra_envelope_field() {
        let line =
            r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{},"extra":1}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::ExtraFields(ref v) if v == &vec!["extra".to_string()]),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_missing_type() {
        let line = r#"{"from":"p","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField("type")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_missing_from() {
        let line = r#"{"type":"event","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField("from")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_missing_ts() {
        let line = r#"{"type":"event","from":"p","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::MissingField("ts")), "got {err:?}");
    }

    #[test]
    fn rejects_missing_body() {
        let line = r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z"}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingField("body")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_body_array() {
        let line = r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z","body":[]}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::BodyNotObject), "got {err:?}");
    }

    #[test]
    fn rejects_body_string() {
        let line = r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z","body":"nope"}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::BodyNotObject), "got {err:?}");
    }

    #[test]
    fn rejects_body_number() {
        let line = r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z","body":42}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::BodyNotObject), "got {err:?}");
    }

    #[test]
    fn rejects_body_null() {
        let line = r#"{"type":"event","from":"p","ts":"2026-04-21T12:34:56.789Z","body":null}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::BodyNotObject), "got {err:?}");
    }

    #[test]
    fn rejects_invalid_json() {
        let line = "not json at all";
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_object_top_level() {
        let line = "[]";
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::NotAnObject), "got {err:?}");
    }

    #[test]
    fn rejects_invalid_type_value() {
        let line = r#"{"type":"other","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidType(ref s) if s == "other"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_unknown_system_kind() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"pray"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownKind(ref s) if s == "pray"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_missing_kind() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::SystemBodyMissingKind),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_non_string_kind() {
        let line =
            r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":5}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::SystemBodyKindNotString),
            "got {err:?}"
        );
    }

    // ---- ready body: invariants --------------------------------------------

    #[test]
    fn rejects_ready_with_invalid_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready","protocol_version":"not.a.version"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidReadyBody(InvalidReadyReason::InvalidProtocolVersion {
                    ref raw
                }) if raw == "not.a.version"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_ready_missing_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidReadyBody(InvalidReadyReason::InvalidProtocolVersion { .. })
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_ready_extra_field() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready","protocol_version":"0.1","bonus":"x"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidReadyBody(InvalidReadyReason::InvalidProtocolVersion { .. })
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn accepts_ready_with_major_minor_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready","protocol_version":"0.1"}}"#;
        let env = Envelope::parse_line(line).expect("valid ready");
        match env.body {
            Body::System(SystemBody::Ready { protocol_version }) => {
                assert_eq!(protocol_version, "0.1");
            }
            other => panic!("expected ready body, got {other:?}"),
        }
    }

    #[test]
    fn accepts_ready_with_full_semver_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready","protocol_version":"0.1.0"}}"#;
        let env = Envelope::parse_line(line).expect("valid ready");
        assert!(matches!(env.body, Body::System(SystemBody::Ready { .. })));
    }

    // ---- non-ready body: structural errors --------------------------------

    #[test]
    fn rejects_shutdown_wrong_grace_type() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"shutdown","grace_ms":"soon"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::Shutdown,
                    reason: InvalidBodyReason::WrongFieldType {
                        field: "grace_ms",
                        ..
                    },
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_ready_ok_extra_field() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready_ok","engine_version":"0.1.0","bonus":1}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::ReadyOk,
                    reason: InvalidBodyReason::ExtraField(ref f),
                } if f == "bonus"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_error_missing_code() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"error","message":"boom"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::Error,
                    reason: InvalidBodyReason::MissingField("code"),
                }
            ),
            "got {err:?}"
        );
    }

    // ---- Display golden strings --------------------------------------------
    //
    // Pin the Display output of each new reason arm so accidental message
    // changes break the parser tests rather than downstream broker tests.

    #[test]
    fn display_invalid_ready_invalid_protocol_version() {
        let err = ParseError::InvalidReadyBody(InvalidReadyReason::InvalidProtocolVersion {
            raw: "not.a.version".into(),
        });
        assert_eq!(
            err.to_string(),
            "invalid ready: `protocol_version` must be SemVer 2.0.0 or MAJOR.MINOR shorthand: \"not.a.version\""
        );
    }

    #[test]
    fn display_invalid_system_body_missing_field() {
        let err = ParseError::InvalidSystemBody {
            kind: SystemBodyKind::ReadyOk,
            reason: InvalidBodyReason::MissingField("engine_version"),
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `ready_ok`: missing required field `engine_version`"
        );
    }

    #[test]
    fn display_invalid_system_body_wrong_type() {
        let err = ParseError::InvalidSystemBody {
            kind: SystemBodyKind::Shutdown,
            reason: InvalidBodyReason::WrongFieldType {
                field: "grace_ms",
                expected: "non-negative integer",
            },
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `shutdown`: field `grace_ms` has wrong JSON type (expected non-negative integer)"
        );
    }

    #[test]
    fn display_invalid_system_body_extra_field() {
        let err = ParseError::InvalidSystemBody {
            kind: SystemBodyKind::ReadyOk,
            reason: InvalidBodyReason::ExtraField("bonus".into()),
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `ready_ok`: unexpected field `bonus`"
        );
    }

    #[test]
    fn display_system_body_missing_kind() {
        let err = ParseError::SystemBodyMissingKind;
        assert_eq!(
            err.to_string(),
            "system body is missing required field `kind`"
        );
    }

    #[test]
    fn display_system_body_kind_not_string() {
        let err = ParseError::SystemBodyKindNotString;
        assert_eq!(
            err.to_string(),
            "system body field `kind` has wrong JSON type (expected string)"
        );
    }

    #[test]
    fn rejects_invalid_timestamp() {
        let line = r#"{"type":"event","from":"p","ts":"yesterday","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidTimestamp(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_empty_from() {
        let line = r#"{"type":"event","from":"","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(matches!(err, ParseError::EmptyFrom), "got {err:?}");
    }

    // ---- plugin outgoing ---------------------------------------------------

    #[test]
    fn outgoing_round_trip_system() {
        let out = PluginOutgoing::system(SystemBody::Ready {
            protocol_version: "0.1".into(),
        });
        let line = out.to_line();
        let parsed = PluginOutgoing::parse_line(&line).expect("round trip");
        assert_eq!(parsed, out);
    }

    #[test]
    fn outgoing_round_trip_event() {
        let mut body = Map::new();
        body.insert("a".into(), json!(1));
        let out = PluginOutgoing::event(body);
        let line = out.to_line();
        let parsed = PluginOutgoing::parse_line(&line).expect("round trip");
        assert_eq!(parsed, out);
    }

    #[test]
    fn outgoing_rejects_from_field() {
        let line = r#"{"type":"event","from":"p","body":{}}"#;
        let err = PluginOutgoing::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::OutgoingHasStampedField("from")),
            "got {err:?}"
        );
    }

    #[test]
    fn outgoing_rejects_ts_field() {
        let line = r#"{"type":"event","ts":"2026-04-21T12:34:56.789Z","body":{}}"#;
        let err = PluginOutgoing::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::OutgoingHasStampedField("ts")),
            "got {err:?}"
        );
    }

    #[test]
    fn outgoing_rejects_extra_field() {
        let line = r#"{"type":"event","body":{},"extra":1}"#;
        let err = PluginOutgoing::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::ExtraFields(ref v) if v == &vec!["extra".to_string()]),
            "got {err:?}"
        );
    }

    // ---- golden / canonical encoding ---------------------------------------

    #[test]
    fn golden_ready() {
        let env = Envelope::system(
            pn("mock-plugin"),
            ts(),
            SystemBody::Ready {
                protocol_version: "0.1".into(),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"mock-plugin","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready","protocol_version":"0.1"}}"#
        );
    }

    #[test]
    fn golden_ready_ok() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::ReadyOk {
                engine_version: "0.1.0".into(),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"ready_ok","engine_version":"0.1.0"}}"#
        );
    }

    #[test]
    fn golden_shutdown() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::Shutdown {
                reason: Some("user quit".into()),
                grace_ms: Some(2000),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"shutdown","reason":"user quit","grace_ms":2000}}"#
        );
    }

    #[test]
    fn golden_error() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::Error {
                code: ErrorCode::MalformedEnvelope,
                message: "body is not a JSON object".into(),
                offending: Some(Offending {
                    from: pn("p"),
                    ts: ts(),
                }),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"error","code":"malformed_envelope","message":"body is not a JSON object","offending":{"from":"p","ts":"2026-04-21T12:34:56.789Z"}}}"#
        );
    }
}
