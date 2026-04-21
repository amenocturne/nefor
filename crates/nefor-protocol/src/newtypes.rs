//! Newtypes for NCP envelope identifiers.
//!
//! Per spec §3 every envelope carries a `from` (plugin identity) and a `ts`
//! (engine-stamped wall clock). Wrapping them in distinct types prevents
//! accidental field-swap bugs when constructing or destructuring envelopes,
//! and lets us attach parse-time invariants to each.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

/// A plugin's wire identity, as carried in `envelope.from`.
///
/// Construction via [`PluginName::new`] rejects the empty string and the
/// reserved literal `"engine"` (spec §3 "Reserved `from` identity").
/// Deserialization applies the same rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginName(String);

/// Error returned when a [`PluginName`] fails validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PluginNameError {
    /// The name was the empty string.
    #[error("plugin name must not be empty")]
    Empty,
    /// The name was the reserved literal `"engine"`.
    #[error("plugin name \"engine\" is reserved for the engine itself")]
    Reserved,
}

impl PluginName {
    /// Construct a [`PluginName`], rejecting the empty string and the
    /// reserved `"engine"` identity.
    pub fn new(name: impl Into<String>) -> Result<Self, PluginNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(PluginNameError::Empty);
        }
        if name == "engine" {
            return Err(PluginNameError::Reserved);
        }
        Ok(Self(name))
    }

    /// Construct the reserved engine identity. Only the engine should ever
    /// produce this; plugins MUST NOT attach under this name.
    pub fn engine() -> Self {
        Self(String::from("engine"))
    }

    /// View the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns true if this is the reserved engine identity.
    pub fn is_engine(&self) -> bool {
        self.0 == "engine"
    }
}

impl std::fmt::Display for PluginName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PluginName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PluginName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        // Accept "engine" on the wire — the engine stamps its own messages
        // with from:"engine", and plugin receivers need to parse those.
        // Plugin-name construction via `PluginName::new` still rejects it
        // for plugin-authored attach payloads (see SystemBody::Attach
        // parsing in parse.rs).
        if raw.is_empty() {
            return Err(serde::de::Error::custom(PluginNameError::Empty));
        }
        Ok(PluginName(raw))
    }
}

/// Engine-stamped timestamp (spec §3: ISO-8601 UTC, millisecond precision).
///
/// Wraps [`time::OffsetDateTime`]. Serializes and deserializes as an
/// ISO-8601 string (not the `time` default epoch encoding). Millisecond
/// precision is enforced on output by truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

/// Error returned when parsing a [`Timestamp`] from a string fails.
#[derive(Debug, thiserror::Error)]
#[error("invalid ISO-8601 UTC timestamp: {0}")]
pub struct TimestampParseError(String);

impl Timestamp {
    /// Wrap an [`OffsetDateTime`] truncated to millisecond precision.
    /// The spec mandates millisecond precision on the wire; truncating at
    /// construction guarantees round-trip stability regardless of how the
    /// caller obtained the value.
    pub fn from_offset(dt: OffsetDateTime) -> Self {
        let ns = dt.nanosecond();
        let truncated_ns = (ns / 1_000_000) * 1_000_000;
        // Replacing nanoseconds with a value in [0, 1e9) cannot overflow a
        // valid OffsetDateTime — fall back to the original on the
        // theoretically-impossible error to avoid panicking.
        let dt = dt.replace_nanosecond(truncated_ns).unwrap_or(dt);
        Self(dt)
    }

    /// Current UTC wall clock, truncated to milliseconds.
    pub fn now() -> Self {
        Self::from_offset(OffsetDateTime::now_utc())
    }

    /// Access the inner [`OffsetDateTime`].
    pub fn as_offset(&self) -> OffsetDateTime {
        self.0
    }

    /// Parse an ISO-8601 UTC timestamp.
    pub fn parse(s: &str) -> Result<Self, TimestampParseError> {
        let dt = OffsetDateTime::parse(s, &Iso8601::DEFAULT)
            .map_err(|e| TimestampParseError(e.to_string()))?;
        Ok(Self::from_offset(dt.to_offset(time::UtcOffset::UTC)))
    }

    /// Format as ISO-8601 UTC with millisecond precision.
    pub fn to_iso8601(&self) -> String {
        // Hand-rolled formatting so we get the exact canonical shape the
        // spec names: `YYYY-MM-DDTHH:MM:SS.mmmZ`. time::Iso8601 with
        // default config also emits a variable number of subsecond digits
        // depending on the value, which would break the §10 canonical
        // encoding guarantee.
        let dt = self.0;
        let ms = dt.nanosecond() / 1_000_000;
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            dt.year(),
            u8::from(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
            ms
        )
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_iso8601())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_iso8601())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Timestamp::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_rejects_empty() {
        assert_eq!(PluginName::new(""), Err(PluginNameError::Empty));
    }

    #[test]
    fn plugin_name_rejects_engine() {
        assert_eq!(PluginName::new("engine"), Err(PluginNameError::Reserved));
    }

    #[test]
    fn plugin_name_accepts_normal() {
        let n = PluginName::new("mock-plugin").expect("valid");
        assert_eq!(n.as_str(), "mock-plugin");
        assert!(!n.is_engine());
    }

    #[test]
    fn plugin_name_engine_constructor() {
        let e = PluginName::engine();
        assert!(e.is_engine());
        assert_eq!(e.as_str(), "engine");
    }

    #[test]
    fn plugin_name_deserialize_accepts_engine() {
        // The wire-level deserializer accepts "engine" because that's what
        // the engine stamps on its own messages; plugin-level name
        // validation happens in attach-body parsing.
        let n: PluginName =
            serde_json::from_str(r#""engine""#).expect("engine is valid on the wire");
        assert!(n.is_engine());
    }

    #[test]
    fn plugin_name_deserialize_rejects_empty() {
        let err = serde_json::from_str::<PluginName>(r#""""#).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn timestamp_round_trip() {
        let s = "2026-04-21T12:34:56.789Z";
        let t = Timestamp::parse(s).expect("valid");
        assert_eq!(t.to_iso8601(), s);
    }

    #[test]
    fn timestamp_truncates_submillisecond() {
        let s = "2026-04-21T12:34:56.789123456Z";
        let t = Timestamp::parse(s).expect("valid");
        assert_eq!(t.to_iso8601(), "2026-04-21T12:34:56.789Z");
    }

    #[test]
    fn timestamp_rejects_junk() {
        assert!(Timestamp::parse("nope").is_err());
    }

    #[test]
    fn timestamp_serde_is_string() {
        let t = Timestamp::parse("2026-04-21T12:34:56.789Z").expect("valid");
        let json = serde_json::to_string(&t).expect("serialize");
        assert_eq!(json, r#""2026-04-21T12:34:56.789Z""#);
        let back: Timestamp = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, t);
    }
}
