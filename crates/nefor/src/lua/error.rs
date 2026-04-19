//! Typed errors for the Lua embedding layer.
//!
//! Per spec §Rust-caliber errors at the Lua boundary: every Rust→Lua boundary
//! function validates eagerly and raises a typed error carrying, where
//! possible, the Lua source location. Callers pattern-match on the variant.
//!
//! `anyhow` is forbidden in this module — the top-boundary aggregator in
//! [`crate::error::NeforError`] is where it's allowed to appear, if at all.

use std::path::PathBuf;

/// File:line(:col) location inside a Lua chunk.
///
/// Populated when [`LuaError::InitLuaExec`] can parse the location out of
/// `mlua`'s error message (mlua prefixes runtime errors with
/// `[string "<chunk name>"]:LINE:` or, for named chunks loaded with
/// `Chunk::set_name`, `<chunk name>:LINE:`). Column information is rarely
/// present in Lua's error messages; we surface it when we can find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaSourceLocation {
    /// Path of the chunk that errored. For `init.lua` this is the absolute
    /// path; for an inline `lua.load(...)` chunk it's whatever string was
    /// passed to `.set_name(...)`.
    pub file: PathBuf,
    /// 1-based line number reported by Lua.
    pub line: u32,
    /// 1-based column number, if mlua's message included one. Almost always
    /// `None` in practice.
    pub col: Option<u32>,
}

impl std::fmt::Display for LuaSourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.col {
            Some(col) => write!(f, "{}:{}:{}", self.file.display(), self.line, col),
            None => write!(f, "{}:{}", self.file.display(), self.line),
        }
    }
}

/// Errors produced by the Lua embedding layer.
///
/// Variants are only added as concrete paths exist in the binary — no
/// pre-populated list. Every binding that can fail has a variant that carries
/// the fact it failed *and* enough context for the user to fix it.
///
/// Some variants (`InvalidEventName`, `MissingHandler`, `InvalidLogLevel`,
/// `Other`) are the Rust-side classification of errors the bindings raise
/// into Lua via `mlua::Error::runtime(...)`. They're produced by
/// [`LuaError::classify_runtime_error`] when a binding error escapes back
/// into Rust — the `classify_*` path may only be exercised from tests in
/// this commit, but the variant set is the stable public contract callers
/// branch on, so the enum is `#[allow(dead_code)]` until more callers arrive.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum LuaError {
    /// Construction of the [`mlua::Lua`] state itself failed. Rare — usually
    /// only when the `nefor` global table can't be installed (e.g., mlua
    /// couldn't allocate).
    #[error("failed to initialize Lua VM: {0}")]
    VmInit(#[source] mlua::Error),

    /// Reading `init.lua` from disk failed. The caller knows the path, so the
    /// path isn't stashed here (keeps the Display message terse); callers
    /// format the path themselves when reporting.
    #[error("failed to read init.lua: {0}")]
    InitLuaRead(#[source] std::io::Error),

    /// `init.lua` executed but raised a Lua error. `location` is `Some` when
    /// mlua's error message parsed into file:line; `None` for mechanically-
    /// raised errors (e.g., stack overflow) that don't carry a location.
    #[error("init.lua execution failed{}: {source}",
        .location.as_ref().map(|l| format!(" at {l}")).unwrap_or_default())]
    InitLuaExec {
        /// Underlying mlua error.
        #[source]
        source: mlua::Error,
        /// Parsed file:line location, when available.
        location: Option<LuaSourceLocation>,
    },

    /// A Lua caller passed a malformed event name to `nefor.events.on`,
    /// `nefor.events.off`, or `nefor.events.emit`. Empty strings are the
    /// usual cause.
    #[error("invalid event name {got:?}: {reason}")]
    InvalidEventName {
        /// The string the caller passed. Bounded to 64 chars by the binding
        /// before it reaches here so an accidental multi-megabyte payload
        /// doesn't explode our logs.
        got: String,
        /// Why it was rejected. Examples: "empty string", "contains control
        /// characters".
        reason: String,
    },

    /// A Lua caller passed something other than a function where a handler
    /// was expected (`nefor.events.on(name, nil)`, etc.).
    #[error("missing handler for {kind}: expected a Lua function")]
    MissingHandler {
        /// The API surface that rejected the call, e.g. `"nefor.events.on"`.
        kind: &'static str,
    },

    /// `nefor.log.<level>(...)` was called with an unknown level. This is
    /// currently unreachable from Lua since we install one Lua function per
    /// level, but exists as a defensive variant for future dynamic-level
    /// paths (e.g., a plugin-level dispatch from a string).
    #[error("invalid log level {got:?}")]
    InvalidLogLevel {
        /// What was passed.
        got: String,
    },

    /// Escape hatch for mlua errors we don't yet have a specific variant for.
    /// Use sparingly — adding a typed variant beats Other for everything a
    /// plugin author might hit during normal use.
    #[error(transparent)]
    Other(mlua::Error),
}

impl LuaError {
    /// Try to extract a Lua source location out of `err`.
    ///
    /// mlua formats runtime and syntax errors like
    /// `runtime error: [string "<chunk-name>"]:LINE: MESSAGE` or, for chunks
    /// loaded with an explicit `.set_name(path)`, `<path>:LINE: MESSAGE`. We
    /// tolerate both shapes and silently return `None` when neither matches.
    ///
    /// `chunk_file` is the path we loaded the chunk under — used both to
    /// populate [`LuaSourceLocation::file`] and as the first candidate for
    /// matching `<chunk-name>` in the error string.
    pub(crate) fn location_from_mlua(
        err: &mlua::Error,
        chunk_file: &std::path::Path,
    ) -> Option<LuaSourceLocation> {
        let msg = match err {
            mlua::Error::RuntimeError(m) | mlua::Error::SyntaxError { message: m, .. } => {
                m.as_str()
            }
            _ => return None,
        };
        parse_line(msg, chunk_file)
    }

    /// Re-classify a `mlua::Error` produced by one of our bindings into a
    /// typed [`LuaError`] variant when the message matches a known shape.
    ///
    /// Bindings raise `mlua::Error::runtime("nefor.events.on: ...")` so the
    /// Lua caller gets a readable message; when that error escapes `pcall`
    /// and bubbles back to Rust (e.g., via `load_init`), this helper turns
    /// it back into the typed variant the rest of the binary can branch on.
    /// Unmatched shapes fall through to [`LuaError::Other`].
    #[allow(dead_code)]
    pub fn classify_runtime_error(err: mlua::Error) -> LuaError {
        let msg = match &err {
            mlua::Error::RuntimeError(m) => m.clone(),
            _ => return LuaError::Other(err),
        };

        // `nefor.events.*: name must be a non-empty string`
        if msg.contains("name must be a non-empty string") {
            return LuaError::InvalidEventName {
                got: "".to_string(),
                reason: "empty string".to_string(),
            };
        }
        if msg.contains("name must be a string") || msg.contains("name too long") {
            return LuaError::InvalidEventName {
                got: "<non-string or too long>".to_string(),
                reason: msg,
            };
        }
        if msg.contains("handler must be a function") {
            return LuaError::MissingHandler {
                kind: "nefor.events.on",
            };
        }

        LuaError::Other(err)
    }
}

/// Pull a line number out of a Lua error message.
///
/// We look for two shapes. First `[string "NAME"]:LINE:` — Lua's default for
/// chunks without a source file. Then `NAME:LINE:` — the shape when a chunk
/// was loaded with `Chunk::set_name(path)`. The first colon-separated integer
/// after the name is taken as the line number.
fn parse_line(msg: &str, chunk_file: &std::path::Path) -> Option<LuaSourceLocation> {
    // Try `[string "..."]:LINE:` form first.
    if let Some(rest) = msg.strip_prefix("[string \"") {
        if let Some((_name, tail)) = rest.split_once("\"]:") {
            if let Some((line_str, _)) = tail.split_once(':') {
                if let Ok(line) = line_str.trim().parse::<u32>() {
                    return Some(LuaSourceLocation {
                        file: chunk_file.to_path_buf(),
                        line,
                        col: None,
                    });
                }
            }
        }
    }
    // Try `<chunk_file>:LINE:` form. We don't require an exact path match —
    // mlua sometimes truncates long paths — but we do need the first colon-
    // separated integer to look like a line number.
    let path_str = chunk_file.display().to_string();
    if let Some(after) = msg.find(&path_str) {
        let tail = &msg[after + path_str.len()..];
        if let Some(rest) = tail.strip_prefix(':') {
            if let Some((line_str, _)) = rest.split_once(':') {
                if let Ok(line) = line_str.trim().parse::<u32>() {
                    return Some(LuaSourceLocation {
                        file: chunk_file.to_path_buf(),
                        line,
                        col: None,
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_chunk_form() {
        let msg = r#"[string "init.lua"]:7: attempt to call a nil value"#;
        let loc = parse_line(msg, &PathBuf::from("/tmp/init.lua")).expect("parse ok");
        assert_eq!(loc.line, 7);
        assert_eq!(loc.file, PathBuf::from("/tmp/init.lua"));
    }

    #[test]
    fn parse_named_chunk_form() {
        let msg = "/tmp/init.lua:12: bad argument #1";
        let loc = parse_line(msg, &PathBuf::from("/tmp/init.lua")).expect("parse ok");
        assert_eq!(loc.line, 12);
    }

    #[test]
    fn parse_unknown_form_returns_none() {
        assert!(parse_line("something else entirely", &PathBuf::from("/tmp/x")).is_none());
    }

    #[test]
    fn display_location_without_col() {
        let loc = LuaSourceLocation {
            file: PathBuf::from("/x/init.lua"),
            line: 3,
            col: None,
        };
        assert_eq!(format!("{loc}"), "/x/init.lua:3");
    }

    #[test]
    fn display_location_with_col() {
        let loc = LuaSourceLocation {
            file: PathBuf::from("/x/init.lua"),
            line: 3,
            col: Some(9),
        };
        assert_eq!(format!("{loc}"), "/x/init.lua:3:9");
    }

    #[test]
    fn classify_empty_name_maps_to_invalid_event_name() {
        let err =
            mlua::Error::runtime("nefor.events.on: name must be a non-empty string (got \"\")");
        let classified = LuaError::classify_runtime_error(err);
        assert!(matches!(classified, LuaError::InvalidEventName { .. }));
    }

    #[test]
    fn classify_handler_message_maps_to_missing_handler() {
        let err = mlua::Error::runtime("nefor.events.on: handler must be a function (got nil)");
        let classified = LuaError::classify_runtime_error(err);
        assert!(matches!(
            classified,
            LuaError::MissingHandler {
                kind: "nefor.events.on"
            }
        ));
    }

    #[test]
    fn classify_unknown_message_falls_through_to_other() {
        let err = mlua::Error::runtime("some unrelated error");
        let classified = LuaError::classify_runtime_error(err);
        assert!(matches!(classified, LuaError::Other(_)));
    }
}
