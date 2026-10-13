//! What the gate does with one tool call.
//!
//! The gate's inputs for a call are the caller's approval policy, the
//! permission mode, and the tool's own assessment of the call (`safe`,
//! `unsure` or `denied`, answered by the tool's source for the call's
//! arguments). [`verdict`] is the one place that turns the three into what
//! happens: the call runs, the user is asked, or the call is denied.
//!
//! | assessment | ReadOnly (every mode) | Default, safe | Default, auto | Default, yolo |
//! |------------|-----------------------|---------------|---------------|---------------|
//! | safe       | run                   | run           | run           | run           |
//! | unsure     | deny                  | ask           | run           | run           |
//! | denied     | deny                  | deny          | deny          | run           |
//!
//! The policy is consulted before the mode: no mode widens a `ReadOnly`
//! caller, and such a caller is never asked about.
//!
//! The gate does not know what a tool does. A tool that does not assess a
//! call falls back to the per-tool setting configured on the command line
//! ([`Policy`]): under `Default`, `auto` means safe, `prompt` means unsure and
//! `deny` means denied; under any narrower policy an unassessed call is
//! denied, because nothing showed that it fits the policy. `--deny` is an
//! operator's override and makes a tool's calls denied whatever the tool says.
//!
//! Per-tool settings come from CLI flags (see `Config`). A tool that isn't
//! named on any flag falls through to `default_decision` (configurable via
//! `--default <auto|prompt|deny>`; ships as `prompt` so an unconfigured tool
//! is safe-by-default).

use std::collections::HashMap;
use std::str::FromStr;

/// The permission mode the user selected for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Safe,
    Auto,
    Yolo,
}

impl GateMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "safe" | "normal" => Some(Self::Safe),
            "auto" => Some(Self::Auto),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }
}

/// What the calling agent may do with its tools, by its wire name. The names
/// are the constructors of MAG's `ToolApprovalPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    ReadOnly,
    Default,
}

impl ApprovalPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ReadOnly" => Some(Self::ReadOnly),
            "Default" => Some(Self::Default),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ReadOnly",
            Self::Default => "Default",
        }
    }
}

/// A tool's account of one call under the caller's approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assessment {
    Safe,
    Unsure,
    Denied,
}

impl Assessment {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "safe" => Some(Self::Safe),
            "unsure" => Some(Self::Unsure),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// What the gate does with a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Run,
    Ask,
    Deny,
}

/// The decision table: policy first, mode second.
pub fn verdict(policy: ApprovalPolicy, mode: GateMode, assessment: Assessment) -> Verdict {
    match (policy, assessment, mode) {
        (_, Assessment::Safe, _) => Verdict::Run,
        (ApprovalPolicy::ReadOnly, Assessment::Unsure | Assessment::Denied, _) => Verdict::Deny,
        (ApprovalPolicy::Default, Assessment::Unsure, GateMode::Safe) => Verdict::Ask,
        (ApprovalPolicy::Default, Assessment::Unsure, GateMode::Auto | GateMode::Yolo) => {
            Verdict::Run
        }
        (ApprovalPolicy::Default, Assessment::Denied, GateMode::Safe | GateMode::Auto) => {
            Verdict::Deny
        }
        (ApprovalPolicy::Default, Assessment::Denied, GateMode::Yolo) => Verdict::Run,
    }
}

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

    /// The assessment of a call its tool did not assess, from the configured
    /// setting, with the reason a denial gives the caller.
    pub fn unassessed(
        &self,
        tool: &str,
        policy: ApprovalPolicy,
    ) -> (Assessment, Option<&'static str>) {
        match (policy, self.decide(tool)) {
            (_, Decision::Deny) => (Assessment::Denied, Some(DENIED_BY_SETTING)),
            (ApprovalPolicy::ReadOnly, _) => (Assessment::Denied, Some(UNDECLARED)),
            (ApprovalPolicy::Default, Decision::Auto) => (Assessment::Safe, None),
            (ApprovalPolicy::Default, Decision::Prompt) => (Assessment::Unsure, None),
        }
    }
}

/// Why a tool the operator set to `--deny` is denied.
pub const DENIED_BY_SETTING: &str = "the gate is configured to deny this tool";
/// Why a call nobody assessed is denied under a policy narrower than `Default`.
pub const UNDECLARED: &str = "the tool does not say what it does under this policy";

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

    /// Every cell of the decision table.
    #[test]
    fn verdict_is_the_decision_table() {
        use ApprovalPolicy::{Default, ReadOnly};
        use Assessment::{Denied, Safe, Unsure};
        use GateMode::{Auto, Safe as SafeMode, Yolo};
        use Verdict::{Ask, Deny, Run};
        let table = [
            (ReadOnly, SafeMode, Safe, Run),
            (ReadOnly, Auto, Safe, Run),
            (ReadOnly, Yolo, Safe, Run),
            (ReadOnly, SafeMode, Unsure, Deny),
            (ReadOnly, Auto, Unsure, Deny),
            (ReadOnly, Yolo, Unsure, Deny),
            (ReadOnly, SafeMode, Denied, Deny),
            (ReadOnly, Auto, Denied, Deny),
            (ReadOnly, Yolo, Denied, Deny),
            (Default, SafeMode, Safe, Run),
            (Default, Auto, Safe, Run),
            (Default, Yolo, Safe, Run),
            (Default, SafeMode, Unsure, Ask),
            (Default, Auto, Unsure, Run),
            (Default, Yolo, Unsure, Run),
            (Default, SafeMode, Denied, Deny),
            (Default, Auto, Denied, Deny),
            (Default, Yolo, Denied, Run),
        ];
        for (policy, mode, assessment, expected) in table {
            assert_eq!(
                verdict(policy, mode, assessment),
                expected,
                "{policy:?} / {mode:?} / {assessment:?}"
            );
        }
    }

    #[test]
    fn a_read_only_caller_is_never_asked_and_never_widened() {
        for mode in [GateMode::Safe, GateMode::Auto, GateMode::Yolo] {
            for assessment in [Assessment::Safe, Assessment::Unsure, Assessment::Denied] {
                let outcome = verdict(ApprovalPolicy::ReadOnly, mode, assessment);
                assert_ne!(outcome, Verdict::Ask);
                assert_eq!(outcome == Verdict::Run, assessment == Assessment::Safe);
            }
        }
    }

    #[test]
    fn an_unassessed_call_follows_the_configured_setting_only_under_default() {
        let mut p = Policy::new(Decision::Prompt);
        p.set("read_file", Decision::Auto);
        p.set("rm", Decision::Deny);
        assert_eq!(
            p.unassessed("read_file", ApprovalPolicy::Default),
            (Assessment::Safe, None)
        );
        assert_eq!(
            p.unassessed("other", ApprovalPolicy::Default),
            (Assessment::Unsure, None)
        );
        assert_eq!(
            p.unassessed("rm", ApprovalPolicy::Default),
            (Assessment::Denied, Some(DENIED_BY_SETTING))
        );
        assert_eq!(
            p.unassessed("read_file", ApprovalPolicy::ReadOnly),
            (Assessment::Denied, Some(UNDECLARED))
        );
        assert_eq!(
            p.unassessed("rm", ApprovalPolicy::ReadOnly),
            (Assessment::Denied, Some(DENIED_BY_SETTING))
        );
    }

    #[test]
    fn wire_names_parse_exactly() {
        assert_eq!(
            ApprovalPolicy::parse("ReadOnly"),
            Some(ApprovalPolicy::ReadOnly)
        );
        assert_eq!(
            ApprovalPolicy::parse("Default"),
            Some(ApprovalPolicy::Default)
        );
        assert_eq!(ApprovalPolicy::parse("readonly"), None);
        assert_eq!(ApprovalPolicy::parse("Rules"), None);
        assert_eq!(Assessment::parse("unsure"), Some(Assessment::Unsure));
        assert_eq!(Assessment::parse("approve"), None);
    }

    #[test]
    fn from_str_accepts_known_words() {
        assert_eq!("auto".parse::<Decision>().unwrap(), Decision::Auto);
        assert_eq!("PROMPT".parse::<Decision>().unwrap(), Decision::Prompt);
        assert_eq!("Deny".parse::<Decision>().unwrap(), Decision::Deny);
        assert!("foo".parse::<Decision>().is_err());
    }
}
