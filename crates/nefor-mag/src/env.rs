use crate::ast::{FnValue, TypeDecl, Value};
use crate::error::MagError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MEMOIZED_CALL_LIMIT: usize = 16_384;

// Compound MAG values are immutable shared allocations. Holding the Value in
// the key both keeps its allocation alive and lets repeated uses compare by
// identity without walking large graphs. Scalars compare by value because
// their identity is their value. Cache eviction changes cost, never semantics.
#[derive(Debug, Clone)]
struct MemoArg(Value);

impl MemoArg {
    fn new(value: &Value) -> Option<Self> {
        if matches!(value, Value::Artifact(_)) {
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
            (Value::List(left), Value::List(right)) => Arc::ptr_eq(left, right),
            (Value::Vector(left), Value::Vector(right)) => Arc::ptr_eq(left, right),
            (Value::Map(left), Value::Map(right)) => Arc::ptr_eq(left, right),
            (Value::Fn(left), Value::Fn(right)) => Arc::ptr_eq(left, right),
            (Value::Type(left), Value::Type(right)) => left == right,
            (Value::TypeTag(left), Value::TypeTag(right)) => left == right,
            (Value::TypeDecl(left), Value::TypeDecl(right)) => left == right,
            (Value::Foreign(left), Value::Foreign(right)) => left == right,
            (Value::ForeignEvidence(left), Value::ForeignEvidence(right)) => left == right,
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
            Value::List(value) | Value::Vector(value) => Arc::as_ptr(value).hash(state),
            Value::Map(value) => Arc::as_ptr(value).hash(state),
            Value::Fn(value) => Arc::as_ptr(value).hash(state),
            Value::Type(value) => value.hash(state),
            Value::TypeTag(value) => value.hash(state),
            Value::TypeDecl(value) => value.hash(state),
            Value::Foreign(value) => value.hash(state),
            Value::ForeignEvidence(value) => value.hash(state),
            Value::Typed(value, ty) => {
                Arc::as_ptr(value).hash(state);
                ty.hash(state);
            }
            Value::Artifact(_) => unreachable!("artifacts are not memoized arguments"),
        }
    }
}

#[derive(Debug, Clone)]
struct MemoCall {
    function: Arc<FnValue>,
    args: Vec<MemoArg>,
}

impl PartialEq for MemoCall {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.function, &other.function) && self.args == other.args
    }
}

impl Eq for MemoCall {}

impl Hash for MemoCall {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.function).hash(state);
        self.args.hash(state);
    }
}

#[derive(Debug, Default)]
struct CompilationState {
    loaded: HashMap<String, BTreeMap<String, Value>>,
    loading: Vec<String>,
    foreign_identities: HashSet<String>,
    file_reads: HashMap<PathBuf, Result<String, String>>,
    memoized_calls: HashMap<MemoCall, Value>,
}

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
    source_dir: PathBuf,
    module_roots: Vec<PathBuf>,
    module: String,
    state: Arc<Mutex<CompilationState>>,
    imports: HashSet<String>,
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
        )
    }
    fn new_in(
        source_dir: &Path,
        module_roots: Vec<PathBuf>,
        module: &str,
        state: Arc<Mutex<CompilationState>>,
    ) -> Self {
        let mut env = Self {
            scopes: vec![HashMap::new()],
            source_dir: source_dir.into(),
            module_roots,
            module: module.into(),
            state,
            imports: HashSet::new(),
        };
        for name in [
            "str",
            "map",
            "indexed-map",
            "filter",
            "flat-map",
            "fold",
            "concat",
            "get",
            "assoc",
            "keys",
            "count",
            "first",
            "canonical",
            "sort-by",
            "remove-at",
            "conforms?",
            "or",
            "not",
            "=",
            "fail",
            "foreign-id",
            "foreign-evidence",
            "type-evidence",
            "read",
            "require",
            "artifact",
            "type-schema",
        ] {
            env.define(name, Value::BuiltinFn(name.into()));
        }
        for (name, ty) in [
            ("Data", crate::types::MagType::Data),
            ("ForeignEvidence", crate::types::MagType::ForeignEvidence),
            ("Artifact", crate::types::MagType::Artifact),
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
        Self::new_in(
            path,
            module_roots,
            "main",
            Arc::new(Mutex::new(CompilationState::default())),
        )
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
        self.scopes.push(HashMap::new());
    }
    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }
    pub fn define(&mut self, name: &str, value: Value) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.into(), value);
        }
    }
    pub fn lookup(&self, name: &str) -> Result<&Value, MagError> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Ok(v);
            }
        }
        Err(MagError::Unresolved(name.into()))
    }
    pub fn type_decl(&self, canonical: &str) -> Option<&TypeDecl> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.values())
            .find_map(|value| match value {
                Value::TypeDecl(decl) if decl.name == canonical => Some(decl),
                _ => None,
            })
    }
    pub fn snapshot(&self) -> Vec<(String, Value)> {
        self.scopes
            .iter()
            .flat_map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect()
    }
    pub fn child_for_call(&self) -> Self {
        Self {
            scopes: vec![HashMap::new()],
            source_dir: self.source_dir.clone(),
            module_roots: self.module_roots.clone(),
            module: self.module.clone(),
            state: self.state.clone(),
            imports: self.imports.clone(),
        }
    }
    pub fn user_defs(&self) -> BTreeMap<String, Value> {
        self.scopes
            .first()
            .into_iter()
            .flat_map(|s| s.iter())
            .filter(|(name, v)| {
                !matches!(v, Value::BuiltinFn(_) | Value::Type(_))
                    && (!name.contains('.') || matches!(v, Value::Foreign(_)))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    pub fn module_cached(&self, name: &str) -> Option<BTreeMap<String, Value>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .loaded
            .get(name)
            .cloned()
    }
    pub fn loaded_modules(&self) -> Vec<(String, BTreeMap<String, Value>)> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .loaded
            .iter()
            .map(|(name, defs)| (name.clone(), defs.clone()))
            .collect()
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
    pub fn register_foreign(&self, identity: &str) -> Result<(), MagError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.foreign_identities.insert(identity.to_string()) {
            return Err(MagError::Type(format!(
                "duplicate foreign declaration for {identity}"
            )));
        }
        Ok(())
    }
    pub fn finish_module(&mut self, name: &str, defs: BTreeMap<String, Value>) {
        let mut m = self.state.lock().unwrap_or_else(|e| e.into_inner());
        m.loading.pop();
        m.loaded.insert(name.into(), defs.clone());
        drop(m);
        self.install_module(name, defs);
    }
    pub fn install_module(&mut self, name: &str, defs: BTreeMap<String, Value>) {
        self.imports.insert(name.into());
        for (local, value) in defs {
            let qualified = if local.contains('.') {
                local
            } else {
                format!("{name}.{local}")
            };
            self.define(&qualified, value);
        }
    }
    pub fn module_env(&self, name: &str) -> Self {
        let mut env = Self::new_in(
            &self.source_dir,
            self.module_roots.clone(),
            name,
            self.state.clone(),
        );
        if let Ok(inputs) = self.lookup("inputs") {
            env.define("inputs", inputs.clone());
        }
        env
    }

    pub fn read_file(&self, path: &Path, requested: &str) -> Result<String, MagError> {
        let key = path.to_path_buf();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = state.file_reads.entry(key.clone()).or_insert_with(|| {
            std::fs::read_to_string(&key)
                .map_err(|error| format!("cannot read {requested}: {error}"))
        });
        result.clone().map_err(MagError::Eval)
    }

    pub fn memoized_call(&self, function: &Arc<FnValue>, args: &[Value]) -> Option<Value> {
        let args = args.iter().map(MemoArg::new).collect::<Option<Vec<_>>>()?;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .memoized_calls
            .get(&MemoCall {
                function: function.clone(),
                args,
            })
            .cloned()
    }

    pub fn memoize_call(&self, function: &Arc<FnValue>, args: &[Value], result: &Value) {
        let Some(args) = args.iter().map(MemoArg::new).collect::<Option<Vec<_>>>() else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // Keep resident programs bounded even when rule functions see a long
        // stream of distinct inputs. Clearing only forfeits prior work.
        if state.memoized_calls.len() >= MEMOIZED_CALL_LIMIT {
            state.memoized_calls.clear();
        }
        state.memoized_calls.insert(
            MemoCall {
                function: function.clone(),
                args,
            },
            result.clone(),
        );
    }
}
