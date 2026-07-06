use crate::ast::Value;
use crate::error::MagError;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
    source_dir: PathBuf,
    loading_modules: HashSet<PathBuf>,
    /// Per-factory counters for auto-generated inline node ids (`bash-1`,
    /// `bash-2`, …). Env-scoped so ids are deterministic per compilation —
    /// two loads of the same source mint identical ids — unlike the global
    /// `node_N` counter, which only guarantees uniqueness.
    node_seq: HashMap<String, usize>,
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
            source_dir: PathBuf::from("."),
            loading_modules: HashSet::new(),
            node_seq: HashMap::new(),
        }
    }

    fn define_stdlib(&mut self) {
        self.define("str", Value::BuiltinFn("str".into()));
        self.define("map", Value::BuiltinFn("map".into()));
        self.define("filter", Value::BuiltinFn("filter".into()));
        self.define("flat-map", Value::BuiltinFn("flat-map".into()));
        self.define("fold", Value::BuiltinFn("fold".into()));
        self.define("concat", Value::BuiltinFn("concat".into()));
        self.define("get", Value::BuiltinFn("get".into()));
        self.define("assoc", Value::BuiltinFn("assoc".into()));
        self.define("keys", Value::BuiltinFn("keys".into()));
        self.define("count", Value::BuiltinFn("count".into()));
        self.define("or", Value::BuiltinFn("or".into()));
        self.define("not", Value::BuiltinFn("not".into()));
        self.define("=", Value::BuiltinFn("=".into()));
        self.define("node", Value::BuiltinFn("node".into()));
        self.define("bash", Value::BuiltinFn("bash".into()));
        self.define("graph", Value::BuiltinFn("graph".into()));
        self.define("subgraph", Value::BuiltinFn("subgraph".into()));
        self.define("agent", Value::BuiltinFn("agent".into()));
        self.define("type", Value::BuiltinFn("type".into()));
        self.define("read", Value::BuiltinFn("read".into()));
        self.define("require", Value::BuiltinFn("require".into()));
    }

    pub fn new_with_stdlib() -> Self {
        let mut env = Self::new();
        env.define_stdlib();
        env
    }

    pub fn new_with_stdlib_and_source_dir(source_dir: &Path) -> Self {
        let mut env = Self {
            scopes: vec![HashMap::new()],
            source_dir: source_dir.to_path_buf(),
            loading_modules: HashSet::new(),
            node_seq: HashMap::new(),
        };
        env.define_stdlib();
        env
    }

    pub fn source_dir(&self) -> &Path {
        &self.source_dir
    }

    pub fn begin_loading(&mut self, path: &Path) -> Result<(), MagError> {
        if !self.loading_modules.insert(path.to_path_buf()) {
            return Err(MagError::Eval(format!(
                "circular require: {} is already being loaded",
                path.display()
            )));
        }
        Ok(())
    }

    pub fn end_loading(&mut self, path: &Path) {
        self.loading_modules.remove(path);
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
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), value);
        }
    }

    pub fn lookup(&self, name: &str) -> Result<&Value, MagError> {
        for scope in self.scopes.iter().rev() {
            if let Some(val) = scope.get(name) {
                return Ok(val);
            }
        }
        Err(MagError::Unresolved(name.to_string()))
    }

    pub fn snapshot(&self) -> Vec<(String, Value)> {
        let mut result = Vec::new();
        for scope in &self.scopes {
            for (k, v) in scope {
                result.push((k.clone(), v.clone()));
            }
        }
        result
    }

    pub fn top_scope_user_defs(&self) -> HashMap<String, Value> {
        let builtins = [
            "str", "map", "flat-map", "filter", "fold", "concat", "get", "assoc", "keys", "count",
            "or", "not", "=", "node", "bash", "graph", "subgraph", "agent", "type", "read",
            "require",
        ];
        self.scopes
            .last()
            .map(|scope| {
                scope
                    .iter()
                    .filter(|(k, _)| !builtins.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Next per-factory sequence number for auto-generated inline node ids
    /// (`<factory>-<n>` in appearance order).
    pub fn next_node_seq(&mut self, factory: &str) -> usize {
        let n = self.node_seq.entry(factory.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    pub fn create_module_env(&self) -> Self {
        let mut env = Self {
            scopes: vec![HashMap::new()],
            source_dir: self.source_dir.clone(),
            loading_modules: self.loading_modules.clone(),
            node_seq: HashMap::new(),
        };
        env.define_stdlib();
        env
    }
}
