use crate::ast::{Artifact, Expr, FnValue, ForeignDecl, TypeDecl, Value};
use crate::env::Env;
use crate::error::MagError;
use crate::types::{ConcreteType, MagType};
use std::collections::{BTreeMap, BTreeSet, HashSet};
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

    #[cfg(test)]
    pub fn remaining() -> Option<u64> {
        REMAINING.with(Cell::get)
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
        Expr::Vector(xs) => Ok(Value::Vector(std::sync::Arc::new(
            xs.iter()
                .map(|x| eval_expr(env, x))
                .collect::<Result<_, _>>()?,
        ))),
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
    Ok(Value::Map(std::sync::Arc::new(map)))
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
    checked_typed_value(env, value, ty)
}

fn eval_type_tag(env: &mut Env, args: &[Expr]) -> Result<Value, MagError> {
    arity(args, 1)?;
    let ty = parse_type(env, &args[0], &HashSet::new())?;
    Ok(Value::TypeTag(crate::types::ConcreteType::resolve(
        env, &ty,
    )?))
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
    for ty in &types {
        crate::types::ConcreteType::resolve(env, ty)?;
    }
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

fn checked_typed_value(env: &Env, value: Value, ty: MagType) -> Result<Value, MagError> {
    if let MagType::Product(components) = &ty {
        let values = match raw(&value) {
            Value::List(values) | Value::Vector(values) | Value::Product(values) => values,
            _ => {
                return Err(MagError::Type(format!(
                    "value does not conform to {ty}: expected an ordered tuple"
                )))
            }
        };
        if values.len() != components.len() {
            return Err(MagError::Type(format!(
                "value does not conform to {ty}: expected {} tuple positions, got {}",
                components.len(),
                values.len()
            )));
        }
        let positions = values
            .iter()
            .cloned()
            .zip(components)
            .map(|(position, component)| checked_typed_value(env, position, component.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Product(std::sync::Arc::new(positions)));
    }

    validate_value(env, &value, &ty)?;
    let Ok(accepted) = crate::types::ConcreteType::resolve(env, &ty) else {
        return Ok(Value::Typed(std::sync::Arc::new(value), ty));
    };
    let selected = explicit_constructor(env, &value)?;
    match (&accepted, selected) {
        (crate::types::ConcreteType::Sum { .. }, Some(constructor))
            if accepted.accepts(&constructor) => {}
        (crate::types::ConcreteType::Sum { .. }, Some(constructor)) => {
            return Err(MagError::Type(format!(
                "constructor {constructor:?} is not accepted by {accepted:?}"
            )));
        }
        (crate::types::ConcreteType::Sum { .. }, None) => {
            return Err(MagError::Type(format!(
                "value has no explicit constructor evidence accepted by {accepted:?}"
            )));
        }
        (crate::types::ConcreteType::Named { .. }, Some(constructor))
            if constructor != accepted =>
        {
            return Err(MagError::Type(format!(
                "cannot replace constructor evidence {constructor:?} with {accepted:?}"
            )));
        }
        _ => {}
    }
    Ok(Value::Typed(std::sync::Arc::new(value), ty))
}

fn explicit_constructor(
    env: &Env,
    value: &Value,
) -> Result<Option<crate::types::ConcreteType>, MagError> {
    let mut current = value;
    while let Value::Typed(inner, evidence) = current {
        match crate::types::ConcreteType::resolve(env, evidence)? {
            constructor @ crate::types::ConcreteType::Named { .. } => {
                return Ok(Some(constructor));
            }
            crate::types::ConcreteType::Sum { .. } => current = inner,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn validate_value(env: &Env, value: &Value, ty: &MagType) -> Result<(), MagError> {
    let original = value;
    let value = raw(value);
    let valid = match ty {
        MagType::Artifact => matches!(value, Value::Artifact(_)),
        MagType::JsonValue => matches!(value, Value::JsonValue(_)),
        MagType::TypeDescriptor => matches!(value, Value::TypeDescriptor(_)),
        MagType::TypeSchema => matches!(value, Value::TypeSchema(_)),
        MagType::SemanticTypeId => matches!(value, Value::SemanticTypeId(_)),
        MagType::PackedValue => matches!(value, Value::PackedValue(_)),
        MagType::HostInputs => matches!(value, Value::HostInputs(_)),
        MagType::Never => false,
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
        MagType::TypeTag(expected) => matches!(
            value,
            Value::TypeTag(actual)
                if crate::types::ConcreteType::resolve(env, expected)
                    .is_ok_and(|expected| actual == &expected)
        ),
        MagType::ForeignEvidence => matches!(value, Value::ForeignEvidence(_)),
        MagType::Union(types) => types.iter().any(|t| validate_value(env, value, t).is_ok()),
        MagType::Product(types) => match value {
            Value::List(values) | Value::Vector(values) | Value::Product(values) => {
                values.len() == types.len()
                    && values
                        .iter()
                        .zip(types)
                        .all(|(position, ty)| validate_value(env, position, ty).is_ok())
            }
            _ => false,
        },
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
            if let Some(result) = caller.memoized_call(fun, args) {
                return Ok(result);
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
            if let Some(name) = &fun.name {
                env.define(name, Value::Fn(fun.clone()));
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
            let result = checked_typed_value(caller, out, expected_return)?;
            caller.memoize_call(fun, args, &result);
            Ok(result)
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
                data: crate::json::value_to_json(env, &args[1])?,
            }))
        }
        "str" => Ok(Value::Str(
            args.iter().map(value_string).collect::<Vec<_>>().join(""),
        )),
        "canonical" => {
            arity(args, 1)?;
            let json = canonical_json(crate::json::value_to_json(env, &args[0])?);
            serde_json::to_string(&json)
                .map(Value::Str)
                .map_err(|error| MagError::Eval(format!("canonical serialization failed: {error}")))
        }
        "conforms?" => {
            arity(args, 2)?;
            let ty = match raw(&args[1]) {
                Value::TypeDescriptor(ty) => ty.to_mag_type(),
                _ => return Err(MagError::Type("conforms? expects a TypeDescriptor".into())),
            };
            let Ok(schema) = crate::schema::TypeSchema::reify(env, &ty) else {
                return Ok(Value::Bool(false));
            };
            let value = crate::json::value_to_json(env, &args[0])?;
            let encoded = serde_json::to_string(&value)
                .map_err(|error| MagError::Eval(format!("serialize conformance value: {error}")))?;
            Ok(Value::Bool(schema.validate_json(&encoded).ok))
        }
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
        "remove-at" => {
            arity(args, 2)?;
            let mut values = match raw(&args[0]) {
                Value::List(values) | Value::Vector(values) => values.as_ref().clone(),
                _ => return Err(MagError::Eval("remove-at expects List".into())),
            };
            let index = match raw(&args[1]) {
                Value::Int(index) if *index >= 0 => *index as usize,
                _ => {
                    return Err(MagError::Eval(
                        "remove-at index must be non-negative Int".into(),
                    ))
                }
            };
            if index >= values.len() {
                return Err(MagError::Eval(format!(
                    "remove-at index {index} is out of bounds for {} values",
                    values.len()
                )));
            }
            values.remove(index);
            Ok(Value::Vector(std::sync::Arc::new(values)))
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
                Value::Map(m) => m.as_ref().clone(),
                _ => return Err(MagError::Eval("assoc expects a map".into())),
            };
            m.insert(
                value_string(&args[1]).trim_start_matches(':').into(),
                args[2].clone(),
            );
            Ok(Value::Map(std::sync::Arc::new(m)))
        }
        "keys" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Map(m) => Ok(Value::Vector(std::sync::Arc::new(
                    m.keys().cloned().map(Value::Str).collect(),
                ))),
                _ => Err(MagError::Eval("keys expects a map".into())),
            }
        }
        "first" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::List(values) | Value::Vector(values) => values
                    .first()
                    .cloned()
                    .ok_or_else(|| MagError::Eval("first expects a non-empty List".into())),
                _ => Err(MagError::Eval("first expects a List".into())),
            }
        }
        "concat" => {
            arity(args, 2)?;
            match (raw(&args[0]), raw(&args[1])) {
                (Value::Str(a), Value::Str(b)) => Ok(Value::Str(format!("{a}{b}"))),
                (Value::Vector(a), Value::Vector(b)) => Ok(Value::Vector(std::sync::Arc::new(
                    a.iter().chain(b.iter()).cloned().collect(),
                ))),
                (Value::List(a), Value::List(b)) => Ok(Value::List(std::sync::Arc::new(
                    a.iter().chain(b.iter()).cloned().collect(),
                ))),
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
            let diagnostic = crate::json::value_to_json(env, &args[0])?;
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
                    arguments: decl
                        .specialization
                        .iter()
                        .map(|ty| crate::types::ConcreteType::resolve(env, ty))
                        .collect::<Result<_, _>>()?,
                    input: crate::types::ConcreteType::resolve(env, &decl.input)?,
                    output: crate::types::ConcreteType::resolve(env, &decl.output)?,
                })),
                _ => Err(MagError::Type(
                    "foreign-evidence expects a Foreign capability".into(),
                )),
            }
        }
        "type-evidence" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::TypeTag(ty) => Ok(Value::TypeDescriptor(ty.clone())),
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
            let schema = crate::schema::TypeSchema::reify(env, &ty.to_mag_type())?;
            Ok(Value::TypeSchema(schema))
        }
        "type-id" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type("type-id expects a TypeDescriptor".into()));
            };
            Ok(Value::SemanticTypeId(ty.stable_id()))
        }
        "value-type-id" => {
            arity(args, 2)?;
            let declared = declared_value_type(&args[1])?;
            let selected = selected_value_type(env, &args[0], declared)?;
            Ok(Value::SemanticTypeId(selected.stable_id()))
        }
        "value-type-evidence" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(declared) = raw(&args[1]) else {
                return Err(MagError::Type(
                    "value-type-evidence expects a TypeDescriptor".into(),
                ));
            };
            Ok(Value::TypeDescriptor(selected_value_type(
                env, &args[0], declared,
            )?))
        }
        "pack" => {
            arity(args, 1)?;
            Ok(Value::PackedValue(std::sync::Arc::new(args[0].clone())))
        }
        "packed-empty-record?" => {
            arity(args, 1)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed-empty-record? expects PackedValue".into(),
                ));
            };
            Ok(Value::Bool(
                matches!(raw(value), Value::Map(fields) if fields.is_empty()),
            ))
        }
        "packed-record-has-only-key?" => {
            arity(args, 2)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed-record-has-only-key? expects PackedValue".into(),
                ));
            };
            let key = args[1]
                .as_str()
                .ok_or_else(|| MagError::Type("packed record key must be String".into()))?;
            Ok(Value::Bool(matches!(
                raw(value),
                Value::Map(fields) if fields.len() == 1 && fields.contains_key(key)
            )))
        }
        "packed-record-has-only-keys?" => {
            arity(args, 2)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed-record-has-only-keys? expects PackedValue".into(),
                ));
            };
            let values = match raw(&args[1]) {
                Value::List(values) | Value::Vector(values) => values,
                _ => {
                    return Err(MagError::Type(
                        "packed-record-has-only-keys? expects a String list".into(),
                    ))
                }
            };
            let keys = values
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        MagError::Type("packed-record-has-only-keys? expects a String list".into())
                    })
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            Ok(Value::Bool(matches!(
                raw(value),
                Value::Map(fields)
                    if fields.len() == keys.len()
                        && fields.keys().all(|key| keys.contains(key.as_str()))
            )))
        }
        "packed-field-conforms?" => {
            arity(args, 3)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed-field-conforms? expects PackedValue".into(),
                ));
            };
            let key = args[1]
                .as_str()
                .ok_or_else(|| MagError::Type("packed record key must be String".into()))?;
            let Value::TypeDescriptor(ty) = raw(&args[2]) else {
                return Err(MagError::Type(
                    "packed-field-conforms? expects TypeDescriptor".into(),
                ));
            };
            let valid = match raw(value) {
                Value::Map(fields) => fields
                    .get(key)
                    .is_some_and(|field| validate_value(env, field, &ty.to_mag_type()).is_ok()),
                _ => false,
            };
            Ok(Value::Bool(valid))
        }
        "descriptor-accepts?" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor-accepts? expects TypeDescriptor arguments".into(),
                ));
            };
            let Value::TypeDescriptor(source) = raw(&args[1]) else {
                return Err(MagError::Type(
                    "descriptor-accepts? expects TypeDescriptor arguments".into(),
                ));
            };
            Ok(Value::Bool(target.accepts_edge_source(source)))
        }
        "descriptor-input-covered-by?" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor-input-covered-by? expects a TypeDescriptor target".into(),
                ));
            };
            let sources = match raw(&args[1]) {
                Value::List(sources) | Value::Vector(sources) => sources,
                _ => {
                    return Err(MagError::Type(
                        "descriptor-input-covered-by? expects a descriptor list".into(),
                    ))
                }
            };
            let sources = sources
                .iter()
                .map(|source| match raw(source) {
                    Value::TypeDescriptor(source) => Ok(source.clone()),
                    _ => Err(MagError::Type(
                        "descriptor-input-covered-by? expects a descriptor list".into(),
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Bool(target.input_is_covered_by(&sources)))
        }
        "descriptor-input-assignments" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor-input-assignments expects a TypeDescriptor target".into(),
                ));
            };
            let sources = descriptor_list(
                &args[1],
                "descriptor-input-assignments expects a descriptor list",
            )?;
            target
                .assign_input_sources(&sources)
                .map(|assignments| {
                    Value::List(std::sync::Arc::new(
                        assignments
                            .into_iter()
                            .map(|position| {
                                Value::Int(position.map_or(-1, |position| position as i64))
                            })
                            .collect(),
                    ))
                })
                .map_err(|error| MagError::Type(error.to_string()))
        }
        "descriptor-output-covered-by?" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor-output-covered-by? expects a TypeDescriptor target".into(),
                ));
            };
            let handlers = descriptor_list(
                &args[1],
                "descriptor-output-covered-by? expects a descriptor list",
            )?;
            Ok(Value::Bool(target.output_is_covered_by(&handlers)))
        }
        "descriptor-table" => {
            arity(args, 1)?;
            let descriptors =
                descriptor_list(&args[0], "descriptor-table expects a descriptor list")?;
            let mut declarations = BTreeMap::new();
            for descriptor in descriptors {
                for (id, declaration) in descriptor.declarations()? {
                    if let Some(existing) = declarations.insert(id.clone(), declaration.clone()) {
                        if existing != declaration {
                            return Err(MagError::Type(format!(
                                "semantic type identity collision at {id}"
                            )));
                        }
                    }
                }
            }
            Ok(Value::Map(std::sync::Arc::new(
                declarations
                    .into_iter()
                    .map(|(id, descriptor)| (id, Value::TypeDescriptor(descriptor)))
                    .collect(),
            )))
        }
        "foreign-contracts" => {
            arity(args, 0)?;
            let Value::HostInputs(inputs) = raw(env.lookup("inputs")?) else {
                return Err(MagError::Type(
                    "foreign-contracts requires compiler host inputs".into(),
                ));
            };
            let contracts = inputs
                .get("foreign_contracts")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    MagError::Type("host inputs need a foreign_contracts list".into())
                })?;
            let projected = contracts
                .iter()
                .map(|contract| {
                    let identity = contract
                        .get("identity")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            MagError::Type("foreign contract needs string identity".into())
                        })?;
                    let scheme = contract.get("type_scheme").ok_or_else(|| {
                        MagError::Type("foreign contract needs type_scheme".into())
                    })?;
                    let input_tags = scheme
                        .get("input_tags")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| {
                            MagError::Type(
                                "foreign contract type_scheme needs input_tags".into(),
                            )
                        })?;
                    let outputs = scheme
                        .get("outputs")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| {
                            MagError::Type("foreign contract type_scheme needs outputs".into())
                        })?;
                    Ok(Value::Map(std::sync::Arc::new(
                        [
                            ("identity".into(), Value::Str(identity.into())),
                            (
                                "type_scheme".into(),
                                Value::Map(std::sync::Arc::new(
                                    [
                                        (
                                            "input_tags".into(),
                                            Value::Vector(std::sync::Arc::new(
                                                input_tags
                                                    .iter()
                                                    .map(|tag| {
                                                        tag.as_str()
                                                            .map(|tag| Value::Str(tag.into()))
                                                            .ok_or_else(|| {
                                                                MagError::Type(
                                                                    "foreign input tag must be String"
                                                                        .into(),
                                                                )
                                                            })
                                                    })
                                                    .collect::<Result<_, _>>()?,
                                            )),
                                        ),
                                        (
                                            "outputs".into(),
                                            Value::Vector(std::sync::Arc::new(
                                                outputs
                                                    .iter()
                                                    .map(|tag| {
                                                        tag.as_str()
                                                            .map(|tag| Value::Str(tag.into()))
                                                            .ok_or_else(|| {
                                                                MagError::Type(
                                                                    "foreign output tag must be String"
                                                                        .into(),
                                                                )
                                                            })
                                                    })
                                                    .collect::<Result<_, _>>()?,
                                            )),
                                        ),
                                    ]
                                    .into_iter()
                                    .collect(),
                                )),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    )))
                })
                .collect::<Result<Vec<_>, MagError>>()?;
            Ok(Value::Vector(std::sync::Arc::new(projected)))
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
        "map" | "indexed-map" | "filter" | "flat-map" | "fold" | "sort-by" => {
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
            let mut s = env.read_file(&full, path)?;
            if let Some(Value::Map(m)) = args.get(1) {
                for (k, v) in m.iter() {
                    s = s.replace(&format!("{{{{{k}}}}}"), &value_string(v));
                }
            }
            Ok(Value::Str(s))
        }
        "read-json" => {
            arity(args, 1)?;
            let path = args[0]
                .as_str()
                .ok_or_else(|| MagError::Eval("read-json path must be a string".into()))?;
            let mut matches = std::iter::once(env.source_dir())
                .chain(env.module_roots().iter().map(PathBuf::as_path))
                .filter_map(|root| {
                    let candidate = resolve_workspace_path(root, path).ok()?;
                    candidate
                        .is_file()
                        .then(|| candidate.canonicalize().unwrap_or(candidate))
                })
                .collect::<Vec<_>>();
            matches.sort();
            matches.dedup();
            let full = match matches.as_slice() {
                [path] => path,
                [] => {
                    return Err(MagError::Eval(format!(
                        "cannot find JSON data {path} in source or module roots"
                    )))
                }
                paths => {
                    return Err(MagError::Eval(format!(
                        "JSON data {path} is ambiguous across source and module roots: {}",
                        paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                }
            };
            let source = env.read_file(full, path)?;
            let value = serde_json::from_str(&source)
                .map_err(|error| MagError::Eval(format!("cannot parse JSON {path}: {error}")))?;
            Ok(crate::json::json_to_value(&value))
        }
        "require" => Err(MagError::Eval("require is a special form".into())),
        _ => Err(MagError::Eval(format!("unknown builtin {name}"))),
    }
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(fields) => {
            let sorted = fields
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
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
            Ok(Value::Vector(std::sync::Arc::new(
                seq(&args[1])?
                    .iter()
                    .map(|v| apply(env, &args[0], std::slice::from_ref(v)))
                    .collect::<Result<_, _>>()?,
            )))
        }
        "indexed-map" => {
            arity(args, 2)?;
            Ok(Value::Vector(std::sync::Arc::new(
                seq(&args[1])?
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(index, value)| apply(env, &args[0], &[Value::Int(index as i64), value]))
                    .collect::<Result<_, _>>()?,
            )))
        }
        "filter" => {
            arity(args, 2)?;
            let mut out = vec![];
            for v in seq(&args[1])?.iter().cloned() {
                if truthy(&apply(env, &args[0], std::slice::from_ref(&v))?) {
                    out.push(v)
                }
            }
            Ok(Value::Vector(std::sync::Arc::new(out)))
        }
        "flat-map" => {
            arity(args, 2)?;
            let mut out = vec![];
            for v in seq(&args[1])?.iter().cloned() {
                out.extend(seq(&apply(env, &args[0], &[v])?)?.iter().cloned())
            }
            Ok(Value::Vector(std::sync::Arc::new(out)))
        }
        "fold" => {
            arity(args, 3)?;
            let mut acc = args[1].clone();
            for v in seq(&args[2])?.iter().cloned() {
                acc = apply(env, &args[0], &[acc, v])?;
            }
            Ok(acc)
        }
        "sort-by" => {
            arity(args, 2)?;
            let mut keyed = seq(&args[1])?
                .iter()
                .cloned()
                .map(|value| {
                    let key = apply(env, &args[0], std::slice::from_ref(&value))?;
                    let key = key
                        .as_str()
                        .ok_or_else(|| {
                            MagError::Eval("sort-by callback must return String".into())
                        })?
                        .to_owned();
                    Ok((key, value))
                })
                .collect::<Result<Vec<_>, MagError>>()?;
            keyed.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(Value::Vector(std::sync::Arc::new(
                keyed.into_iter().map(|(_, value)| value).collect(),
            )))
        }
        _ => unreachable!(),
    }
}

fn descriptor_list(value: &Value, error: &str) -> Result<Vec<ConcreteType>, MagError> {
    let values = match raw(value) {
        Value::List(values) | Value::Vector(values) => values,
        _ => return Err(MagError::Type(error.into())),
    };
    values
        .iter()
        .map(|value| match raw(value) {
            Value::TypeDescriptor(descriptor) => Ok(descriptor.clone()),
            _ => Err(MagError::Type(error.into())),
        })
        .collect()
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
        Value::TypeTag(t) => t.to_mag_type().to_string(),
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
        (Value::List(a), Value::List(b))
        | (Value::Vector(a), Value::Vector(b))
        | (Value::Product(a), Value::Product(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(left, right)| equal(left, right))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| equal(value, other)))
        }
        (Value::Type(a), Value::Type(b)) => a == b,
        (Value::TypeTag(a), Value::TypeTag(b)) => a == b,
        (Value::TypeDecl(a), Value::TypeDecl(b)) => a == b,
        (Value::Foreign(a), Value::Foreign(b)) => a == b,
        (Value::ForeignEvidence(a), Value::ForeignEvidence(b)) => a == b,
        (Value::TypeDescriptor(a), Value::TypeDescriptor(b)) => a == b,
        (Value::TypeSchema(a), Value::TypeSchema(b)) => a == b,
        (Value::SemanticTypeId(a), Value::SemanticTypeId(b)) => a == b,
        (Value::PackedValue(a), Value::PackedValue(b)) => equal(a, b),
        (Value::JsonValue(a), Value::JsonValue(b)) => a == b,
        (Value::HostInputs(a), Value::HostInputs(b)) => a == b,
        (Value::Artifact(a), Value::Artifact(b)) => a == b,
        _ => false,
    }
}

fn raw(value: &Value) -> &Value {
    match value {
        Value::Typed(inner, _) => raw(inner),
        other => other,
    }
}

fn declared_value_type(value: &Value) -> Result<&ConcreteType, MagError> {
    match raw(value) {
        Value::TypeTag(declared) | Value::TypeDescriptor(declared) => Ok(declared),
        _ => Err(MagError::Type(
            "value type evidence must be TypeTag or TypeDescriptor".into(),
        )),
    }
}

fn selected_value_type(
    env: &Env,
    value: &Value,
    declared: &ConcreteType,
) -> Result<ConcreteType, MagError> {
    if matches!(declared, ConcreteType::Sum { .. }) {
        explicit_constructor(env, value)?
            .ok_or_else(|| MagError::Type("sum value lacks selected constructor evidence".into()))
    } else {
        Ok(declared.clone())
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
        return Ok(Value::Map(std::sync::Arc::new(defs)));
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
            Ok(Value::Map(std::sync::Arc::new(defs)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_call_reuses_the_cached_shared_result_without_spending_fuel() {
        let source = r#"
            (def values [1 2 3])
            (def copy (fn [[items (List Int)]] -> (List Int)
              (map (fn [[item Int]] -> Int item) items)))
            (artifact "test.memo/v1" {})
        "#;
        let expressions = crate::parser::parse(&crate::lexer::tokenize(source).unwrap()).unwrap();
        let mut env = Env::new();
        let _fuel = fuel::install(1_000);
        eval_program(&mut env, &expressions).unwrap();
        let argument = env.lookup("values").unwrap().clone();

        let first = apply_named(&env, "copy", argument.clone()).unwrap();
        let after_first = fuel::remaining().unwrap();
        let second = apply_named(&env, "copy", argument).unwrap();

        assert_eq!(fuel::remaining(), Some(after_first));
        match (first, second) {
            (Value::Typed(left, _), Value::Typed(right, _)) => {
                assert!(std::sync::Arc::ptr_eq(&left, &right));
            }
            values => panic!("expected typed results, got {values:?}"),
        }
    }
}
