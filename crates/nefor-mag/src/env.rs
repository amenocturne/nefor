use crate::ast::Value;
use crate::error::MagError;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Env {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn new_with_stdlib() -> Self {
        let mut env = Self::new();
        env.define("str", Value::BuiltinFn("str".into()));
        env.define("map", Value::BuiltinFn("map".into()));
        env.define("filter", Value::BuiltinFn("filter".into()));
        env.define("fold", Value::BuiltinFn("fold".into()));
        env.define("concat", Value::BuiltinFn("concat".into()));
        env.define("get", Value::BuiltinFn("get".into()));
        env.define("assoc", Value::BuiltinFn("assoc".into()));
        env.define("keys", Value::BuiltinFn("keys".into()));
        env.define("count", Value::BuiltinFn("count".into()));
        env.define("or", Value::BuiltinFn("or".into()));
        env.define("not", Value::BuiltinFn("not".into()));
        env.define("=", Value::BuiltinFn("=".into()));
        env.define("node", Value::BuiltinFn("node".into()));
        env.define("graph", Value::BuiltinFn("graph".into()));
        env.define("type", Value::BuiltinFn("type".into()));
        env.define("template", Value::BuiltinFn("template".into()));
        env
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
}
