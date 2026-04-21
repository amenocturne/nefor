//! Human-readable logging of NCP messages to stderr.
//!
//! Every received line becomes one stderr row:
//!
//! ```text
//! <ts> <from> <type>: <summary>
//! ```
//!
//! For system bodies, the summary names the kind plus the distinguishing
//! field (name, code, reason…). For event bodies, the summary names the
//! `kind` field if present, else `<opaque>`.

use nefor_protocol::{Body, Envelope, MessageKind, SystemBody};

/// Format an envelope as a single human-readable line (no trailing newline).
pub fn format_envelope(env: &Envelope) -> String {
    let type_str = match env.kind {
        MessageKind::System => "system",
        MessageKind::Event => "event",
    };
    format!(
        "{ts} {from:<16} {typ}: {summary}",
        ts = env.ts,
        from = env.from,
        typ = type_str,
        summary = summarize_body(&env.body),
    )
}

/// Format a raw (not-yet-parsed) line for the "couldn't parse" case. The
/// harness still shows the line so developers can see what the plugin
/// emitted.
pub fn format_unparseable(raw: &str, reason: &str) -> String {
    format!("<unparseable> {reason}: {raw}")
}

fn summarize_body(body: &Body) -> String {
    match body {
        Body::System(sys) => summarize_system(sys),
        Body::Event(map) => {
            // Event bodies are opaque — but in the nefor ecosystem the
            // convention is to carry `kind` for dispatch. Surface it if
            // present so developers see what's flowing.
            match map.get("kind") {
                Some(serde_json::Value::String(k)) => format!("kind={k}"),
                _ => String::from("<opaque>"),
            }
        }
    }
}

fn summarize_system(sys: &SystemBody) -> String {
    match sys {
        SystemBody::Ready { protocol_version } => {
            format!("ready protocol={protocol_version}")
        }
        SystemBody::ReadyOk { engine_version } => {
            format!("ready_ok engine_version={engine_version}")
        }
        SystemBody::Shutdown { reason, grace_ms } => {
            let r = reason.as_deref().unwrap_or("");
            let g = grace_ms.map(|m| m.to_string()).unwrap_or_default();
            format!("shutdown reason={r:?} grace_ms={g}")
        }
        SystemBody::Error {
            code,
            message,
            offending,
        } => match offending {
            Some(o) => format!(
                "error code={code:?} message={message:?} offending={}@{}",
                o.from, o.ts
            ),
            None => format!("error code={code:?} message={message:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{PluginName, Timestamp};

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-21T12:34:56.789Z").expect("valid")
    }

    #[test]
    fn formats_system_ready() {
        let env = Envelope::system(
            PluginName::new("nefor-tui").expect("valid"),
            ts(),
            SystemBody::Ready {
                protocol_version: "0.1".into(),
            },
        );
        let line = format_envelope(&env);
        assert!(line.contains("nefor-tui"));
        assert!(line.contains("ready"));
        assert!(line.contains("protocol=0.1"));
    }

    #[test]
    fn formats_event_with_kind() {
        let mut body = serde_json::Map::new();
        body.insert("kind".into(), serde_json::json!("nefor-tui.grid.flush"));
        let env = Envelope::event(PluginName::new("p").expect("valid"), ts(), body);
        let line = format_envelope(&env);
        assert!(line.contains("event"));
        assert!(line.contains("kind=nefor-tui.grid.flush"));
    }

    #[test]
    fn formats_opaque_event() {
        let body = serde_json::Map::new();
        let env = Envelope::event(PluginName::new("p").expect("valid"), ts(), body);
        let line = format_envelope(&env);
        assert!(line.contains("<opaque>"));
    }
}
