use crate::ast::{Artifact, Expr, FnValue, ForeignDecl, TypeDecl, Value};
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

pub mod fuel {
    use crate::error::MagError;
    use std::cell::Cell;

    thread_local! {
        static REMAINING: Cell<Option<u64>> = const { Cell::new(None) };
        static CALL_DEPTH: Cell<u16> = const { Cell::new(0) };
        static EXPR_DEPTH: Cell<u16> = const { Cell::new(0) };
    }

    pub struct Guard {
        previous: Option<u64>,
        active: bool,
    }
    pub fn install(limit: u64) -> Guard {
        let previous = REMAINING.with(|remaining| remaining.replace(Some(limit)));
        Guard {
            previous,
            active: true,
        }
    }
    pub fn ensure(limit: u64) -> Guard {
        REMAINING.with(|remaining| {
            if remaining.get().is_some() {
                Guard {
                    previous: None,
                    active: false,
                }
            } else {
                remaining.set(Some(limit));
                Guard {
                    previous: None,
                    active: true,
                }
            }
        })
    }
    pub fn step() -> Result<(), MagError> {
        REMAINING.with(|remaining| match remaining.get() {
            Some(0) => Err(MagError::Budget("expression step limit reached".into())),
            Some(left) => {
                remaining.set(Some(left - 1));
                Ok(())
            }
            None => Err(MagError::Budget(
                "evaluation started without an installed budget".into(),
            )),
        })
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.active {
                REMAINING.with(|remaining| remaining.set(self.previous));
            }
        }
    }

    pub struct CallGuard;
    pub fn enter_call() -> Result<CallGuard, MagError> {
        CALL_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= 64 {
                Err(MagError::Budget("function call depth limit reached".into()))
            } else {
                depth.set(current + 1);
                Ok(CallGuard)
            }
        })
    }
    impl Drop for CallGuard {
        fn drop(&mut self) {
            CALL_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }

    pub struct ExprGuard;
    pub fn enter_expr() -> Result<ExprGuard, MagError> {
        EXPR_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= 128 {
                Err(MagError::Budget("expression nesting limit reached".into()))
            } else {
                depth.set(current + 1);
                Ok(ExprGuard)
            }
        })
    }
    impl Drop for ExprGuard {
        fn drop(&mut self) {
            EXPR_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }
}

pub fn eval_program(env: &mut Env, exprs: &[Expr]) -> Result<Value, MagError> {
    let _fuel = fuel::ensure(100_000);
    let mut result = Value::Unit;
    for expr in exprs {
        result = eval_expr(env, expr)?;
    }
    Ok(result)
}

fn eval_expr(env: &mut Env, expr: &Expr) -> Result<Value, MagError> {
    let _depth = fuel::enter_expr()?;
    fuel::step()?;
    match expr {
        Expr::Str(v) => Ok(Value::Str(v.clone())),
        Expr::Int(v) => Ok(Value::Int(*v)),
        Expr::Float(v) => Ok(Value::Float(*v)),
        Expr::Bool(v) => Ok(Value::Bool(*v)),
        Expr::Nil => Ok(Value::Unit),
        Expr::Keyword(v) => Ok(Value::Keyword(v.clone())),
        Expr::Symbol(v) => env.lookup(v).cloned(),
        Expr::Vector(xs) => Ok(Value::Vector(
            xs.iter()
                .map(|x| eval_expr(env, x))
                .collect::<Result<_, _>>()?,
        )),
        Expr::Map(xs) => eval_map(env, xs),
        Expr::List(xs) => eval_list(env, xs),
    }
}

fn eval_map(env: &mut Env, pairs: &[(Expr, Expr)]) -> Result<Value, MagError> {
    let mut map = BTreeMap::new();
    for (k, v) in pairs {
        let key = match k {
            Expr::Keyword(s) | Expr::Str(s) | Expr::Symbol(s) => s.clone(),
            _ => value_string(&eval_expr(env, k)?),
        };
        map.insert(key, eval_expr(env, v)?);
    }
    Ok(Value::Map(map))
}

fn eval_list(env: &mut Env, items: &[Expr]) -> Result<Value, MagError> {
    if items.is_empty() {
        return Ok(Value::Unit);
    }
    if let Some(result) = maybe_require(env, items) {
        return result;
    }
    if let Expr::Symbol(head) = &items[0] {
        match head.as_str() {
            "def" => return eval_def(env, &items[1..]),
            "fn" => return eval_fn_form(env, &items[1..]),
            "let" => return eval_let(env, &items[1..]),
            "if" => return eval_if(env, &items[1..]),
            "type" => return eval_type_decl(env, &items[1..]),
            "foreign" => return eval_foreign(env, &items[1..]),
            "as" => return eval_as(env, &items[1..]),
            "type-tag" => return eval_type_tag(env, &items[1..]),
            "specialize" => return eval_specialize(env, &items[1..]),
            "|" | "+" => {
                return Ok(Value::Type(parse_type(
                    env,
                    &Expr::List(items.to_vec()),
                    &HashSet::new(),
                )?))
            }
            _ => {}
        }
    }
    let f = eval_expr(env, &items[0])?;
    let args = items[1..]
        .iter()
        .map(|x| eval_expr(env, x))
        .collect::<Result<Vec<_>, _>>()?;
    apply(env, &f, &args)
}

fn eval_def(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    arity(args, 2)?;
    let name = args[0]
        .as_symbol()
        .ok_or_else(|| MagError::Eval("def requires a symbol".into()))?;
    let value = match &args[1] {
        Expr::List(items) if matches!(items.first(), Some(Expr::Symbol(head)) if head == "fn") => {
            eval_fn_form_with_binding(env, &items[1..], Some(name))?
        }
        expr => eval_expr(env, expr)?,
    };
    env.define(name, value.clone());
    Ok(value)
}

fn eval_fn_form(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    eval_fn_form_with_binding(env, args, None)
}

fn eval_fn_form_with_binding(
    env: &mut Env,
    args: &[Expr],
    binding: Option<&str>,
) -> Result<Value, MagError> {
    if args.len() < 4 {
        return Err(MagError::Eval("fn syntax is (fn [[name Type] ...] -> Return body...) or (fn [T ...] [[name Type] ...] -> Return body...)".into()));
    }
    let empty = Expr::Vector(vec![]);
    let (type_expr, param_expr, arrow, return_expr, body) =
        if matches!(args.get(1), Some(Expr::Vector(_))) {
            (&args[0], &args[1], &args[2], &args[3], &args[4..])
        } else {
            (&empty, &args[0], &args[1], &args[2], &args[3..])
        };
    if !matches!(arrow,Expr::Symbol(s) if s=="->") {
        return Err(MagError::Eval(
            "fn signature requires -> before its return type".into(),
        ));
    }
    let type_params = match type_expr {
        Expr::Vector(xs) => xs
            .iter()
            .map(|x| {
                x.as_symbol()
                    .map(str::to_owned)
                    .ok_or_else(|| MagError::Type("fn generic parameters must be symbols".into()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => unreachable!(),
    };
    let vars = type_params.iter().cloned().collect::<HashSet<_>>();
    let pairs = match param_expr {
        Expr::Vector(xs) => xs,
        _ => return Err(MagError::Type("fn parameters must be a vector".into())),
    };
    let mut params = vec![];
    let mut param_types = vec![];
    for pair in pairs {
        match pair {
            Expr::Vector(items) if items.len() == 2 => {
                params.push(
                    items[0]
                        .as_symbol()
                        .ok_or_else(|| MagError::Type("fn parameter name must be a symbol".into()))?
                        .to_string(),
                );
                param_types.push(parse_type(env, &items[1], &vars)?);
            }
            _ => {
                return Err(MagError::Type(
                    "fn parameters must be [name Type] pairs".into(),
                ))
            }
        }
    }
    let return_type = parse_type(env, return_expr, &vars)?;
    crate::checker::check_function_with_binding(
        env,
        binding,
        &params,
        &param_types,
        &return_type,
        body,
    )?;
    Ok(Value::Fn(std::sync::Arc::new(FnValue {
        name: binding.map(str::to_owned),
        type_params,
        params,
        param_types,
        return_type,
        body: body.to_vec(),
        closure: env.snapshot(),
    })))
}

fn eval_as(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    arity(args, 2)?;
    let ty = parse_type(env, &args[0], &HashSet::new())?;
    let value = eval_expr(env, &args[1])?;
    validate_value(env, &value, &ty)?;
    Ok(Value::Typed(Box::new(raw(&value).clone()), ty))
}

fn eval_type_tag(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    arity(args, 1)?;
    Ok(Value::TypeTag(parse_type(env, &args[0], &HashSet::new())?))
}

fn eval_specialize(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    arity(args, 2)?;
    let decl = match eval_expr(env, &args[0])? {
        Value::Foreign(decl) => decl,
        _ => {
            return Err(MagError::Type(
                "specialize expects a Foreign declaration".into(),
            ))
        }
    };
    let type_exprs = match &args[1] {
        Expr::Vector(items) => items,
        _ => {
            return Err(MagError::Type(
                "specialize type arguments must be a vector".into(),
            ))
        }
    };
    if type_exprs.len() != decl.type_params.len() {
        return Err(MagError::Type(format!(
            "{} expects {} type arguments, got {}",
            decl.name,
            decl.type_params.len(),
            type_exprs.len()
        )));
    }
    let types = type_exprs
        .iter()
        .map(|expr| parse_type(env, expr, &HashSet::new()))
        .collect::<Result<Vec<_>, _>>()?;
    let subst = decl
        .type_params
        .iter()
        .cloned()
        .zip(types.clone())
        .collect();
    Ok(Value::Foreign(ForeignDecl {
        name: decl.name,
        type_params: vec![],
        specialization: types,
        params: crate::checker::substitute(&decl.params, &subst),
        input: crate::checker::substitute(&decl.input, &subst),
        output: crate::checker::substitute(&decl.output, &subst),
    }))
}

fn eval_let(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.len() < 2 {
        return Err(MagError::Eval("let requires bindings and body".into()));
    }
    let pairs = match &args[0] {
        Expr::Vector(xs) if xs.len() % 2 == 0 => xs,
        _ => return Err(MagError::Eval("let bindings must be pairs".into())),
    };
    env.push_scope();
    let result = (|| {
        for pair in pairs.chunks(2) {
            let n = pair[0]
                .as_symbol()
                .ok_or_else(|| MagError::Eval("let binding must be a symbol".into()))?;
            let v = eval_expr(env, &pair[1])?;
            env.define(n, v);
        }
        let mut out = Value::Unit;
        for expr in &args[1..] {
            out = eval_expr(env, expr)?;
        }
        Ok(out)
    })();
    env.pop_scope();
    result
}

fn eval_if(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if !(2..=3).contains(&args.len()) {
        return Err(MagError::Eval(
            "if requires condition, then, and optional else".into(),
        ));
    }
    if truthy(&eval_expr(env, &args[0])?) {
        eval_expr(env, &args[1])
    } else if args.len() == 3 {
        eval_expr(env, &args[2])
    } else {
        Ok(Value::Unit)
    }
}

fn eval_type_decl(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.is_empty() || args.len() > 3 {
        return Err(MagError::Eval(
            "type requires a name, optional generic parameters, and a body".into(),
        ));
    }
    let local = args[0]
        .as_symbol()
        .ok_or_else(|| MagError::Eval("type name must be a symbol".into()))?;
    let (params, body_expr) = match args {
        [_, Expr::Vector(ps), body] => (
            ps.iter()
                .map(|p| {
                    p.as_symbol()
                        .map(str::to_owned)
                        .ok_or_else(|| MagError::Type("generic parameters must be symbols".into()))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(body),
        ),
        [_, body] => (vec![], Some(body)),
        [_] => (vec![], None),
        _ => unreachable!(),
    };
    let vars = params.iter().cloned().collect();
    let body = body_expr
        .map(|x| parse_type(env, x, &vars))
        .transpose()?
        .unwrap_or_else(|| MagType::Record(BTreeMap::new()));
    let decl = TypeDecl {
        name: env.qualify(local),
        params,
        body,
    };
    let value = Value::TypeDecl(decl.clone());
    env.define(local, value.clone());
    Ok(value)
}

fn eval_foreign(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    if args.len() != 2 && args.len() != 3 {
        return Err(MagError::Eval(
            "foreign requires a name, optional generic binders, and a schema".into(),
        ));
    }
    let local = args[0]
        .as_symbol()
        .ok_or_else(|| MagError::Eval("foreign name must be a symbol".into()))?;
    let (binder_names, schema_expr) = if args.len() == 3 {
        let names = match &args[1] {
            Expr::Vector(xs) => xs
                .iter()
                .map(|x| {
                    x.as_symbol().map(str::to_owned).ok_or_else(|| {
                        MagError::Type("foreign generic parameters must be symbols".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(MagError::Type(
                    "foreign generic parameters must be a vector".into(),
                ))
            }
        };
        (names, &args[2])
    } else {
        (Vec::new(), &args[1])
    };
    let binders = binder_names.iter().cloned().collect::<HashSet<_>>();
    let schema = match schema_expr {
        Expr::Map(m) => m,
        _ => {
            return Err(MagError::Type(
                "foreign declaration requires a schema map".into(),
            ))
        }
    };
    let field = |name: &str| {
        schema
            .iter()
            .find(|(k, _)| matches!(k,Expr::Keyword(s)|Expr::Symbol(s) if s==name))
            .map(|(_, v)| v)
            .ok_or_else(|| MagError::Type(format!("foreign declaration missing :{name}")))
    };
    let name = if local.contains('.') {
        local.to_string()
    } else {
        env.qualify(local)
    };
    let decl = ForeignDecl {
        name,
        type_params: binder_names,
        specialization: vec![],
        params: parse_type(env, field("params")?, &binders)?,
        input: parse_type(env, field("input")?, &binders)?,
        output: parse_type(env, field("output")?, &binders)?,
    };
    env.register_foreign(&decl.name)?;
    let value = Value::Foreign(decl.clone());
    env.define(local, value.clone());
    env.define(&decl.name, value.clone());
    Ok(value)
}

pub(crate) fn parse_type(
    env: &Env,
    expr: &Expr,
    vars: &HashSet<String>,
) -> Result<MagType, MagError> {
    match expr {
        Expr::Symbol(name) if vars.contains(name) => Ok(MagType::Var(name.clone())),
        Expr::Symbol(name) => match env.lookup(name)? {
            Value::Type(t) => Ok(t.clone()),
            Value::TypeDecl(d) => Ok(MagType::Named(d.name.clone(), vec![])),
            other => Err(MagError::Type(format!(
                "{name} is {}, not a type",
                other.type_name()
            ))),
        },
        Expr::Map(fields) => {
            let mut out = BTreeMap::new();
            for (k, v) in fields {
                let name = match k {
                    Expr::Keyword(s) | Expr::Symbol(s) | Expr::Str(s) => s.clone(),
                    _ => {
                        return Err(MagError::Type(
                            "record field names must be symbols or keywords".into(),
                        ))
                    }
                };
                out.insert(name, parse_type(env, v, vars)?);
            }
            Ok(MagType::Record(out))
        }
        Expr::List(xs) if !xs.is_empty() => {
            let head = xs[0]
                .as_symbol()
                .ok_or_else(|| MagError::Type("type application head must be a symbol".into()))?;
            match head {
                "|" => Ok(MagType::Union(
                    xs[1..]
                        .iter()
                        .map(|x| parse_type(env, x, vars))
                        .collect::<Result<_, _>>()?,
                )),
                "+" => Ok(MagType::Product(
                    xs[1..]
                        .iter()
                        .map(|x| parse_type(env, x, vars))
                        .collect::<Result<_, _>>()?,
                )),
                "List" if xs.len() == 2 => {
                    Ok(MagType::List(Box::new(parse_type(env, &xs[1], vars)?)))
                }
                "Map" if xs.len() == 3 => Ok(MagType::Map(
                    Box::new(parse_type(env, &xs[1], vars)?),
                    Box::new(parse_type(env, &xs[2], vars)?),
                )),
                "TypeTag" if xs.len() == 2 => {
                    Ok(MagType::TypeTag(Box::new(parse_type(env, &xs[1], vars)?)))
                }
                "Fn" if xs.len() >= 2 => {
                    let types = xs[1..]
                        .iter()
                        .map(|x| parse_type(env, x, vars))
                        .collect::<Result<Vec<_>, _>>()?;
                    let (result, params) = types
                        .split_last()
                        .ok_or_else(|| MagError::Type("Fn requires a return type".into()))?;
                    Ok(MagType::Function(params.to_vec(), Box::new(result.clone())))
                }
                "Foreign" if xs.len() == 4 => Ok(MagType::Foreign(
                    Box::new(parse_type(env, &xs[1], vars)?),
                    Box::new(parse_type(env, &xs[2], vars)?),
                    Box::new(parse_type(env, &xs[3], vars)?),
                )),
                _ => {
                    let decl = match env.lookup(head)? {
                        Value::TypeDecl(d) => d,
                        _ => return Err(MagError::Type(format!("{head} is not a declared type"))),
                    };
                    if decl.params.len() != xs.len() - 1 {
                        return Err(MagError::Type(format!(
                            "{} expects {} type arguments, got {}",
                            decl.name,
                            decl.params.len(),
                            xs.len() - 1
                        )));
                    }
                    Ok(MagType::Named(
                        decl.name.clone(),
                        xs[1..]
                            .iter()
                            .map(|x| parse_type(env, x, vars))
                            .collect::<Result<_, _>>()?,
                    ))
                }
            }
        }
        _ => Err(MagError::Type("invalid type expression".into())),
    }
}

fn record_fields(env: &Env, ty: &MagType) -> Option<BTreeMap<String, MagType>> {
    match ty {
        MagType::Record(fields) => Some(fields.clone()),
        MagType::Named(name, args) => {
            let decl = env.type_decl(name)?;
            let substitutions = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = crate::checker::substitute(&decl.body, &substitutions);
            record_fields(env, &body)
        }
        _ => None,
    }
}

fn record_field_diff(env: &Env, value: &Value, ty: &MagType) -> Option<String> {
    let Value::Map(actual) = raw(value) else {
        return None;
    };
    let expected = record_fields(env, ty)?;
    let missing = expected
        .keys()
        .filter(|key| !actual.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    let unexpected = actual
        .keys()
        .filter(|key| !expected.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() && unexpected.is_empty() {
        return None;
    }
    let mut details = Vec::new();
    if !missing.is_empty() {
        details.push(format!("missing fields: {}", missing.join(", ")));
    }
    if !unexpected.is_empty() {
        details.push(format!("unexpected fields: {}", unexpected.join(", ")));
    }
    Some(details.join("; "))
}

fn validate_value(env: &Env, value: &Value, ty: &MagType) -> Result<(), MagError> {
    let original = value;
    let value = raw(value);
    let valid = match ty {
        MagType::Data => match value {
            Value::Unit | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_) => true,
            Value::List(items) | Value::Vector(items) => items
                .iter()
                .all(|item| validate_value(env, item, &MagType::Data).is_ok()),
            Value::Map(map) => map
                .values()
                .all(|item| validate_value(env, item, &MagType::Data).is_ok()),
            _ => false,
        },
        MagType::Artifact => matches!(value, Value::Artifact(_)),
        MagType::Unit => matches!(value, Value::Unit),
        MagType::Bool => matches!(value, Value::Bool(_)),
        MagType::Int => matches!(value, Value::Int(_)),
        MagType::Float => matches!(value, Value::Float(_)),
        MagType::String => matches!(value, Value::Str(_)),
        MagType::List(item) => match value {
            Value::List(xs) | Value::Vector(xs) => {
                xs.iter().all(|x| validate_value(env, x, item).is_ok())
            }
            _ => false,
        },
        MagType::EmptyList => {
            matches!(value, Value::List(items) | Value::Vector(items) if items.is_empty())
        }
        MagType::Map(_, item) => match value {
            Value::Map(map) => map.values().all(|x| validate_value(env, x, item).is_ok()),
            _ => false,
        },
        MagType::Record(fields) => match value {
            Value::Map(map) => {
                map.len() == fields.len()
                    && fields.iter().all(|(key, field)| {
                        map.get(key)
                            .is_some_and(|v| validate_value(env, v, field).is_ok())
                    })
            }
            _ => false,
        },
        MagType::Named(name, args) => env.type_decl(name).is_some_and(|decl| {
            let substitutions = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = crate::checker::substitute(&decl.body, &substitutions);
            validate_value(env, value, &body).is_ok()
        }),
        MagType::TypeTag(expected) => {
            matches!(value, Value::TypeTag(actual) if actual == expected.as_ref())
        }
        MagType::ForeignEvidence => matches!(value, Value::ForeignEvidence(_)),
        MagType::Union(types) => types.iter().any(|t| validate_value(env, value, t).is_ok()),
        MagType::Product(types) => types.iter().all(|t| validate_value(env, value, t).is_ok()),
        MagType::Function(_, _) => matches!(value, Value::Fn(_)),
        MagType::Foreign(_, _, _) => matches!(value, Value::Foreign(_)),
        MagType::Var(_) => true,
    };
    if valid {
        Ok(())
    } else if let Some(diff) = record_field_diff(env, original, ty) {
        Err(MagError::Type(format!(
            "value does not conform to {ty}: {diff}"
        )))
    } else {
        Err(MagError::Type(format!("value does not conform to {ty}")))
    }
}

fn apply(caller: &Env, f: &Value, args: &[Value]) -> Result<Value, MagError> {
    match f {
        Value::Fn(fun) => {
            let _call_depth = fuel::enter_call()?;
            if args.len() != fun.params.len() {
                return Err(MagError::Arity {
                    expected: fun.params.len(),
                    got: args.len(),
                });
            }
            let (expected_return, type_bindings) = crate::checker::check_call(caller, fun, args)
                .map_err(|error| match &fun.name {
                    Some(name) => MagError::Type(format!("calling {name}: {error}")),
                    None => error,
                })?;
            let mut env = caller.child_for_call();
            for (name, value) in caller.snapshot() {
                env.define(&name, value);
            }
            for (k, v) in &fun.closure {
                env.define(k, v.clone());
            }
            env.push_scope();
            for (p, v) in fun.params.iter().zip(args) {
                env.define(p, v.clone());
            }
            for (name, ty) in type_bindings {
                env.define(&name, Value::Type(ty));
            }
            let mut out = Value::Unit;
            for expr in &fun.body {
                out = eval_expr(&mut env, expr)?;
            }
            validate_value(caller, &out, &expected_return)?;
            Ok(Value::Typed(Box::new(out), expected_return))
        }
        Value::BuiltinFn(name) => builtin(caller, name, args),
        _ => Err(MagError::Eval(format!("cannot call {}", f.type_name()))),
    }
}

pub fn apply_named(env: &Env, name: &str, arg: Value) -> Result<Value, MagError> {
    let f = env.lookup(name)?.clone();
    apply(env, &f, &[arg])
}

fn builtin(env: &Env, name: &str, args: &[Value]) -> Result<Value, MagError> {
    match name {
        "artifact" => {
            arity(args, 2)?;
            let format = args[0].as_str().ok_or_else(|| {
                MagError::Eval("artifact format must be a qualified string".into())
            })?;
            if !format.contains('.') && !format.contains('/') {
                return Err(MagError::Eval("artifact format must be qualified".into()));
            }
            Ok(Value::Artifact(Artifact {
                format: format.into(),
                data: crate::json::value_to_json(&args[1])?,
            }))
        }
        "str" => Ok(Value::Str(
            args.iter().map(value_string).collect::<Vec<_>>().join(""),
        )),
        "count" => {
            arity(args, 1)?;
            let n = match raw(&args[0]) {
                Value::List(v) | Value::Vector(v) => v.len(),
                Value::Map(v) => v.len(),
                Value::Str(v) => v.chars().count(),
                _ => return Err(MagError::Eval("count expects a collection".into())),
            };
            Ok(Value::Int(n as i64))
        }
        "get" => {
            arity(args, 2)?;
            let key = value_string(&args[1]);
            match raw(&args[0]) {
                Value::Map(m) => Ok(m
                    .get(key.trim_start_matches(':'))
                    .cloned()
                    .unwrap_or(Value::Unit)),
                _ => Err(MagError::Eval("get expects a map".into())),
            }
        }
        "assoc" => {
            arity(args, 3)?;
            let mut m = match raw(&args[0]) {
                Value::Map(m) => m.clone(),
                _ => return Err(MagError::Eval("assoc expects a map".into())),
            };
            m.insert(
                value_string(&args[1]).trim_start_matches(':').into(),
                args[2].clone(),
            );
            Ok(Value::Map(m))
        }
        "keys" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Map(m) => Ok(Value::Vector(m.keys().cloned().map(Value::Str).collect())),
                _ => Err(MagError::Eval("keys expects a map".into())),
            }
        }
        "concat" => {
            arity(args, 2)?;
            match (raw(&args[0]), raw(&args[1])) {
                (Value::Str(a), Value::Str(b)) => Ok(Value::Str(format!("{a}{b}"))),
                (Value::Vector(a), Value::Vector(b)) => {
                    Ok(Value::Vector(a.iter().chain(b).cloned().collect()))
                }
                (Value::List(a), Value::List(b)) => {
                    Ok(Value::List(a.iter().chain(b).cloned().collect()))
                }
                _ => Err(MagError::Eval(
                    "concat expects matching strings or collections".into(),
                )),
            }
        }
        "=" => {
            arity(args, 2)?;
            Ok(Value::Bool(equal(&args[0], &args[1])))
        }
        "fail" => {
            arity(args, 1)?;
            let diagnostic = crate::json::value_to_json(&args[0])?;
            Err(MagError::Eval(format!("validation failed: {diagnostic}")))
        }
        "foreign-id" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Foreign(decl) => Ok(Value::Str(decl.name.clone())),
                _ => Err(MagError::Type(
                    "foreign-id expects a Foreign capability".into(),
                )),
            }
        }
        "foreign-evidence" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Foreign(decl) => Ok(Value::ForeignEvidence(crate::ast::ForeignEvidence {
                    identity: decl.name.clone(),
                    arguments: decl.specialization.clone(),
                    input: decl.input.clone(),
                    output: decl.output.clone(),
                })),
                _ => Err(MagError::Type(
                    "foreign-evidence expects a Foreign capability".into(),
                )),
            }
        }
        "type-evidence" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::TypeTag(ty) => Ok(crate::json::json_to_value(
                    &crate::json::type_evidence_to_json(ty)?,
                )),
                other => Err(MagError::Type(format!(
                    "type-evidence expects TypeTag, got {}",
                    other.type_name()
                ))),
            }
        }
        "type-schema" => {
            arity(args, 1)?;
            let ty = match raw(&args[0]) {
                Value::TypeTag(ty) => ty,
                other => {
                    return Err(MagError::Type(format!(
                        "type-schema expects TypeTag, got {}",
                        other.type_name()
                    )))
                }
            };
            let schema = crate::schema::TypeSchema::reify(env, ty)?;
            let json = serde_json::to_value(schema)
                .map_err(|error| MagError::Eval(format!("serialize type schema: {error}")))?;
            Ok(crate::json::json_to_value(&json))
        }
        "not" => {
            arity(args, 1)?;
            Ok(Value::Bool(!truthy(&args[0])))
        }
        "or" => {
            arity(args, 2)?;
            Ok(if truthy(&args[0]) {
                args[0].clone()
            } else {
                args[1].clone()
            })
        }
        "map" | "indexed-map" | "filter" | "flat-map" | "fold" => {
            collection_builtin(env, name, args)
        }
        "read" => {
            if args.is_empty() || args.len() > 2 {
                return Err(MagError::Eval(
                    "read requires path and optional interpolation map".into(),
                ));
            }
            let path = args[0]
                .as_str()
                .ok_or_else(|| MagError::Eval("read path must be a string".into()))?;
            let full = resolve_workspace_path(env.source_dir(), path)?;
            let mut s = std::fs::read_to_string(full)
                .map_err(|e| MagError::Eval(format!("cannot read {path}: {e}")))?;
            if let Some(Value::Map(m)) = args.get(1) {
                for (k, v) in m {
                    s = s.replace(&format!("{{{{{k}}}}}"), &value_string(v));
                }
            }
            Ok(Value::Str(s))
        }
        "require" => Err(MagError::Eval("require is a special form".into())),
        _ => Err(MagError::Eval(format!("unknown builtin {name}"))),
    }
}

fn collection_builtin(env: &Env, name: &str, args: &[Value]) -> Result<Value, MagError> {
    let seq = |v: &Value| match raw(v) {
        Value::List(v) | Value::Vector(v) => Ok(v.clone()),
        _ => Err(MagError::Eval(format!("{name} expects a collection"))),
    };
    match name {
        "map" => {
            arity(args, 2)?;
            Ok(Value::Vector(
                seq(&args[1])?
                    .iter()
                    .map(|v| apply(env, &args[0], std::slice::from_ref(v)))
                    .collect::<Result<_, _>>()?,
            ))
        }
        "indexed-map" => {
            arity(args, 2)?;
            Ok(Value::Vector(
                seq(&args[1])?
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| apply(env, &args[0], &[Value::Int(index as i64), value]))
                    .collect::<Result<_, _>>()?,
            ))
        }
        "filter" => {
            arity(args, 2)?;
            let mut out = vec![];
            for v in seq(&args[1])? {
                if truthy(&apply(env, &args[0], std::slice::from_ref(&v))?) {
                    out.push(v)
                }
            }
            Ok(Value::Vector(out))
        }
        "flat-map" => {
            arity(args, 2)?;
            let mut out = vec![];
            for v in seq(&args[1])? {
                out.extend(seq(&apply(env, &args[0], &[v])?)?)
            }
            Ok(Value::Vector(out))
        }
        "fold" => {
            arity(args, 3)?;
            let mut acc = args[1].clone();
            for v in seq(&args[2])? {
                acc = apply(env, &args[0], &[acc, v])?;
            }
            Ok(acc)
        }
        _ => unreachable!(),
    }
}

fn arity<T>(args: &[T], expected: usize) -> Result<(), MagError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(MagError::Arity {
            expected,
            got: args.len(),
        })
    }
}
fn truthy(v: &Value) -> bool {
    !matches!(raw(v), Value::Unit | Value::Bool(false))
}
fn value_string(v: &Value) -> String {
    match raw(v) {
        Value::Unit => "()".into(),
        Value::Str(s) | Value::Symbol(s) => s.clone(),
        Value::Keyword(s) => format!(":{s}"),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Bool(n) => n.to_string(),
        Value::Type(t) => t.to_string(),
        Value::TypeDecl(d) => d.name.clone(),
        Value::TypeTag(t) => t.to_string(),
        Value::Foreign(d) => d.name.clone(),
        _ => format!("<{:?}>", v.type_name()),
    }
}
fn equal(a: &Value, b: &Value) -> bool {
    match (raw(a), raw(b)) {
        (Value::Unit, Value::Unit) => true,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Keyword(a), Value::Keyword(b)) => a == b,
        (Value::Symbol(a), Value::Symbol(b)) => a == b,
        (Value::List(a), Value::List(b)) | (Value::Vector(a), Value::Vector(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(left, right)| equal(left, right))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| equal(value, other)))
        }
        _ => false,
    }
}

fn raw(value: &Value) -> &Value {
    match value {
        Value::Typed(inner, _) => raw(inner),
        other => other,
    }
}

fn module_path(name: &str) -> Result<String, MagError> {
    if name.split('.').any(|p| {
        p.is_empty()
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return Err(MagError::Eval(format!("invalid module name: {name}")));
    }
    Ok(format!("{}.mag", name.replace('.', "/")))
}

fn eval_require(env: &mut Env, name: &str) -> Result<Value, MagError> {
    if let Some(defs) = env.module_cached(name) {
        env.install_module(name, defs.clone());
        return Ok(Value::Map(defs));
    }
    env.begin_module(name)?;
    let relative = module_path(name)?;
    let mut matches = env
        .module_roots()
        .iter()
        .filter_map(|root| {
            let path = resolve_workspace_path(root, &relative).ok()?;
            path.is_file().then(|| path.canonicalize().unwrap_or(path))
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    let path = match matches.as_slice() {
        [path] => path.clone(),
        [] => {
            return Err(MagError::Eval(format!(
                "cannot find module {name} in search roots"
            )))
        }
        paths => {
            return Err(MagError::Eval(format!(
                "module {name} is ambiguous across search roots: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| MagError::Eval(format!("cannot read module {name}: {e}")))?;
    let exprs = crate::parser::parse(&crate::lexer::tokenize(&content)?)?;
    let mut module = env.module_env(name);
    let result = eval_program(&mut module, &exprs);
    match result {
        Ok(_) => {
            let defs = module.user_defs();
            env.finish_module(name, defs.clone());
            for (module_name, module_defs) in env.loaded_modules() {
                env.install_module(&module_name, module_defs);
            }
            Ok(Value::Map(defs))
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn resolve_workspace_path(root: &Path, relative: &str) -> Result<PathBuf, MagError> {
    let p = Path::new(relative);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(MagError::Eval(format!(
            "path escapes workspace: {relative}"
        )));
    }
    let joined = root.join(p);
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.into());
    if let Some(parent) = joined.parent() {
        let canonical_parent = parent.canonicalize().unwrap_or_else(|_| parent.into());
        if !canonical_parent.starts_with(canonical_root) {
            return Err(MagError::Eval(format!(
                "path escapes workspace: {relative}"
            )));
        }
    }
    Ok(joined)
}

// `require` needs its raw module name rather than an evaluated module symbol.
fn maybe_require(env: &mut Env, items: &[Expr]) -> Option<Result<Value, MagError>> {
    if matches!(items.first(),Some(Expr::Symbol(s)) if s=="require") {
        Some(if items.len() != 2 {
            Err(MagError::Arity {
                expected: 1,
                got: items.len() - 1,
            })
        } else {
            match &items[1] {
                Expr::Str(s) => eval_require(env, s),
                _ => Err(MagError::Eval("require expects a module string".into())),
            }
        })
    } else {
        None
    }
}
