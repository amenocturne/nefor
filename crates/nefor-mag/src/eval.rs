use crate::ast::{EdgeValue, Expr, FnValue, GraphValue, NodeValue, Port, SubgraphValue, Value};
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

static NODE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_node_id() -> String {
    let n = NODE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("node_{n}")
}

/// Bounded-evaluation fuel. Two coupled rails, both surfacing `MagError::Budget`
/// so a runaway evaluation is always a clean error, never a hang or a native
/// stack overflow:
///
/// - **Step budget** (`STEPS`, decremented once per `eval_expr`): caps total
///   work. Catches wide non-recursive blowups (e.g. folding an enormous list).
/// - **Recursion-depth cap** (`DEPTH`, tracked across `apply_fn`): caps native
///   call-stack growth. Catches non-terminating recursion — the only way a MAG
///   program can loop, since the interpreter has no TCO. This is the rail that
///   actually fires for `(fn [x] (self x))`, well before the step budget and
///   far below the stack ceiling of the dedicated evaluation thread.
///
/// Both are dormant by default (`STEPS == UNLIMITED`) so the `compile` dev path
/// and unit tests evaluate unbounded, exactly as before. A budget is armed only
/// by the resident entry points (`load`, `eval_fn` in lib.rs), which run on a
/// large-stack thread so the depth cap has ample headroom.
pub(crate) mod fuel {
    use crate::error::MagError;
    use std::cell::Cell;

    const UNLIMITED: u64 = u64::MAX;

    /// Native-stack safety rail. Each MAG call frame nests a handful of Rust
    /// frames; the resident evaluator runs on a 64 MiB stack (lib.rs), leaving
    /// this cap a wide margin. Ample for any real rule fn — recursion this deep
    /// is a non-terminating program, not legitimate work.
    const MAX_DEPTH: usize = 4096;

    thread_local! {
        static STEPS: Cell<u64> = const { Cell::new(UNLIMITED) };
        static DEPTH: Cell<usize> = const { Cell::new(0) };
    }

    /// Arm a step budget for the current thread; restores the prior budget and
    /// depth on drop so nested evaluations compose.
    pub(crate) struct Guard {
        prev_steps: u64,
        prev_depth: usize,
    }

    pub(crate) fn install(steps: u64) -> Guard {
        let prev_steps = STEPS.with(|c| c.replace(steps));
        let prev_depth = DEPTH.with(|c| c.replace(0));
        Guard {
            prev_steps,
            prev_depth,
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            STEPS.with(|c| c.set(self.prev_steps));
            DEPTH.with(|c| c.set(self.prev_depth));
        }
    }

    fn is_bounded() -> bool {
        STEPS.with(|c| c.get()) != UNLIMITED
    }

    /// Consume one evaluation step. Called once per `eval_expr`.
    pub(crate) fn step() -> Result<(), MagError> {
        STEPS.with(|c| {
            let remaining = c.get();
            if remaining == UNLIMITED {
                Ok(())
            } else if remaining == 0 {
                Err(MagError::Budget("step budget exhausted".into()))
            } else {
                c.set(remaining - 1);
                Ok(())
            }
        })
    }

    /// Enter a function application, incrementing native-recursion depth. The
    /// returned guard decrements on drop. Only enforced while a budget is armed
    /// (the unbounded `compile`/test paths keep their prior behavior).
    pub(crate) fn enter_call() -> Result<CallGuard, MagError> {
        if !is_bounded() {
            return Ok(CallGuard { active: false });
        }
        let depth = DEPTH.with(|c| {
            let d = c.get() + 1;
            c.set(d);
            d
        });
        if depth > MAX_DEPTH {
            // Undo this frame's increment before erroring so the count stays
            // consistent for the unwinding path.
            DEPTH.with(|c| c.set(c.get() - 1));
            return Err(MagError::Budget(format!(
                "recursion depth limit ({MAX_DEPTH}) exceeded"
            )));
        }
        Ok(CallGuard { active: true })
    }

    pub(crate) struct CallGuard {
        active: bool,
    }

    impl Drop for CallGuard {
        fn drop(&mut self) {
            if self.active {
                DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
            }
        }
    }
}

pub fn eval_program(env: &mut Env, exprs: &[Expr]) -> Result<Value, MagError> {
    let mut result = Value::Nil;
    for expr in exprs {
        result = eval_expr(env, expr)?;
    }
    Ok(result)
}

fn eval_expr(env: &mut Env, expr: &Expr) -> Result<Value, MagError> {
    fuel::step()?;
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(n) => Ok(Value::Float(*n)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Nil => Ok(Value::Nil),
        Expr::Keyword(k) => Ok(Value::Keyword(k.clone())),
        Expr::Symbol(name) => env.lookup(name).cloned(),
        Expr::Vector(items) => {
            let vals: Result<Vec<_>, _> = items.iter().map(|e| eval_expr(env, e)).collect();
            Ok(Value::Vector(vals?))
        }
        Expr::Map(pairs) => {
            let mut map = BTreeMap::new();
            for (k, v) in pairs {
                let key = match k {
                    Expr::Keyword(s) | Expr::Str(s) | Expr::Symbol(s) => s.clone(),
                    other => {
                        let val = eval_expr(env, other)?;
                        value_to_string(&val)
                    }
                };
                let val = eval_expr(env, v)?;
                map.insert(key, val);
            }
            Ok(Value::Map(map))
        }
        Expr::List(items) => eval_list(env, items),
    }
}

fn validate_workspace_path(
    source_dir: &std::path::Path,
    relative: &str,
) -> Result<std::path::PathBuf, MagError> {
    let path = std::path::Path::new(relative);

    if path.is_absolute() {
        return Err(MagError::Eval(format!(
            "absolute paths not allowed: {relative}"
        )));
    }

    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(MagError::Eval(format!(
                "path traversal not allowed: {relative}"
            )));
        }
    }

    Ok(source_dir.join(relative))
}

fn eval_list(env: &mut Env, items: &[Expr]) -> Result<Value, MagError> {
    if items.is_empty() {
        return Ok(Value::List(vec![]));
    }

    // Check for special forms
    if let Expr::Symbol(head) = &items[0] {
        match head.as_str() {
            "def" => return eval_def(env, &items[1..]),
            "fn" => return eval_fn(env, &items[1..]),
            "let" => return eval_let(env, &items[1..]),
            "if" => return eval_if(env, &items[1..]),
            "->" => return eval_threading(env, &items[1..]),
            _ => {}
        }
    }

    // Function application
    let func = eval_expr(env, &items[0])?;
    match func {
        Value::BuiltinFn(name) => eval_builtin(env, &name, &items[1..]),
        Value::Fn(fv) => {
            let args: Result<Vec<_>, _> = items[1..].iter().map(|e| eval_expr(env, e)).collect();
            apply_fn(&fv, &args?)
        }
        other => Err(MagError::Eval(format!(
            "cannot call value of type {}",
            other.type_name()
        ))),
    }
}

fn eval_def(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.len() != 2 {
        return Err(MagError::Arity {
            expected: 2,
            got: args.len(),
        });
    }
    let name = args[0]
        .as_symbol()
        .ok_or_else(|| MagError::Eval("def requires a symbol name".into()))?
        .to_string();
    let mut val = eval_expr(env, &args[1])?;

    // If binding a NodeValue, update its id to the binding name
    if let Value::Node(ref mut node) = val {
        node.id = name.clone();
    }

    env.define(&name, val.clone());
    Ok(val)
}

fn eval_fn(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.is_empty() {
        return Err(MagError::Eval(
            "fn requires parameter vector and body".into(),
        ));
    }
    let params = match &args[0] {
        Expr::Vector(params) => {
            let mut names = Vec::new();
            for p in params {
                let name = p
                    .as_symbol()
                    .ok_or_else(|| MagError::Eval("fn parameter must be a symbol".into()))?;
                names.push(name.to_string());
            }
            names
        }
        _ => return Err(MagError::Eval("fn requires a vector of parameters".into())),
    };
    let body = args[1..].to_vec();
    let closure = env.snapshot();
    Ok(Value::Fn(FnValue {
        params,
        body,
        closure,
    }))
}

fn eval_let(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.is_empty() {
        return Err(MagError::Eval(
            "let requires bindings vector and body".into(),
        ));
    }
    let bindings = match &args[0] {
        Expr::Vector(pairs) => pairs,
        _ => return Err(MagError::Eval("let requires a vector of bindings".into())),
    };
    if bindings.len() % 2 != 0 {
        return Err(MagError::Eval("let bindings must be pairs".into()));
    }

    env.push_scope();
    for pair in bindings.chunks(2) {
        let name = pair[0]
            .as_symbol()
            .ok_or_else(|| MagError::Eval("let binding name must be a symbol".into()))?
            .to_string();
        let mut val = eval_expr(env, &pair[1])?;

        // If binding a NodeValue, update its id to the binding name
        if let Value::Node(ref mut node) = val {
            node.id = name.clone();
        }

        env.define(&name, val);
    }

    let body = &args[1..];
    let mut result = Value::Nil;
    for expr in body {
        result = eval_expr(env, expr)?;
    }
    env.pop_scope();
    Ok(result)
}

fn eval_if(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.len() < 2 {
        return Err(MagError::Eval(
            "if requires at least condition and then branch".into(),
        ));
    }
    let cond = eval_expr(env, &args[0])?;
    if is_truthy(&cond) {
        eval_expr(env, &args[1])
    } else if args.len() > 2 {
        eval_expr(env, &args[2])
    } else {
        Ok(Value::Nil)
    }
}

fn eval_threading(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.is_empty() {
        return Err(MagError::Eval(
            "-> requires a value and at least one function".into(),
        ));
    }
    let mut val = eval_expr(env, &args[0])?;
    for f_expr in &args[1..] {
        let func = eval_expr(env, f_expr)?;
        match func {
            Value::Fn(fv) => val = apply_fn(&fv, &[val])?,
            Value::BuiltinFn(name) => {
                // Create a synthetic Expr for the arg since builtins expect unevaluated forms
                // We need to pass the already-evaluated value, so wrap it
                val = call_builtin_with_values(&name, &[val])?;
            }
            other => {
                return Err(MagError::Eval(format!(
                    "-> requires functions, got {}",
                    other.type_name()
                )))
            }
        }
    }
    Ok(val)
}

fn apply_fn(fv: &FnValue, args: &[Value]) -> Result<Value, MagError> {
    if args.len() != fv.params.len() {
        return Err(MagError::Arity {
            expected: fv.params.len(),
            got: args.len(),
        });
    }
    let _call = fuel::enter_call()?;
    let mut fn_env = Env::new();
    // Restore closure
    for (k, v) in &fv.closure {
        fn_env.define(k, v.clone());
    }
    fn_env.push_scope();
    for (param, arg) in fv.params.iter().zip(args.iter()) {
        fn_env.define(param, arg.clone());
    }
    let mut result = Value::Nil;
    for expr in &fv.body {
        result = eval_expr(&mut fn_env, expr)?;
    }
    fn_env.pop_scope();
    Ok(result)
}

/// Apply a named unary function from a resident environment to one argument.
/// The entry point for fire-time rule evaluation: the kernel hands a name and a
/// value, this resolves the name in the cached env and evaluates the body. Runs
/// under whatever fuel budget the caller installed on this thread.
pub fn apply_named(env: &Env, name: &str, arg: Value) -> Result<Value, MagError> {
    match env.lookup(name)? {
        Value::Fn(fv) if fv.params.len() == 1 => {
            let fv = fv.clone();
            apply_fn(&fv, &[arg])
        }
        Value::Fn(fv) => Err(MagError::Eval(format!(
            "rule fn '{name}' must be unary, takes {} arguments",
            fv.params.len()
        ))),
        other => Err(MagError::Eval(format!(
            "rule fn '{name}' is not a function (got {})",
            other.type_name()
        ))),
    }
}

fn eval_builtin(env: &mut Env, name: &str, args: &[Expr]) -> Result<Value, MagError> {
    match name {
        "str" => {
            let vals: Result<Vec<_>, _> = args.iter().map(|e| eval_expr(env, e)).collect();
            builtin_str(&vals?)
        }
        "map" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let f = eval_expr(env, &args[0])?;
            let coll = eval_expr(env, &args[1])?;
            builtin_map(&f, &coll)
        }
        "filter" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let f = eval_expr(env, &args[0])?;
            let coll = eval_expr(env, &args[1])?;
            builtin_filter(&f, &coll)
        }
        "flat-map" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let f = eval_expr(env, &args[0])?;
            let coll = eval_expr(env, &args[1])?;
            builtin_flat_map(&f, &coll)
        }
        "fold" => {
            if args.len() != 3 {
                return Err(MagError::Arity {
                    expected: 3,
                    got: args.len(),
                });
            }
            let f = eval_expr(env, &args[0])?;
            let init = eval_expr(env, &args[1])?;
            let coll = eval_expr(env, &args[2])?;
            builtin_fold(&f, init, &coll)
        }
        "concat" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let a = eval_expr(env, &args[0])?;
            let b = eval_expr(env, &args[1])?;
            builtin_concat(&a, &b)
        }
        "get" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let m = eval_expr(env, &args[0])?;
            let k = eval_expr(env, &args[1])?;
            builtin_get(&m, &k)
        }
        "assoc" => {
            if args.len() != 3 {
                return Err(MagError::Arity {
                    expected: 3,
                    got: args.len(),
                });
            }
            let m = eval_expr(env, &args[0])?;
            let k = eval_expr(env, &args[1])?;
            let v = eval_expr(env, &args[2])?;
            builtin_assoc(&m, &k, &v)
        }
        "keys" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            let m = eval_expr(env, &args[0])?;
            builtin_keys(&m)
        }
        "count" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            let v = eval_expr(env, &args[0])?;
            builtin_count(&v)
        }
        "or" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let a = eval_expr(env, &args[0])?;
            if is_truthy(&a) {
                Ok(a)
            } else {
                eval_expr(env, &args[1])
            }
        }
        "not" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            let v = eval_expr(env, &args[0])?;
            Ok(Value::Bool(!is_truthy(&v)))
        }
        "=" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            let a = eval_expr(env, &args[0])?;
            let b = eval_expr(env, &args[1])?;
            Ok(Value::Bool(values_equal(&a, &b)))
        }
        "type" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            // type takes a raw symbol name, not an evaluated expression
            match &args[0] {
                Expr::Symbol(name) => Ok(Value::TypeDecl(name.clone())),
                Expr::Str(name) => Ok(Value::TypeDecl(name.clone())),
                other => {
                    let v = eval_expr(env, other)?;
                    match v {
                        Value::Str(name) => Ok(Value::TypeDecl(name)),
                        _ => Err(MagError::Eval(format!(
                            "type expects a symbol or string, got {:?}",
                            other
                        ))),
                    }
                }
            }
        }
        "node" => eval_node(env, args),
        "graph" => eval_graph(env, args),
        "agent" => eval_agent(env, args),
        "read" => {
            if args.is_empty() || args.len() > 2 {
                return Err(MagError::Eval(
                    "read requires 1-2 args: (read \"path\") or (read \"path\" data-map)".into(),
                ));
            }
            let path_val = eval_expr(env, &args[0])?;
            let path_str = match &path_val {
                Value::Str(s) => s.clone(),
                other => {
                    return Err(MagError::Eval(format!(
                        "read path must be a string, got {}",
                        other.type_name()
                    )))
                }
            };
            let full_path = validate_workspace_path(env.source_dir(), &path_str)?;
            let contents = std::fs::read_to_string(&full_path)
                .map_err(|_| MagError::Eval(format!("file not found: {path_str}")))?;
            if args.len() == 2 {
                let data = eval_expr(env, &args[1])?;
                let data_map = match &data {
                    Value::Map(m) => m,
                    other => {
                        return Err(MagError::Eval(format!(
                            "read data must be a map, got {}",
                            other.type_name()
                        )))
                    }
                };
                Ok(Value::Str(interpolate_template(&contents, data_map)))
            } else {
                Ok(Value::Str(contents))
            }
        }
        "require" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            let path_val = eval_expr(env, &args[0])?;
            let path = path_val
                .as_str()
                .ok_or_else(|| MagError::Eval("require expects a string path".into()))?;
            let full_path = validate_workspace_path(env.source_dir(), &format!("{path}.mag"))?;
            let canonical = full_path
                .canonicalize()
                .unwrap_or_else(|_| full_path.clone());
            env.begin_loading(&canonical)?;
            let content = std::fs::read_to_string(&full_path)
                .map_err(|e| MagError::Eval(format!("cannot read module {path}: {e}")))?;
            let tokens = crate::lexer::tokenize(&content)?;
            let exprs = crate::parser::parse(&tokens)?;
            let mut module_env = env.create_module_env();
            let _last = eval_program(&mut module_env, &exprs)?;
            env.end_loading(&canonical);
            let defs = module_env.top_scope_user_defs();
            let map = defs
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>();
            Ok(Value::Map(map))
        }
        other => Err(MagError::Eval(format!("unknown builtin: {other}"))),
    }
}

/// Call a builtin with already-evaluated values (used by threading macro)
fn call_builtin_with_values(name: &str, args: &[Value]) -> Result<Value, MagError> {
    match name {
        "str" => builtin_str(args),
        "count" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            builtin_count(&args[0])
        }
        "keys" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            builtin_keys(&args[0])
        }
        "not" => {
            if args.len() != 1 {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            Ok(Value::Bool(!is_truthy(&args[0])))
        }
        "map" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            builtin_map(&args[0], &args[1])
        }
        "filter" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            builtin_filter(&args[0], &args[1])
        }
        "flat-map" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            builtin_flat_map(&args[0], &args[1])
        }
        "fold" => {
            if args.len() != 3 {
                return Err(MagError::Arity {
                    expected: 3,
                    got: args.len(),
                });
            }
            builtin_fold(&args[0], args[1].clone(), &args[2])
        }
        "concat" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            builtin_concat(&args[0], &args[1])
        }
        "get" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            builtin_get(&args[0], &args[1])
        }
        "assoc" => {
            if args.len() != 3 {
                return Err(MagError::Arity {
                    expected: 3,
                    got: args.len(),
                });
            }
            builtin_assoc(&args[0], &args[1], &args[2])
        }
        "or" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            if is_truthy(&args[0]) {
                Ok(args[0].clone())
            } else {
                Ok(args[1].clone())
            }
        }
        "=" => {
            if args.len() != 2 {
                return Err(MagError::Arity {
                    expected: 2,
                    got: args.len(),
                });
            }
            Ok(Value::Bool(values_equal(&args[0], &args[1])))
        }
        other => Err(MagError::Eval(format!(
            "cannot thread through builtin: {other}"
        ))),
    }
}

fn eval_node(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    // (node "type-name" {:args map} : InputType -> OutputType)
    // Find the colon separator to split args from type annotation
    let colon_pos = args
        .iter()
        .position(|e| matches!(e, Expr::Symbol(s) if s == ":"));

    let (value_args, type_args) = match colon_pos {
        Some(pos) => (&args[..pos], &args[pos + 1..]),
        None => {
            return Err(MagError::Eval(
                "node requires type annotation after ':'".into(),
            ))
        }
    };

    if value_args.is_empty() {
        return Err(MagError::Eval("node requires a type name".into()));
    }

    let node_type = match eval_expr(env, &value_args[0])? {
        Value::Str(s) => s,
        Value::Symbol(s) => s,
        other => {
            return Err(MagError::Eval(format!(
                "node type must be a string, got {}",
                other.type_name()
            )))
        }
    };

    let node_args = if value_args.len() > 1 {
        match eval_expr(env, &value_args[1])? {
            Value::Map(m) => m,
            other => {
                return Err(MagError::Eval(format!(
                    "node args must be a map, got {}",
                    other.type_name()
                )))
            }
        }
    } else {
        BTreeMap::new()
    };

    // Parse type annotation: InputType -> OutputType
    let (input_type, output_type) = parse_type_annotation(type_args)?;

    Ok(Value::Node(NodeValue {
        id: next_node_id(),
        node_type,
        args: node_args,
        input_type,
        output_type,
    }))
}

fn parse_type_annotation(exprs: &[Expr]) -> Result<(MagType, MagType), MagError> {
    // Find the -> separator
    let arrow_pos = exprs
        .iter()
        .position(|e| matches!(e, Expr::Symbol(s) if s == "->"));

    match arrow_pos {
        Some(pos) => {
            let input_exprs = &exprs[..pos];
            let output_exprs = &exprs[pos + 1..];
            let input = parse_type_exprs(input_exprs)?;
            let output = parse_type_exprs(output_exprs)?;
            Ok((input, output))
        }
        None => Err(MagError::Eval(
            "type annotation requires '->' between input and output types".into(),
        )),
    }
}

fn parse_type_exprs(exprs: &[Expr]) -> Result<MagType, MagError> {
    if exprs.len() == 1 {
        parse_type_expr(&exprs[0])
    } else {
        Err(MagError::Eval(format!(
            "expected single type expression, got {} items",
            exprs.len()
        )))
    }
}

fn parse_type_expr(expr: &Expr) -> Result<MagType, MagError> {
    match expr {
        Expr::Symbol(name) => {
            if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !name.is_empty() {
                Ok(MagType::Var(name.clone()))
            } else {
                Ok(MagType::Named(name.clone()))
            }
        }
        Expr::List(items) => {
            // Check for union (A | B) or intersection (A + B)
            if items.len() >= 3 {
                // Look for | or + operators
                if items
                    .iter()
                    .any(|e| matches!(e, Expr::Symbol(s) if s == "|"))
                {
                    let types: Result<Vec<_>, _> = items
                        .iter()
                        .filter(|e| !matches!(e, Expr::Symbol(s) if s == "|"))
                        .map(parse_type_expr)
                        .collect();
                    return Ok(MagType::union(types?));
                }
                if items
                    .iter()
                    .any(|e| matches!(e, Expr::Symbol(s) if s == "+"))
                {
                    let types: Result<Vec<_>, _> = items
                        .iter()
                        .filter(|e| !matches!(e, Expr::Symbol(s) if s == "+"))
                        .map(parse_type_expr)
                        .collect();
                    return Ok(MagType::intersection(types?));
                }
            }
            // Single-item list
            if items.len() == 1 {
                return parse_type_expr(&items[0]);
            }
            Err(MagError::Eval(format!(
                "invalid type expression in list with {} items",
                items.len()
            )))
        }
        _ => Err(MagError::Eval(format!(
            "expected type name (symbol), got {:?}",
            expr
        ))),
    }
}

/// A composition fragment: either a plain node or a spliced subgraph. Both
/// expose one input handle and one-or-more output handles the enclosing graph
/// wires edges against, plus the internal actors/edges to splice in.
struct Fragment {
    input: String,
    outputs: Vec<String>,
    nodes: Vec<NodeValue>,
    edges: Vec<EdgeValue>,
}

fn fragment_of(val: &Value) -> Result<Fragment, MagError> {
    match val {
        Value::Node(n) => Ok(Fragment {
            input: n.id.clone(),
            outputs: vec![n.id.clone()],
            nodes: vec![n.clone()],
            edges: vec![],
        }),
        Value::Subgraph(s) => Ok(Fragment {
            input: s.input.actor.clone(),
            outputs: s.outputs.iter().map(|p| p.actor.clone()).collect(),
            nodes: s.nodes.clone(),
            edges: s.edges.clone(),
        }),
        other => Err(MagError::Eval(format!(
            "graph expects node or subgraph values, got {}",
            other.type_name()
        ))),
    }
}

/// Structural equality for actors sharing an id — distinguishes a legitimate
/// dedup (same node value referenced by two edges) from an id collision (two
/// different actors claiming one id).
fn nodes_equal(a: &NodeValue, b: &NodeValue) -> bool {
    a.node_type == b.node_type
        && a.input_type == b.input_type
        && a.output_type == b.output_type
        && a.args.len() == b.args.len()
        && a.args
            .iter()
            .all(|(k, v)| b.args.get(k).is_some_and(|bv| values_equal(v, bv)))
}

/// Splice a fragment's internal actors and edges into the accumulating graph.
/// Identical duplicate actors dedup silently; an id claimed by two structurally
/// different actors is a collision — rejected, naming the clash.
fn splice_fragment(
    frag_nodes: &[NodeValue],
    frag_edges: &[EdgeValue],
    nodes: &mut Vec<NodeValue>,
    index: &mut std::collections::HashMap<String, NodeValue>,
    edges: &mut Vec<EdgeValue>,
) -> Result<(), MagError> {
    for node in frag_nodes {
        match index.get(&node.id) {
            Some(existing) if !nodes_equal(existing, node) => {
                return Err(MagError::Graph(format!(
                    "duplicate actor id '{}' — instance ids must be unique across the program",
                    node.id
                )));
            }
            Some(_) => {}
            None => {
                index.insert(node.id.clone(), node.clone());
                nodes.push(node.clone());
            }
        }
    }
    for edge in frag_edges {
        if !edges.iter().any(|e| e.from == edge.from && e.to == edge.to) {
            edges.push(edge.clone());
        }
    }
    Ok(())
}

fn eval_graph(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    // Selectively evaluate args: skip -> symbols and pass keywords through
    let mut vals = Vec::new();
    for e in args {
        match e {
            Expr::Symbol(s) if s == "->" => vals.push(Value::Symbol("->".into())),
            _ => vals.push(eval_expr(env, e)?),
        }
    }

    let mut nodes: Vec<NodeValue> = Vec::new();
    let mut edges: Vec<EdgeValue> = Vec::new();
    let mut index: std::collections::HashMap<String, NodeValue> = std::collections::HashMap::new();
    let mut terminal: Option<String> = None;

    let mut i = 0;
    while i < vals.len() {
        // Check for :terminal keyword — the program sink is a plain node.
        if let Value::Keyword(kw) = &vals[i] {
            if kw == "terminal" {
                i += 1;
                if i < vals.len() {
                    match &vals[i] {
                        Value::Node(n) => {
                            if terminal.is_some() {
                                return Err(MagError::Eval(
                                    "graph must have exactly one :terminal (sink) node".into(),
                                ));
                            }
                            terminal = Some(n.id.clone());
                            splice_fragment(
                                std::slice::from_ref(n),
                                &[],
                                &mut nodes,
                                &mut index,
                                &mut edges,
                            )?;
                            i += 1;
                        }
                        other => {
                            return Err(MagError::Eval(format!(
                                "terminal expects a node value, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                continue;
            }
        }

        // Edge pattern: fragment -> fragment. Boundary edges from a subgraph
        // dissolve into one edge per output-port actor (lowering.md).
        if i + 2 < vals.len() {
            if let Value::Symbol(arrow) = &vals[i + 1] {
                if arrow == "->" {
                    let from = fragment_of(&vals[i])?;
                    let to = fragment_of(&vals[i + 2])?;
                    splice_fragment(&from.nodes, &from.edges, &mut nodes, &mut index, &mut edges)?;
                    splice_fragment(&to.nodes, &to.edges, &mut nodes, &mut index, &mut edges)?;
                    for out in &from.outputs {
                        if !edges.iter().any(|e| e.from == *out && e.to == to.input) {
                            edges.push(EdgeValue {
                                from: out.clone(),
                                to: to.input.clone(),
                            });
                        }
                    }
                    i += 3;
                    continue;
                }
            }
        }

        // Standalone node or subgraph (not part of an edge).
        if matches!(&vals[i], Value::Node(_) | Value::Subgraph(_)) {
            let frag = fragment_of(&vals[i])?;
            splice_fragment(&frag.nodes, &frag.edges, &mut nodes, &mut index, &mut edges)?;
            i += 1;
            continue;
        }

        // Skip arrow symbols that were already evaluated from variables.
        i += 1;
    }

    let terminal = match terminal {
        Some(t) => t,
        None => {
            // Auto-detect: sink node is terminal by definition.
            let sinks: Vec<&str> = nodes
                .iter()
                .filter(|n| n.node_type == "sink")
                .map(|n| n.id.as_str())
                .collect();
            match sinks.len() {
                0 => return Err(MagError::Eval(
                    "graph has no sink node — every graph must end with exactly one (node \"sink\" ...) that collects the final output".into(),
                )),
                1 => sinks[0].to_string(),
                _ => return Err(MagError::Eval(format!(
                    "graph has {} sink nodes ({}); exactly one is required",
                    sinks.len(),
                    sinks.join(", ")
                ))),
            }
        }
    };

    Ok(Value::Graph(GraphValue {
        nodes,
        edges,
        terminal,
    }))
}

/// Prefix every internal actor id, edge endpoint, and boundary port of a
/// subgraph with `prefix.`, so two instantiations of one template occupy
/// disjoint id subtrees (lowering.md namespacing).
fn prefix_subgraph(prefix: &str, sub: &mut SubgraphValue) {
    let ns = |id: &str| format!("{prefix}.{id}");
    for node in &mut sub.nodes {
        node.id = ns(&node.id);
    }
    for edge in &mut sub.edges {
        edge.from = ns(&edge.from);
        edge.to = ns(&edge.to);
    }
    sub.input.actor = ns(&sub.input.actor);
    for port in &mut sub.outputs {
        port.actor = ns(&port.actor);
    }
}

fn provider_out() -> MagType {
    MagType::Named("generic-provider.ProviderOut".into())
}

/// `(agent {:id … :model … :system … :tools […] :max-steps N} : IN -> OUT)`
///
/// A stdlib template: composes the primitive agentic-loop actors (entry
/// adapter, llm call, run-tool, tool-result, loop-counter, exhaust summarizer)
/// into one cyclic constellation, namespaced under `:id`, exposing a single
/// input port (`entry`) and the boundary output type emitted by `llm`/`exhaust`.
fn eval_agent(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    let colon_pos = args
        .iter()
        .position(|e| matches!(e, Expr::Symbol(s) if s == ":"));
    let (value_args, type_args) = match colon_pos {
        Some(pos) => (&args[..pos], &args[pos + 1..]),
        None => {
            return Err(MagError::Eval(
                "agent requires a type annotation after ':'".into(),
            ))
        }
    };
    if value_args.is_empty() {
        return Err(MagError::Eval("agent requires a config map".into()));
    }

    let config = match eval_expr(env, &value_args[0])? {
        Value::Map(m) => m,
        other => {
            return Err(MagError::Eval(format!(
                "agent config must be a map, got {}",
                other.type_name()
            )))
        }
    };
    let (input_type, output_type) = parse_type_annotation(type_args)?;

    let id = match config.get("id") {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(MagError::Eval("agent requires a string :id".into())),
    };

    // llm params: model, system, tools, provider (present keys only, sorted by
    // BTreeMap). `provider` names the provider capability the llm actor targets;
    // the kernel llm factory rejects construction without it, so it is authored
    // on the agent and threaded through to both llm actors below.
    let mut llm_args: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(model) = config.get("model") {
        llm_args.insert("model".into(), model.clone());
    }
    if let Some(system) = config.get("system") {
        llm_args.insert("system".into(), system.clone());
    }
    if let Some(tools) = config.get("tools") {
        llm_args.insert("tools".into(), tools.clone());
    }
    if let Some(provider) = config.get("provider") {
        llm_args.insert("provider".into(), provider.clone());
    }

    // exhaust: a summarizing llm that leaves the bounded loop.
    let mut exhaust_args: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(model) = config.get("model") {
        exhaust_args.insert("model".into(), model.clone());
    }
    exhaust_args.insert(
        "system".into(),
        Value::Str("Summarize what you found so far.".into()),
    );
    if let Some(provider) = config.get("provider") {
        exhaust_args.insert("provider".into(), provider.clone());
    }

    let max_steps = config.get("max-steps").cloned().unwrap_or(Value::Int(50));

    let entry = NodeValue {
        id: "entry".into(),
        node_type: "adapter".into(),
        args: BTreeMap::from([("seed".into(), Value::Str("provider-in".into()))]),
        input_type: input_type.clone(),
        output_type: provider_out(),
    };
    let llm = NodeValue {
        id: "llm".into(),
        node_type: "llm".into(),
        args: llm_args,
        input_type: provider_out(),
        output_type: MagType::Union(vec![
            MagType::Named("generic-tool.ToolCalls".into()),
            output_type.clone(),
        ]),
    };
    let run_tool = NodeValue {
        id: "run-tool".into(),
        node_type: "run-tool".into(),
        args: BTreeMap::new(),
        input_type: MagType::Named("generic-tool.ToolCalls".into()),
        output_type: MagType::Named("generic-tool.ToolHandle".into()),
    };
    let tool_result = NodeValue {
        id: "tool-result".into(),
        node_type: "tool-result".into(),
        args: BTreeMap::new(),
        input_type: MagType::Named("generic-tool.ToolHandle".into()),
        output_type: provider_out(),
    };
    let loop_counter = NodeValue {
        id: "loop-counter".into(),
        node_type: "loop-counter".into(),
        args: BTreeMap::from([("max".into(), max_steps)]),
        input_type: provider_out(),
        output_type: MagType::Union(vec![
            provider_out(),
            MagType::Named("mag.LoopExhausted".into()),
        ]),
    };
    let exhaust = NodeValue {
        id: "exhaust".into(),
        node_type: "llm".into(),
        args: exhaust_args,
        input_type: MagType::Named("mag.LoopExhausted".into()),
        output_type: output_type.clone(),
    };

    let mut sub = SubgraphValue {
        nodes: vec![entry, llm, run_tool, tool_result, loop_counter, exhaust],
        edges: vec![
            EdgeValue {
                from: "entry".into(),
                to: "llm".into(),
            },
            EdgeValue {
                from: "llm".into(),
                to: "run-tool".into(),
            },
            EdgeValue {
                from: "run-tool".into(),
                to: "tool-result".into(),
            },
            EdgeValue {
                from: "tool-result".into(),
                to: "loop-counter".into(),
            },
            EdgeValue {
                from: "loop-counter".into(),
                to: "llm".into(),
            },
            EdgeValue {
                from: "loop-counter".into(),
                to: "exhaust".into(),
            },
        ],
        input: Port {
            actor: "entry".into(),
            ty: input_type,
        },
        outputs: vec![
            Port {
                actor: "llm".into(),
                ty: output_type.clone(),
            },
            Port {
                actor: "exhaust".into(),
                ty: output_type,
            },
        ],
    };
    prefix_subgraph(&id, &mut sub);

    Ok(Value::Subgraph(sub))
}

/// Simple `{key}` interpolation: scan for `{` and `}`, look up the key in the data map.
fn interpolate_template(template: &str, data: &BTreeMap<String, Value>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut key = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    found_close = true;
                    break;
                }
                key.push(inner);
            }
            if found_close {
                if let Some(val) = data.get(&key) {
                    result.push_str(&value_to_string(val));
                } else {
                    // No match — preserve the original `{key}` literal
                    result.push('{');
                    result.push_str(&key);
                    result.push('}');
                }
            } else {
                // Unclosed `{` — preserve literally
                result.push('{');
                result.push_str(&key);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn is_truthy(val: &Value) -> bool {
    !matches!(val, Value::Nil | Value::Bool(false))
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Nil => "nil".to_string(),
        Value::Keyword(k) => format!(":{k}"),
        Value::Symbol(s) => s.clone(),
        Value::TypeDecl(s) => s.clone(),
        Value::List(items) | Value::Vector(items) => {
            let parts: Vec<String> = items.iter().map(value_to_string).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Map(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k}: {}", value_to_string(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Node(n) => format!("node:{}", n.id),
        Value::Graph(_) => "graph".to_string(),
        Value::Subgraph(_) => "subgraph".to_string(),
        Value::Fn(_) => "fn".to_string(),
        Value::BuiltinFn(name) => format!("builtin:{name}"),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Nil, Value::Nil) => true,
        (Value::Keyword(a), Value::Keyword(b)) => a == b,
        (Value::Symbol(a), Value::Symbol(b)) => a == b,
        (Value::List(a), Value::List(b)) | (Value::Vector(a), Value::Vector(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
        }
        _ => false,
    }
}

// --- Builtin implementations ---

fn builtin_str(args: &[Value]) -> Result<Value, MagError> {
    let mut result = String::new();
    for val in args {
        result.push_str(&value_to_string(val));
    }
    Ok(Value::Str(result))
}

fn builtin_map(f: &Value, coll: &Value) -> Result<Value, MagError> {
    let items = extract_list(coll)?;
    let mut result = Vec::new();
    for item in items {
        result.push(apply_value(f, std::slice::from_ref(item))?);
    }
    Ok(Value::List(result))
}

fn builtin_filter(f: &Value, coll: &Value) -> Result<Value, MagError> {
    let items = extract_list(coll)?;
    let mut result = Vec::new();
    for item in items {
        let test = apply_value(f, std::slice::from_ref(item))?;
        if is_truthy(&test) {
            result.push(item.clone());
        }
    }
    Ok(Value::List(result))
}

fn builtin_fold(f: &Value, init: Value, coll: &Value) -> Result<Value, MagError> {
    let items = extract_list(coll)?;
    let mut acc = init;
    for item in items {
        acc = apply_value(f, &[acc, item.clone()])?;
    }
    Ok(acc)
}

fn builtin_flat_map(f: &Value, coll: &Value) -> Result<Value, MagError> {
    let items = extract_list(coll)?;
    let mut result = Vec::new();
    for item in items {
        let mapped = apply_value(f, std::slice::from_ref(item))?;
        match mapped {
            Value::List(inner) | Value::Vector(inner) => result.extend(inner),
            other => result.push(other),
        }
    }
    Ok(Value::List(result))
}

fn builtin_concat(a: &Value, b: &Value) -> Result<Value, MagError> {
    let mut items_a = extract_list(a)?.to_vec();
    let items_b = extract_list(b)?;
    items_a.extend(items_b.iter().cloned());
    Ok(Value::List(items_a))
}

fn builtin_get(m: &Value, key: &Value) -> Result<Value, MagError> {
    match m {
        Value::Map(map) => {
            let k = value_to_string(key);
            Ok(map.get(&k).cloned().unwrap_or(Value::Nil))
        }
        other => Err(MagError::Eval(format!(
            "get expects a map, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_assoc(m: &Value, key: &Value, val: &Value) -> Result<Value, MagError> {
    match m {
        Value::Map(map) => {
            let mut new_map = map.clone();
            let k = value_to_string(key);
            new_map.insert(k, val.clone());
            Ok(Value::Map(new_map))
        }
        other => Err(MagError::Eval(format!(
            "assoc expects a map, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_keys(m: &Value) -> Result<Value, MagError> {
    match m {
        Value::Map(map) => {
            let keys: Vec<Value> = map.keys().map(|k| Value::Str(k.clone())).collect();
            Ok(Value::List(keys))
        }
        other => Err(MagError::Eval(format!(
            "keys expects a map, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_count(v: &Value) -> Result<Value, MagError> {
    match v {
        Value::List(items) | Value::Vector(items) => Ok(Value::Int(items.len() as i64)),
        Value::Map(m) => Ok(Value::Int(m.len() as i64)),
        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
        other => Err(MagError::Eval(format!(
            "count expects a collection, got {}",
            other.type_name()
        ))),
    }
}

fn extract_list(val: &Value) -> Result<&[Value], MagError> {
    match val {
        Value::List(items) | Value::Vector(items) => Ok(items),
        other => Err(MagError::Eval(format!(
            "expected list/vector, got {}",
            other.type_name()
        ))),
    }
}

fn apply_value(f: &Value, args: &[Value]) -> Result<Value, MagError> {
    match f {
        Value::Fn(fv) => apply_fn(fv, args),
        Value::BuiltinFn(name) => call_builtin_with_values(name, args),
        other => Err(MagError::Eval(format!(
            "cannot call value of type {}",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Expr;
    use crate::env::Env;

    fn eval(exprs: Vec<Expr>) -> Result<Value, MagError> {
        let mut env = Env::new_with_stdlib();
        eval_program(&mut env, &exprs)
    }

    fn eval_with_env(env: &mut Env, exprs: Vec<Expr>) -> Result<Value, MagError> {
        eval_program(env, &exprs)
    }

    #[test]
    fn def_basic() {
        // (def x 42) -> Int(42)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("def".into()),
            Expr::Symbol("x".into()),
            Expr::Int(42),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn def_then_lookup() {
        // (def x 42) x -> Int(42)
        let result = eval(vec![
            Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("x".into()),
                Expr::Int(42),
            ]),
            Expr::Symbol("x".into()),
        ])
        .unwrap();
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn let_binding() {
        // (let [x 1 y 2] (str x y)) -> Str("12")
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("let".into()),
            Expr::Vector(vec![
                Expr::Symbol("x".into()),
                Expr::Int(1),
                Expr::Symbol("y".into()),
                Expr::Int(2),
            ]),
            Expr::List(vec![
                Expr::Symbol("str".into()),
                Expr::Symbol("x".into()),
                Expr::Symbol("y".into()),
            ]),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "12"));
    }

    #[test]
    fn fn_basic() {
        // (def f (fn [x] x)) (f 42) -> Int(42)
        let result = eval(vec![
            Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("f".into()),
                Expr::List(vec![
                    Expr::Symbol("fn".into()),
                    Expr::Vector(vec![Expr::Symbol("x".into())]),
                    Expr::Symbol("x".into()),
                ]),
            ]),
            Expr::List(vec![Expr::Symbol("f".into()), Expr::Int(42)]),
        ])
        .unwrap();
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn if_true_branch() {
        // (if true 1 2) -> Int(1)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("if".into()),
            Expr::Bool(true),
            Expr::Int(1),
            Expr::Int(2),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(1)));
    }

    #[test]
    fn if_false_branch() {
        // (if false 1 2) -> Int(2)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("if".into()),
            Expr::Bool(false),
            Expr::Int(1),
            Expr::Int(2),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(2)));
    }

    #[test]
    fn threading_macro() {
        // (def inc (fn [x] (str x "!")))
        // (-> "hello" inc inc) -> Str("hello!!")
        let result = eval(vec![
            Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("inc".into()),
                Expr::List(vec![
                    Expr::Symbol("fn".into()),
                    Expr::Vector(vec![Expr::Symbol("x".into())]),
                    Expr::List(vec![
                        Expr::Symbol("str".into()),
                        Expr::Symbol("x".into()),
                        Expr::Str("!".into()),
                    ]),
                ]),
            ]),
            Expr::List(vec![
                Expr::Symbol("->".into()),
                Expr::Str("hello".into()),
                Expr::Symbol("inc".into()),
                Expr::Symbol("inc".into()),
            ]),
        ])
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "hello!!"));
    }

    #[test]
    fn builtin_str_concat() {
        // (str "a" "b" "c") -> Str("abc")
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("str".into()),
            Expr::Str("a".into()),
            Expr::Str("b".into()),
            Expr::Str("c".into()),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "abc"));
    }

    #[test]
    fn builtin_equality() {
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("=".into()),
            Expr::Int(42),
            Expr::Int(42),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn builtin_count_list() {
        // (count [1 2 3]) -> Int(3)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("count".into()),
            Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(3)));
    }

    #[test]
    fn builtin_map_fn() {
        // (def double (fn [x] (str x x)))
        // (map double [1 2]) -> List([Str("11"), Str("22")])
        let result = eval(vec![
            Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("double".into()),
                Expr::List(vec![
                    Expr::Symbol("fn".into()),
                    Expr::Vector(vec![Expr::Symbol("x".into())]),
                    Expr::List(vec![
                        Expr::Symbol("str".into()),
                        Expr::Symbol("x".into()),
                        Expr::Symbol("x".into()),
                    ]),
                ]),
            ]),
            Expr::List(vec![
                Expr::Symbol("map".into()),
                Expr::Symbol("double".into()),
                Expr::Vector(vec![Expr::Int(1), Expr::Int(2)]),
            ]),
        ])
        .unwrap();

        if let Value::List(items) = result {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], Value::Str(s) if s == "11"));
            assert!(matches!(&items[1], Value::Str(s) if s == "22"));
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn builtin_filter_fn() {
        // (filter (fn [x] (= x 2)) [1 2 3]) -> List([Int(2)])
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("filter".into()),
            Expr::List(vec![
                Expr::Symbol("fn".into()),
                Expr::Vector(vec![Expr::Symbol("x".into())]),
                Expr::List(vec![
                    Expr::Symbol("=".into()),
                    Expr::Symbol("x".into()),
                    Expr::Int(2),
                ]),
            ]),
            Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]),
        ])])
        .unwrap();

        if let Value::List(items) = result {
            assert_eq!(items.len(), 1);
            assert!(matches!(&items[0], Value::Int(2)));
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn builtin_or_not() {
        // (or false 42) -> Int(42)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("or".into()),
            Expr::Bool(false),
            Expr::Int(42),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(42)));

        // (not true) -> Bool(false)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("not".into()),
            Expr::Bool(true),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Bool(false)));
    }

    #[test]
    fn type_decl() {
        // (type Findings) -> TypeDecl("Findings")
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("type".into()),
            Expr::Symbol("Findings".into()),
        ])])
        .unwrap();
        assert!(matches!(result, Value::TypeDecl(ref s) if s == "Findings"));
    }

    #[test]
    fn node_creation_with_types() {
        // (node "llm" {} : Patch -> Findings)
        let mut env = Env::new_with_stdlib();
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("node".into()),
                Expr::Str("llm".into()),
                Expr::Map(vec![]),
                Expr::Symbol(":".into()),
                Expr::Symbol("Patch".into()),
                Expr::Symbol("->".into()),
                Expr::Symbol("Findings".into()),
            ])],
        )
        .unwrap();

        if let Value::Node(node) = result {
            assert_eq!(node.node_type, "llm");
            assert_eq!(node.input_type, MagType::Named("Patch".into()));
            assert_eq!(node.output_type, MagType::Named("Findings".into()));
        } else {
            panic!("expected node, got {:?}", result);
        }
    }

    #[test]
    fn node_with_union_output() {
        // (node "build" {} : Patch -> (BuildResult | BuildError))
        let mut env = Env::new_with_stdlib();
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("node".into()),
                Expr::Str("build".into()),
                Expr::Map(vec![]),
                Expr::Symbol(":".into()),
                Expr::Symbol("Patch".into()),
                Expr::Symbol("->".into()),
                Expr::List(vec![
                    Expr::Symbol("BuildResult".into()),
                    Expr::Symbol("|".into()),
                    Expr::Symbol("BuildError".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Node(node) = result {
            assert_eq!(
                node.output_type,
                MagType::Union(vec![
                    MagType::Named("BuildResult".into()),
                    MagType::Named("BuildError".into()),
                ])
            );
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn node_with_type_var() {
        // (node "passthrough" {} : INPUT -> INPUT)
        let mut env = Env::new_with_stdlib();
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("node".into()),
                Expr::Str("passthrough".into()),
                Expr::Map(vec![]),
                Expr::Symbol(":".into()),
                Expr::Symbol("INPUT".into()),
                Expr::Symbol("->".into()),
                Expr::Symbol("INPUT".into()),
            ])],
        )
        .unwrap();

        if let Value::Node(node) = result {
            assert_eq!(node.input_type, MagType::Var("INPUT".into()));
            assert_eq!(node.output_type, MagType::Var("INPUT".into()));
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn def_node_updates_id() {
        // (def my-node (node "llm" {} : A -> B))
        let mut env = Env::new_with_stdlib();
        eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("my-node".into()),
                Expr::List(vec![
                    Expr::Symbol("node".into()),
                    Expr::Str("llm".into()),
                    Expr::Map(vec![]),
                    Expr::Symbol(":".into()),
                    Expr::Symbol("A".into()),
                    Expr::Symbol("->".into()),
                    Expr::Symbol("B".into()),
                ]),
            ])],
        )
        .unwrap();

        let val = env.lookup("my-node").unwrap();
        if let Value::Node(node) = val {
            assert_eq!(node.id, "my-node");
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn graph_creation_with_edges() {
        // (let [a (node "llm" {} : A -> B)
        //       b (node "check" {} : B -> C)]
        //   (graph a -> b :terminal b))
        let mut env = Env::new_with_stdlib();
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("let".into()),
                Expr::Vector(vec![
                    Expr::Symbol("a".into()),
                    Expr::List(vec![
                        Expr::Symbol("node".into()),
                        Expr::Str("llm".into()),
                        Expr::Map(vec![]),
                        Expr::Symbol(":".into()),
                        Expr::Symbol("A".into()),
                        Expr::Symbol("->".into()),
                        Expr::Symbol("B".into()),
                    ]),
                    Expr::Symbol("b".into()),
                    Expr::List(vec![
                        Expr::Symbol("node".into()),
                        Expr::Str("check".into()),
                        Expr::Map(vec![]),
                        Expr::Symbol(":".into()),
                        Expr::Symbol("B".into()),
                        Expr::Symbol("->".into()),
                        Expr::Symbol("C".into()),
                    ]),
                ]),
                Expr::List(vec![
                    Expr::Symbol("graph".into()),
                    Expr::Symbol("a".into()),
                    Expr::Symbol("->".into()),
                    Expr::Symbol("b".into()),
                    Expr::Keyword("terminal".into()),
                    Expr::Symbol("b".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Graph(g) = result {
            assert_eq!(g.nodes.len(), 2);
            assert_eq!(g.edges.len(), 1);
            assert_eq!(g.edges[0].from, "a");
            assert_eq!(g.edges[0].to, "b");
            assert_eq!(g.terminal, "b");
        } else {
            panic!("expected graph, got {:?}", result);
        }
    }

    #[test]
    fn builtin_get_assoc() {
        // (get (assoc {} "k" 42) "k") -> Int(42)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("get".into()),
            Expr::List(vec![
                Expr::Symbol("assoc".into()),
                Expr::Map(vec![]),
                Expr::Str("k".into()),
                Expr::Int(42),
            ]),
            Expr::Str("k".into()),
        ])])
        .unwrap();
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn builtin_fold_sum() {
        // (def add (fn [a b] (str a b)))
        // (fold add "" [1 2 3]) -> Str("123")
        let result = eval(vec![
            Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("add".into()),
                Expr::List(vec![
                    Expr::Symbol("fn".into()),
                    Expr::Vector(vec![Expr::Symbol("a".into()), Expr::Symbol("b".into())]),
                    Expr::List(vec![
                        Expr::Symbol("str".into()),
                        Expr::Symbol("a".into()),
                        Expr::Symbol("b".into()),
                    ]),
                ]),
            ]),
            Expr::List(vec![
                Expr::Symbol("fold".into()),
                Expr::Symbol("add".into()),
                Expr::Str("".into()),
                Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]),
            ]),
        ])
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "123"));
    }

    #[test]
    fn read_reads_file() {
        let dir = std::env::temp_dir().join("mag_test_read_reads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.md"), "hello from read").unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("read".into()),
                Expr::Str("test.md".into()),
            ])],
        )
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "hello from read"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_interpolates_data() {
        let dir = std::env::temp_dir().join("mag_test_read_interp");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("greet.md"), "hello {name}, you are {role}").unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("read".into()),
                Expr::Str("greet.md".into()),
                Expr::Map(vec![
                    (Expr::Keyword("name".into()), Expr::Str("world".into())),
                    (Expr::Keyword("role".into()), Expr::Str("explorer".into())),
                ]),
            ])],
        )
        .unwrap();
        assert!(matches!(result, Value::Str(ref s) if s == "hello world, you are explorer"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_missing_file_errors() {
        let dir = std::env::temp_dir().join("mag_test_read_missing");
        std::fs::create_dir_all(&dir).unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("read".into()),
                Expr::Str("nonexistent.md".into()),
            ])],
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("file not found: nonexistent.md"), "got: {err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builtin_keys_sorted() {
        // Keys come out sorted because BTreeMap
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("keys".into()),
            Expr::Map(vec![
                (Expr::Keyword("b".into()), Expr::Int(2)),
                (Expr::Keyword("a".into()), Expr::Int(1)),
            ]),
        ])])
        .unwrap();

        if let Value::List(keys) = result {
            assert_eq!(keys.len(), 2);
            assert!(matches!(&keys[0], Value::Str(s) if s == "a"));
            assert!(matches!(&keys[1], Value::Str(s) if s == "b"));
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn builtin_concat_lists() {
        // (concat [1 2] [3 4]) -> List([1,2,3,4])
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("concat".into()),
            Expr::Vector(vec![Expr::Int(1), Expr::Int(2)]),
            Expr::Vector(vec![Expr::Int(3), Expr::Int(4)]),
        ])])
        .unwrap();

        if let Value::List(items) = result {
            assert_eq!(items.len(), 4);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn unresolved_symbol_errors() {
        let result = eval(vec![Expr::Symbol("nonexistent".into())]);
        assert!(result.is_err());
    }

    #[test]
    fn empty_program() {
        let result = eval(vec![]).unwrap();
        assert!(matches!(result, Value::Nil));
    }

    #[test]
    fn node_with_dotted_types_compiles() {
        // (node "llm" {} : generic-provider.ProviderOut -> (generic-tool.ToolCalls | generic-provider.FinalAnswer))
        let mut env = Env::new_with_stdlib();
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("node".into()),
                Expr::Str("llm".into()),
                Expr::Map(vec![]),
                Expr::Symbol(":".into()),
                Expr::Symbol("generic-provider.ProviderOut".into()),
                Expr::Symbol("->".into()),
                Expr::List(vec![
                    Expr::Symbol("generic-tool.ToolCalls".into()),
                    Expr::Symbol("|".into()),
                    Expr::Symbol("generic-provider.FinalAnswer".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Node(node) = &result {
            assert_eq!(
                node.input_type,
                MagType::Named("generic-provider.ProviderOut".into())
            );
            assert_eq!(
                node.output_type,
                MagType::Union(vec![
                    MagType::Named("generic-tool.ToolCalls".into()),
                    MagType::Named("generic-provider.FinalAnswer".into()),
                ])
            );
        } else {
            panic!("expected node, got {:?}", result);
        }
    }

    // --- require tests ---

    #[test]
    fn require_returns_namespace_map() {
        let dir = std::env::temp_dir().join("mag_test_require_defs");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("constants.mag"),
            "(def answer 42)\n(def greeting \"hello\")\n",
        )
        .unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("c".into()),
                Expr::List(vec![
                    Expr::Symbol("require".into()),
                    Expr::Str("constants".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Map(map) = result {
            assert!(matches!(map.get("answer"), Some(Value::Int(42))));
            assert!(matches!(map.get("greeting"), Some(Value::Str(ref s)) if s == "hello"));
        } else {
            panic!("expected map, got {:?}", result);
        }
        // defs do NOT leak into caller's scope
        assert!(env.lookup("answer").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn require_captures_all_defs_in_map() {
        let dir = std::env::temp_dir().join("mag_test_require_ret");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("multi.mag"), "(def x 1)\n(def y 2)\n").unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("m".into()),
                Expr::List(vec![
                    Expr::Symbol("require".into()),
                    Expr::Str("multi".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Map(map) = result {
            assert!(matches!(map.get("x"), Some(Value::Int(1))));
            assert!(matches!(map.get("y"), Some(Value::Int(2))));
        } else {
            panic!("expected map, got {:?}", result);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn require_circular_errors() {
        let dir = std::env::temp_dir().join("mag_test_require_circ");
        let _ = std::fs::create_dir_all(&dir);
        // a.mag requires b.mag, b.mag requires a.mag
        std::fs::write(dir.join("a.mag"), "(require \"b\")\n").unwrap();
        std::fs::write(dir.join("b.mag"), "(require \"a\")\n").unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("require".into()),
                Expr::Str("a".into()),
            ])],
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("circular require"),
            "expected circular require error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn require_missing_module_errors() {
        let dir = std::env::temp_dir().join("mag_test_require_missing");
        let _ = std::fs::create_dir_all(&dir);

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("require".into()),
                Expr::Str("nonexistent".into()),
            ])],
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot read module nonexistent"),
            "expected clear error about missing module, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn require_nested_subdirectory() {
        let dir = std::env::temp_dir().join("mag_test_require_nested");
        let _ = std::fs::create_dir_all(dir.join("lib"));
        std::fs::write(
            dir.join("lib/utils.mag"),
            "(def tools [\"grep\" \"find\"])\n",
        )
        .unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("u".into()),
                Expr::List(vec![
                    Expr::Symbol("require".into()),
                    Expr::Str("lib/utils".into()),
                ]),
            ])],
        )
        .unwrap();

        if let Value::Map(map) = result {
            if let Some(Value::Vector(items)) = map.get("tools") {
                assert_eq!(items.len(), 2);
            } else {
                panic!("expected tools vector in map, got {:?}", map);
            }
        } else {
            panic!("expected map, got {:?}", result);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_flat_map_basic() {
        // (flat-map (fn [x] [x x]) [1 2 3]) => (1 1 2 2 3 3)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("flat-map".into()),
            Expr::List(vec![
                Expr::Symbol("fn".into()),
                Expr::Vector(vec![Expr::Symbol("x".into())]),
                Expr::Vector(vec![Expr::Symbol("x".into()), Expr::Symbol("x".into())]),
            ]),
            Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]),
        ])])
        .unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 6);
                assert!(matches!(items[0], Value::Int(1)));
                assert!(matches!(items[1], Value::Int(1)));
                assert!(matches!(items[2], Value::Int(2)));
                assert!(matches!(items[3], Value::Int(2)));
                assert!(matches!(items[4], Value::Int(3)));
                assert!(matches!(items[5], Value::Int(3)));
            }
            other => panic!("expected list from flat-map, got {:?}", other),
        }
    }

    #[test]
    fn builtin_flat_map_non_list_result() {
        // When the mapped fn returns a non-list, treat as single element
        // (flat-map (fn [x] x) [1 2 3]) => (1 2 3)
        let result = eval(vec![Expr::List(vec![
            Expr::Symbol("flat-map".into()),
            Expr::List(vec![
                Expr::Symbol("fn".into()),
                Expr::Vector(vec![Expr::Symbol("x".into())]),
                Expr::Symbol("x".into()),
            ]),
            Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]),
        ])])
        .unwrap();
        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
            }
            other => panic!("expected list from flat-map, got {:?}", other),
        }
    }

    #[test]
    fn read_rejects_path_traversal() {
        let dir = std::env::temp_dir().join("mag_test_traversal");
        std::fs::create_dir_all(&dir).unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("read".into()),
                Expr::Str("../../etc/passwd".into()),
            ])],
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("path traversal"),
            "expected path traversal error"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_rejects_absolute_path() {
        let dir = std::env::temp_dir().join("mag_test_absolute");
        std::fs::create_dir_all(&dir).unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("read".into()),
                Expr::Str("/etc/passwd".into()),
            ])],
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("absolute"),
            "expected absolute path error"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn require_rejects_path_traversal() {
        let dir = std::env::temp_dir().join("mag_test_require_traversal");
        std::fs::create_dir_all(&dir).unwrap();

        let mut env = Env::new_with_stdlib_and_source_dir(&dir);
        let result = eval_with_env(
            &mut env,
            vec![Expr::List(vec![
                Expr::Symbol("require".into()),
                Expr::Str("../../../etc/evil".into()),
            ])],
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("path traversal"),
            "expected path traversal error"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
