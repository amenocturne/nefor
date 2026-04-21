//! JSON Lines parser for NCP envelopes.
//!
//! The engine's broker calls [`Envelope::parse_line`](crate::Envelope) or
//! [`PluginOutgoing::parse_line`](crate::PluginOutgoing) (re-exported from
//! this module via the inherent impls) and maps the returned [`ParseError`]
//! to a [`SystemBody::Error`](crate::SystemBody::Error) error code per §8.

use serde_json::{Map, Value};

use crate::envelope::{Body, Envelope, MessageKind, PluginOutgoing};
use crate::newtypes::{PluginName, Timestamp};
use crate::system::{ErrorCode, Offending, PluginLeftReason, SystemBody};

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

    /// Semantic or structural validation failure inside an `attach` body.
    /// Per NCP §8 this maps to `invalid_attach` and closes the connection.
    #[error("invalid attach: {0}")]
    InvalidAttachBody(#[source] InvalidAttachReason),

    /// Structural failure inside a non-attach system body. Per NCP §8 this
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

/// Why an `attach` body was rejected.
///
/// Covers both §5.1's semantic invariants (empty name, reserved identity,
/// malformed version strings) and structural faults (missing required
/// fields, wrong types, extra fields). Per §8 every one of these produces
/// `invalid_attach` and closes the connection.
#[derive(Debug, thiserror::Error)]
pub enum InvalidAttachReason {
    /// `name` was the empty string.
    #[error("`name` must not be empty")]
    EmptyName,
    /// `name` was the reserved `"engine"` identity.
    #[error("`name` must not be the reserved identity \"engine\"")]
    ReservedName,
    /// `version` did not parse as SemVer 2.0.0.
    #[error("`version` is not a valid SemVer 2.0.0 string: {raw:?}")]
    InvalidVersion {
        /// The offending raw string.
        raw: String,
        /// The underlying semver parse error.
        #[source]
        source: semver::Error,
    },
    /// `protocol_version` did not parse as SemVer 2.0.0 or `MAJOR.MINOR`.
    #[error("`protocol_version` must be SemVer 2.0.0 or MAJOR.MINOR shorthand: {raw:?}")]
    InvalidProtocolVersion {
        /// The offending raw string.
        raw: String,
    },
    /// Attach body failed structural validation (missing/extra/wrong-type
    /// field). Kept as a nested `InvalidBodyReason` so the vocabulary is
    /// shared with non-attach bodies.
    #[error("{0}")]
    Structural(#[source] InvalidBodyReason),
}

/// Identifies which system message kind a body-level structural error was
/// produced for. Printed verbatim in error messages and used for
/// broker-side diagnostics.
///
/// `Attach` is intentionally excluded — attach errors flow through
/// [`ParseError::InvalidAttachBody`] / [`InvalidAttachReason`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemBodyKind {
    /// `attach_ok` (§5.2).
    AttachOk,
    /// `detach` (§5.3).
    Detach,
    /// `plugin_joined` (§5.4).
    PluginJoined,
    /// `plugin_left` (§5.5).
    PluginLeft,
    /// `shutdown` (§5.6).
    Shutdown,
    /// `error` (§5.7).
    Error,
}

impl SystemBodyKind {
    /// Wire-format name (snake_case, matches the `kind` discriminant).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::AttachOk => "attach_ok",
            Self::Detach => "detach",
            Self::PluginJoined => "plugin_joined",
            Self::PluginLeft => "plugin_left",
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
    /// (e.g. `plugin_left.reason` or `error.code`).
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
        "attach" => parse_attach(obj).map_err(ParseError::InvalidAttachBody),
        "attach_ok" => parse_attach_ok(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::AttachOk,
            reason,
        }),
        "detach" => parse_detach(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::Detach,
            reason,
        }),
        "plugin_joined" => {
            parse_plugin_joined(obj).map_err(|reason| ParseError::InvalidSystemBody {
                kind: SystemBodyKind::PluginJoined,
                reason,
            })
        }
        "plugin_left" => parse_plugin_left(obj).map_err(|reason| ParseError::InvalidSystemBody {
            kind: SystemBodyKind::PluginLeft,
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
// applies any per-kind semantic invariants. They return a reason type the
// caller wraps into `ParseError` — that keeps the variant-mapping for
// attach-vs-other co-located with the dispatch in `parse_system_body`.

fn parse_attach(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidAttachReason> {
    let name = take_string(&mut obj, "name")
        .map_err(InvalidAttachReason::Structural)?
        .ok_or(InvalidAttachReason::Structural(
            InvalidBodyReason::MissingField("name"),
        ))?;
    let version = take_string(&mut obj, "version")
        .map_err(InvalidAttachReason::Structural)?
        .ok_or(InvalidAttachReason::Structural(
            InvalidBodyReason::MissingField("version"),
        ))?;
    let protocol_version = take_string(&mut obj, "protocol_version")
        .map_err(InvalidAttachReason::Structural)?
        .ok_or(InvalidAttachReason::Structural(
            InvalidBodyReason::MissingField("protocol_version"),
        ))?;
    reject_extras(obj).map_err(InvalidAttachReason::Structural)?;

    if name.is_empty() {
        return Err(InvalidAttachReason::EmptyName);
    }
    if name == "engine" {
        return Err(InvalidAttachReason::ReservedName);
    }
    if let Err(source) = semver::Version::parse(&version) {
        return Err(InvalidAttachReason::InvalidVersion {
            raw: version,
            source,
        });
    }
    // Spec §5.1 says protocol_version is "SemVer 2.0.0 format", but both
    // §5.1's own example (`"0.1"`) and §9's negotiation rule ("v0.1 engine
    // rejects any protocol_version other than \"0.1\"") use a two-part
    // form that strict SemVer would reject. Accept full SemVer OR the
    // `MAJOR.MINOR` shorthand to cover both the spec's prose and its
    // examples.
    if !is_valid_protocol_version(&protocol_version) {
        return Err(InvalidAttachReason::InvalidProtocolVersion {
            raw: protocol_version,
        });
    }

    Ok(SystemBody::Attach {
        name,
        version,
        protocol_version,
    })
}

fn parse_attach_ok(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let engine_version = take_string(&mut obj, "engine_version")?
        .ok_or(InvalidBodyReason::MissingField("engine_version"))?;
    reject_extras(obj)?;
    Ok(SystemBody::AttachOk { engine_version })
}

fn parse_detach(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let reason = take_string(&mut obj, "reason")?;
    reject_extras(obj)?;
    Ok(SystemBody::Detach { reason })
}

fn parse_plugin_joined(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let name = take_string(&mut obj, "name")?.ok_or(InvalidBodyReason::MissingField("name"))?;
    let version =
        take_string(&mut obj, "version")?.ok_or(InvalidBodyReason::MissingField("version"))?;
    reject_extras(obj)?;
    Ok(SystemBody::PluginJoined { name, version })
}

fn parse_plugin_left(mut obj: Map<String, Value>) -> Result<SystemBody, InvalidBodyReason> {
    let name = take_string(&mut obj, "name")?.ok_or(InvalidBodyReason::MissingField("name"))?;
    let reason_raw =
        take_string(&mut obj, "reason")?.ok_or(InvalidBodyReason::MissingField("reason"))?;
    reject_extras(obj)?;
    let reason = match reason_raw.as_str() {
        "detach" => PluginLeftReason::Detach,
        "disconnect" => PluginLeftReason::Disconnect,
        "crash" => PluginLeftReason::Crash,
        "evicted" => PluginLeftReason::Evicted,
        _ => {
            return Err(InvalidBodyReason::InvalidEnumValue {
                field: "reason",
                value: reason_raw,
                expected: "\"detach\", \"disconnect\", \"crash\", or \"evicted\"",
            })
        }
    };
    Ok(SystemBody::PluginLeft { name, reason })
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
        "name_taken" => ErrorCode::NameTaken,
        "invalid_attach" => ErrorCode::InvalidAttach,
        "malformed_envelope" => ErrorCode::MalformedEnvelope,
        "body_not_object" => ErrorCode::BodyNotObject,
        "unknown_kind" => ErrorCode::UnknownKind,
        "queue_overflow" => ErrorCode::QueueOverflow,
        "rate_limited" => ErrorCode::RateLimited,
        _ => {
            return Err(InvalidBodyReason::InvalidEnumValue {
                field: "code",
                value: code_raw,
                expected: "a §8 error code (e.g. \"malformed_envelope\", \"invalid_attach\")",
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
    use crate::system::{ErrorCode, Offending, PluginLeftReason};
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
    fn round_trip_attach() {
        roundtrip(SystemBody::Attach {
            name: "p".into(),
            version: "0.3.1".into(),
            protocol_version: "0.1.0".into(),
        });
    }

    #[test]
    fn round_trip_attach_ok() {
        // attach_ok is engine-stamped so the `from` is synthetic here;
        // on the wire the engine would use PluginName::engine().
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::AttachOk {
                engine_version: "0.1.0".into(),
            },
        );
        let line = env.to_line();
        let parsed = Envelope::parse_line(&line).expect("round-trip");
        assert_eq!(parsed, env);
    }

    #[test]
    fn round_trip_detach_with_reason() {
        roundtrip(SystemBody::Detach {
            reason: Some("user quit".into()),
        });
    }

    #[test]
    fn round_trip_detach_no_reason() {
        roundtrip(SystemBody::Detach { reason: None });
    }

    #[test]
    fn round_trip_plugin_joined() {
        roundtrip(SystemBody::PluginJoined {
            name: "peer".into(),
            version: "0.2.0".into(),
        });
    }

    #[test]
    fn round_trip_plugin_left() {
        for reason in [
            PluginLeftReason::Detach,
            PluginLeftReason::Disconnect,
            PluginLeftReason::Crash,
            PluginLeftReason::Evicted,
        ] {
            roundtrip(SystemBody::PluginLeft {
                name: "peer".into(),
                reason,
            });
        }
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

    // ---- attach body: semantic invariants ----------------------------------

    #[test]
    fn rejects_attach_with_engine_name() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"engine","version":"0.1.0","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::ReservedName)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_empty_name() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"","version":"0.1.0","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::EmptyName)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_invalid_semver() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":"not.a.version","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::InvalidVersion { ref raw, .. })
                    if raw == "not.a.version"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_invalid_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":"0.1.0","protocol_version":"not.a.version"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::InvalidProtocolVersion {
                    ref raw
                }) if raw == "not.a.version"
            ),
            "got {err:?}"
        );
    }

    // ---- attach body: structural errors ------------------------------------

    #[test]
    fn rejects_attach_missing_name() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","version":"0.1.0","protocol_version":"0.1"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::Structural(
                    InvalidBodyReason::MissingField("name")
                ))
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_wrong_type_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":42,"protocol_version":"0.1"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::Structural(
                    InvalidBodyReason::WrongFieldType {
                        field: "version",
                        expected: "string",
                    }
                ))
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_extra_field() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":"0.1.0","protocol_version":"0.1","bonus":"x"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidAttachBody(InvalidAttachReason::Structural(
                    InvalidBodyReason::ExtraField(ref f)
                )) if f == "bonus"
            ),
            "got {err:?}"
        );
    }

    // ---- non-attach body: structural errors --------------------------------

    #[test]
    fn rejects_plugin_left_missing_name() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"plugin_left","reason":"detach"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::PluginLeft,
                    reason: InvalidBodyReason::MissingField("name"),
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_plugin_left_invalid_reason() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"plugin_left","name":"p","reason":"retired"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::PluginLeft,
                    reason: InvalidBodyReason::InvalidEnumValue {
                        field: "reason",
                        ref value,
                        ..
                    },
                } if value == "retired"
            ),
            "got {err:?}"
        );
    }

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
    fn rejects_attach_ok_extra_field() {
        let line = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach_ok","engine_version":"0.1.0","bonus":1}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::AttachOk,
                    reason: InvalidBodyReason::ExtraField(ref f),
                } if f == "bonus"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_detach_wrong_reason_type() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"detach","reason":42}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidSystemBody {
                    kind: SystemBodyKind::Detach,
                    reason: InvalidBodyReason::WrongFieldType {
                        field: "reason",
                        ..
                    },
                }
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
    fn display_invalid_attach_empty_name() {
        let err = ParseError::InvalidAttachBody(InvalidAttachReason::EmptyName);
        assert_eq!(err.to_string(), "invalid attach: `name` must not be empty");
    }

    #[test]
    fn display_invalid_attach_reserved_name() {
        let err = ParseError::InvalidAttachBody(InvalidAttachReason::ReservedName);
        assert_eq!(
            err.to_string(),
            "invalid attach: `name` must not be the reserved identity \"engine\""
        );
    }

    #[test]
    fn display_invalid_attach_invalid_version() {
        let source = semver::Version::parse("not.a.version").unwrap_err();
        let err = ParseError::InvalidAttachBody(InvalidAttachReason::InvalidVersion {
            raw: "not.a.version".into(),
            source,
        });
        assert_eq!(
            err.to_string(),
            "invalid attach: `version` is not a valid SemVer 2.0.0 string: \"not.a.version\""
        );
    }

    #[test]
    fn display_invalid_attach_invalid_protocol_version() {
        let err = ParseError::InvalidAttachBody(InvalidAttachReason::InvalidProtocolVersion {
            raw: "not.a.version".into(),
        });
        assert_eq!(
            err.to_string(),
            "invalid attach: `protocol_version` must be SemVer 2.0.0 or MAJOR.MINOR shorthand: \"not.a.version\""
        );
    }

    #[test]
    fn display_invalid_attach_structural_missing() {
        let err = ParseError::InvalidAttachBody(InvalidAttachReason::Structural(
            InvalidBodyReason::MissingField("name"),
        ));
        assert_eq!(
            err.to_string(),
            "invalid attach: missing required field `name`"
        );
    }

    #[test]
    fn display_invalid_system_body_missing_field() {
        let err = ParseError::InvalidSystemBody {
            kind: SystemBodyKind::PluginLeft,
            reason: InvalidBodyReason::MissingField("name"),
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `plugin_left`: missing required field `name`"
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
            kind: SystemBodyKind::AttachOk,
            reason: InvalidBodyReason::ExtraField("bonus".into()),
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `attach_ok`: unexpected field `bonus`"
        );
    }

    #[test]
    fn display_invalid_system_body_enum_value() {
        let err = ParseError::InvalidSystemBody {
            kind: SystemBodyKind::PluginLeft,
            reason: InvalidBodyReason::InvalidEnumValue {
                field: "reason",
                value: "retired".into(),
                expected: "\"detach\", \"disconnect\", \"crash\", or \"evicted\"",
            },
        };
        assert_eq!(
            err.to_string(),
            "invalid system body for kind `plugin_left`: field `reason` has invalid value \"retired\" (expected one of \"detach\", \"disconnect\", \"crash\", or \"evicted\")"
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
        let out = PluginOutgoing::system(SystemBody::Attach {
            name: "p".into(),
            version: "0.1.0".into(),
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
    fn golden_attach() {
        let env = Envelope::system(
            pn("mock-plugin"),
            ts(),
            SystemBody::Attach {
                name: "mock-plugin".into(),
                version: "0.3.1".into(),
                protocol_version: "0.1".into(),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"mock-plugin","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"mock-plugin","version":"0.3.1","protocol_version":"0.1"}}"#
        );
    }

    #[test]
    fn golden_attach_ok() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::AttachOk {
                engine_version: "0.1.0".into(),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach_ok","engine_version":"0.1.0"}}"#
        );
    }

    #[test]
    fn golden_detach_with_reason() {
        let env = Envelope::system(
            pn("p"),
            ts(),
            SystemBody::Detach {
                reason: Some("user quit".into()),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"detach","reason":"user quit"}}"#
        );
    }

    #[test]
    fn golden_detach_no_reason() {
        let env = Envelope::system(pn("p"), ts(), SystemBody::Detach { reason: None });
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"detach"}}"#
        );
    }

    #[test]
    fn golden_plugin_joined() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::PluginJoined {
                name: "peer".into(),
                version: "0.2.0".into(),
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"plugin_joined","name":"peer","version":"0.2.0"}}"#
        );
    }

    #[test]
    fn golden_plugin_left() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::PluginLeft {
                name: "peer".into(),
                reason: PluginLeftReason::Crash,
            },
        );
        assert_eq!(
            env.to_line(),
            r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"plugin_left","name":"peer","reason":"crash"}}"#
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
