//! In-memory registration state.
//!
//! Plugins call `combinators.register` with a namespace-local view (bare
//! type names, bare handler kinds). The registry stores everything under
//! fully-qualified names (`plugin.Type`, `plugin.bare.kind`) so dispatch
//! can look them up by exactly what `combinators.run` sends on the wire.
//!
//! Re-registration semantics: re-sending `types` from the same sender
//! replaces that sender's prior declaration. Types present in the new
//! `types` but with no matching implementations are unregistered.
//! Handlers from other plugins are never touched.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::error::{CombinatorsError, ErrorCode};

/// A fully-qualified type name. `plugin` is the NCP `from` identity of the
/// plugin that owns the type (or a cross-namespace reference for `Into`
/// targets); `name` is the bare type name within that plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FullyQualifiedType {
    /// Owning plugin identity (matches `envelope.from`).
    pub plugin: String,
    /// Bare type name in that plugin's namespace.
    pub name: String,
}

impl FullyQualifiedType {
    /// Canonical `plugin.Name` rendering for the wire.
    pub fn to_wire(&self) -> String {
        format!("{}.{}", self.plugin, self.name)
    }

    /// Parse a wire-form type tag.
    ///
    /// Plugin names may contain dashes but not dots (§3); bare type names
    /// may not contain dots either. So the first `.` cleanly splits the
    /// tag into (plugin, bare).
    pub fn parse(raw: &str) -> Option<Self> {
        let (plugin, name) = raw.split_once('.')?;
        if plugin.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self {
            plugin: plugin.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// A fully-qualified handler kind — what appears as `body.kind` on the
/// dispatch event the combinators plugin emits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FullyQualifiedKind {
    /// Owning plugin identity.
    pub plugin: String,
    /// Bare kind string (e.g. `message.concat`). May contain dots — the
    /// registering plugin chooses its own internal naming.
    pub bare: String,
}

impl FullyQualifiedKind {
    /// `plugin.bare` on the wire.
    pub fn to_wire(&self) -> String {
        format!("{}.{}", self.plugin, self.bare)
    }
}

/// Trait declaration parsed from a single entry in
/// `combinators.register.implementations`. The `trait` wire field is the
/// discriminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraitImpl {
    /// `Merge<T> -> handler`.
    Merge {
        /// Type argument (must be declared by the sender).
        type_: FullyQualifiedType,
        /// Handler kind to dispatch to.
        handler: FullyQualifiedKind,
    },
    /// `Into<In, Out> -> handler`. `out` may be cross-namespace.
    Into {
        /// Input type (must be declared by the sender).
        in_: FullyQualifiedType,
        /// Output type — bare name within `sender` OR cross-namespace
        /// `other-plugin.Type`.
        out: FullyQualifiedType,
        /// Handler kind to dispatch to.
        handler: FullyQualifiedKind,
    },
}

/// In-memory registry of declared types and trait implementations.
#[derive(Debug, Default)]
pub struct Registry {
    /// Per-type Merge handler. Key: fully-qualified type.
    merges: HashMap<FullyQualifiedType, FullyQualifiedKind>,
    /// Into: (in_type, out_type) -> handler.
    intos: HashMap<(FullyQualifiedType, FullyQualifiedType), FullyQualifiedKind>,
    /// Which types each sender has declared — used for re-register replace
    /// semantics so we don't clobber entries registered by other plugins.
    declared_by: HashMap<String, HashSet<FullyQualifiedType>>,
}

impl Registry {
    /// Fresh empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a full registration payload from `sender`.
    ///
    /// Replaces every prior declaration by this sender with the new set.
    /// Types declared but not referenced by any implementation are kept in
    /// `declared_by` (so a second register with no impls cleanly removes
    /// them on replace); handlers are only stored for declared types.
    pub fn install(
        &mut self,
        sender: &str,
        declared: Vec<FullyQualifiedType>,
        impls: Vec<TraitImpl>,
    ) -> Result<(), CombinatorsError> {
        // Every declared type must belong to the sender's namespace.
        for t in &declared {
            if t.plugin != sender {
                return Err(CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: format!(
                        "declared type `{}` is not in sender namespace `{sender}`",
                        t.to_wire()
                    ),
                });
            }
        }
        let declared_set: HashSet<FullyQualifiedType> = declared.iter().cloned().collect();

        // Validate every impl references a declared type.
        for imp in &impls {
            match imp {
                TraitImpl::Merge { type_, .. } => {
                    if !declared_set.contains(type_) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Merge handler references type `{}` not in `types`",
                                type_.to_wire()
                            ),
                        });
                    }
                }
                TraitImpl::Into { in_, .. } => {
                    if !declared_set.contains(in_) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Into handler references in-type `{}` not in `types`",
                                in_.to_wire()
                            ),
                        });
                    }
                }
            }
        }

        // Handlers must live in the sender's namespace — the register path
        // only lets a plugin promise work it can actually do.
        for imp in &impls {
            let handler = match imp {
                TraitImpl::Merge { handler, .. } | TraitImpl::Into { handler, .. } => handler,
            };
            if handler.plugin != sender {
                return Err(CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: format!(
                        "handler `{}` is not in sender namespace `{sender}`",
                        handler.to_wire()
                    ),
                });
            }
        }

        // Wipe prior entries owned by this sender. We can't just drop keys
        // by iterating `declared_by[sender]` because an Into can use an
        // out-type from another namespace; we scope the purge to entries
        // whose handler namespace matches the sender.
        self.merges.retain(|_, handler| handler.plugin != sender);
        self.intos.retain(|_, handler| handler.plugin != sender);

        // Install.
        for imp in impls {
            match imp {
                TraitImpl::Merge { type_, handler } => {
                    self.merges.insert(type_, handler);
                }
                TraitImpl::Into { in_, out, handler } => {
                    self.intos.insert((in_, out), handler);
                }
            }
        }

        self.declared_by.insert(sender.to_owned(), declared_set);
        Ok(())
    }

    /// Look up the handler for `Merge<type_>`.
    pub fn merge_handler(&self, type_: &FullyQualifiedType) -> Option<&FullyQualifiedKind> {
        self.merges.get(type_)
    }

    /// Look up the handler for `Into<in_, out>`. Retained for the Slice 2
    /// full-Into path — currently unused because Slice 1 stubs Into.
    #[allow(dead_code)]
    pub fn lookup_into(
        &self,
        in_: &FullyQualifiedType,
        out: &FullyQualifiedType,
    ) -> Option<&FullyQualifiedKind> {
        self.intos.get(&(in_.clone(), out.clone()))
    }

    /// How many Merge handlers are registered overall (test helper).
    #[cfg(test)]
    pub fn merge_count(&self) -> usize {
        self.merges.len()
    }
}

/// Parse a `combinators.register` event body into the declared types +
/// impls, using `sender` as the namespace for every bare name.
///
/// Wire shape (per architecture doc):
/// ```json
/// {
///   "kind": "combinators.register",
///   "types": ["Context", "Message"],
///   "implementations": [
///     { "trait": "Merge", "type": "Message", "handler": "message.concat" }
///   ]
/// }
/// ```
pub fn parse_register_body(
    sender: &str,
    body: &Map<String, Value>,
) -> Result<(Vec<FullyQualifiedType>, Vec<TraitImpl>), CombinatorsError> {
    let types_raw = body
        .get("types")
        .ok_or_else(|| CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing `types`".into(),
        })?;
    let types_arr = types_raw
        .as_array()
        .ok_or_else(|| CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: "`types` must be an array".into(),
        })?;
    let mut declared: Vec<FullyQualifiedType> = Vec::with_capacity(types_arr.len());
    for t in types_arr {
        let s = t
            .as_str()
            .ok_or_else(|| CombinatorsError::RegisterRejected {
                code: ErrorCode::MalformedEntry,
                message: "every entry in `types` must be a string".into(),
            })?;
        if s.is_empty() {
            return Err(CombinatorsError::RegisterRejected {
                code: ErrorCode::MalformedEntry,
                message: "entry in `types` must not be empty".into(),
            });
        }
        if s.contains('.') {
            return Err(CombinatorsError::RegisterRejected {
                code: ErrorCode::MalformedEntry,
                message: format!("entry in `types` must be a bare name (no `.`): {s:?}"),
            });
        }
        declared.push(FullyQualifiedType {
            plugin: sender.to_owned(),
            name: s.to_owned(),
        });
    }

    // `implementations` may be absent (empty array is semantically
    // equivalent to "no impls" which is the unregister signal).
    let impls_raw = body
        .get("implementations")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let impls_arr = match impls_raw {
        Value::Array(a) => a,
        _ => {
            return Err(CombinatorsError::RegisterRejected {
                code: ErrorCode::MalformedEntry,
                message: "`implementations` must be an array".into(),
            });
        }
    };

    let mut impls: Vec<TraitImpl> = Vec::with_capacity(impls_arr.len());
    for entry in impls_arr {
        impls.push(parse_impl_entry(sender, &entry)?);
    }

    Ok((declared, impls))
}

fn parse_impl_entry(sender: &str, entry: &Value) -> Result<TraitImpl, CombinatorsError> {
    let obj = entry
        .as_object()
        .ok_or_else(|| CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: "every implementation entry must be an object".into(),
        })?;
    let trait_name = obj.get("trait").and_then(Value::as_str).ok_or_else(|| {
        CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: "implementation entry missing `trait` (string)".into(),
        }
    })?;

    match trait_name {
        "Merge" => {
            let type_bare = obj.get("type").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Merge entry missing `type`".into(),
                }
            })?;
            let handler_bare = obj.get("handler").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Merge entry missing `handler`".into(),
                }
            })?;
            Ok(TraitImpl::Merge {
                type_: FullyQualifiedType {
                    plugin: sender.to_owned(),
                    name: type_bare.to_owned(),
                },
                handler: FullyQualifiedKind {
                    plugin: sender.to_owned(),
                    bare: handler_bare.to_owned(),
                },
            })
        }
        "Into" => {
            let in_bare = obj.get("in").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Into entry missing `in`".into(),
                }
            })?;
            let out_raw = obj.get("out").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Into entry missing `out`".into(),
                }
            })?;
            let handler_bare = obj.get("handler").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Into entry missing `handler`".into(),
                }
            })?;
            let out = if out_raw.contains('.') {
                FullyQualifiedType::parse(out_raw).ok_or_else(|| {
                    CombinatorsError::RegisterRejected {
                        code: ErrorCode::MalformedEntry,
                        message: format!("Into `out` not parseable as `plugin.Type`: {out_raw:?}"),
                    }
                })?
            } else {
                FullyQualifiedType {
                    plugin: sender.to_owned(),
                    name: out_raw.to_owned(),
                }
            };
            Ok(TraitImpl::Into {
                in_: FullyQualifiedType {
                    plugin: sender.to_owned(),
                    name: in_bare.to_owned(),
                },
                out,
                handler: FullyQualifiedKind {
                    plugin: sender.to_owned(),
                    bare: handler_bare.to_owned(),
                },
            })
        }
        other => Err(CombinatorsError::RegisterRejected {
            code: ErrorCode::UnknownTrait,
            message: format!("unknown trait `{other}`"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fqt(plugin: &str, name: &str) -> FullyQualifiedType {
        FullyQualifiedType {
            plugin: plugin.into(),
            name: name.into(),
        }
    }

    fn fqk(plugin: &str, bare: &str) -> FullyQualifiedKind {
        FullyQualifiedKind {
            plugin: plugin.into(),
            bare: bare.into(),
        }
    }

    #[test]
    fn fully_qualified_type_round_trip() {
        let t = FullyQualifiedType::parse("mock-plugin.Message").expect("parse");
        assert_eq!(t.plugin, "mock-plugin");
        assert_eq!(t.name, "Message");
        assert_eq!(t.to_wire(), "mock-plugin.Message");
    }

    #[test]
    fn fully_qualified_type_rejects_empty_sides() {
        assert!(FullyQualifiedType::parse(".Message").is_none());
        assert!(FullyQualifiedType::parse("mock-plugin.").is_none());
        assert!(FullyQualifiedType::parse("nodot").is_none());
    }

    #[test]
    fn registration_stores_namespaced_keys() {
        let mut r = Registry::new();
        let declared = vec![fqt("mock-plugin", "Message")];
        let impls = vec![TraitImpl::Merge {
            type_: fqt("mock-plugin", "Message"),
            handler: fqk("mock-plugin", "message.concat"),
        }];
        r.install("mock-plugin", declared, impls).expect("install");
        let got = r
            .merge_handler(&fqt("mock-plugin", "Message"))
            .expect("found");
        assert_eq!(got.to_wire(), "mock-plugin.message.concat");
    }

    #[test]
    fn reregister_replaces_prior_entries_from_same_sender() {
        let mut r = Registry::new();
        r.install(
            "mock-plugin",
            vec![fqt("mock-plugin", "Message")],
            vec![TraitImpl::Merge {
                type_: fqt("mock-plugin", "Message"),
                handler: fqk("mock-plugin", "message.concat"),
            }],
        )
        .expect("first install");
        // Now register again with a different handler kind.
        r.install(
            "mock-plugin",
            vec![fqt("mock-plugin", "Message")],
            vec![TraitImpl::Merge {
                type_: fqt("mock-plugin", "Message"),
                handler: fqk("mock-plugin", "message.newconcat"),
            }],
        )
        .expect("second install");
        let got = r
            .merge_handler(&fqt("mock-plugin", "Message"))
            .expect("found");
        assert_eq!(got.to_wire(), "mock-plugin.message.newconcat");
        assert_eq!(r.merge_count(), 1, "still only one merge entry");
    }

    #[test]
    fn reregister_with_empty_impls_unregisters_sender_handlers() {
        let mut r = Registry::new();
        r.install(
            "mock-plugin",
            vec![fqt("mock-plugin", "Message")],
            vec![TraitImpl::Merge {
                type_: fqt("mock-plugin", "Message"),
                handler: fqk("mock-plugin", "message.concat"),
            }],
        )
        .expect("install");
        r.install("mock-plugin", vec![], vec![]).expect("unregister");
        assert!(r.merge_handler(&fqt("mock-plugin", "Message")).is_none());
        assert_eq!(r.merge_count(), 0);
    }

    #[test]
    fn reregister_does_not_clobber_other_plugins() {
        let mut r = Registry::new();
        r.install(
            "mock-plugin",
            vec![fqt("mock-plugin", "Message")],
            vec![TraitImpl::Merge {
                type_: fqt("mock-plugin", "Message"),
                handler: fqk("mock-plugin", "message.concat"),
            }],
        )
        .expect("cc install");
        r.install(
            "other",
            vec![fqt("other", "Thing")],
            vec![TraitImpl::Merge {
                type_: fqt("other", "Thing"),
                handler: fqk("other", "thing.merge"),
            }],
        )
        .expect("other install");
        // mock-plugin re-registers with empty impls — must not wipe `other`.
        r.install("mock-plugin", vec![], vec![])
            .expect("cc unregister");
        assert!(r.merge_handler(&fqt("mock-plugin", "Message")).is_none());
        assert!(r.merge_handler(&fqt("other", "Thing")).is_some());
    }

    #[test]
    fn rejects_merge_impl_with_undeclared_type() {
        let mut r = Registry::new();
        let err = r
            .install(
                "mock-plugin",
                vec![fqt("mock-plugin", "Message")],
                vec![TraitImpl::Merge {
                    type_: fqt("mock-plugin", "Context"),
                    handler: fqk("mock-plugin", "ctx.concat"),
                }],
            )
            .unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::TypeNotDeclared);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_handler_outside_sender_namespace() {
        let mut r = Registry::new();
        let err = r
            .install(
                "mock-plugin",
                vec![fqt("mock-plugin", "Message")],
                vec![TraitImpl::Merge {
                    type_: fqt("mock-plugin", "Message"),
                    // forged: handler claims to live in someone else's namespace
                    handler: fqk("other", "thing.merge"),
                }],
            )
            .unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parses_valid_register_body() {
        let body = json!({
            "kind": "combinators.register",
            "types": ["Message", "Context"],
            "implementations": [
                { "trait": "Merge", "type": "Message", "handler": "message.concat" },
                { "trait": "Into",  "in": "Message", "out": "Context", "handler": "msg.to.ctx" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let (declared, impls) = parse_register_body("mock-plugin", &obj).expect("parse");
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0].to_wire(), "mock-plugin.Message");
        assert_eq!(impls.len(), 2);
        match &impls[0] {
            TraitImpl::Merge { type_, handler } => {
                assert_eq!(type_.to_wire(), "mock-plugin.Message");
                assert_eq!(handler.to_wire(), "mock-plugin.message.concat");
            }
            other => panic!("expected Merge, got {other:?}"),
        }
        match &impls[1] {
            TraitImpl::Into { in_, out, handler } => {
                assert_eq!(in_.to_wire(), "mock-plugin.Message");
                assert_eq!(out.to_wire(), "mock-plugin.Context");
                assert_eq!(handler.to_wire(), "mock-plugin.msg.to.ctx");
            }
            other => panic!("expected Into, got {other:?}"),
        }
    }

    #[test]
    fn register_body_accepts_cross_namespace_into_out() {
        let body = json!({
            "types": ["Message"],
            "implementations": [
                { "trait": "Into", "in": "Message", "out": "other-plugin.Thing", "handler": "msg.to.thing" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let (_declared, impls) = parse_register_body("mock-plugin", &obj).expect("parse");
        match &impls[0] {
            TraitImpl::Into { out, .. } => {
                assert_eq!(out.plugin, "other-plugin");
                assert_eq!(out.name, "Thing");
            }
            other => panic!("expected Into, got {other:?}"),
        }
    }

    #[test]
    fn register_body_rejects_unknown_trait() {
        let body = json!({
            "types": ["Message"],
            "implementations": [
                { "trait": "Frobnicate", "type": "Message", "handler": "nope" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_register_body("mock-plugin", &obj).unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::UnknownTrait);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn register_body_rejects_missing_handler() {
        let body = json!({
            "types": ["Message"],
            "implementations": [
                { "trait": "Merge", "type": "Message" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_register_body("mock-plugin", &obj).unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn register_body_rejects_missing_types() {
        let body = json!({ "implementations": [] });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_register_body("mock-plugin", &obj).unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn register_body_rejects_dotted_bare_type() {
        let body = json!({
            "types": ["mock-plugin.Message"],
            "implementations": []
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_register_body("mock-plugin", &obj).unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
