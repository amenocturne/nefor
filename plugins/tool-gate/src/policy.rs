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
//! The gate does not know what a tool does and never stands in for a tool
//! that did not say: an assessment that is missing, unknown, or `unsure` /
//! `denied` without a reason is an error for the call, not a decision.
//!
//! `--deny <tool>` is the one configured restriction: an operator's explicit
//! "this tool is denied", applied whatever the tool says ([`Denied`]).

use std::collections::HashSet;

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

/// The tools an operator denied with `--deny`.
#[derive(Debug, Clone, Default)]
pub struct Denied {
    tools: HashSet<String>,
}

impl Denied {
    pub fn deny(&mut self, tool: impl Into<String>) {
        self.tools.insert(tool.into());
    }

    pub fn contains(&self, tool: &str) -> bool {
        self.tools.contains(tool)
    }
}

/// Why a tool the operator set to `--deny` is denied.
pub const DENIED_BY_SETTING: &str = "the gate is configured to deny this tool";

#[cfg(test)]
mod tests {
    use super::*;

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
    fn only_named_tools_are_denied() {
        let mut denied = Denied::default();
        denied.deny("rm");
        assert!(denied.contains("rm"));
        assert!(!denied.contains("read_file"));
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
}
