//! Unified registry for combinator declarations.
//!
//! Stage 1 reshape (per `nefor-combinators-spec` §8): replaces the per-trait
//! split maps (`merges`, `intos`) with a single `HashMap<Identity, Owned>`
//! keyed on `(arity, input_type, output_multiset)`. Per-trait validation
//! runs at registration time; storage and lookup are uniform after that.
//!
//! Plugins call `combinators.register` with a namespace-local view (bare
//! type names, bare handler kinds). The registry stores everything under
//! fully-qualified names (`plugin.Type`, `plugin.bare.kind`) so dispatch
//! can look them up by exactly what callers send on the wire.
//!
//! Re-registration semantics: re-sending from the same sender atomically
//! replaces every entry the sender owns. Empty `implementations` is the
//! unregister signal. Late-binding override: identity collisions across
//! senders are not errors — last write wins. Multiset equality on the
//! output side: `[A, B]` and `[B, A]` collide.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::error::{CombinatorsError, ErrorCode};

/// A fully-qualified type name. `plugin` is the NCP `from` identity of the
/// plugin that owns the type (or a cross-namespace reference for `Into`
/// targets); `name` is the bare type name within that plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Plugin names may contain dashes but not dots; bare type names may
    /// not contain dots either. So the first `.` cleanly splits the tag
    /// into (plugin, bare).
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

/// Combinator identity per spec §3: `(arity, input_type, output_multiset)`.
///
/// Output multiset uses a sorted `Vec` (not `BTreeSet`) because spec §3.3
/// admits duplicates at registration time (`Fanout :: T -> {T, T}`). The
/// scheduler's submit-time check rejects duplicate downstream types; the
/// registry stays permissive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    /// Number of input values: 1 for `Into`/`Fanout`, 2 for `Merge`.
    pub arity: u8,
    /// Single input type tag.
    pub input_type: FullyQualifiedType,
    /// Multiset of output types, kept sorted on insertion so vec equality
    /// is multiset equality.
    pub output_multiset: Vec<FullyQualifiedType>,
}

impl Identity {
    /// Construct an identity with the multiset normalised (sorted).
    pub fn new(
        arity: u8,
        input_type: FullyQualifiedType,
        mut outputs: Vec<FullyQualifiedType>,
    ) -> Self {
        outputs.sort();
        Self {
            arity,
            input_type,
            output_multiset: outputs,
        }
    }
}

/// A registered handler with the namespace that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedHandler {
    /// Sender plugin namespace (matches `envelope.from`).
    pub owner: String,
    /// Fully-qualified handler kind to dispatch to.
    pub handler: FullyQualifiedKind,
}

/// Trait declaration parsed from a single entry in
/// `combinators.register.implementations`. The `trait` wire field is the
/// discriminator. Each variant carries the data needed to compute its
/// `Identity`; storage is uniform after that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraitImpl {
    /// `Merge<T> :: (T, T) -> T`.
    Merge {
        /// Type argument (must be declared by the sender).
        type_: FullyQualifiedType,
        /// Handler kind to dispatch to.
        handler: FullyQualifiedKind,
    },
    /// `Into<In, Out> :: In -> Out`. `out` may be cross-namespace.
    Into {
        /// Input type (must be declared by the sender).
        in_: FullyQualifiedType,
        /// Output type — bare in sender or `<plugin>.<Type>`.
        out: FullyQualifiedType,
        /// Handler kind to dispatch to.
        handler: FullyQualifiedKind,
    },
    /// `Fanout :: T -> { U_1, ..., U_k }` (multiset, k >= 1).
    Fanout {
        /// Input type (must be declared by the sender).
        in_: FullyQualifiedType,
        /// Output types (multiset; cross-namespace entries allowed).
        outs: Vec<FullyQualifiedType>,
        /// Handler kind to dispatch to.
        handler: FullyQualifiedKind,
    },
}

impl TraitImpl {
    /// The handler this entry installs.
    fn handler(&self) -> &FullyQualifiedKind {
        match self {
            Self::Merge { handler, .. }
            | Self::Into { handler, .. }
            | Self::Fanout { handler, .. } => handler,
        }
    }

    /// Identity tuple this entry registers under.
    fn identity(&self) -> Identity {
        match self {
            Self::Merge { type_, .. } => Identity::new(2, type_.clone(), vec![type_.clone()]),
            Self::Into { in_, out, .. } => Identity::new(1, in_.clone(), vec![out.clone()]),
            Self::Fanout { in_, outs, .. } => Identity::new(1, in_.clone(), outs.clone()),
        }
    }
}

/// In-memory registry of declared types and trait implementations.
#[derive(Debug, Default)]
pub struct Registry {
    /// Unified identity → owner+handler map.
    entries: HashMap<Identity, OwnedHandler>,
    /// Which types each sender has declared. Tracked for diagnostics and
    /// the install-time TypeNotDeclared check; ownership of installed
    /// entries lives on the entries themselves.
    declared_by: HashMap<String, HashSet<FullyQualifiedType>>,
}

impl Registry {
    /// Fresh empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a parsed registration payload from `sender`.
    ///
    /// Replaces every prior entry whose owner is `sender` with the new set
    /// (atomic from the caller's view; we wipe-then-install). Late-binding
    /// override: an identity already owned by a *different* sender is
    /// silently overwritten — that's the feature.
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

        // Per-trait admissibility:
        //   Merge<T>:  T must be sender-owned (declared).
        //   Fanout::T -> {U_1..U_k}:  T must be sender-owned; U_i may be
        //     cross-namespace.
        //   Into<A, B>: A and B both may be cross-namespace, but the sender
        //     must own AT LEAST ONE side. Preserves namespace-ownership
        //     invariant ("no plugin spoofs another's identity") while
        //     allowing the LSP-shape pattern from parent spec §3 — concrete
        //     provider plugins declare conversions whose `in` is the
        //     canonical generic-provider type.
        for imp in &impls {
            match imp {
                TraitImpl::Merge { type_, .. } => {
                    if !declared_set.contains(type_) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Merge references input type `{}` not in `types`",
                                type_.to_wire()
                            ),
                        });
                    }
                }
                TraitImpl::Fanout { in_, outs, .. } => {
                    if !declared_set.contains(in_) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Fanout references input type `{}` not in `types`",
                                in_.to_wire()
                            ),
                        });
                    }
                    if outs.is_empty() {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::EmptyOutputMultiset,
                            message: format!(
                                "Fanout from `{}` has empty output multiset",
                                in_.to_wire()
                            ),
                        });
                    }
                }
                TraitImpl::Into { in_, out, .. } => {
                    if in_ == out {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::MalformedEntry,
                            message: format!(
                                "Into requires distinct in/out types; got `{}` for both",
                                in_.to_wire()
                            ),
                        });
                    }
                    let owns_in = in_.plugin == sender;
                    let owns_out = out.plugin == sender;
                    if !owns_in && !owns_out {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::MalformedEntry,
                            message: format!(
                                "Into<{}, {}>: sender `{sender}` owns neither side; \
                                 at least one of in/out must be in sender's namespace",
                                in_.to_wire(),
                                out.to_wire()
                            ),
                        });
                    }
                    // If `in` is sender-bare, it must be declared. (If `in`
                    // is cross-namespace, the sender doesn't have to declare
                    // it — they're just declaring how to convert.)
                    if owns_in && !declared_set.contains(in_) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Into references sender-owned input type `{}` not in `types`",
                                in_.to_wire()
                            ),
                        });
                    }
                    // Same for `out`: if sender-owned, must be declared.
                    if owns_out && !declared_set.contains(out) {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::TypeNotDeclared,
                            message: format!(
                                "Into references sender-owned output type `{}` not in `types`",
                                out.to_wire()
                            ),
                        });
                    }
                }
            }
        }

        // Handlers must live in the sender's namespace.
        for imp in &impls {
            let handler = imp.handler();
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

        // Wipe prior entries owned by this sender. Identity-based ownership
        // means any entry with `owner == sender` is replaced; entries owned
        // by other plugins are untouched.
        self.entries.retain(|_, owned| owned.owner != sender);

        // Install new entries. Identity collisions (same identity, different
        // sender) silently overwrite — late-binding override is the feature.
        for imp in impls {
            let identity = imp.identity();
            let handler = imp.handler().clone();
            self.entries.insert(
                identity,
                OwnedHandler {
                    owner: sender.to_owned(),
                    handler,
                },
            );
        }

        self.declared_by.insert(sender.to_owned(), declared_set);
        Ok(())
    }

    /// Install a built-in registration that bypasses the
    /// "sender owns the input/declared types" check.
    ///
    /// Used by the host plugin to register cross-namespace built-ins that
    /// don't fit the wire-side namespace-ownership invariant — `tool_split`
    /// is the canonical example: per parent spec §6.2 its signature is
    /// `generic-provider.ProviderOut -> { generic-tool.ToolCalls,
    /// generic-provider.FinalAnswer }`, none of which live in
    /// `nefor-combinators`'s namespace, but the implementation logic is
    /// hosted here for v1 convenience.
    ///
    /// Wire-side `combinators.register` payloads still go through
    /// [`Registry::install`] which keeps the strict namespace-ownership
    /// rules. Built-ins use this hatch because they don't represent
    /// "plugin X claims ownership of these types"; they represent "the
    /// combinators host has bundled an implementation against types
    /// declared elsewhere."
    ///
    /// The structural validators (Fanout output multiset non-empty, Into
    /// distinct in/out, handler-in-sender-namespace) still run.
    pub fn install_builtin(
        &mut self,
        sender: &str,
        impls: Vec<TraitImpl>,
    ) -> Result<(), CombinatorsError> {
        for imp in &impls {
            match imp {
                TraitImpl::Merge { .. } => {
                    // Merge is unary in its type — no sensible "built-in"
                    // semantics for cross-namespace Merge; reject so we
                    // notice if it ever shows up.
                    return Err(CombinatorsError::RegisterRejected {
                        code: ErrorCode::MalformedEntry,
                        message: "install_builtin does not accept Merge entries".into(),
                    });
                }
                TraitImpl::Fanout { outs, .. } => {
                    if outs.is_empty() {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::EmptyOutputMultiset,
                            message: "built-in Fanout has empty output multiset".into(),
                        });
                    }
                }
                TraitImpl::Into { in_, out, .. } => {
                    if in_ == out {
                        return Err(CombinatorsError::RegisterRejected {
                            code: ErrorCode::MalformedEntry,
                            message: "built-in Into requires distinct in/out types".into(),
                        });
                    }
                }
            }
            let handler = imp.handler();
            if handler.plugin != sender {
                return Err(CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: format!(
                        "built-in handler `{}` not in sender namespace `{sender}`",
                        handler.to_wire()
                    ),
                });
            }
        }

        // Wipe prior entries owned by this sender (so re-installing during
        // tests is idempotent), then insert.
        self.entries.retain(|_, owned| owned.owner != sender);
        for imp in impls {
            let identity = imp.identity();
            let handler = imp.handler().clone();
            self.entries.insert(
                identity,
                OwnedHandler {
                    owner: sender.to_owned(),
                    handler,
                },
            );
        }
        Ok(())
    }

    /// Look up the handler registered for an exact identity. No synthesis.
    /// Used by test code; production callers go through
    /// [`Registry::lookup_or_pass_through`].
    #[allow(dead_code)]
    pub fn lookup(&self, identity: &Identity) -> Option<&OwnedHandler> {
        self.entries.get(identity)
    }

    /// Look up an identity, falling back to `pass_through` synthesis.
    ///
    /// Synthesis covers `Identity { arity: 1, input_type: T, output_multiset: [T] }`
    /// when no explicit registration is found. Spec §6.1: a dispatch-side
    /// fall-through, not a pre-populated entry. Explicit registrations of
    /// the same shape (e.g. a logging pass-through) take precedence — checked
    /// first.
    pub fn lookup_or_pass_through(&self, identity: &Identity) -> Option<OwnedHandler> {
        if let Some(entry) = self.entries.get(identity) {
            return Some(entry.clone());
        }
        if is_pass_through_shape(identity) {
            return Some(OwnedHandler {
                owner: PASS_THROUGH_OWNER.to_owned(),
                handler: FullyQualifiedKind {
                    plugin: PASS_THROUGH_OWNER.to_owned(),
                    bare: PASS_THROUGH_HANDLER.to_owned(),
                },
            });
        }
        None
    }

    /// Convenience: Slice 1's Merge lookup, kept for legacy callers and
    /// the existing test suite. Production dispatch goes through
    /// [`Registry::lookup_or_pass_through`].
    #[allow(dead_code)]
    pub fn merge_handler(&self, type_: &FullyQualifiedType) -> Option<&FullyQualifiedKind> {
        let identity = Identity::new(2, type_.clone(), vec![type_.clone()]);
        self.entries.get(&identity).map(|owned| &owned.handler)
    }

    /// Total entries (test helper).
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

/// Owner namespace for synthesised `pass_through` and the built-in
/// `tool_split` registration. Both live under this plugin.
pub const PASS_THROUGH_OWNER: &str = "nefor-combinators";
/// Bare handler name for synthesised `pass_through`.
pub const PASS_THROUGH_HANDLER: &str = "pass_through";

/// True when `identity` is an arity-1 echo: `T -> {T}`.
fn is_pass_through_shape(identity: &Identity) -> bool {
    identity.arity == 1
        && identity.output_multiset.len() == 1
        && identity.output_multiset[0] == identity.input_type
}

/// Parse a `combinators.register` event body into the declared types +
/// impls, using `sender` as the namespace for every bare name.
///
/// Wire shapes (extending Slice 1):
/// ```json
/// { "kind": "combinators.register",
///   "types": ["Context", "Message"],
///   "implementations": [
///     { "trait": "Merge", "type": "Message", "handler": "message.concat" },
///     { "trait": "Into", "in": "Message", "out": "Context", "handler": "msg.to.ctx" },
///     { "trait": "Fanout", "in": "ProviderOut",
///       "out": ["ToolCalls", "generic-provider.FinalAnswer"],
///       "handler": "provider_out.tool_split" },
///     { "trait": "Equivalent", "a": "ProviderIn", "b": "openai-provider.RawRequest",
///       "handler_a_to_b": "provider_in.to_raw",
///       "handler_b_to_a": "raw.to_provider_in" }
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
        for parsed in parse_impl_entry(sender, &entry)? {
            impls.push(parsed);
        }
    }

    Ok((declared, impls))
}

/// Parse one entry from `implementations[]`. Returns 1 or more `TraitImpl`s
/// — `Equivalent` desugars into two `Into`s.
fn parse_impl_entry(sender: &str, entry: &Value) -> Result<Vec<TraitImpl>, CombinatorsError> {
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
            Ok(vec![TraitImpl::Merge {
                type_: bare_type(sender, type_bare),
                handler: bare_kind(sender, handler_bare),
            }])
        }
        "Into" => {
            let in_raw = obj.get("in").and_then(Value::as_str).ok_or_else(|| {
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
            // `in` may be bare-in-sender-namespace OR cross-namespace per
            // spec §4.1: canonical-protocol-type plugins (generic-provider)
            // own `<them>.ProviderIn` and concrete plugins declare
            // `Into<generic-provider.ProviderIn, openai-provider.RawRequest>`
            // from openai-provider's namespace. The "sender owns at least
            // one side" check lives in install() below.
            let in_ = parse_out_type(sender, in_raw, "Into `in`")?;
            let out = parse_out_type(sender, out_raw, "Into `out`")?;
            Ok(vec![TraitImpl::Into {
                in_,
                out,
                handler: bare_kind(sender, handler_bare),
            }])
        }
        "Fanout" => {
            let in_bare = obj.get("in").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Fanout entry missing `in`".into(),
                }
            })?;
            let out_raw = obj.get("out").and_then(Value::as_array).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Fanout entry missing `out` (array of types)".into(),
                }
            })?;
            if out_raw.is_empty() {
                return Err(CombinatorsError::RegisterRejected {
                    code: ErrorCode::EmptyOutputMultiset,
                    message: "Fanout `out` must be non-empty".into(),
                });
            }
            let mut outs: Vec<FullyQualifiedType> = Vec::with_capacity(out_raw.len());
            for v in out_raw {
                let raw = v
                    .as_str()
                    .ok_or_else(|| CombinatorsError::RegisterRejected {
                        code: ErrorCode::MalformedEntry,
                        message: "Fanout `out` entries must be strings".into(),
                    })?;
                outs.push(parse_out_type(sender, raw, "Fanout `out` entry")?);
            }
            let handler_bare = obj.get("handler").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Fanout entry missing `handler`".into(),
                }
            })?;
            Ok(vec![TraitImpl::Fanout {
                in_: bare_type(sender, in_bare),
                outs,
                handler: bare_kind(sender, handler_bare),
            }])
        }
        "Equivalent" => {
            let a_bare = obj.get("a").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Equivalent entry missing `a`".into(),
                }
            })?;
            let b_raw = obj.get("b").and_then(Value::as_str).ok_or_else(|| {
                CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Equivalent entry missing `b`".into(),
                }
            })?;
            let h_ab = obj
                .get("handler_a_to_b")
                .and_then(Value::as_str)
                .ok_or_else(|| CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Equivalent entry missing `handler_a_to_b`".into(),
                })?;
            let h_ba = obj
                .get("handler_b_to_a")
                .and_then(Value::as_str)
                .ok_or_else(|| CombinatorsError::RegisterRejected {
                    code: ErrorCode::MalformedEntry,
                    message: "Equivalent entry missing `handler_b_to_a`".into(),
                })?;
            // Symmetric: either side may be cross-namespace; install()
            // enforces "sender owns at least one side" via the desugared
            // Into entries.
            let a = parse_out_type(sender, a_bare, "Equivalent `a`")?;
            let b = parse_out_type(sender, b_raw, "Equivalent `b`")?;
            Ok(vec![
                TraitImpl::Into {
                    in_: a.clone(),
                    out: b.clone(),
                    handler: bare_kind(sender, h_ab),
                },
                TraitImpl::Into {
                    in_: b,
                    out: a,
                    handler: bare_kind(sender, h_ba),
                },
            ])
        }
        other => Err(CombinatorsError::RegisterRejected {
            code: ErrorCode::UnknownTrait,
            message: format!("unknown trait `{other}`"),
        }),
    }
}

fn bare_type(sender: &str, bare: &str) -> FullyQualifiedType {
    FullyQualifiedType {
        plugin: sender.to_owned(),
        name: bare.to_owned(),
    }
}

fn bare_kind(sender: &str, bare: &str) -> FullyQualifiedKind {
    FullyQualifiedKind {
        plugin: sender.to_owned(),
        bare: bare.to_owned(),
    }
}

/// Parse an output-position type tag: bare → sender namespace,
/// `<plugin>.<Type>` → cross-namespace.
fn parse_out_type(
    sender: &str,
    raw: &str,
    field_label: &str,
) -> Result<FullyQualifiedType, CombinatorsError> {
    if raw.contains('.') {
        FullyQualifiedType::parse(raw).ok_or_else(|| CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: format!("{field_label} not parseable as `plugin.Type`: {raw:?}"),
        })
    } else if raw.is_empty() {
        Err(CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: format!("{field_label} must not be empty"),
        })
    } else {
        Ok(FullyQualifiedType {
            plugin: sender.to_owned(),
            name: raw.to_owned(),
        })
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
    fn identity_normalises_multiset_order() {
        let id_ab = Identity::new(1, fqt("p", "T"), vec![fqt("p", "A"), fqt("p", "B")]);
        let id_ba = Identity::new(1, fqt("p", "T"), vec![fqt("p", "B"), fqt("p", "A")]);
        assert_eq!(id_ab, id_ba);
    }

    #[test]
    fn identity_distinguishes_arity() {
        let merge = Identity::new(2, fqt("p", "T"), vec![fqt("p", "T")]);
        let fanout = Identity::new(1, fqt("p", "T"), vec![fqt("p", "T")]);
        assert_ne!(merge, fanout);
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
        assert_eq!(r.entry_count(), 1, "still only one entry");
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
        r.install("mock-plugin", vec![], vec![])
            .expect("unregister");
        assert!(r.merge_handler(&fqt("mock-plugin", "Message")).is_none());
        assert_eq!(r.entry_count(), 0);
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
    fn fanout_registration_stores_under_unified_identity() {
        let mut r = Registry::new();
        r.install(
            "p",
            vec![fqt("p", "T")],
            vec![TraitImpl::Fanout {
                in_: fqt("p", "T"),
                outs: vec![fqt("p", "A"), fqt("p", "B")],
                handler: fqk("p", "split"),
            }],
        )
        .expect("install fanout");
        // Lookup with reordered multiset must hit.
        let id = Identity::new(1, fqt("p", "T"), vec![fqt("p", "B"), fqt("p", "A")]);
        let entry = r.lookup(&id).expect("multiset-equality lookup");
        assert_eq!(entry.handler.to_wire(), "p.split");
        assert_eq!(entry.owner, "p");
    }

    #[test]
    fn fanout_rejects_empty_output_multiset() {
        let mut r = Registry::new();
        let err = r
            .install(
                "p",
                vec![fqt("p", "T")],
                vec![TraitImpl::Fanout {
                    in_: fqt("p", "T"),
                    outs: vec![],
                    handler: fqk("p", "split"),
                }],
            )
            .unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::EmptyOutputMultiset);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn into_accepts_cross_namespace_in_when_sender_owns_out() {
        // LSP-shape: a concrete provider plugin declares
        // Into<generic-provider.ProviderIn → openai-provider.RawRequest>
        // from openai-provider's namespace. `in` is cross-namespace; `out`
        // is sender-owned. Must succeed (per spec §4.1).
        let mut r = Registry::new();
        r.install(
            "openai-provider",
            vec![fqt("openai-provider", "RawRequest")],
            vec![TraitImpl::Into {
                in_: fqt("generic-provider", "ProviderIn"),
                out: fqt("openai-provider", "RawRequest"),
                handler: fqk("openai-provider", "into.provider_in"),
            }],
        )
        .expect("LSP-shape Into accepted");
    }

    #[test]
    fn into_rejects_when_sender_owns_neither_side() {
        // Sender must own at least one side. Registering an Into between
        // two foreign types is namespace-spoofing and rejected.
        let mut r = Registry::new();
        let err = r
            .install(
                "spoofer",
                vec![],
                vec![TraitImpl::Into {
                    in_: fqt("generic-provider", "ProviderIn"),
                    out: fqt("openai-provider", "RawRequest"),
                    handler: fqk("spoofer", "spoof"),
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
    fn into_rejects_same_in_out() {
        let mut r = Registry::new();
        let err = r
            .install(
                "p",
                vec![fqt("p", "T")],
                vec![TraitImpl::Into {
                    in_: fqt("p", "T"),
                    out: fqt("p", "T"),
                    handler: fqk("p", "id"),
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
    fn late_binding_override_takes_latest_owner_within_namespace() {
        let mut r = Registry::new();
        r.install(
            "p",
            vec![fqt("p", "T")],
            vec![TraitImpl::Fanout {
                in_: fqt("p", "T"),
                outs: vec![fqt("p", "A"), fqt("p", "B")],
                handler: fqk("p", "h1"),
            }],
        )
        .expect("first");
        r.install(
            "p",
            vec![fqt("p", "T")],
            vec![TraitImpl::Fanout {
                in_: fqt("p", "T"),
                outs: vec![fqt("p", "B"), fqt("p", "A")], // same multiset
                handler: fqk("p", "h2"),
            }],
        )
        .expect("second");
        let id = Identity::new(1, fqt("p", "T"), vec![fqt("p", "A"), fqt("p", "B")]);
        let got = r.lookup(&id).expect("found");
        assert_eq!(got.handler.to_wire(), "p.h2", "latest write wins");
    }

    #[test]
    fn pass_through_synthesised_when_unregistered() {
        let r = Registry::new();
        let id = Identity::new(1, fqt("foo", "X"), vec![fqt("foo", "X")]);
        let got = r.lookup_or_pass_through(&id).expect("synthesised");
        assert_eq!(got.handler.to_wire(), "nefor-combinators.pass_through");
        assert_eq!(got.owner, "nefor-combinators");
    }

    #[test]
    fn pass_through_does_not_synthesise_when_arity_mismatches() {
        let r = Registry::new();
        let id = Identity::new(2, fqt("foo", "X"), vec![fqt("foo", "X")]); // Merge shape
        assert!(r.lookup_or_pass_through(&id).is_none());
    }

    #[test]
    fn pass_through_does_not_synthesise_when_outputs_differ() {
        let r = Registry::new();
        let id = Identity::new(1, fqt("foo", "X"), vec![fqt("foo", "Y")]);
        assert!(r.lookup_or_pass_through(&id).is_none());
    }

    #[test]
    fn explicit_registration_beats_pass_through_synthesis() {
        let mut r = Registry::new();
        r.install(
            "logger",
            vec![fqt("logger", "Event")],
            vec![TraitImpl::Fanout {
                in_: fqt("logger", "Event"),
                outs: vec![fqt("logger", "Event")],
                handler: fqk("logger", "log_through"),
            }],
        )
        .expect("install");
        let id = Identity::new(1, fqt("logger", "Event"), vec![fqt("logger", "Event")]);
        let got = r.lookup_or_pass_through(&id).expect("found");
        assert_eq!(got.handler.to_wire(), "logger.log_through");
        assert_eq!(got.owner, "logger");
    }

    #[test]
    fn equivalent_desugars_into_two_intos() {
        let body = json!({
            "types": ["A"],
            "implementations": [
                { "trait": "Equivalent",
                  "a": "A",
                  "b": "other.B",
                  "handler_a_to_b": "ab",
                  "handler_b_to_a": "ba" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let (declared, impls) = parse_register_body("p", &obj).expect("parse");
        assert_eq!(declared.len(), 1);
        assert_eq!(impls.len(), 2);
        match &impls[0] {
            TraitImpl::Into { in_, out, handler } => {
                assert_eq!(in_.to_wire(), "p.A");
                assert_eq!(out.to_wire(), "other.B");
                assert_eq!(handler.to_wire(), "p.ab");
            }
            other => panic!("expected Into, got {other:?}"),
        }
        match &impls[1] {
            TraitImpl::Into { in_, out, handler } => {
                assert_eq!(in_.to_wire(), "other.B");
                assert_eq!(out.to_wire(), "p.A");
                assert_eq!(handler.to_wire(), "p.ba");
            }
            other => panic!("expected Into, got {other:?}"),
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
    }

    #[test]
    fn parses_fanout_register_body() {
        let body = json!({
            "types": ["ProviderOut"],
            "implementations": [
                { "trait": "Fanout",
                  "in": "ProviderOut",
                  "out": ["ToolCalls", "generic-provider.FinalAnswer"],
                  "handler": "provider_out.tool_split" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let (_decl, impls) = parse_register_body("openai-provider", &obj).expect("parse");
        match &impls[0] {
            TraitImpl::Fanout { in_, outs, handler } => {
                assert_eq!(in_.to_wire(), "openai-provider.ProviderOut");
                assert_eq!(outs.len(), 2);
                assert!(outs
                    .iter()
                    .any(|t| t.to_wire() == "openai-provider.ToolCalls"));
                assert!(outs
                    .iter()
                    .any(|t| t.to_wire() == "generic-provider.FinalAnswer"));
                assert_eq!(handler.to_wire(), "openai-provider.provider_out.tool_split");
            }
            other => panic!("expected Fanout, got {other:?}"),
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

    #[test]
    fn fanout_empty_out_array_rejected_at_parse() {
        let body = json!({
            "types": ["T"],
            "implementations": [
                { "trait": "Fanout", "in": "T", "out": [], "handler": "h" }
            ]
        });
        let obj = body.as_object().expect("obj").clone();
        let err = parse_register_body("p", &obj).unwrap_err();
        match err {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::EmptyOutputMultiset);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
