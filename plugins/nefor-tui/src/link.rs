/// The parser-provided Markdown destination, preserved verbatim for user policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget(String);

impl LinkTarget {
    pub fn new(raw: &str) -> Self {
        Self(raw.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_destinations_without_interpretation() {
        for raw in [
            "HTTPS://Example.COM",
            "docs/a.md",
            "#section",
            "custom:thing",
            "file:///a",
            "",
        ] {
            assert_eq!(LinkTarget::new(raw).as_str(), raw);
        }
    }
}
