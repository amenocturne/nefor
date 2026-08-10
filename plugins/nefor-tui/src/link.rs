use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget(String);

impl LinkTarget {
    pub fn parse(raw: &str) -> Option<Self> {
        let url = Url::parse(raw).ok()?;
        match url.scheme() {
            "http" | "https" | "mailto" => Some(Self(url.into())),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_external_targets_the_system_can_meaningfully_open() {
        assert_eq!(
            LinkTarget::parse("https://example.com/a?q=1")
                .map(|target| target.as_str().to_string()),
            Some("https://example.com/a?q=1".into())
        );
        assert!(LinkTarget::parse("mailto:hello@example.com").is_some());
    }

    #[test]
    fn rejects_relative_fragments_and_effectful_schemes() {
        for target in [
            "docs/readme.md",
            "#section",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(LinkTarget::parse(target).is_none(), "accepted {target}");
        }
    }
}
