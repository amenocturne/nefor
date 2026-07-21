use crate::ast::TypeDecl;
use crate::ast::Value;
use crate::error::MagError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Modules {
    loaded: HashMap<String, BTreeMap<String, Value>>,
    loading: Vec<String>,
    foreign_identities: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
    source_dir: PathBuf,
    module_roots: Vec<PathBuf>,
    module: String,
    modules: Arc<Mutex<Modules>>,
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
            Arc::new(Mutex::new(Modules::default())),
        )
    }
    fn new_in(
        source_dir: &Path,
        module_roots: Vec<PathBuf>,
        module: &str,
        modules: Arc<Mutex<Modules>>,
    ) -> Self {
        let mut env = Self {
            scopes: vec![HashMap::new()],
            source_dir: source_dir.into(),
            module_roots,
            module: module.into(),
            modules,
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
            Arc::new(Mutex::new(Modules::default())),
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
            modules: self.modules.clone(),
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
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .loaded
            .get(name)
            .cloned()
    }
    pub fn loaded_modules(&self) -> Vec<(String, BTreeMap<String, Value>)> {
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .loaded
            .iter()
            .map(|(name, defs)| (name.clone(), defs.clone()))
            .collect()
    }
    pub fn begin_module(&self, name: &str) -> Result<(), MagError> {
        let mut m = self.modules.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut modules = self.modules.lock().unwrap_or_else(|e| e.into_inner());
        if !modules.foreign_identities.insert(identity.to_string()) {
            return Err(MagError::Type(format!(
                "duplicate foreign declaration for {identity}"
            )));
        }
        Ok(())
    }
    pub fn finish_module(&mut self, name: &str, defs: BTreeMap<String, Value>) {
        let mut m = self.modules.lock().unwrap_or_else(|e| e.into_inner());
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
            self.modules.clone(),
        );
        if let Ok(inputs) = self.lookup("inputs") {
            env.define("inputs", inputs.clone());
        }
        env
    }
}
