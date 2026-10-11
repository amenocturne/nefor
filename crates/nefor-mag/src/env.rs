use crate::ast::{
    BindingId, BindingSlot, CheckedExpr, FnValue, FrameId, Scope, ScopeFrame, TypeDecl, Value,
};
use crate::error::MagError;
use crate::profile::{CompileProfiler, Phase, ProfileTimer};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

const FRAME_COLLECTION_THRESHOLD: usize = 1_024;

// Compound MAG values are immutable shared allocations. Holding the Value in
// the key both keeps its allocation alive and lets repeated uses compare by
// identity without walking large graphs. Scalars compare by value because
// their identity is their value. Cache eviction changes cost, never semantics.
#[derive(Debug, Clone)]
struct MemoArg(Value);

impl MemoArg {
    fn new(value: &Value) -> Option<Self> {
        if matches!(
            value,
            Value::Artifact(_)
                | Value::TypeDescriptor(_)
                | Value::TypeSchema(_)
                | Value::SemanticTypeId(_)
                | Value::PackedValue(_)
                | Value::JsonValue(_)
                | Value::HostInputs(_)
        ) {
            None
        } else {
            Some(Self(value.clone()))
        }
    }
}

impl PartialEq for MemoArg {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Value::Unit, Value::Unit) => true,
            (Value::Str(left), Value::Str(right))
            | (Value::Keyword(left), Value::Keyword(right))
            | (Value::Symbol(left), Value::Symbol(right))
            | (Value::BuiltinFn(left), Value::BuiltinFn(right)) => left == right,
            (Value::Int(left), Value::Int(right)) => left == right,
            (Value::Float(left), Value::Float(right)) => left.to_bits() == right.to_bits(),
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::List(left), Value::List(right))
            | (Value::Product(left), Value::Product(right))
            | (Value::Set(left), Value::Set(right)) => Arc::ptr_eq(left, right),
            (Value::Fields(left), Value::Fields(right))
            | (Value::ModuleNamespace(left), Value::ModuleNamespace(right)) => {
                Arc::ptr_eq(left, right)
            }
            (Value::Map(left), Value::Map(right)) => Arc::ptr_eq(left, right),
            (Value::Fn(left), Value::Fn(right)) => Arc::ptr_eq(left, right),
            (Value::Type(left), Value::Type(right)) => left == right,
            (Value::TypeTag(left), Value::TypeTag(right)) => left == right,
            (Value::TypeDecl(left), Value::TypeDecl(right)) => left == right,
            (
                Value::Adt {
                    owner: left_owner,
                    constructor: left_constructor,
                    payload: left,
                },
                Value::Adt {
                    owner: right_owner,
                    constructor: right_constructor,
                    payload: right,
                },
            ) => {
                left_owner == right_owner
                    && left_constructor == right_constructor
                    && Arc::ptr_eq(left, right)
            }
            (Value::Typed(left, left_type), Value::Typed(right, right_type)) => {
                left_type == right_type && Arc::ptr_eq(left, right)
            }
            _ => false,
        }
    }
}

impl Eq for MemoArg {}

impl Hash for MemoArg {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(&self.0).hash(state);
        match &self.0 {
            Value::Unit => {}
            Value::Str(value)
            | Value::Keyword(value)
            | Value::Symbol(value)
            | Value::BuiltinFn(value) => value.hash(state),
            Value::Int(value) => value.hash(state),
            Value::Float(value) => value.to_bits().hash(state),
            Value::Bool(value) => value.hash(state),
            Value::List(value) | Value::Product(value) | Value::Set(value) => {
                Arc::as_ptr(value).hash(state)
            }
            Value::Fields(value) | Value::ModuleNamespace(value) => Arc::as_ptr(value).hash(state),
            Value::Map(value) => Arc::as_ptr(value).hash(state),
            Value::Fn(value) => Arc::as_ptr(value).hash(state),
            Value::Type(value) => value.hash(state),
            Value::TypeTag(value) => value.hash(state),
            Value::TypeDecl(value) => value.hash(state),
            Value::Adt {
                owner,
                constructor,
                payload,
            } => {
                owner.hash(state);
                constructor.hash(state);
                Arc::as_ptr(payload).hash(state);
            }
            Value::Typed(value, ty) => {
                Arc::as_ptr(value).hash(state);
                ty.hash(state);
            }
            Value::Artifact(_)
            | Value::TypeDescriptor(_)
            | Value::TypeSchema(_)
            | Value::SemanticTypeId(_)
            | Value::PackedValue(_)
            | Value::JsonValue(_)
            | Value::HostInputs(_) => unreachable!("opaque values are not memoized arguments"),
        }
    }
}

// Import idempotence includes compiler values; it is not language-level equality.
fn same_imported_value(env: &Env, left: &Value, right: &Value) -> bool {
    if MemoArg::new(left)
        .zip(MemoArg::new(right))
        .is_some_and(|(a, b)| a == b)
    {
        return true;
    }
    match (left, right) {
        (Value::TypeDescriptor(a), Value::TypeDescriptor(b)) => a == b,
        (Value::TypeSchema(a), Value::TypeSchema(b)) => a == b,
        (Value::SemanticTypeId(a), Value::SemanticTypeId(b)) => a == b,
        (Value::PackedValue(a), Value::PackedValue(b)) => Arc::ptr_eq(a, b),
        (Value::HostInputs(a), Value::HostInputs(b)) | (Value::Artifact(a), Value::Artifact(b)) => {
            a == b
        }
        _ => crate::eval::equal(env, left, right),
    }
}

#[derive(Debug, Clone)]
struct MemoCall {
    function: Arc<FnValue>,
    resolved_signature: Option<crate::types::MagType>,
    type_bindings: BTreeMap<String, crate::types::MagType>,
    args: Vec<MemoArg>,
}

impl PartialEq for MemoCall {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.function, &other.function)
            && self.args == other.args
            && self.resolved_signature == other.resolved_signature
            && self.type_bindings == other.type_bindings
    }
}

impl Eq for MemoCall {}

impl Hash for MemoCall {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.function).hash(state);
        self.resolved_signature.hash(state);
        self.type_bindings.hash(state);
        self.args.hash(state);
    }
}

#[derive(Debug, Default)]
struct CompilationState {
    limits: crate::CompilerLimits,
    next_binding_id: u64,
    next_frame_id: u64,
    frame_allocations_since_collection: usize,
    frame_collection_interval: usize,
    bindings: HashMap<BindingId, BindingMetadata>,
    equality_requirements: HashMap<BindingId, Vec<crate::types::MagType>>,
    frames: HashMap<FrameId, ScopeFrame>,
    frame_roots: HashMap<FrameId, usize>,
    loaded: HashMap<String, BTreeMap<String, Vec<Value>>>,
    semantic_types: HashMap<String, TypeDecl>,
    loading: Vec<String>,
    file_reads: HashMap<PathBuf, Result<String, String>>,
    memoized_calls: HashMap<MemoCall, Value>,
}

#[derive(Debug, Clone)]
pub struct BindingMetadata {
    pub name: String,
    pub ty: Option<crate::types::MagType>,
}

#[derive(Debug, Clone)]
pub struct BindingHandle {
    pub id: BindingId,
    pub frame: Scope,
    state: Weak<Mutex<CompilationState>>,
    profiler: Option<CompileProfiler>,
}

#[derive(Debug, Clone)]
pub enum BindingForce {
    Ready(Value),
    Initialize {
        handle: BindingHandle,
        initializer: Arc<CheckedExpr>,
    },
}

#[derive(Debug)]
pub struct Env {
    observer: Option<crate::observation::Observer>,
    scopes: Vec<Scope>,
    source_dir: PathBuf,
    module_roots: Vec<PathBuf>,
    module: String,
    state: Arc<Mutex<CompilationState>>,
    profiler: Option<CompileProfiler>,
    /// Import-collision diagnostics for names this module both imports and
    /// declares. Whether such a pair collides depends on the declaration's
    /// type, which only the checker knows.
    pending_import_collisions: Arc<BTreeMap<String, String>>,
}

impl Clone for Env {
    fn clone(&self) -> Self {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            for frame in &self.scopes {
                *state.frame_roots.entry(*frame).or_default() += 1;
            }
        }
        Self {
            observer: self.observer.clone(),
            scopes: self.scopes.clone(),
            source_dir: self.source_dir.clone(),
            module_roots: self.module_roots.clone(),
            module: self.module.clone(),
            state: self.state.clone(),
            profiler: self.profiler.clone(),
            pending_import_collisions: self.pending_import_collisions.clone(),
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for frame in &self.scopes {
            if let Some(count) = state.frame_roots.get_mut(frame) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.frame_roots.remove(frame);
                }
            }
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    pub fn new() -> Self {
        Self::new_in(
            Path::new("."),
            vec![PathBuf::from(".")],
            "main",
            Arc::new(Mutex::new(CompilationState::default())),
            None,
        )
    }
    fn new_in(
        source_dir: &Path,
        module_roots: Vec<PathBuf>,
        module: &str,
        state: Arc<Mutex<CompilationState>>,
        profiler: Option<CompileProfiler>,
    ) -> Self {
        let root = Self::allocate_frame_in(&state);
        let mut env = Self {
            observer: None,
            scopes: vec![root],
            source_dir: source_dir.into(),
            module_roots,
            module: module.into(),
            state,
            profiler,
            pending_import_collisions: Arc::default(),
        };
        for &name in crate::checker::BUILTIN_NAMES {
            env.define(name, Value::BuiltinFn(name.into()));
        }
        for (name, params) in [
            ("List", vec!["T"]),
            ("Map", vec!["K", "V"]),
            ("Set", vec!["T"]),
        ] {
            env.define(
                name,
                Value::TypeDecl(TypeDecl {
                    name: format!("core.{name}"),
                    params: params.into_iter().map(str::to_owned).collect(),
                    body: crate::ast::TypeDeclBody::Native,
                }),
            );
        }
        for (name, ty) in [
            ("Artifact", crate::types::MagType::Artifact),
            ("JsonValue", crate::types::MagType::JsonValue),
            ("TypeDescriptor", crate::types::MagType::TypeDescriptor),
            ("TypeSchema", crate::types::MagType::TypeSchema),
            ("SemanticTypeId", crate::types::MagType::SemanticTypeId),
            ("PackedValue", crate::types::MagType::PackedValue),
            ("Unit", crate::types::MagType::Unit),
            ("Bool", crate::types::MagType::Bool),
            ("Int", crate::types::MagType::Int),
            ("Float", crate::types::MagType::Float),
            ("String", crate::types::MagType::String),
        ] {
            env.define(name, Value::Type(ty));
        }
        env
    }
    fn allocate_frame_in(state: &Arc<Mutex<CompilationState>>) -> FrameId {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        let id = FrameId(state.next_frame_id);
        state.next_frame_id = state.next_frame_id.saturating_add(1);
        state.frame_allocations_since_collection =
            state.frame_allocations_since_collection.saturating_add(1);
        state.frames.insert(id, ScopeFrame::default());
        *state.frame_roots.entry(id).or_default() += 1;
        id
    }
    pub fn new_with_stdlib() -> Self {
        Self::new()
    }
    pub fn new_with_stdlib_and_source_dir(path: &Path) -> Self {
        Self::new_with_stdlib_source_dir_and_module_roots(path, vec![path.to_path_buf()])
    }
    pub fn new_with_stdlib_source_dir_and_module_roots(
        path: &Path,
        module_roots: Vec<PathBuf>,
    ) -> Self {
        Self::new_with_stdlib_source_dir_module_roots_and_profiler(path, module_roots, None)
    }
    pub fn new_with_stdlib_source_dir_module_roots_and_profiler(
        path: &Path,
        module_roots: Vec<PathBuf>,
        profiler: Option<CompileProfiler>,
    ) -> Self {
        Self::new_with_stdlib_source_dir_module_roots_profiler_and_limits(
            path,
            module_roots,
            profiler,
            crate::CompilerLimits::default(),
        )
    }
    pub(crate) fn new_with_stdlib_source_dir_module_roots_profiler_and_limits(
        path: &Path,
        module_roots: Vec<PathBuf>,
        profiler: Option<CompileProfiler>,
        limits: crate::CompilerLimits,
    ) -> Self {
        Self::new_in(
            path,
            module_roots,
            "main",
            Arc::new(Mutex::new(CompilationState {
                limits,
                ..CompilationState::default()
            })),
            profiler,
        )
    }
    pub(crate) fn compiler_limits(&self) -> crate::CompilerLimits {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .limits
    }
    pub fn source_dir(&self) -> &Path {
        &self.source_dir
    }
    pub fn module_roots(&self) -> &[PathBuf] {
        &self.module_roots
    }
    pub fn module(&self) -> &str {
        &self.module
    }
    pub fn qualify(&self, local: &str) -> String {
        if self.module == "main" {
            format!("main.{local}")
        } else {
            format!("{}.{local}", self.module)
        }
    }
    pub fn push_scope(&mut self) {
        self.scopes.push(Self::allocate_frame_in(&self.state));
    }
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            if let Some(frame) = self.scopes.pop() {
                self.release_frame_root(frame);
            }
        }
    }
    fn release_frame_root(&self, frame: FrameId) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(count) = state.frame_roots.get_mut(&frame) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.frame_roots.remove(&frame);
            }
        }
    }
    pub fn allocate_binding_id(&self, name: &str, ty: Option<crate::types::MagType>) -> BindingId {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let id = BindingId(state.next_binding_id);
        state.next_binding_id = state.next_binding_id.saturating_add(1);
        state.bindings.insert(
            id,
            BindingMetadata {
                name: name.to_owned(),
                ty,
            },
        );
        id
    }
    pub fn binding_metadata(&self, id: BindingId) -> Option<BindingMetadata> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bindings
            .get(&id)
            .cloned()
    }
    pub fn set_binding_type(&self, id: BindingId, ty: crate::types::MagType) {
        if let Some(binding) = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bindings
            .get_mut(&id)
        {
            binding.ty = Some(ty);
        }
    }
    pub(crate) fn set_equality_requirements(
        &self,
        id: BindingId,
        requirements: Vec<crate::types::MagType>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if requirements.is_empty() {
            state.equality_requirements.remove(&id);
        } else {
            state.equality_requirements.insert(id, requirements);
        }
    }
    pub(crate) fn equality_requirements(&self, id: BindingId) -> Vec<crate::types::MagType> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .equality_requirements
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
    pub fn declare_binding_slot(
        &mut self,
        id: BindingId,
        name: &str,
        initializer: Arc<CheckedExpr>,
    ) -> Result<(), MagError> {
        let frame = self
            .scopes
            .last()
            .ok_or_else(|| MagError::Eval("binding declaration requires a scope".into()))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = state
            .frames
            .get_mut(frame)
            .ok_or_else(|| MagError::Eval("binding declaration frame was reclaimed".into()))?;
        if frame.slots.contains_key(&id) {
            return Err(MagError::Eval(format!(
                "binding slot {} is already declared",
                id.0
            )));
        }
        frame.names.entry(name.to_owned()).or_default().push(id);
        frame
            .slots
            .insert(id, BindingSlot::Uninitialized(initializer));
        drop(state);
        self.profile_counters(|counters| {
            counters.binding_slots_declared = counters.binding_slots_declared.saturating_add(1);
        });
        Ok(())
    }
    pub fn define_ready(&mut self, id: BindingId, name: &str, value: Value) {
        if let Some(frame) = self.scopes.last() {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(frame) = state.frames.get_mut(frame) else {
                return;
            };
            let ids = frame.names.entry(name.to_owned()).or_default();
            if !ids.contains(&id) {
                ids.push(id);
            }
            frame.slots.insert(id, BindingSlot::Ready(value));
        }
    }
    pub fn lookup_candidate_ids(&self, name: &str) -> Vec<BindingId> {
        let mut seen = HashSet::new();
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| {
                state
                    .frames
                    .get(scope)
                    .and_then(|frame| frame.names.get(name))
                    .cloned()
                    .unwrap_or_default()
            })
            .filter(|id| seen.insert(*id))
            .collect()
    }
    pub fn lookup_handles(&self, name: &str) -> Vec<BindingHandle> {
        let mut seen = HashSet::new();
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| {
                let ids = state
                    .frames
                    .get(scope)
                    .and_then(|frame| frame.names.get(name))
                    .cloned()
                    .unwrap_or_default();
                ids.into_iter().map(|id| BindingHandle {
                    id,
                    frame: *scope,
                    state: Arc::downgrade(&self.state),
                    profiler: self.profiler.clone(),
                })
            })
            .filter(|handle| seen.insert(handle.id))
            .collect()
    }
    fn frame_for_binding(&self, id: BindingId) -> Option<Scope> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes.iter().rev().find_map(|scope| {
            state
                .frames
                .get(scope)
                .is_some_and(|frame| frame.slots.contains_key(&id))
                .then_some(*scope)
        })
    }
    pub fn binding_handle(&self, id: BindingId) -> Result<BindingHandle, MagError> {
        let frame = self
            .frame_for_binding(id)
            .ok_or_else(|| MagError::Unresolved(format!("binding#{}", id.0)))?;
        Ok(BindingHandle {
            id,
            frame,
            state: Arc::downgrade(&self.state),
            profiler: self.profiler.clone(),
        })
    }

    pub(crate) fn profile_force_cycle(handle: &BindingHandle) {
        if let Some(profiler) = &handle.profiler {
            profiler.update_counters(|counters| {
                counters.binding_force_cycles = counters.binding_force_cycles.saturating_add(1);
            });
        }
    }
    pub fn begin_binding_force(&self, id: BindingId) -> Result<BindingForce, MagError> {
        Self::begin_handle_force(&self.binding_handle(id)?)
    }
    pub fn begin_handle_force(handle: &BindingHandle) -> Result<BindingForce, MagError> {
        let state = handle
            .state
            .upgrade()
            .ok_or_else(|| MagError::Eval("binding program has been dropped".into()))?;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = state
            .frames
            .get_mut(&handle.frame)
            .ok_or_else(|| MagError::Eval("binding frame was reclaimed".into()))?;
        match frame.slots.get_mut(&handle.id) {
            Some(BindingSlot::Ready(value)) => {
                if let Some(profiler) = &handle.profiler {
                    profiler.update_counters(|counters| {
                        counters.binding_force_ready_hits =
                            counters.binding_force_ready_hits.saturating_add(1)
                    });
                }
                Ok(BindingForce::Ready(value.clone()))
            }
            Some(slot @ BindingSlot::Uninitialized(_)) => {
                if let Some(profiler) = &handle.profiler {
                    profiler.update_counters(|counters| {
                        counters.binding_force_initializations =
                            counters.binding_force_initializations.saturating_add(1)
                    });
                }
                let BindingSlot::Uninitialized(initializer) =
                    std::mem::replace(slot, BindingSlot::Initializing)
                else {
                    unreachable!()
                };
                Ok(BindingForce::Initialize {
                    handle: handle.clone(),
                    initializer,
                })
            }
            Some(BindingSlot::Initializing) => {
                if let Some(profiler) = &handle.profiler {
                    profiler.update_counters(|counters| {
                        counters.binding_force_cycles =
                            counters.binding_force_cycles.saturating_add(1)
                    });
                }
                Err(MagError::Eval(format!(
                    "binding initialization cycle at binding#{}",
                    handle.id.0
                )))
            }
            None => Err(MagError::Unresolved(format!("binding#{}", handle.id.0))),
        }
    }
    pub fn complete_binding_force(&self, id: BindingId, value: Value) -> Result<(), MagError> {
        let frame = self
            .frame_for_binding(id)
            .ok_or_else(|| MagError::Unresolved(format!("binding#{}", id.0)))?;
        Self::complete_handle_force(
            &BindingHandle {
                id,
                frame,
                state: Arc::downgrade(&self.state),
                profiler: self.profiler.clone(),
            },
            value,
        )
    }
    pub fn complete_handle_force(handle: &BindingHandle, value: Value) -> Result<(), MagError> {
        let state = handle
            .state
            .upgrade()
            .ok_or_else(|| MagError::Eval("binding program has been dropped".into()))?;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = state
            .frames
            .get_mut(&handle.frame)
            .ok_or_else(|| MagError::Eval("binding frame was reclaimed".into()))?;
        match frame.slots.get_mut(&handle.id) {
            Some(slot @ BindingSlot::Initializing) => {
                *slot = BindingSlot::Ready(value);
                Ok(())
            }
            Some(_) => Err(MagError::Eval(format!(
                "binding#{} completed outside initialization",
                handle.id.0
            ))),
            None => Err(MagError::Unresolved(format!("binding#{}", handle.id.0))),
        }
    }
    pub fn reset_binding_force(
        &self,
        id: BindingId,
        initializer: Arc<CheckedExpr>,
    ) -> Result<(), MagError> {
        let frame = self
            .frame_for_binding(id)
            .ok_or_else(|| MagError::Unresolved(format!("binding#{}", id.0)))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = state
            .frames
            .get_mut(&frame)
            .ok_or_else(|| MagError::Eval("binding frame was reclaimed".into()))?;
        match frame.slots.get_mut(&id) {
            Some(slot @ BindingSlot::Initializing) => {
                *slot = BindingSlot::Uninitialized(initializer);
                Ok(())
            }
            Some(_) => Err(MagError::Eval(format!(
                "binding#{} reset outside initialization",
                id.0
            ))),
            None => Err(MagError::Unresolved(format!("binding#{}", id.0))),
        }
    }
    pub fn reset_handle_force(
        handle: &BindingHandle,
        initializer: Arc<CheckedExpr>,
    ) -> Result<(), MagError> {
        let state = handle
            .state
            .upgrade()
            .ok_or_else(|| MagError::Eval("binding program has been dropped".into()))?;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = state
            .frames
            .get_mut(&handle.frame)
            .ok_or_else(|| MagError::Eval("binding frame was reclaimed".into()))?;
        match frame.slots.get_mut(&handle.id) {
            Some(slot @ BindingSlot::Initializing) => {
                *slot = BindingSlot::Uninitialized(initializer);
                Ok(())
            }
            Some(_) => Err(MagError::Eval(format!(
                "binding#{} reset outside initialization",
                handle.id.0
            ))),
            None => Err(MagError::Unresolved(format!("binding#{}", handle.id.0))),
        }
    }
    pub fn ready_handle(handle: &BindingHandle) -> Result<Value, MagError> {
        match Self::begin_handle_force(handle)? {
            BindingForce::Ready(value) => Ok(value),
            BindingForce::Initialize {
                handle,
                initializer,
            } => {
                Self::reset_handle_force(&handle, initializer)?;
                Err(MagError::Eval(format!(
                    "binding#{} has not been initialized",
                    handle.id.0
                )))
            }
        }
    }
    pub fn ready_binding(&self, id: BindingId) -> Result<Value, MagError> {
        match self.begin_binding_force(id)? {
            BindingForce::Ready(value) => Ok(value),
            BindingForce::Initialize {
                handle,
                initializer,
            } => {
                Self::reset_handle_force(&handle, initializer)?;
                Err(MagError::Eval(format!(
                    "binding#{} has not been initialized",
                    id.0
                )))
            }
        }
    }
    pub fn define(&mut self, name: &str, value: Value) {
        if let Value::TypeDecl(declaration) = &value {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .semantic_types
                .entry(declaration.name.clone())
                .or_insert_with(|| declaration.clone());
        }
        let ty = crate::checker::value_type(&value);
        let id = self.allocate_binding_id(name, ty);
        self.define_ready(id, name, value);
    }
    pub fn define_binding(&mut self, name: &str, value: Value) -> Result<(), MagError> {
        if let Some(candidate_type) = crate::checker::canonical_value_type(&value) {
            for existing in self.lookup_candidates(name) {
                if crate::checker::canonical_value_type(&existing).as_ref() == Some(&candidate_type)
                {
                    return Err(MagError::Type(format!(
                        "duplicate visible overload {name}: {candidate_type}"
                    )));
                }
            }
        }
        self.define(name, value);
        Ok(())
    }
    pub fn lookup_candidates(&self, name: &str) -> Vec<Value> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| {
                let Some(frame) = state.frames.get(scope) else {
                    return Vec::new();
                };
                frame
                    .names
                    .get(name)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| match frame.slots.get(id) {
                        Some(BindingSlot::Ready(value)) => Some(value.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    pub fn lookup(&self, name: &str) -> Result<Value, MagError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let candidates = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| {
                let frame = state.frames.get(scope)?;
                let values = frame
                    .names
                    .get(name)?
                    .iter()
                    .filter_map(|id| match frame.slots.get(id) {
                        Some(BindingSlot::Ready(value)) => Some(value.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                Some(values)
            })
            .ok_or_else(|| MagError::Unresolved(name.into()))?;
        let values = candidates
            .iter()
            .filter(|value| !matches!(value, Value::Type(_)))
            .cloned()
            .collect::<Vec<_>>();
        let candidates = if values.is_empty() {
            candidates
        } else {
            values
        };
        let data_candidates = candidates
            .iter()
            .filter(|value| !matches!(value, Value::Fn(_) | Value::BuiltinFn(_)))
            .cloned()
            .collect::<Vec<_>>();
        if let [value] = data_candidates.as_slice() {
            return Ok(value.clone());
        }
        match candidates.as_slice() {
            [] => Err(MagError::Unresolved(name.into())),
            [value] => Ok(value.clone()),
            values => Err(MagError::Type(format!(
                "ambiguous overload {name}; candidates: {}",
                values
                    .iter()
                    .filter_map(crate::checker::value_type)
                    .map(|ty| ty.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
    pub fn lookup_by_type(
        &self,
        name: &str,
        expected: &crate::types::MagType,
    ) -> Result<Value, MagError> {
        let matches = self
            .lookup_candidates(name)
            .into_iter()
            .filter(|value| crate::checker::value_type(value).as_ref() == Some(expected))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [value] => Ok(value.clone()),
            [] => Err(MagError::Type(format!(
                "no overload {name} matches {expected}"
            ))),
            _ => Err(MagError::Type(format!(
                "ambiguous overload {name} for {expected}"
            ))),
        }
    }
    pub fn type_decl(&self, canonical: &str) -> Option<TypeDecl> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes
            .iter()
            .rev()
            .filter_map(|scope| state.frames.get(scope))
            .flat_map(|frame| frame.slots.values())
            .find_map(|slot| match slot {
                BindingSlot::Ready(Value::TypeDecl(declaration))
                    if declaration.name == canonical =>
                {
                    Some(declaration.clone())
                }
                _ => None,
            })
            .or_else(|| state.semantic_types.get(canonical).cloned())
    }
    // Nominal declarations are the compilation-wide type namespace, not dynamic values.
    pub fn define_type_declarations_from(&mut self, source: &Self) {
        let declarations = {
            let state = source
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            source
                .scopes
                .iter()
                // Captured frames are already visible; only caller-only declarations
                // need importing into the call's lexical environment.
                .filter(|scope| {
                    !Arc::ptr_eq(&self.state, &source.state) || !self.scopes.contains(scope)
                })
                .filter_map(|scope| state.frames.get(scope))
                .flat_map(|frame| {
                    frame
                        .names
                        .iter()
                        .map(|(name, ids)| {
                            let values = ids
                                .iter()
                                .filter_map(|id| match frame.slots.get(id) {
                                    Some(BindingSlot::Ready(value @ Value::TypeDecl(_))) => {
                                        Some(value.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            (name.clone(), values)
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        for (name, values) in declarations {
            for value in values {
                if matches!(value, Value::TypeDecl(_))
                    && !self
                        .lookup_candidates(&name)
                        .iter()
                        .any(|existing| same_imported_value(self, existing, &value))
                {
                    self.define(&name, value);
                }
            }
        }
    }
    pub fn define_semantic_type_declarations_from(&mut self, source: &Self) {
        let declarations = {
            let state = source
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            source
                .scopes
                .iter()
                .filter_map(|scope| state.frames.get(scope))
                .flat_map(|frame| frame.slots.values())
                .filter_map(|slot| match slot {
                    BindingSlot::Ready(Value::TypeDecl(declaration)) => Some(declaration.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for declaration in declarations {
            let canonical = declaration.name.clone();
            if self.type_decl(&canonical).is_none() {
                self.define(&canonical, Value::TypeDecl(declaration));
            }
        }
    }

    pub fn snapshot(&self) -> Vec<Scope> {
        let snapshot = self.scopes.clone();
        self.profile_counters(|counters| {
            counters.environment_snapshots = counters.environment_snapshots.saturating_add(1);
            counters.environment_snapshot_bindings = counters
                .environment_snapshot_bindings
                .saturating_add(snapshot.len() as u64);
        });
        snapshot
    }
    pub fn replace_scopes(&mut self, scopes: Vec<Scope>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            for frame in &self.scopes {
                if let Some(count) = state.frame_roots.get_mut(frame) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.frame_roots.remove(frame);
                    }
                }
            }
            for frame in &scopes {
                *state.frame_roots.entry(*frame).or_default() += 1;
            }
        }
        self.scopes = scopes;
    }
    pub fn live_frame_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .frames
            .len()
    }
    pub fn frame_collection_due(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.frame_allocations_since_collection
            >= state
                .frame_collection_interval
                .max(FRAME_COLLECTION_THRESHOLD)
    }
    pub fn collect_frames(&self, extra_values: &[Value]) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let collection_interval = state
            .frame_collection_interval
            .max(FRAME_COLLECTION_THRESHOLD);
        state.frame_allocations_since_collection = 0;
        let before = state.frames.len();
        let mut reachable = HashSet::new();
        let mut visited_values = HashSet::new();
        let mut pending_frames = state.frame_roots.keys().copied().collect::<Vec<_>>();
        let mut pending_values = extra_values.to_vec();
        pending_values.extend(
            state
                .loaded
                .values()
                .flat_map(|module| module.values())
                .flatten()
                .cloned(),
        );
        for (call, result) in &state.memoized_calls {
            pending_values.push(Value::Fn(call.function.clone()));
            pending_values.extend(call.args.iter().map(|argument| argument.0.clone()));
            pending_values.push(result.clone());
        }

        while !pending_frames.is_empty() || !pending_values.is_empty() {
            while let Some(value) = pending_values.pop() {
                match value {
                    Value::Fn(function)
                        if visited_values.insert(Arc::as_ptr(&function).cast::<()>()) =>
                    {
                        pending_frames.extend(function.closure.iter().copied());
                    }
                    Value::List(values) | Value::Product(values) | Value::Set(values)
                        if visited_values.insert(Arc::as_ptr(&values).cast::<()>()) =>
                    {
                        pending_values.extend(values.iter().cloned());
                    }
                    Value::Fields(values) | Value::ModuleNamespace(values)
                        if visited_values.insert(Arc::as_ptr(&values).cast::<()>()) =>
                    {
                        pending_values.extend(values.values().cloned());
                    }
                    Value::Map(values)
                        if visited_values.insert(Arc::as_ptr(&values).cast::<()>()) =>
                    {
                        pending_values.extend(
                            values
                                .iter()
                                .flat_map(|(key, value)| [key.clone(), value.clone()]),
                        );
                    }
                    Value::Typed(value, _)
                    | Value::PackedValue(value)
                    | Value::Adt { payload: value, .. }
                        if visited_values.insert(Arc::as_ptr(&value).cast::<()>()) =>
                    {
                        pending_values.push(value.as_ref().clone());
                    }
                    _ => {}
                }
            }
            let Some(frame_id) = pending_frames.pop() else {
                continue;
            };
            if !reachable.insert(frame_id) {
                continue;
            }
            if let Some(frame) = state.frames.get(&frame_id) {
                pending_values.extend(frame.slots.values().filter_map(|slot| match slot {
                    BindingSlot::Ready(value) => Some(value.clone()),
                    BindingSlot::Uninitialized(_) | BindingSlot::Initializing => None,
                }));
            }
        }

        state.frames.retain(|id, _| reachable.contains(id));
        state.frame_roots.retain(|id, _| reachable.contains(id));
        let reclaimed = before.saturating_sub(state.frames.len());
        state.frame_collection_interval = if reclaimed < before / 4 {
            collection_interval.saturating_mul(2)
        } else {
            state.frames.len().max(FRAME_COLLECTION_THRESHOLD)
        };
        reclaimed
    }
    pub fn child_for_call(&self) -> Self {
        let frame = Self::allocate_frame_in(&self.state);
        Self {
            observer: self.observer.clone(),
            scopes: vec![frame],
            source_dir: self.source_dir.clone(),
            module_roots: self.module_roots.clone(),
            module: self.module.clone(),
            state: self.state.clone(),
            profiler: self.profiler.clone(),
            pending_import_collisions: self.pending_import_collisions.clone(),
        }
    }
    pub(crate) fn set_pending_import_collisions(&mut self, collisions: BTreeMap<String, String>) {
        self.pending_import_collisions = Arc::new(collisions);
    }
    /// The import-collision diagnostic for `name` when this module imports a
    /// value under that name and also declares one.
    pub(crate) fn pending_import_collision(&self, name: &str) -> Option<&str> {
        self.pending_import_collisions.get(name).map(String::as_str)
    }
    pub fn user_defs(&self, direct_names: &HashSet<String>) -> BTreeMap<String, Vec<Value>> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.scopes
            .first()
            .into_iter()
            .flat_map(|scope| {
                let Some(frame) = state.frames.get(scope) else {
                    return Vec::new();
                };
                direct_names
                    .iter()
                    .filter_map(|name| {
                        let ids = frame.names.get(name)?;
                        let values = ids
                            .iter()
                            .filter_map(|id| match frame.slots.get(id) {
                                Some(BindingSlot::Ready(value))
                                    if !matches!(value, Value::BuiltinFn(_) | Value::Type(_)) =>
                                {
                                    Some(value.clone())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        (!values.is_empty()).then_some((name.clone(), values))
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    pub fn module_cached(&self, name: &str) -> Option<BTreeMap<String, Vec<Value>>> {
        let cached = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .loaded
            .get(name)
            .cloned();
        self.profile_counters(|counters| {
            counters.module_requests = counters.module_requests.saturating_add(1);
            if cached.is_some() {
                counters.module_cache_hits = counters.module_cache_hits.saturating_add(1);
            }
        });
        cached
    }
    pub fn module_defs(&self, name: &str) -> Option<BTreeMap<String, Vec<Value>>> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .loaded
            .get(name)
            .cloned()
    }
    pub fn begin_module(&self, name: &str) -> Result<(), MagError> {
        let mut m = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = m.loading.iter().position(|x| x == name) {
            let mut cycle = m.loading[at..].to_vec();
            cycle.push(name.into());
            return Err(MagError::Eval(format!(
                "circular require: {}",
                cycle.join(" -> ")
            )));
        }
        m.loading.push(name.into());
        Ok(())
    }
    pub fn finish_module(&mut self, name: &str, defs: BTreeMap<String, Vec<Value>>) {
        let mut m = self.state.lock().unwrap_or_else(|e| e.into_inner());
        m.loading.pop();
        m.loaded.insert(name.into(), defs.clone());
        drop(m);
        self.profile_counters(|counters| {
            counters.modules_loaded = counters.modules_loaded.saturating_add(1);
        });
    }
    pub fn install_module(&mut self, name: &str, defs: BTreeMap<String, Vec<Value>>) {
        for (local, values) in defs {
            for value in values {
                let qualified = format!("{name}.{local}");
                if !self
                    .lookup_candidates(&qualified)
                    .iter()
                    .any(|existing| same_imported_value(self, existing, &value))
                {
                    self.define(&qualified, value);
                }
            }
        }
    }

    pub fn module_env(&self, name: &str) -> Self {
        let mut env = Self::new_in(
            &self.source_dir,
            self.module_roots.clone(),
            name,
            self.state.clone(),
            self.profiler.clone(),
        );
        env.observer = self.observer.clone();
        if let Ok(inputs) = self.lookup_by_type("inputs", &crate::types::MagType::HostInputs) {
            env.define("inputs", inputs);
        }
        env
    }

    pub(crate) fn set_observer(&mut self, observer: Option<crate::observation::Observer>) {
        self.observer = observer;
    }

    pub(crate) fn observe(
        &self,
        query: impl FnOnce() -> crate::observation::Query,
        path: &Path,
        source: &str,
    ) {
        if let Some(observer) = &self.observer {
            observer.record(query(), path, source);
        }
    }

    pub fn read_file(&self, path: &Path, requested: &str) -> Result<String, MagError> {
        let key = path.to_path_buf();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let hit = state.file_reads.contains_key(&key);
        let phase = self.profile_phase(Phase::ModuleRead);
        let result = state.file_reads.entry(key.clone()).or_insert_with(|| {
            std::fs::read_to_string(&key)
                .map_err(|error| format!("cannot read {requested}: {error}"))
        });
        let result = result.clone().map_err(MagError::Eval);
        drop(state);
        drop(phase);
        self.profile_counters(|counters| {
            counters.file_read_requests = counters.file_read_requests.saturating_add(1);
            if hit {
                counters.file_read_cache_hits = counters.file_read_cache_hits.saturating_add(1);
            } else {
                counters.file_read_cache_misses = counters.file_read_cache_misses.saturating_add(1);
            }
        });
        result
    }

    pub fn memoized_call(
        &self,
        function: &Arc<FnValue>,
        resolved_signature: Option<&crate::types::MagType>,
        type_bindings: &BTreeMap<String, crate::types::MagType>,
        args: &[Value],
    ) -> Option<Value> {
        let args = args.iter().map(MemoArg::new).collect::<Option<Vec<_>>>()?;
        let cached = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .memoized_calls
            .get(&MemoCall {
                function: function.clone(),
                resolved_signature: resolved_signature.cloned(),
                type_bindings: type_bindings.clone(),
                args,
            })
            .cloned();
        self.profile_counters(|counters| {
            let name = function.name.as_deref().unwrap_or("<anonymous>").to_owned();
            if cached.is_some() {
                counters.memoized_call_hits = counters.memoized_call_hits.saturating_add(1);
                *counters.memoized_call_hits_by_name.entry(name).or_default() += 1;
            } else {
                counters.memoized_call_misses = counters.memoized_call_misses.saturating_add(1);
                *counters
                    .memoized_call_misses_by_name
                    .entry(name)
                    .or_default() += 1;
            }
        });
        cached
    }

    pub fn memoize_call(
        &self,
        function: &Arc<FnValue>,
        resolved_signature: Option<&crate::types::MagType>,
        type_bindings: &BTreeMap<String, crate::types::MagType>,
        args: &[Value],
        result: &Value,
    ) {
        let Some(args) = args.iter().map(MemoArg::new).collect::<Option<Vec<_>>>() else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let limit = state.limits.memoized_calls;
        if limit == 0 {
            return;
        }
        // Bound distinct memoized inputs within one compilation. Clearing only
        // forfeits prior work.
        if state.memoized_calls.len() >= limit {
            state.memoized_calls.clear();
        }
        state.memoized_calls.insert(
            MemoCall {
                function: function.clone(),
                resolved_signature: resolved_signature.cloned(),
                type_bindings: type_bindings.clone(),
                args,
            },
            result.clone(),
        );
        drop(state);
        self.profile_counters(|counters| {
            counters.memoized_call_stores = counters.memoized_call_stores.saturating_add(1);
        });
    }

    pub(crate) fn profile_counters(
        &self,
        update: impl FnOnce(&mut crate::profile::OperationCounters),
    ) {
        if let Some(profiler) = &self.profiler {
            profiler.update_counters(update);
        }
    }

    pub(crate) fn profiling_enabled(&self) -> bool {
        self.profiler.is_some()
    }

    pub(crate) fn profiler(&self) -> Option<CompileProfiler> {
        self.profiler.clone()
    }

    pub(crate) fn profile_phase(&self, phase: Phase) -> Option<ProfileTimer> {
        self.profiler
            .as_ref()
            .map(|profiler| profiler.start_phase(phase))
    }
}

#[cfg(test)]
mod frame_arena_tests {
    fn declaration(name: &str) -> super::Value {
        super::Value::TypeDecl(super::TypeDecl {
            name: name.into(),
            params: vec![],
            body: crate::ast::TypeDeclBody::Alias(crate::types::MagType::Int),
        })
    }

    #[test]
    fn declaration_import_only_copies_caller_only_types() {
        let mut caller = super::Env::new();
        caller.define("Captured", declaration("main.Captured"));
        let mut callee = caller.clone();
        callee.push_scope();
        caller.push_scope();
        caller.define("Local", declaration("main.Local"));
        caller.define("data", super::Value::Str("not a declaration".into()));
        callee.define_type_declarations_from(&caller);
        callee.define_type_declarations_from(&caller);
        assert!(callee.type_decl("main.Captured").is_some());
        assert!(callee.type_decl("main.Local").is_some());
        assert!(callee.lookup("data").is_err());
        let state = callee.state.lock().unwrap();
        let frame = &state.frames[callee.scopes.last().unwrap()];
        assert_eq!(frame.slots.len(), 1, "captured types must not be copied");
    }

    #[test]
    fn declaration_import_does_not_confuse_independent_frame_ids() {
        let mut source = super::Env::new();
        source.define("Source", declaration("main.Source"));
        let mut target = super::Env::new();
        assert_eq!(source.scopes, target.scopes);
        target.define_type_declarations_from(&source);
        assert!(target.type_decl("main.Source").is_some());
    }

    #[test]
    fn declaration_lookup_preserves_nearest_scope_and_missing_results() {
        let mut env = super::Env::new();
        env.define("Outer", declaration("main.Type"));
        env.push_scope();
        let mut inner = match declaration("main.Type") {
            super::Value::TypeDecl(decl) => decl,
            _ => unreachable!(),
        };
        inner.body = crate::ast::TypeDeclBody::Alias(crate::types::MagType::String);
        env.define("Inner", super::Value::TypeDecl(inner.clone()));
        assert_eq!(env.type_decl("main.Type"), Some(inner));
        assert!(env.type_decl("missing").is_none());
        env.pop_scope();
        assert_eq!(
            env.type_decl("main.Type").unwrap().body,
            crate::ast::TypeDeclBody::Alias(crate::types::MagType::Int)
        );
    }

    use super::*;

    fn closure(captures: Vec<Scope>) -> Value {
        Value::Fn(Arc::new(FnValue {
            name: None,
            type_params: vec![],
            equality_params: vec![],
            params: vec![],
            param_types: vec![],
            return_type: crate::types::MagType::Unit,
            checked: Arc::new(crate::ast::CheckedFn {
                name: None,
                type_params: vec![],
                equality_params: vec![],
                params: vec![],
                result: crate::types::MagType::Unit,
                body: Arc::new(crate::ast::CheckedBlock {
                    frame_layout: vec![],
                    bindings: vec![],
                    expressions: vec![],
                }),
            }),
            closure: captures,
        }))
    }

    #[test]
    fn self_referential_activation_is_reclaimed() {
        let mut env = Env::new();
        let baseline = env.live_frame_count();
        env.push_scope();
        let activation = *env.scopes.last().unwrap();
        env.define("recursive", closure(vec![activation]));
        env.pop_scope();

        assert_eq!(env.collect_frames(&[]), 1);
        assert_eq!(env.live_frame_count(), baseline);
    }

    #[test]
    fn escaped_closure_keeps_its_activation_until_the_external_root_is_gone() {
        let mut env = Env::new();
        let baseline = env.live_frame_count();
        env.push_scope();
        let activation = *env.scopes.last().unwrap();
        let escaped = closure(vec![activation]);
        env.define("recursive", escaped.clone());
        env.pop_scope();

        assert_eq!(env.collect_frames(std::slice::from_ref(&escaped)), 0);
        assert_eq!(env.live_frame_count(), baseline + 1);
        assert_eq!(env.collect_frames(&[]), 1);
        assert_eq!(env.live_frame_count(), baseline);
    }

    #[test]
    fn repeated_activations_do_not_accumulate_frames() {
        let mut env = Env::new();
        let baseline = env.live_frame_count();
        for _ in 0..128 {
            env.push_scope();
            let activation = *env.scopes.last().unwrap();
            env.define("recursive", closure(vec![activation]));
            env.pop_scope();
            env.collect_frames(&[]);
            assert_eq!(env.live_frame_count(), baseline);
        }
    }

    #[test]
    fn retained_frames_do_not_retrigger_collection_without_new_allocations() {
        let mut env = Env::new();
        let mut escaped = Vec::with_capacity(FRAME_COLLECTION_THRESHOLD);
        for _ in 0..FRAME_COLLECTION_THRESHOLD {
            env.push_scope();
            escaped.push(closure(vec![*env.scopes.last().unwrap()]));
            env.pop_scope();
        }

        assert!(env.frame_collection_due());
        assert_eq!(env.collect_frames(&escaped), 0);
        assert!(!env.frame_collection_due());

        for _ in 0..FRAME_COLLECTION_THRESHOLD {
            env.push_scope();
            env.pop_scope();
        }
        assert!(!env.frame_collection_due());

        for _ in 0..FRAME_COLLECTION_THRESHOLD {
            env.push_scope();
            env.pop_scope();
        }
        assert!(env.frame_collection_due());
    }

    #[test]
    fn shared_value_dags_do_not_expand_during_frame_collection() {
        let mut env = Env::new();
        let baseline = env.live_frame_count();
        env.push_scope();
        let mut shared = closure(vec![*env.scopes.last().unwrap()]);
        env.pop_scope();

        for _ in 0..24 {
            shared = Value::List(Arc::new(vec![shared.clone(), shared]));
        }

        assert_eq!(env.collect_frames(std::slice::from_ref(&shared)), 0);
        assert_eq!(env.live_frame_count(), baseline + 1);
        assert_eq!(env.collect_frames(&[]), 1);
    }

    #[test]
    fn binding_handles_do_not_keep_a_dropped_program_alive() {
        let handle = {
            let mut env = Env::new();
            env.define("answer", Value::Int(42));
            env.lookup_handles("answer").pop().unwrap()
        };

        assert!(matches!(
            Env::ready_handle(&handle),
            Err(MagError::Eval(message)) if message == "binding program has been dropped"
        ));
    }
}
