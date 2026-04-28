//! Per-tool gating policy.
//!
//! Three decisions:
//!
//! - `Auto`   — forward the call without prompting.
//! - `Prompt` — emit `chat.tool.permission_request`, await user decision.
//! - `Deny`   — reject immediately with a "denied by policy" error.
//!
//! Policy is currently sourced from CLI flags (see `Config`). A tool that
//! isn't named on any flag falls through to `default_decision` (configurable
//! via `--default <auto|prompt|deny>`; ships as `prompt` so an unconfigured
//! tool is safe-by-default).

use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Auto,
    Prompt,
    Deny,
}

impl FromStr for Decision {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "unknown decision `{other}`; expected auto|prompt|deny"
            )),
        }
    }
}

/// Resolved policy: a per-tool override map plus a fallback for unlisted
/// tools.
#[derive(Debug, Clone)]
pub struct Policy {
    by_tool: HashMap<String, Decision>,
    default_decision: Decision,
}

impl Policy {
    pub fn new(default_decision: Decision) -> Self {
        Self {
            by_tool: HashMap::new(),
            default_decision,
        }
    }

    pub fn set(&mut self, tool: impl Into<String>, decision: Decision) {
        self.by_tool.insert(tool.into(), decision);
    }

    pub fn decide(&self, tool: &str) -> Decision {
        self.by_tool
            .get(tool)
            .copied()
            .unwrap_or(self.default_decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_through_to_default() {
        let p = Policy::new(Decision::Prompt);
        assert_eq!(p.decide("anything"), Decision::Prompt);
    }

    #[test]
    fn explicit_overrides_default() {
        let mut p = Policy::new(Decision::Deny);
        p.set("read_file", Decision::Auto);
        assert_eq!(p.decide("read_file"), Decision::Auto);
        assert_eq!(p.decide("write_file"), Decision::Deny);
    }

    #[test]
    fn from_str_accepts_known_words() {
        assert_eq!("auto".parse::<Decision>().unwrap(), Decision::Auto);
        assert_eq!("PROMPT".parse::<Decision>().unwrap(), Decision::Prompt);
        assert_eq!("Deny".parse::<Decision>().unwrap(), Decision::Deny);
        assert!("foo".parse::<Decision>().is_err());
    }
}
