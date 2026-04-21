//! System message bodies (spec §5).
//!
//! Each variant of [`SystemBody`] maps one-to-one to a recognized system
//! `kind` from §5.1–§5.7. Adding a variant is a spec change — the enum is
//! intentionally closed, not `#[non_exhaustive]`.

use serde::{Deserialize, Serialize};

use crate::newtypes::{PluginName, Timestamp};

/// Validated body for `type: "system"` envelopes.
///
/// Serde tags on `kind` drive JSON encoding: `{"kind": "...", ...}`. The
/// enum is closed; extending NCP with a new system message is a minor
/// version bump (§9) that requires a new variant here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemBody {
    /// §5.1 — plugin → engine. First message a plugin sends after
    /// connecting; announces identity and protocol version.
    Attach {
        /// The plugin's identity claim (§5.1). Validated on parse:
        /// non-empty and not `"engine"`.
        name: String,
        /// Plugin version (SemVer 2.0.0). Validated on parse.
        version: String,
        /// NCP protocol version the plugin implements (SemVer 2.0.0).
        protocol_version: String,
    },
    /// §5.2 — engine → plugin. Accepts the attach.
    AttachOk {
        /// Engine implementation version (SemVer 2.0.0, not validated
        /// here — the plugin receiver decides what to do with it).
        engine_version: String,
    },
    /// §5.3 — plugin → engine. Graceful shutdown signal.
    Detach {
        /// Free-form explanation; logged and forwarded in `plugin_left`.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// §5.4 — engine → all. A new plugin has successfully attached.
    PluginJoined {
        /// The joining plugin's name.
        name: String,
        /// The joining plugin's declared version.
        version: String,
    },
    /// §5.5 — engine → all. A plugin has disconnected.
    PluginLeft {
        /// The departed plugin's name.
        name: String,
        /// Categorical reason for departure.
        reason: PluginLeftReason,
    },
    /// §5.6 — engine → all. The engine is shutting down.
    Shutdown {
        /// Free-form explanation of why the engine is shutting down.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Milliseconds between this broadcast and forced connection close.
        #[serde(skip_serializing_if = "Option::is_none")]
        grace_ms: Option<u64>,
    },
    /// §5.7 — engine → one plugin. Protocol-level error report.
    Error {
        /// Machine-readable error code drawn from §8.
        code: ErrorCode,
        /// Human-readable explanation.
        message: String,
        /// Delivery facts of the offending message, if attributable.
        #[serde(skip_serializing_if = "Option::is_none")]
        offending: Option<Offending>,
    },
}

/// Categorical reason carried by `plugin_left` (§5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginLeftReason {
    /// Plugin sent a graceful `detach` before closing.
    Detach,
    /// Connection closed without a `detach`.
    Disconnect,
    /// Plugin process terminated abnormally.
    Crash,
    /// Engine closed the connection for non-protocol reasons.
    Evicted,
}

/// Error codes carried by `error.code` (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Plugin's declared `protocol_version` is not supported (closes).
    ProtocolVersionMismatch,
    /// Another plugin with the same name is currently attached (closes).
    NameTaken,
    /// Attach body missing fields, wrong types, or malformed version
    /// strings (closes).
    InvalidAttach,
    /// Received line is not valid JSON, or has missing/forbidden envelope
    /// fields.
    MalformedEnvelope,
    /// Envelope's `body` is not a JSON object.
    BodyNotObject,
    /// System-typed message has an unrecognized `kind`.
    UnknownKind,
    /// Plugin's receive queue was full; engine dropped a message.
    QueueOverflow,
    /// Plugin exceeded per-connection rate limits.
    RateLimited,
}

/// Delivery-fact pair identifying the message that caused an [`ErrorCode`].
/// Absent when the error cannot be attributed to a specific message
/// (e.g. connection-level framing errors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offending {
    /// Identity the engine assigned (or would have assigned) to the
    /// offending message.
    pub from: PluginName,
    /// Timestamp the engine stamped (or would have stamped).
    pub ts: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_serializes_as_snake_case_kind() {
        let body = SystemBody::Attach {
            name: "mock-plugin".into(),
            version: "0.1.0".into(),
            protocol_version: "0.1".into(),
        };
        let json = serde_json::to_value(&body).expect("serialize");
        assert_eq!(json["kind"], "attach");
        assert_eq!(json["name"], "mock-plugin");
    }

    #[test]
    fn detach_omits_none_reason() {
        let body = SystemBody::Detach { reason: None };
        let json = serde_json::to_string(&body).expect("serialize");
        assert_eq!(json, r#"{"kind":"detach"}"#);
    }

    #[test]
    fn shutdown_omits_none_fields() {
        let body = SystemBody::Shutdown {
            reason: None,
            grace_ms: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert_eq!(json, r#"{"kind":"shutdown"}"#);
    }

    #[test]
    fn plugin_left_reason_round_trips() {
        let body = SystemBody::PluginLeft {
            name: "p".into(),
            reason: PluginLeftReason::Crash,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(json.contains(r#""reason":"crash""#));
        let back: SystemBody = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, body);
    }

    #[test]
    fn error_code_round_trips() {
        for code in [
            ErrorCode::ProtocolVersionMismatch,
            ErrorCode::NameTaken,
            ErrorCode::InvalidAttach,
            ErrorCode::MalformedEnvelope,
            ErrorCode::BodyNotObject,
            ErrorCode::UnknownKind,
            ErrorCode::QueueOverflow,
            ErrorCode::RateLimited,
        ] {
            let json = serde_json::to_string(&code).expect("serialize");
            let back: ErrorCode = serde_json::from_str(&json).expect("parse");
            assert_eq!(back, code);
        }
    }

    #[test]
    fn unknown_kind_is_rejected_by_serde() {
        let raw = r#"{"kind":"not_real"}"#;
        let err = serde_json::from_str::<SystemBody>(raw).unwrap_err();
        assert!(err.to_string().contains("not_real") || err.to_string().contains("unknown"));
    }

    #[test]
    fn system_body_rejects_unknown_fields() {
        let raw = r#"{"kind":"detach","reason":"x","extra":1}"#;
        let err = serde_json::from_str::<SystemBody>(raw).unwrap_err();
        assert!(err.to_string().contains("extra"));
    }
}
