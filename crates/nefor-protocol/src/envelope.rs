//! NCP envelope types (spec §3).
//!
//! Two wire-level shapes live here:
//!
//! - [`Envelope`] — fully-stamped `{type, from, ts, body}` as seen by plugin
//!   receivers and produced by the engine broadcaster.
//! - [`PluginOutgoing`] — the reduced `{type, body}` form a plugin emits;
//!   the engine stamps `from` and `ts` on receive.
//!
//! Both are serialized compactly to JSON Lines via [`Envelope::to_line`] /
//! [`PluginOutgoing::to_line`], and parsed via [`parse_line`] helpers in
//! [`crate::parse`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::newtypes::{PluginName, Timestamp};
use crate::system::SystemBody;

/// The two values spec §3 allows for the envelope `type` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Body follows a shape defined in §5.
    System,
    /// Body is plugin-authored; opaque to the engine.
    Event,
}

/// Envelope body — either a validated system body or an opaque event
/// object. Spec §3 requires `body` to be a JSON object in both cases, which
/// the [`Body::Event`] variant preserves by using [`serde_json::Map`].
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// A recognized system message body. See [`SystemBody`] for the §5
    /// vocabulary.
    System(SystemBody),
    /// Opaque event body. Must be a JSON object (§3); this variant
    /// guarantees that by construction.
    Event(Map<String, Value>),
}

/// A fully-stamped NCP envelope on the wire.
///
/// This is what engine broadcasts look like, and what plugin receivers
/// observe. Plugins produce [`PluginOutgoing`] instead and let the engine
/// stamp `from` / `ts`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// `type` on the wire (renamed to avoid Rust's `type` keyword).
    pub kind: MessageKind,
    /// Engine-stamped sender identity.
    pub from: PluginName,
    /// Engine-stamped wall-clock timestamp.
    pub ts: Timestamp,
    /// Validated body.
    pub body: Body,
}

/// A plugin-sent envelope (§3 "Plugin-sent vs engine-broadcast envelopes").
/// Only `type` and `body` are plugin-authored; the engine fills in `from`
/// and `ts` before broadcast.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginOutgoing {
    /// `type` on the wire.
    pub kind: MessageKind,
    /// Body (validated system or opaque event).
    pub body: Body,
}

impl Envelope {
    /// Construct a system envelope with a validated body.
    pub fn system(from: PluginName, ts: Timestamp, body: SystemBody) -> Self {
        Self {
            kind: MessageKind::System,
            from,
            ts,
            body: Body::System(body),
        }
    }

    /// Construct an event envelope.
    pub fn event(from: PluginName, ts: Timestamp, body: Map<String, Value>) -> Self {
        Self {
            kind: MessageKind::Event,
            from,
            ts,
            body: Body::Event(body),
        }
    }

    /// Serialize to a single JSON line (no trailing newline — caller adds
    /// the `\n` frame boundary).
    pub fn to_line(&self) -> String {
        let v = encode_envelope(self);
        // serde_json::to_string on a serde_json::Value cannot fail: all
        // values are representable as JSON by construction.
        serde_json::to_string(&v).unwrap_or_else(|_| String::from("{}"))
    }
}

impl PluginOutgoing {
    /// Construct a system outgoing message.
    pub fn system(body: SystemBody) -> Self {
        Self {
            kind: MessageKind::System,
            body: Body::System(body),
        }
    }

    /// Construct an event outgoing message.
    pub fn event(body: Map<String, Value>) -> Self {
        Self {
            kind: MessageKind::Event,
            body: Body::Event(body),
        }
    }

    /// Serialize to a single JSON line.
    pub fn to_line(&self) -> String {
        let v = encode_outgoing(self);
        serde_json::to_string(&v).unwrap_or_else(|_| String::from("{}"))
    }
}

// --- internal: Value-based canonical encoding -------------------------------
//
// We build a serde_json::Value by hand rather than deriving Serialize on
// Envelope. The reason: `body` must be either the SystemBody's flattened
// JSON object *or* the Event's opaque Map, both serialized as plain objects
// under the single "body" key. Rust enums don't encode that way without an
// adjacently-tagged representation that would leak extra fields into
// `body`.

fn encode_body(body: &Body) -> Value {
    match body {
        Body::System(sys) => {
            // SystemBody derives Serialize with #[serde(tag = "kind")], so
            // it already encodes to a JSON object with a `kind` key.
            serde_json::to_value(sys).unwrap_or(Value::Object(Map::new()))
        }
        Body::Event(map) => Value::Object(map.clone()),
    }
}

fn encode_envelope(env: &Envelope) -> Value {
    // Canonical key order: type, from, ts, body — matches the §3 listing
    // order. serde_json preserves insertion order for Map, so downstream
    // golden tests see a stable shape.
    let mut obj = Map::with_capacity(4);
    obj.insert(
        String::from("type"),
        serde_json::to_value(env.kind).unwrap_or(Value::Null),
    );
    obj.insert(
        String::from("from"),
        Value::String(env.from.as_str().to_owned()),
    );
    obj.insert(String::from("ts"), Value::String(env.ts.to_iso8601()));
    obj.insert(String::from("body"), encode_body(&env.body));
    Value::Object(obj)
}

fn encode_outgoing(out: &PluginOutgoing) -> Value {
    let mut obj = Map::with_capacity(2);
    obj.insert(
        String::from("type"),
        serde_json::to_value(out.kind).unwrap_or(Value::Null),
    );
    obj.insert(String::from("body"), encode_body(&out.body));
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::newtypes::PluginName;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-21T12:34:56.789Z").expect("valid")
    }

    #[test]
    fn system_envelope_encodes_canonically() {
        let env = Envelope::system(
            PluginName::new("mock-plugin").expect("valid"),
            ts(),
            SystemBody::Detach {
                reason: Some("user quit".into()),
            },
        );
        let line = env.to_line();
        assert_eq!(
            line,
            r#"{"type":"system","from":"mock-plugin","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"detach","reason":"user quit"}}"#
        );
    }

    #[test]
    fn event_envelope_preserves_opaque_body() {
        let mut body = Map::new();
        body.insert(String::from("custom"), Value::String("payload".into()));
        body.insert(
            String::from("nested"),
            serde_json::json!({"a": 1, "b": [true, null]}),
        );
        let env = Envelope::event(PluginName::new("p").expect("valid"), ts(), body);
        let line = env.to_line();
        assert!(line.contains(r#""type":"event""#));
        assert!(line.contains(r#""custom":"payload""#));
        assert!(line.contains(r#""nested":{"a":1,"b":[true,null]}"#));
    }

    #[test]
    fn engine_sender_allowed() {
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::AttachOk {
                engine_version: "0.1.0".into(),
            },
        );
        let line = env.to_line();
        assert!(line.contains(r#""from":"engine""#));
    }

    #[test]
    fn outgoing_encodes_only_type_and_body() {
        let out = PluginOutgoing::system(SystemBody::Attach {
            name: "p".into(),
            version: "0.1.0".into(),
            protocol_version: "0.1".into(),
        });
        let line = out.to_line();
        assert_eq!(
            line,
            r#"{"type":"system","body":{"kind":"attach","name":"p","version":"0.1.0","protocol_version":"0.1"}}"#
        );
    }
}
