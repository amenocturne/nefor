//! Script parsing and playback helpers.
//!
//! A script is a `.jsonl` file. Each non-blank line is one of:
//!
//! - `# ...` — comment, skipped.
//! - `# sleep <dur>` (e.g. `500ms`, `2s`) — pause playback.
//! - `{ "type": ..., "body": ... }` — plugin-outgoing shape; stamped with
//!   `from: "fake-engine"` and a fresh `ts` before sending.
//! - `{ "type": ..., "from": ..., "ts": ..., "body": ... }` — fully stamped
//!   envelope; sent verbatim.
//!
//! Parsing is pure (no I/O beyond reading the file contents); playback is
//! performed by the harness.

use std::time::Duration;

use nefor_protocol::{Envelope, PluginOutgoing};

/// One step in a parsed script.
#[derive(Debug)]
pub enum ScriptStep {
    /// Send a pre-stamped envelope verbatim.
    SendVerbatim(Envelope),
    /// Send a plugin-outgoing message, stamping `from` and `ts` at send time.
    SendStamped(PluginOutgoing),
    /// Pause playback for the given duration.
    Sleep(Duration),
}

/// Failure modes from [`parse_script`].
#[derive(Debug, thiserror::Error)]
pub enum ScriptParseError {
    /// A line was non-empty, non-comment, but was not valid JSON and not a
    /// recognizable pragma.
    #[error("line {line}: {source}")]
    BadLine {
        /// 1-based line number in the source script.
        line: usize,
        /// Inner reason.
        #[source]
        source: LineError,
    },
}

/// Detailed per-line error.
#[derive(Debug, thiserror::Error)]
pub enum LineError {
    /// A `# sleep ...` pragma could not be understood.
    #[error("invalid sleep pragma {raw:?}: {reason}")]
    BadSleep {
        /// The pragma text after `# sleep`.
        raw: String,
        /// Why it failed.
        reason: String,
    },
    /// JSON parse failure.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// Neither a stamped envelope nor an outgoing shape.
    #[error("line is not a valid NCP envelope or outgoing message: {0}")]
    NotAnEnvelope(String),
}

/// Parse a full script source into steps. Blank lines and comment lines are
/// skipped. Sleep pragmas become [`ScriptStep::Sleep`]. Everything else must
/// parse as either a fully-stamped envelope or a plugin-outgoing message.
pub fn parse_script(source: &str) -> Result<Vec<ScriptStep>, ScriptParseError> {
    let mut steps = Vec::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            // Pragma or plain comment. `# sleep ...` is a pragma; everything
            // else starting with `#` is a comment and discarded.
            let rest = rest.trim_start();
            if let Some(dur_text) = rest.strip_prefix("sleep") {
                let dur = parse_duration(dur_text.trim()).map_err(|reason| {
                    ScriptParseError::BadLine {
                        line: line_no,
                        source: LineError::BadSleep {
                            raw: dur_text.trim().to_string(),
                            reason,
                        },
                    }
                })?;
                steps.push(ScriptStep::Sleep(dur));
            }
            continue;
        }
        // Non-comment: JSON. Try the stamped envelope first; if that fails
        // because fields are missing, try the outgoing shape.
        let step = parse_json_step(trimmed).map_err(|e| ScriptParseError::BadLine {
            line: line_no,
            source: e,
        })?;
        steps.push(step);
    }
    Ok(steps)
}

fn parse_json_step(line: &str) -> Result<ScriptStep, LineError> {
    // Peek at keys to decide shape: if the object carries `from` or `ts`,
    // it claims to be a fully-stamped envelope; otherwise treat it as an
    // outgoing message. This avoids the ambiguity where a bare outgoing
    // object would be rejected by Envelope::parse_line with a misleading
    // "missing from" error.
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| LineError::Json(e.to_string()))?;
    let obj = match &value {
        serde_json::Value::Object(m) => m,
        _ => {
            return Err(LineError::NotAnEnvelope(
                "top-level is not an object".into(),
            ))
        }
    };
    let looks_stamped = obj.contains_key("from") || obj.contains_key("ts");
    if looks_stamped {
        let env = Envelope::parse_line(line)
            .map_err(|e| LineError::NotAnEnvelope(format!("as envelope: {e}")))?;
        Ok(ScriptStep::SendVerbatim(env))
    } else {
        let out = PluginOutgoing::parse_line(line)
            .map_err(|e| LineError::NotAnEnvelope(format!("as outgoing: {e}")))?;
        Ok(ScriptStep::SendStamped(out))
    }
}

/// Parse a duration like `500ms`, `2s`, `1500ms`. Only `ms` and `s` units.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    if text.is_empty() {
        return Err("empty duration".into());
    }
    let (num_part, unit) = if let Some(n) = text.strip_suffix("ms") {
        (n, "ms")
    } else if let Some(n) = text.strip_suffix('s') {
        (n, "s")
    } else {
        return Err(format!("unknown unit in {text:?} (expected ms or s)"));
    };
    let num: u64 = num_part
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| format!("bad number in {text:?}: {e}"))?;
    Ok(match unit {
        "ms" => Duration::from_millis(num),
        "s" => Duration::from_secs(num),
        _ => unreachable!("unit checked above"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_ms() {
        assert_eq!(
            parse_duration("500ms").expect("ms"),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_duration("0ms").expect("0ms"),
            Duration::from_millis(0)
        );
    }

    #[test]
    fn parse_duration_s() {
        assert_eq!(parse_duration("2s").expect("s"), Duration::from_secs(2));
    }

    #[test]
    fn parse_duration_rejects_bad() {
        assert!(parse_duration("2min").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn parses_empty_and_comments() {
        let src = r#"
# this is a comment

# another
"#;
        let steps = parse_script(src).expect("ok");
        assert!(steps.is_empty(), "got {steps:?}");
    }

    #[test]
    fn parses_sleep_pragma() {
        let src = "# sleep 100ms\n# sleep 2s\n";
        let steps = parse_script(src).expect("ok");
        assert_eq!(steps.len(), 2);
        assert!(matches!(&steps[0], ScriptStep::Sleep(d) if *d == Duration::from_millis(100)));
        assert!(matches!(&steps[1], ScriptStep::Sleep(d) if *d == Duration::from_secs(2)));
    }

    #[test]
    fn parses_outgoing_event() {
        let src = r#"{"type":"event","body":{"kind":"nefor-tui.grid.flush"}}"#;
        let steps = parse_script(src).expect("ok");
        assert_eq!(steps.len(), 1);
        assert!(matches!(&steps[0], ScriptStep::SendStamped(_)));
    }

    #[test]
    fn parses_outgoing_system() {
        let src = r#"{"type":"system","body":{"kind":"shutdown","grace_ms":1000}}"#;
        let steps = parse_script(src).expect("ok");
        assert!(matches!(&steps[0], ScriptStep::SendStamped(_)));
    }

    #[test]
    fn parses_verbatim_envelope() {
        let src = r#"{"type":"system","from":"engine","ts":"2026-04-21T12:34:56.789Z","body":{"kind":"attach_ok","engine_version":"fake-0.1.0"}}"#;
        let steps = parse_script(src).expect("ok");
        assert!(matches!(&steps[0], ScriptStep::SendVerbatim(_)));
    }

    #[test]
    fn mixed_script() {
        let src = r#"# header
{"type":"event","body":{"kind":"nefor-tui.grid.flush"}}
# sleep 250ms
{"type":"system","body":{"kind":"shutdown","grace_ms":1000}}
"#;
        let steps = parse_script(src).expect("ok");
        assert_eq!(steps.len(), 3);
        assert!(matches!(&steps[0], ScriptStep::SendStamped(_)));
        assert!(matches!(&steps[1], ScriptStep::Sleep(d) if *d == Duration::from_millis(250)));
        assert!(matches!(&steps[2], ScriptStep::SendStamped(_)));
    }

    #[test]
    fn invalid_json_reports_line_number() {
        let src = "\n\n{not json\n";
        let err = parse_script(src).unwrap_err();
        match err {
            ScriptParseError::BadLine { line, .. } => assert_eq!(line, 3),
        }
    }

    #[test]
    fn invalid_sleep_reports_line_number() {
        let src = "# sleep nonsense\n";
        let err = parse_script(src).unwrap_err();
        let ScriptParseError::BadLine { line, source } = err;
        assert_eq!(line, 1);
        assert!(matches!(source, LineError::BadSleep { .. }));
    }
}
