//! JSON Lines parser for NCP envelopes.
//!
//! The engine's broker calls [`Envelope::parse_line`](crate::Envelope) or
//! [`PluginOutgoing::parse_line`](crate::PluginOutgoing) (re-exported from
//! this module via the inherent impls) and maps the returned [`ParseError`]
//! to a [`SystemBody::Error`](crate::SystemBody::Error) error code per §8.

use serde_json::{Map, Value};

use crate::envelope::{Body, Envelope, MessageKind, PluginOutgoing};
use crate::newtypes::{PluginName, Timestamp};
use crate::system::SystemBody;

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

    /// `type` was `"system"` and the body either failed structural
    /// validation (missing fields, extra fields, wrong types) or one of
    /// §5.1's semantic invariants (empty name, reserved `engine` name,
    /// invalid SemVer in attach). Maps to `invalid_attach` for attach
    /// bodies and `malformed_envelope` otherwise. The message carries
    /// enough detail for operator diagnostics.
    #[error("invalid system body: {0}")]
    InvalidSystemBody(String),

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
        // Use the deserializer path so "engine" is accepted on the wire
        // (the engine stamps from:"engine" on its own messages).
        Some(
            serde_json::from_value::<PluginName>(Value::String(s))
                .map_err(|e| ParseError::InvalidSystemBody(e.to_string()))?,
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

fn parse_system_body(obj: Map<String, Value>) -> Result<SystemBody, ParseError> {
    // Peek `kind` first so we can emit the distinct UnknownKind error
    // instead of a generic "invalid system body" when the caller just
    // used a typo.
    let kind = match obj.get("kind") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(ParseError::InvalidSystemBody(
                "`kind` must be a string".into(),
            ))
        }
        None => return Err(ParseError::InvalidSystemBody("missing `kind`".into())),
    };

    // Validate `kind` is in the spec vocabulary before handing to serde,
    // so unknown kinds get the clean UnknownKind variant.
    match kind.as_str() {
        "attach" | "attach_ok" | "detach" | "plugin_joined" | "plugin_left" | "shutdown"
        | "error" => {}
        _ => return Err(ParseError::UnknownKind(kind)),
    }

    // Re-serialize and let serde do the structural validation (missing
    // fields, wrong types, extra fields via deny_unknown_fields).
    let value = Value::Object(obj);
    let body: SystemBody =
        serde_json::from_value(value).map_err(|e| ParseError::InvalidSystemBody(e.to_string()))?;

    // Apply §5.1 semantic invariants for attach bodies.
    if let SystemBody::Attach {
        name,
        version,
        protocol_version,
    } = &body
    {
        if name.is_empty() {
            return Err(ParseError::InvalidSystemBody(
                "attach.name must not be empty".into(),
            ));
        }
        if name == "engine" {
            return Err(ParseError::InvalidSystemBody(
                "attach.name \"engine\" is reserved".into(),
            ));
        }
        if semver::Version::parse(version).is_err() {
            return Err(ParseError::InvalidSystemBody(format!(
                "attach.version {version:?} is not valid SemVer 2.0.0"
            )));
        }
        // Spec §5.1 says protocol_version is "SemVer 2.0.0 format", but
        // both §5.1's own example (`"0.1"`) and §9's negotiation rule
        // ("v0.1 engine rejects any protocol_version other than \"0.1\"")
        // use a two-part form that strict SemVer would reject. Accept
        // full SemVer OR the `MAJOR.MINOR` shorthand to cover both the
        // spec's prose and its examples.
        if !is_valid_protocol_version(protocol_version) {
            return Err(ParseError::InvalidSystemBody(format!(
                "attach.protocol_version {protocol_version:?} is not a valid SemVer 2.0.0 or MAJOR.MINOR string"
            )));
        }
    }

    Ok(body)
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
    fn rejects_attach_with_engine_name() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"engine","version":"0.1.0","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidSystemBody(ref m) if m.contains("reserved")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_empty_name() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"","version":"0.1.0","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidSystemBody(ref m) if m.contains("empty")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_invalid_semver() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":"not.a.version","protocol_version":"0.1.0"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidSystemBody(ref m) if m.contains("SemVer")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_attach_with_invalid_protocol_version() {
        let line = r#"{"type":"system","from":"p","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach","name":"p","version":"0.1.0","protocol_version":"not.a.version"}}"#;
        let err = Envelope::parse_line(line).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidSystemBody(ref m) if m.contains("SemVer")),
            "got {err:?}"
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
