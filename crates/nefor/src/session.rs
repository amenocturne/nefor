//! In-memory log-entry types passed between the broker and the Lua VM.
//!
//! Post-session-blind refactor the engine no longer persists anything to
//! disk. The broker stamps every inbound and outbound envelope into a
//! [`LogEntry`] and hands it to the Lua dispatch hook (and to bus
//! subscribers); persistence to a per-session jsonl file lives entirely in
//! `starter/sessions.lua`. The module name `session` is a historical
//! artefact — the types here aren't session-scoped, they're just the
//! engine's log-line shape.
//!
//! Kept separate from `nefor-protocol`'s envelope types because the broker
//! deliberately does not parse envelope bodies: `payload` is a raw line.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use nefor_protocol::{PluginName, Timestamp};

/// Where a given line in the log came from.
///
/// `Origin::Plugin(name)` serializes as the plugin's name; `Origin::Step`
/// serializes as the literal `"step"`. `PluginName` construction reserves
/// `"step"` so the two wire forms can never collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Message came from a plugin's stdout (inbound).
    Plugin(PluginName),
    /// Message came from the Lua dispatch hook (outbound).
    Step,
}

const STEP_ORIGIN: &str = "step";

impl Serialize for Origin {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Origin::Plugin(name) => s.serialize_str(name.as_str()),
            Origin::Step => s.serialize_str(STEP_ORIGIN),
        }
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        if raw == STEP_ORIGIN {
            return Ok(Origin::Step);
        }
        // On-wire deserialization must accept any plugin identity the engine
        // actually stamped, including reserved names like "engine" (which
        // `PluginName::new` rejects at spawn time but the protocol-level
        // deserializer still admits). Using the serde path matches that
        // behavior.
        let pn: PluginName =
            serde_json::from_value(Value::String(raw)).map_err(serde::de::Error::custom)?;
        Ok(Origin::Plugin(pn))
    }
}

/// One inbound or outbound message, as held in the broker's in-memory
/// event log and surfaced to Lua subscribers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Engine-stamped wall clock (ISO-8601 UTC, ms precision).
    pub ts: Timestamp,
    /// Who produced the line.
    pub origin: Origin,
    /// For `Origin::Step` emissions, the intended plugin target (if any).
    /// `None` means broadcast or untargeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PluginName>,
    /// Raw line text — verbatim, with no trailing newline.
    pub payload: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_plugin(name: &str) -> PluginName {
        PluginName::new(name).expect("valid plugin name")
    }

    fn mk_ts(s: &str) -> Timestamp {
        Timestamp::parse(s).expect("valid ts")
    }

    #[test]
    fn origin_plugin_serializes_as_plain_string() {
        let entry = LogEntry {
            ts: mk_ts("2026-04-23T12:00:00.000Z"),
            origin: Origin::Plugin(mk_plugin("mock-plugin")),
            target: None,
            payload: "hi".into(),
        };
        let line = serde_json::to_string(&entry).expect("serialize");
        assert!(
            line.contains(r#""origin":"mock-plugin""#),
            "plugin origin should be a bare string: {line}"
        );
    }

    #[test]
    fn origin_step_serializes_as_literal_step() {
        let entry = LogEntry {
            ts: mk_ts("2026-04-23T12:00:00.000Z"),
            origin: Origin::Step,
            target: None,
            payload: "hi".into(),
        };
        let line = serde_json::to_string(&entry).expect("serialize");
        assert!(
            line.contains(r#""origin":"step""#),
            "step origin should be the literal \"step\": {line}"
        );
    }

    #[test]
    fn origin_target_omitted_when_none() {
        let entry = LogEntry {
            ts: mk_ts("2026-04-23T12:00:00.000Z"),
            origin: Origin::Step,
            target: None,
            payload: "hi".into(),
        };
        let line = serde_json::to_string(&entry).expect("serialize");
        assert!(
            !line.contains("\"target\""),
            "target=None must be omitted: {line}"
        );
    }

    #[test]
    fn origin_target_present_when_some() {
        let entry = LogEntry {
            ts: mk_ts("2026-04-23T12:00:00.000Z"),
            origin: Origin::Step,
            target: Some(mk_plugin("nefor-tui")),
            payload: "hi".into(),
        };
        let line = serde_json::to_string(&entry).expect("serialize");
        assert!(
            line.contains(r#""target":"nefor-tui""#),
            "target=Some must serialize: {line}"
        );
    }

    #[test]
    fn plugin_name_reserves_step() {
        // Pin the assumption that `"step"` cannot be spawned as a plugin
        // name, so `Origin::Plugin(...)` and `Origin::Step` cannot collide
        // at runtime.
        assert!(PluginName::new("step").is_err());
    }
}
