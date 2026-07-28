use crate::ast::{Expr, Value};
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use std::collections::{BTreeMap, HashMap, HashSet};

pub fn check_function_with_binding(
    env: &Env,
    binding: Option<&str>,
    params: &[String],
    param_types: &[MagType],
    result: &MagType,
    body: &[Expr],
) -> Result<(), MagError> {
    let mut locals = params
        .iter()
        .cloned()
        .zip(param_types.iter().cloned())
        .collect::<HashMap<_, _>>();
    if let Some(name) = binding {
        locals.insert(
            name.to_string(),
            MagType::Function(param_types.to_vec(), Box::new(result.clone())),
        );
    }
    let mut actual = MagType::Unit;
    for expr in body {
        actual = infer(env, &mut locals, expr).map_err(|error| match binding {
            Some(name) => MagError::Type(format!("in function {name}: {error}")),
            None => error,
        })?;
    }
    compatible(env, &actual, result, &mut HashMap::new()).map_err(|message| {
        MagError::Type(format!(
            "function {}returns {actual}, declared {result}: {message}",
            binding.map(|name| format!("{name} ")).unwrap_or_default()
        ))
    })
}

pub fn check_call(
    env: &Env,
    function: &crate::ast::FnValue,
    args: &[Value],
) -> Result<(MagType, HashMap<String, MagType>), MagError> {
    if function.param_types.len() != args.len() {
        return Err(MagError::Arity {
            expected: function.param_types.len(),
            got: args.len(),
        });
    }
    let mut subst = HashMap::new();
    for (value, expected) in args.iter().zip(&function.param_types) {
        let actual = value_type(value).ok_or_else(|| {
            MagError::Type(format!(
                "cannot pass {} as a typed argument",
                value.type_name()
            ))
        })?;
        compatible(env, &actual, expected, &mut subst).map_err(MagError::Type)?;
    }
    Ok((substitute(&function.return_type, &subst), subst))
}

fn infer(
    env: &Env,
    locals: &mut HashMap<String, MagType>,
    expr: &Expr,
) -> Result<MagType, MagError> {
    match expr {
        Expr::Nil => Ok(MagType::Unit),
        Expr::Bool(_) => Ok(MagType::Bool),
        Expr::Int(_) => Ok(MagType::Int),
        Expr::Float(_) => Ok(MagType::Float),
        Expr::Str(_) => Ok(MagType::String),
        Expr::Keyword(_) => Ok(MagType::String),
        Expr::Symbol(name) => locals
            .get(name)
            .cloned()
            .or_else(|| env.lookup(name).ok().and_then(value_type))
            .ok_or_else(|| MagError::Unresolved(name.clone())),
        Expr::Vector(xs) => infer_list(env, locals, xs),
        Expr::Map(fields) => {
            let mut out = BTreeMap::new();
            for (k, v) in fields {
                let key = match k {
                    Expr::Keyword(s) | Expr::Symbol(s) | Expr::Str(s) => s.clone(),
                    _ => {
                        return Err(MagError::Type(
                            "record key must be a keyword, symbol, or string".into(),
                        ))
                    }
                };
                out.insert(key, infer(env, locals, v)?);
            }
            Ok(MagType::Record(out))
        }
        Expr::List(items) => infer_form(env, locals, items),
    }
}

fn infer_list(
    env: &Env,
    locals: &mut HashMap<String, MagType>,
    xs: &[Expr],
) -> Result<MagType, MagError> {
    if xs.is_empty() {
        return Ok(MagType::EmptyList);
    }
    let first = infer(env, locals, &xs[0])?;
    for item in &xs[1..] {
        let ty = infer(env, locals, item)?;
        compatible(env, &ty, &first, &mut HashMap::new()).map_err(MagError::Type)?;
    }
    Ok(MagType::List(Box::new(first)))
}

fn infer_form(
    env: &Env,
    locals: &mut HashMap<String, MagType>,
    items: &[Expr],
) -> Result<MagType, MagError> {
    if items.is_empty() {
        return Ok(MagType::Unit);
    }
    if let Some(head) = items[0].as_symbol() {
        match head {
            "if" => {
                if items.len() < 3 || items.len() > 4 {
                    return Err(MagError::Type("if expects 2-3 arguments".into()));
                }
                compatible(
                    env,
                    &infer(env, locals, &items[1])?,
                    &MagType::Bool,
                    &mut HashMap::new(),
                )
                .map_err(MagError::Type)?;
                let left = infer(env, locals, &items[2])?;
                let right = if items.len() == 4 {
                    infer(env, locals, &items[3])?
                } else {
                    MagType::Unit
                };
                if compatible(env, &left, &right, &mut HashMap::new()).is_ok() {
                    return Ok(right);
                }
                return Ok(MagType::Union(vec![left, right]));
            }
            "let" => {
                let bindings = match items.get(1) {
                    Some(Expr::Vector(v)) if v.len() % 2 == 0 => v,
                    _ => return Err(MagError::Type("let bindings must be pairs".into())),
                };
                let mut nested = locals.clone();
                for pair in bindings.chunks(2) {
                    let name = pair[0]
                        .as_symbol()
                        .ok_or_else(|| MagError::Type("let name must be a symbol".into()))?;
                    let ty = infer(env, &mut nested, &pair[1])?;
                    nested.insert(name.into(), ty);
                }
                let mut out = MagType::Unit;
                for body in &items[2..] {
                    out = infer(env, &mut nested, body)?;
                }
                return Ok(out);
            }
            "as" => {
                if items.len() != 3 {
                    return Err(MagError::Type("as expects a type and value".into()));
                }
                let mut vars = HashSet::new();
                for ty in locals.values() {
                    collect_vars(ty, &mut vars);
                }
                let target = crate::eval::parse_type(env, &items[1], &vars)?;
                let _source = infer(env, locals, &items[2])?;
                return Ok(target);
            }
            "type-tag" => {
                if items.len() != 2 {
                    return Err(MagError::Type("type-tag expects one type".into()));
                }
                let mut vars = HashSet::new();
                for ty in locals.values() {
                    collect_vars(ty, &mut vars);
                }
                return Ok(MagType::TypeTag(Box::new(crate::eval::parse_type(
                    env, &items[1], &vars,
                )?)));
            }
            "specialize" => {
                if items.len() != 3 {
                    return Err(MagError::Type(
                        "specialize expects a foreign and type vector".into(),
                    ));
                }
                let decl = match items[1].as_symbol().and_then(|name| env.lookup(name).ok()) {
                    Some(Value::Foreign(decl)) => decl,
                    _ => {
                        return Err(MagError::Type(
                            "specialize expects a Foreign declaration".into(),
                        ))
                    }
                };
                let exprs = match &items[2] {
                    Expr::Vector(v) => v,
                    _ => return Err(MagError::Type("specialize types must be a vector".into())),
                };
                if exprs.len() != decl.type_params.len() {
                    return Err(MagError::Type(format!(
                        "{} expects {} type arguments",
                        decl.name,
                        decl.type_params.len()
                    )));
                }
                let mut vars = HashSet::new();
                for ty in locals.values() {
                    collect_vars(ty, &mut vars);
                }
                let args = exprs
                    .iter()
                    .map(|expr| crate::eval::parse_type(env, expr, &vars))
                    .collect::<Result<Vec<_>, _>>()?;
                let subst = decl.type_params.iter().cloned().zip(args).collect();
                return Ok(MagType::Foreign(
                    Box::new(substitute(&decl.params, &subst)),
                    Box::new(substitute(&decl.input, &subst)),
                    Box::new(substitute(&decl.output, &subst)),
                ));
            }
            "fn" => return infer_fn(env, locals, items),
            _ => {}
        }
    }
    if let Some(name) = items[0].as_symbol() {
        if let Ok(Value::BuiltinFn(_)) = env.lookup(name) {
            return infer_builtin(env, locals, name, &items[1..]);
        }
    }
    let callable = infer(env, locals, &items[0])?;
    let (params, result) = match callable {
        MagType::Function(p, r) => (p, r),
        other => return Err(MagError::Type(format!("cannot call {other}"))),
    };
    if params.len() != items.len() - 1 {
        return Err(MagError::Type(format!(
            "call expects {} arguments, got {}",
            params.len(),
            items.len() - 1
        )));
    }
    let mut subst = HashMap::new();
    for (arg, expected) in items[1..].iter().zip(&params) {
        let actual = infer(env, locals, arg)?;
        compatible(env, &actual, expected, &mut subst).map_err(MagError::Type)?;
    }
    Ok(substitute(&result, &subst))
}

fn infer_fn(
    env: &Env,
    outer: &HashMap<String, MagType>,
    items: &[Expr],
) -> Result<MagType, MagError> {
    let args = &items[1..];
    if args.len() < 4 {
        return Err(MagError::Type("typed fn signature required".into()));
    }
    let empty = Expr::Vector(vec![]);
    let (type_expr, param_expr, arrow, return_expr, body) =
        if matches!(args.get(1), Some(Expr::Vector(_))) {
            (&args[0], &args[1], &args[2], &args[3], &args[4..])
        } else {
            (&empty, &args[0], &args[1], &args[2], &args[3..])
        };
    if !matches!(arrow,Expr::Symbol(s) if s=="->") {
        return Err(MagError::Type("fn signature requires ->".into()));
    }
    let mut vars = HashSet::new();
    for ty in outer.values() {
        collect_vars(ty, &mut vars);
    }
    let declared_vars = match type_expr {
        Expr::Vector(xs) => xs
            .iter()
            .map(|x| {
                x.as_symbol()
                    .map(str::to_owned)
                    .ok_or_else(|| MagError::Type("generic binder must be a symbol".into()))
            })
            .collect::<Result<HashSet<_>, _>>()?,
        _ => unreachable!(),
    };
    vars.extend(declared_vars);
    let pairs = match param_expr {
        Expr::Vector(v) => v,
        _ => return Err(MagError::Type("fn parameters must be a vector".into())),
    };
    let mut names = vec![];
    let mut types = vec![];
    for pair in pairs {
        match pair {
            Expr::Vector(v) if v.len() == 2 => {
                names.push(
                    v[0].as_symbol()
                        .ok_or_else(|| MagError::Type("parameter name must be a symbol".into()))?
                        .into(),
                );
                types.push(crate::eval::parse_type(env, &v[1], &vars)?);
            }
            _ => return Err(MagError::Type("parameter must be [name Type]".into())),
        }
    }
    let result = crate::eval::parse_type(env, return_expr, &vars)?;
    let mut locals = outer.clone();
    for (name, ty) in names.iter().cloned().zip(types.iter().cloned()) {
        locals.insert(name, ty);
    }
    let mut actual = MagType::Unit;
    for expr in body {
        actual = infer(env, &mut locals, expr)?;
    }
    compatible(env, &actual, &result, &mut HashMap::new()).map_err(|message| {
        MagError::Type(format!(
            "nested function returns {actual}, declared {result}: {message}"
        ))
    })?;
    Ok(MagType::Function(types, Box::new(result)))
}

fn infer_builtin(
    env: &Env,
    locals: &mut HashMap<String, MagType>,
    name: &str,
    args: &[Expr],
) -> Result<MagType, MagError> {
    let exact = |expected| {
        if args.len() == expected {
            Ok(())
        } else {
            Err(MagError::Type(format!(
                "{name} expects {expected} arguments, got {}",
                args.len()
            )))
        }
    };
    match name {
        "get" => {
            exact(2)?;
            let target = infer(env, locals, &args[0])?;
            let key = match &args[1] {
                Expr::Str(s) | Expr::Keyword(s) => Some(s.as_str()),
                _ => None,
            };
            field_type(env, &target, key)
                .ok_or_else(|| MagError::Type(format!("cannot get {:?} from {target}", key)))
        }
        "assoc" => {
            exact(3)?;
            let target = infer(env, locals, &args[0])?;
            let key = match &args[1] {
                Expr::Str(s) | Expr::Keyword(s) => Some(s.as_str()),
                _ => None,
            };
            let expected = field_type(env, &target, key)
                .ok_or_else(|| MagError::Type(format!("cannot assoc {:?} into {target}", key)))?;
            let value = infer(env, locals, &args[2])?;
            compatible(env, &value, &expected, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(target)
        }
        "count" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::List(_) | MagType::Map(_, _) | MagType::Record(_) | MagType::String => {
                    Ok(MagType::Int)
                }
                actual => Err(MagError::Type(format!(
                    "count expects a collection, got {actual}"
                ))),
            }
        }
        "=" => {
            exact(2)?;
            let left = infer(env, locals, &args[0])?;
            let right = infer(env, locals, &args[1])?;
            compatible(env, &left, &right, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "not" => {
            exact(1)?;
            let actual = infer(env, locals, &args[0])?;
            compatible(env, &actual, &MagType::Bool, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "foreign-id" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Foreign(_, _, _) => Ok(MagType::String),
                actual => Err(MagError::Type(format!(
                    "foreign-id expects Foreign, got {actual}"
                ))),
            }
        }
        "foreign-evidence" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Foreign(_, _, _) => Ok(MagType::ForeignEvidence),
                actual => Err(MagError::Type(format!(
                    "foreign-evidence expects Foreign, got {actual}"
                ))),
            }
        }
        "type-evidence" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::TypeTag(_) => Ok(MagType::TypeDescriptor),
                actual => Err(MagError::Type(format!(
                    "type-evidence expects TypeTag, got {actual}"
                ))),
            }
        }
        "type-schema" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::TypeTag(_) => Ok(MagType::TypeSchema),
                actual => Err(MagError::Type(format!(
                    "type-schema expects TypeTag, got {actual}"
                ))),
            }
        }
        "type-id" => {
            exact(1)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::SemanticTypeId)
        }
        "or" => {
            exact(2)?;
            let a = infer(env, locals, &args[0])?;
            let b = infer(env, locals, &args[1])?;
            if compatible(env, &a, &b, &mut HashMap::new()).is_ok() {
                Ok(b)
            } else {
                Ok(MagType::Union(vec![a, b]))
            }
        }
        "str" => Ok(MagType::String),
        "canonical" => {
            exact(1)?;
            let _ = infer(env, locals, &args[0])?;
            Ok(MagType::String)
        }
        "conforms?" => {
            exact(2)?;
            let _ = infer(env, locals, &args[0])?;
            let evidence = infer(env, locals, &args[1])?;
            compatible(
                env,
                &evidence,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "fail" => {
            exact(1)?;
            let _ = infer(env, locals, &args[0])?;
            Ok(MagType::Never)
        }
        "pack" => {
            exact(1)?;
            let _ = infer(env, locals, &args[0])?;
            Ok(MagType::PackedValue)
        }
        "packed-empty-record?" => {
            exact(1)?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::PackedValue, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "packed-record-has-only-key?" | "packed-field-conforms?" => {
            exact(if name == "packed-record-has-only-key?" {
                2
            } else {
                3
            })?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::PackedValue, &mut HashMap::new())
                .map_err(MagError::Type)?;
            let key = infer(env, locals, &args[1])?;
            compatible(env, &key, &MagType::String, &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "packed-field-conforms?" {
                let descriptor = infer(env, locals, &args[2])?;
                compatible(
                    env,
                    &descriptor,
                    &MagType::TypeDescriptor,
                    &mut HashMap::new(),
                )
                .map_err(MagError::Type)?;
            }
            Ok(MagType::Bool)
        }
        "descriptor-accepts?"
        | "descriptor-input-covered-by?"
        | "descriptor-input-assignments"
        | "descriptor-output-covered-by?" => {
            exact(2)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            let expected = if name == "descriptor-accepts?" {
                MagType::TypeDescriptor
            } else {
                MagType::List(Box::new(MagType::TypeDescriptor))
            };
            let value = infer(env, locals, &args[1])?;
            compatible(env, &value, &expected, &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "descriptor-input-assignments" {
                Ok(MagType::List(Box::new(MagType::Int)))
            } else {
                Ok(MagType::Bool)
            }
        }
        "descriptor-table" => {
            exact(1)?;
            let descriptors = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptors,
                &MagType::List(Box::new(MagType::TypeDescriptor)),
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::Map(
                Box::new(MagType::String),
                Box::new(MagType::TypeDescriptor),
            ))
        }
        "foreign-contracts" => {
            exact(0)?;
            Ok(MagType::List(Box::new(MagType::Record(
                [
                    ("identity".into(), MagType::String),
                    (
                        "type_scheme".into(),
                        MagType::Record(
                            [
                                (
                                    "input_tags".into(),
                                    MagType::List(Box::new(MagType::String)),
                                ),
                                ("outputs".into(), MagType::List(Box::new(MagType::String))),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    ),
                ]
                .into_iter()
                .collect(),
            ))))
        }
        "read" => {
            if !(1..=2).contains(&args.len()) {
                return Err(MagError::Type(format!(
                    "read expects 1-2 arguments, got {}",
                    args.len()
                )));
            }
            let path = infer(env, locals, &args[0])?;
            compatible(env, &path, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
            if args.len() == 2 {
                let _ = infer(env, locals, &args[1])?;
            }
            Ok(MagType::String)
        }
        "artifact" => {
            exact(2)?;
            let format = infer(env, locals, &args[0])?;
            compatible(env, &format, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
            let _ = infer(env, locals, &args[1])?;
            Ok(MagType::Artifact)
        }
        "concat" => {
            exact(2)?;
            let a = infer(env, locals, &args[0])?;
            let b = infer(env, locals, &args[1])?;
            compatible(env, &a, &b, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(a)
        }
        "remove-at" => {
            exact(2)?;
            let collection = infer(env, locals, &args[0])?;
            let index = infer(env, locals, &args[1])?;
            compatible(env, &index, &MagType::Int, &mut HashMap::new()).map_err(MagError::Type)?;
            match collection {
                MagType::List(_) => Ok(collection),
                actual => Err(MagError::Type(format!(
                    "remove-at expects List, got {actual}"
                ))),
            }
        }
        "keys" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Map(_, _) | MagType::Record(_) => {
                    Ok(MagType::List(Box::new(MagType::String)))
                }
                actual => Err(MagError::Type(format!("keys expects a map, got {actual}"))),
            }
        }
        "first" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::List(item) => Ok(*item),
                actual => Err(MagError::Type(format!("first expects List, got {actual}"))),
            }
        }
        "map" | "filter" | "flat-map" | "sort-by" => {
            exact(2)?;
            let fun = infer(env, locals, &args[0])?;
            let collection = infer(env, locals, &args[1])?;
            let item = match collection {
                MagType::List(t) => *t,
                _ => return Err(MagError::Type(format!("{name} expects List"))),
            };
            let (params, result) = match fun {
                MagType::Function(p, r) => (p, r),
                _ => return Err(MagError::Type(format!("{name} expects function"))),
            };
            if params.len() != 1 {
                return Err(MagError::Type(format!(
                    "{name} callback expects 1 parameter"
                )));
            }
            compatible(env, &item, &params[0], &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "filter" {
                compatible(env, &result, &MagType::Bool, &mut HashMap::new())
                    .map_err(MagError::Type)?;
                Ok(MagType::List(Box::new(item)))
            } else if name == "sort-by" {
                compatible(env, &result, &MagType::String, &mut HashMap::new())
                    .map_err(MagError::Type)?;
                Ok(MagType::List(Box::new(item)))
            } else if name == "flat-map" {
                match *result {
                    MagType::List(_) => Ok(*result),
                    actual => Err(MagError::Type(format!(
                        "flat-map callback must return List, got {actual}"
                    ))),
                }
            } else {
                Ok(MagType::List(result))
            }
        }
        "indexed-map" => {
            exact(2)?;
            let fun = infer(env, locals, &args[0])?;
            let collection = infer(env, locals, &args[1])?;
            let item = match collection {
                MagType::List(t) => *t,
                _ => return Err(MagError::Type("indexed-map expects List".into())),
            };
            let (params, result) = match fun {
                MagType::Function(p, r) => (p, r),
                _ => return Err(MagError::Type("indexed-map expects function".into())),
            };
            if params.len() != 2 {
                return Err(MagError::Type(
                    "indexed-map callback expects 2 parameters".into(),
                ));
            }
            compatible(env, &MagType::Int, &params[0], &mut HashMap::new())
                .map_err(MagError::Type)?;
            compatible(env, &item, &params[1], &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(MagType::List(result))
        }
        "fold" => {
            exact(3)?;
            let fun = infer(env, locals, &args[0])?;
            let init = infer(env, locals, &args[1])?;
            let collection = infer(env, locals, &args[2])?;
            let item = match collection {
                MagType::List(t) => *t,
                _ => return Err(MagError::Type("fold expects List".into())),
            };
            let (params, result) = match fun {
                MagType::Function(p, r) => (p, r),
                _ => return Err(MagError::Type("fold expects function".into())),
            };
            if params.len() != 2 {
                return Err(MagError::Type("fold callback expects 2 parameters".into()));
            }
            compatible(env, &init, &params[0], &mut HashMap::new()).map_err(MagError::Type)?;
            compatible(env, &item, &params[1], &mut HashMap::new()).map_err(MagError::Type)?;
            compatible(env, &result, &params[0], &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(*result)
        }
        _ => Err(MagError::Type(format!("no type rule for builtin {name}"))),
    }
}

fn value_type(value: &Value) -> Option<MagType> {
    match value {
        Value::Unit => Some(MagType::Unit),
        Value::Bool(_) => Some(MagType::Bool),
        Value::Int(_) => Some(MagType::Int),
        Value::Float(_) => Some(MagType::Float),
        Value::Str(_) | Value::Keyword(_) | Value::Symbol(_) => Some(MagType::String),
        Value::List(v) | Value::Vector(v) if v.is_empty() => Some(MagType::EmptyList),
        Value::List(v) | Value::Vector(v) => {
            Some(MagType::List(Box::new(v.first().and_then(value_type)?)))
        }
        Value::Product(v) => Some(MagType::Product(
            v.iter().map(value_type).collect::<Option<Vec<_>>>()?,
        )),
        Value::Map(m) => Some(MagType::Record(
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), value_type(v)?)))
                .collect(),
        )),
        Value::Fn(f) => Some(MagType::Function(
            f.param_types.clone(),
            Box::new(f.return_type.clone()),
        )),
        Value::Type(t) => Some(t.clone()),
        Value::TypeDecl(d) => Some(MagType::Named(d.name.clone(), vec![])),
        Value::TypeTag(ty) => Some(MagType::TypeTag(Box::new(ty.to_mag_type()))),
        Value::Foreign(decl) => Some(MagType::Foreign(
            Box::new(decl.params.clone()),
            Box::new(decl.input.clone()),
            Box::new(decl.output.clone()),
        )),
        Value::ForeignEvidence(_) => Some(MagType::ForeignEvidence),
        Value::TypeDescriptor(_) => Some(MagType::TypeDescriptor),
        Value::TypeSchema(_) => Some(MagType::TypeSchema),
        Value::SemanticTypeId(_) => Some(MagType::SemanticTypeId),
        Value::PackedValue(_) => Some(MagType::PackedValue),
        Value::JsonValue(_) => Some(MagType::JsonValue),
        Value::HostInputs(_) => Some(MagType::HostInputs),
        Value::Artifact(_) => Some(MagType::Artifact),
        Value::Typed(_, ty) => Some(ty.clone()),
        Value::BuiltinFn(_) => None,
    }
}

fn field_type(env: &Env, ty: &MagType, key: Option<&str>) -> Option<MagType> {
    match ty {
        MagType::Map(_, v) => Some((**v).clone()),
        MagType::Record(fields) => key.and_then(|k| fields.get(k).cloned()),
        MagType::Named(name, args) => env.type_decl(name).and_then(|decl| {
            let substitutions = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            let body = substitute(&decl.body, &substitutions);
            field_type(env, &body, key)
        }),
        MagType::Union(ts) => {
            let fields = ts
                .iter()
                .filter_map(|t| field_type(env, t, key))
                .collect::<Vec<_>>();
            if fields.is_empty() {
                None
            } else {
                Some(MagType::Union(fields))
            }
        }
        _ => None,
    }
}

fn compatible(
    env: &Env,
    actual: &MagType,
    expected: &MagType,
    subst: &mut HashMap<String, MagType>,
) -> Result<(), String> {
    if let MagType::Var(name) = expected {
        if let Some(bound) = subst.get(name) {
            let bound = bound.clone();
            return compatible(env, actual, &bound, subst)
                .map_err(|_| format!("{name} was {bound}, got {actual}"));
        }
        subst.insert(name.clone(), actual.clone());
        return Ok(());
    }
    if actual == expected {
        return Ok(());
    }
    if matches!(actual, MagType::Never) {
        return Ok(());
    }
    if matches!(actual, MagType::EmptyList) && matches!(expected, MagType::List(_)) {
        return Ok(());
    }
    // Once inference has removed open variables, compatibility is owned by
    // the same normalized descriptor relation emitted to the runtime.
    if let (Ok(actual), Ok(expected)) = (
        crate::types::ConcreteType::resolve(env, actual),
        crate::types::ConcreteType::resolve(env, expected),
    ) {
        return expected.accepts(&actual).then_some(()).ok_or_else(|| {
            if matches!(expected, crate::types::ConcreteType::Named { .. }) {
                format!(
                    "expected nominal {expected:?}, got {actual:?}; use as for explicit refinement"
                )
            } else {
                format!("expected {expected:?}, got {actual:?}")
            }
        });
    }
    if let MagType::Union(variants) = actual {
        for variant in variants {
            compatible(env, variant, expected, subst)?;
        }
        return Ok(());
    }
    match expected {
        MagType::Union(options) => {
            let mut matches = options.iter().filter_map(|option| {
                let mut candidate = subst.clone();
                compatible(env, actual, option, &mut candidate)
                    .ok()
                    .map(|_| candidate)
            });
            let selected = matches
                .next()
                .ok_or_else(|| format!("expected {expected}, got {actual}"))?;
            for candidate in matches {
                if candidate != selected {
                    return Err(format!(
                        "ambiguous union match for {actual} against {expected}"
                    ));
                }
            }
            *subst = selected;
            Ok(())
        }
        MagType::Named(expected_name, expected_args) => match actual {
            MagType::Named(actual_name, actual_args)
                if actual_name == expected_name && actual_args.len() == expected_args.len() =>
            {
                for (actual_arg, expected_arg) in actual_args.iter().zip(expected_args) {
                    compatible(env, actual_arg, expected_arg, subst)?;
                }
                Ok(())
            }
            _ => Err(format!(
                "expected nominal {expected}, got {actual}; use as for explicit refinement"
            )),
        },
        MagType::TypeTag(expected_type) => match actual {
            MagType::TypeTag(actual_type) => compatible(env, actual_type, expected_type, subst),
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::List(e) => match actual {
            MagType::List(a) => compatible(env, a, e, subst),
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Map(ek, ev) => match actual {
            MagType::Map(ak, av) => {
                compatible(env, ak, ek, subst)?;
                compatible(env, av, ev, subst)
            }
            MagType::Record(fields) if matches!(ek.as_ref(), MagType::String) => {
                for value in fields.values() {
                    compatible(env, value, ev, subst)?;
                }
                Ok(())
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Record(ef) => match actual {
            MagType::Record(af) if ef.len() == af.len() => {
                for (k, e) in ef {
                    compatible(
                        env,
                        af.get(k).ok_or_else(|| format!("missing field {k}"))?,
                        e,
                        subst,
                    )?;
                }
                Ok(())
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Function(ep, er) => match actual {
            MagType::Function(ap, ar) if ap.len() == ep.len() => {
                for (a, e) in ap.iter().zip(ep) {
                    compatible(env, a, e, subst)?;
                }
                compatible(env, ar, er, subst)
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Foreign(ep, ei, eo) => match actual {
            MagType::Foreign(ap, ai, ao) => {
                compatible(env, ap, ep, subst)?;
                compatible(env, ai, ei, subst)?;
                compatible(env, ao, eo, subst)
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        _ => Err(format!("expected {expected}, got {actual}")),
    }
}
pub(crate) fn substitute(ty: &MagType, subst: &HashMap<String, MagType>) -> MagType {
    match ty {
        MagType::Var(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        MagType::Named(n, a) => {
            MagType::Named(n.clone(), a.iter().map(|t| substitute(t, subst)).collect())
        }
        MagType::TypeTag(t) => MagType::TypeTag(Box::new(substitute(t, subst))),
        MagType::List(t) => MagType::List(Box::new(substitute(t, subst))),
        MagType::Map(k, v) => MagType::Map(
            Box::new(substitute(k, subst)),
            Box::new(substitute(v, subst)),
        ),
        MagType::Record(f) => MagType::Record(
            f.iter()
                .map(|(k, v)| (k.clone(), substitute(v, subst)))
                .collect(),
        ),
        MagType::Union(v) => MagType::Union(v.iter().map(|t| substitute(t, subst)).collect()),
        MagType::Product(v) => MagType::Product(v.iter().map(|t| substitute(t, subst)).collect()),
        MagType::Function(p, r) => MagType::Function(
            p.iter().map(|t| substitute(t, subst)).collect(),
            Box::new(substitute(r, subst)),
        ),
        MagType::Foreign(p, i, o) => MagType::Foreign(
            Box::new(substitute(p, subst)),
            Box::new(substitute(i, subst)),
            Box::new(substitute(o, subst)),
        ),
        _ => ty.clone(),
    }
}

fn collect_vars(ty: &MagType, out: &mut HashSet<String>) {
    match ty {
        MagType::Var(name) => {
            out.insert(name.clone());
        }
        MagType::Named(_, args) | MagType::Union(args) | MagType::Product(args) => {
            for arg in args {
                collect_vars(arg, out);
            }
        }
        MagType::List(item) => collect_vars(item, out),
        MagType::TypeTag(item) => collect_vars(item, out),
        MagType::Map(key, value) => {
            collect_vars(key, out);
            collect_vars(value, out);
        }
        MagType::Record(fields) => {
            for field in fields.values() {
                collect_vars(field, out);
            }
        }
        MagType::Function(params, result) => {
            for param in params {
                collect_vars(param, out);
            }
            collect_vars(result, out);
        }
        MagType::Foreign(params, input, output) => {
            collect_vars(params, out);
            collect_vars(input, out);
            collect_vars(output, out);
        }
        _ => {}
    }
}
