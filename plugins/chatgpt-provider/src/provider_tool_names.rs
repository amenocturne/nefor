//! Reversible tool-name projection at the ChatGPT Responses boundary.
//!
//! Nefor capability identities are provider-neutral and may contain dots or
//! Unicode. ChatGPT accepts only `[A-Za-z0-9_-]+` function names and currently
//! caps them at 64 ASCII bytes. Valid internal names are preserved. Every other
//! name becomes `__nefor_` plus the first 56 lowercase hex digits of SHA-256 of
//! its UTF-8 bytes (exactly 64 bytes). The map is rebuilt from the advertised
//! internal catalog for each request, so persisted history and routing never
//! adopt provider aliases. Duplicate internal names and the astronomically
//! unlikely hash/provider-name collision are rejected before HTTP with the
//! colliding internal names in the diagnostic.

use std::collections::{BTreeSet, HashMap};

use sha2::{Digest, Sha256};

use crate::catalog::ToolSpec;
use crate::responses::request::ResponseItem;

const MAX_PROVIDER_NAME_BYTES: usize = 64;
const ENCODED_PREFIX: &str = "__nefor_";
const HASH_HEX_BYTES: usize = MAX_PROVIDER_NAME_BYTES - ENCODED_PREFIX.len();

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderToolNameError {
    #[error("duplicate internal tool name `{name}` in ChatGPT request catalog; tool names must be unique before provider translation")]
    DuplicateInternal { name: String },
    #[error("ChatGPT provider tool-name collision on `{provider}` between internal tools `{first}` and `{second}`; rename one capability or reduce the request tool catalog")]
    Collision {
        provider: String,
        first: String,
        second: String,
    },
    #[error("ChatGPT returned unknown provider tool name `{name}`; it was not advertised in this Responses request")]
    UnknownProvider { name: String },
    #[error("cannot encode internal tool name `{name}` because it was not advertised in this Responses request")]
    UnknownInternal { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderToolNames {
    internal_to_provider: HashMap<String, String>,
    provider_to_internal: HashMap<String, String>,
}

impl ProviderToolNames {
    pub fn from_specs(specs: &[ToolSpec]) -> Result<Self, ProviderToolNameError> {
        let mut names = BTreeSet::new();
        for spec in specs {
            if !names.insert(spec.name.clone()) {
                return Err(ProviderToolNameError::DuplicateInternal {
                    name: spec.name.clone(),
                });
            }
        }

        let mut internal_to_provider = HashMap::with_capacity(names.len());
        let mut provider_to_internal = HashMap::with_capacity(names.len());
        for internal in names {
            let provider = if is_valid_provider_name(&internal) {
                internal.clone()
            } else {
                encoded_name(&internal)
            };
            if let Some(first) = provider_to_internal.insert(provider.clone(), internal.clone()) {
                return Err(ProviderToolNameError::Collision {
                    provider,
                    first,
                    second: internal,
                });
            }
            internal_to_provider.insert(internal, provider);
        }
        Ok(Self {
            internal_to_provider,
            provider_to_internal,
        })
    }

    pub fn to_provider(&self, internal: &str) -> Result<&str, ProviderToolNameError> {
        self.internal_to_provider
            .get(internal)
            .map(String::as_str)
            .ok_or_else(|| ProviderToolNameError::UnknownInternal {
                name: internal.to_owned(),
            })
    }

    pub fn to_internal(&self, provider: &str) -> Result<&str, ProviderToolNameError> {
        self.provider_to_internal
            .get(provider)
            .map(String::as_str)
            .ok_or_else(|| ProviderToolNameError::UnknownProvider {
                name: provider.to_owned(),
            })
    }

    pub fn map_input_to_provider(
        &self,
        items: &mut [ResponseItem],
    ) -> Result<(), ProviderToolNameError> {
        for item in items {
            if let ResponseItem::FunctionCall { name, .. } = item {
                *name = self.to_provider(name)?.to_owned();
            }
        }
        Ok(())
    }

    pub fn map_output_to_internal(
        &self,
        items: &mut [ResponseItem],
    ) -> Result<(), ProviderToolNameError> {
        for item in items {
            if let ResponseItem::FunctionCall { name, .. } = item {
                *name = self.to_internal(name)?.to_owned();
            }
        }
        Ok(())
    }
}

pub fn is_valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROVIDER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn encoded_name(internal: &str) -> String {
    let digest = Sha256::digest(internal.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("{ENCODED_PREFIX}{}", &hex[..HASH_HEX_BYTES])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn specs(names: &[&str]) -> Vec<ToolSpec> {
        names
            .iter()
            .map(|name| ToolSpec {
                name: (*name).into(),
                description: String::new(),
                input_schema: json!({}),
            })
            .collect()
    }

    #[test]
    fn valid_names_are_preserved_and_invalid_names_are_stable_and_reversible() {
        let catalog = specs(&[
            "read_file",
            "git-worktree",
            "process.exec",
            "shell.script",
            "punctuation!?",
            "инструмент",
        ]);
        let first = ProviderToolNames::from_specs(&catalog).expect("mapping");
        let second = ProviderToolNames::from_specs(&catalog).expect("stable mapping");
        assert_eq!(first, second);
        assert_eq!(first.to_provider("read_file"), Ok("read_file"));
        assert_eq!(first.to_provider("git-worktree"), Ok("git-worktree"));

        for internal in [
            "process.exec",
            "shell.script",
            "punctuation!?",
            "инструмент",
        ] {
            let provider = first.to_provider(internal).expect("provider name");
            assert!(is_valid_provider_name(provider), "{provider}");
            assert_eq!(provider.len(), MAX_PROVIDER_NAME_BYTES);
            assert_eq!(first.to_internal(provider), Ok(internal));
        }
    }

    #[test]
    fn realistic_catalog_pins_process_exec_as_the_offending_tenth_tool() {
        let catalog = specs(&[
            "read_file",
            "read_image",
            "write_file",
            "edit_file",
            "search_text",
            "list_dir",
            "instructions",
            "discover_instruction_files",
            "git_worktree_create",
            "process.exec",
            "shell.script",
        ]);
        assert_eq!(catalog[9].name, "process.exec");
        assert!(!is_valid_provider_name(&catalog[9].name));
        let names = ProviderToolNames::from_specs(&catalog).expect("mapping");
        assert!(is_valid_provider_name(
            names
                .to_provider(&catalog[9].name)
                .expect("mapped tenth tool")
        ));
    }

    #[test]
    fn collisions_and_duplicates_fail_before_request() {
        let invalid = "process.exec";
        let alias = encoded_name(invalid);
        let collision = ProviderToolNames::from_specs(&specs(&[invalid, &alias]))
            .expect_err("valid internal alias must collide with encoded name");
        assert!(matches!(collision, ProviderToolNameError::Collision { .. }));
        assert!(collision.to_string().contains(invalid));
        assert!(collision.to_string().contains(&alias));

        assert!(matches!(
            ProviderToolNames::from_specs(&specs(&["same", "same"])),
            Err(ProviderToolNameError::DuplicateInternal { .. })
        ));
    }

    #[test]
    fn unknown_provider_and_internal_names_fail_clearly() {
        let names = ProviderToolNames::from_specs(&specs(&["process.exec"])).expect("mapping");
        assert!(matches!(
            names.to_internal("made_up"),
            Err(ProviderToolNameError::UnknownProvider { .. })
        ));
        assert!(matches!(
            names.to_provider("unadvertised"),
            Err(ProviderToolNameError::UnknownInternal { .. })
        ));
    }

    #[test]
    fn response_items_round_trip_without_changing_call_correlation() {
        let names = ProviderToolNames::from_specs(&specs(&["process.exec"])).expect("mapping");
        let mut items = vec![
            ResponseItem::FunctionCall {
                id: Some("fc_1".into()),
                call_id: "call_1".into(),
                name: "process.exec".into(),
                arguments: "{}".into(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call_1".into(),
                output: "done".into(),
            },
        ];
        names
            .map_input_to_provider(&mut items)
            .expect("forward mapping");
        let ResponseItem::FunctionCall { name, call_id, .. } = &items[0] else {
            panic!("function call");
        };
        assert_ne!(name, "process.exec");
        assert_eq!(call_id, "call_1");
        names
            .map_output_to_internal(&mut items)
            .expect("reverse mapping");
        let ResponseItem::FunctionCall { name, call_id, .. } = &items[0] else {
            panic!("function call");
        };
        assert_eq!(name, "process.exec");
        assert_eq!(call_id, "call_1");
        assert!(matches!(
            &items[1],
            ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "call_1"
        ));
    }
}
