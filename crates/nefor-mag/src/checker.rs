use crate::ast::{
    BindingId, CheckedBinding, CheckedBlock, CheckedExpr, CheckedExprKind, CheckedFn,
    CheckedMatchArm, CheckedParam, ConstructorDecl, ConstructorDeclarationId, TypeDeclBody, Value,
};
use crate::authored::{BlockItem, Expr, Function, NamePath, Type, TypeArgument};
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

thread_local! {
    static EQUALITY_OBLIGATIONS: RefCell<Vec<HashSet<String>>> = const { RefCell::new(Vec::new()) };
}

fn with_equality_obligation_scope<T>(
    operation: impl FnOnce() -> Result<T, MagError>,
) -> Result<(T, HashSet<String>), MagError> {
    EQUALITY_OBLIGATIONS.with(|scopes| scopes.borrow_mut().push(HashSet::new()));
    let result = operation();
    let obligations =
        EQUALITY_OBLIGATIONS.with(|scopes| scopes.borrow_mut().pop().unwrap_or_default());
    result.map(|value| (value, obligations))
}

fn record_equality_variables(variables: impl IntoIterator<Item = String>) {
    EQUALITY_OBLIGATIONS.with(|scopes| {
        if let Some(scope) = scopes.borrow_mut().last_mut() {
            scope.extend(variables);
        }
    });
}

#[derive(Clone)]
struct LocalCandidate {
    ty: MagType,
    generic_binders: Vec<String>,
}

type Locals = HashMap<String, Vec<LocalCandidate>>;

fn add_local(locals: &mut Locals, name: impl Into<String>, ty: MagType) -> Result<(), MagError> {
    let name = name.into();
    let overloads = locals.entry(name.clone()).or_default();
    if overloads.iter().any(|candidate| candidate.ty == ty) {
        return Err(MagError::Type(format!(
            "duplicate visible overload {name}: {ty}"
        )));
    }
    overloads.push(LocalCandidate {
        ty,
        generic_binders: Vec::new(),
    });
    Ok(())
}

#[cfg(test)]
pub(crate) fn compile_resolved_function(
    env: &Env,
    name: Option<&str>,
    type_params: &[String],
    params: &[String],
    param_types: &[MagType],
    result: &MagType,
    body: &[BlockItem],
) -> Result<CheckedFn, MagError> {
    let mut parameter_scope = CheckedScope::new();
    if !type_params.is_empty() {
        parameter_scope.insert(
            TYPE_BINDER_SCOPE_KEY.into(),
            vec![CheckedCandidate {
                id: BindingId(u64::MAX),
                ty: MagType::Product(type_params.iter().cloned().map(MagType::Var).collect()),
                generic_binders: vec![],
                parameter_names: None,
                contributes_type_vars: true,
            }],
        );
    }
    let mut checked_params = Vec::with_capacity(params.len());
    for (parameter_name, ty) in params.iter().zip(param_types) {
        let id = env.allocate_binding_id(parameter_name, Some(ty.clone()));
        insert_checked_candidate(
            env,
            &[],
            &mut parameter_scope,
            parameter_name,
            CheckedCandidate {
                id,
                ty: ty.clone(),
                generic_binders: vec![],
                parameter_names: None,
                contributes_type_vars: false,
            },
        )?;
        checked_params.push(CheckedParam {
            id,
            name: parameter_name.clone(),
            ty: ty.clone(),
        });
    }
    let (checked_body, equality_variables) = with_equality_obligation_scope(|| {
        compile_block_in(env, &[parameter_scope], body, Some(result))
    })?;
    let equality_params = type_params
        .iter()
        .filter(|parameter| equality_variables.contains(*parameter))
        .cloned()
        .collect::<Vec<_>>();
    let actual = checked_body
        .expressions
        .last()
        .map(|expression| expression.ty.clone())
        .unwrap_or(MagType::Unit);
    compatible_static(env, &actual, result, &mut HashMap::new()).map_err(|message| {
        MagError::Type(format!(
            "function {}returns {actual}, declared {result}: {message}",
            name.map(|name| format!("{name} ")).unwrap_or_default()
        ))
    })?;
    Ok(CheckedFn {
        name: name.map(str::to_owned),
        type_params: type_params.to_vec(),
        equality_params,
        params: checked_params,
        result: result.clone(),
        body: Arc::new(checked_body),
    })
}

fn direct_let(item: &BlockItem) -> Result<Option<(&str, &Expr)>, MagError> {
    match item {
        BlockItem::Let { name, value } => Ok(Some((name, value))),
        BlockItem::Expr(_) => Ok(None),
        BlockItem::Invalid(error) => Err(error.clone().into_mag_error()),
    }
}

fn block_expr(item: &BlockItem) -> Result<Option<&Expr>, MagError> {
    match item {
        BlockItem::Expr(expression) => Ok(Some(expression)),
        BlockItem::Let { .. } => Ok(None),
        BlockItem::Invalid(error) => Err(error.clone().into_mag_error()),
    }
}

fn is_fn(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(_))
}

fn function_type_params(expression: &Expr) -> Result<Vec<String>, MagError> {
    match expression {
        Expr::Function(function) => Ok(function.type_params.clone()),
        Expr::Invalid(error) => Err(error.clone().into_mag_error()),
        _ => Ok(vec![]),
    }
}

fn function_parameter_names(expression: &Expr) -> Option<Vec<String>> {
    let function = match expression {
        Expr::Function(function) => function,
        Expr::Ascribe { value, .. } | Expr::Annotate { value, .. } => {
            return function_parameter_names(value)
        }
        _ => return None,
    };
    Some(
        function
            .params
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect(),
    )
}

fn infer_fn_signature(env: &Env, outer: &Locals, expression: &Expr) -> Result<MagType, MagError> {
    let scoped_type_vars = outer
        .values()
        .flatten()
        .flat_map(|candidate| {
            let mut vars = HashSet::new();
            collect_vars(&candidate.ty, &mut vars);
            vars
        })
        .collect();
    infer_fn_signature_scoped(env, &scoped_type_vars, expression)
}

fn infer_fn_signature_scoped(
    env: &Env,
    scoped_type_vars: &HashSet<String>,
    expression: &Expr,
) -> Result<MagType, MagError> {
    let Expr::Function(function) = expression else {
        return match expression {
            Expr::Invalid(error) => Err(error.clone().into_mag_error()),
            _ => Err(MagError::Type("typed fn signature required".into())),
        };
    };
    let mut vars = scoped_type_vars.clone();
    vars.extend(function.type_params.iter().cloned());
    let params = function
        .params
        .iter()
        .map(|parameter| resolve_type(env, &parameter.ty, &vars))
        .collect::<Result<Vec<_>, _>>()?;
    let result = resolve_type(env, &function.result, &vars)?;
    Ok(MagType::Function(params, Box::new(result)))
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
    let actual_types = args
        .iter()
        .map(|value| {
            value_type(value).ok_or_else(|| {
                MagError::Type(format!(
                    "cannot pass {} as a typed argument",
                    value.type_name()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut subst = HashMap::new();
    let mut order = (0..function.param_types.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| contains_union(&function.param_types[*index]));
    for index in order {
        let actual = &actual_types[index];
        let expected = substitute(&function.param_types[index], &subst);
        compatible(env, actual, &expected, &mut subst).map_err(MagError::Type)?;
    }
    validate_equality_requirements(
        env,
        &function
            .equality_params
            .iter()
            .cloned()
            .map(MagType::Var)
            .collect::<Vec<_>>(),
        &subst,
    )?;
    Ok((substitute(&function.return_type, &subst), subst))
}

pub fn check_resolved_call(
    env: &Env,
    function: &crate::ast::FnValue,
    resolved: &MagType,
    explicit_bindings: &BTreeMap<String, MagType>,
) -> Result<(MagType, HashMap<String, MagType>), MagError> {
    let MagType::Function(resolved_params, result) = resolved else {
        return Err(MagError::Type(format!(
            "checked call target must be a function, got {resolved}"
        )));
    };
    if resolved_params.len() != function.param_types.len() {
        return Err(MagError::Type(format!(
            "checked call arity changed from {} to {}",
            function.param_types.len(),
            resolved_params.len()
        )));
    }
    let mut substitution = explicit_bindings
        .iter()
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .collect::<HashMap<_, _>>();
    let mut order = (0..function.param_types.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| contains_union(&function.param_types[*index]));
    for index in order {
        let expected = substitute(&function.param_types[index], &substitution);
        compatible(env, &resolved_params[index], &expected, &mut substitution)
            .map_err(MagError::Type)?;
    }
    let expected_result = substitute(&function.return_type, &substitution);
    compatible(env, result, &expected_result, &mut substitution).map_err(MagError::Type)?;
    validate_equality_requirements(
        env,
        &function
            .equality_params
            .iter()
            .cloned()
            .map(MagType::Var)
            .collect::<Vec<_>>(),
        &substitution,
    )?;
    Ok((result.as_ref().clone(), substitution))
}

fn infer(env: &Env, locals: &mut Locals, expr: &Expr) -> Result<MagType, MagError> {
    match expr {
        Expr::Unit => Ok(MagType::Unit),
        Expr::Bool(_) => Ok(MagType::Bool),
        Expr::Int(_) => Ok(MagType::Int),
        Expr::Float(_) => Ok(MagType::Float),
        Expr::Str(_) | Expr::Keyword(_) => Ok(MagType::String),
        Expr::Name(name) => match locals.get(name.as_str()).map(Vec::as_slice) {
            Some([candidate]) => Ok(candidate.ty.clone()),
            Some(candidates) => {
                let data = candidates
                    .iter()
                    .filter(|candidate| !matches!(candidate.ty, MagType::Function(_, _)))
                    .collect::<Vec<_>>();
                match data.as_slice() {
                    [candidate] => Ok(candidate.ty.clone()),
                    _ => Err(MagError::Type(format!(
                        "ambiguous overload {name}; candidates: {}",
                        candidates
                            .iter()
                            .map(|candidate| candidate.ty.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                }
            }
            None => env
                .lookup(name)
                .ok()
                .and_then(|value| value_type(&value))
                .ok_or_else(|| MagError::Unresolved(name.to_string())),
        },
        Expr::Vector(items) => infer_list(env, locals, items),
        Expr::Fields(_) => Err(MagError::Type(
            "standalone record values are unsupported; use a named type or Map".into(),
        )),
        Expr::If {
            condition,
            then_branch,
            else_branch,
        } => {
            compatible(
                env,
                &infer(env, locals, condition)?,
                &MagType::Bool,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            let left = infer(env, locals, then_branch)?;
            let right = infer(env, locals, else_branch)?;
            if left == MagType::Never {
                return Ok(right);
            }
            if right == MagType::Never {
                return Ok(left);
            }
            compatible(env, &left, &right, &mut HashMap::new()).map_err(|_| {
                MagError::Type(format!(
                    "if branches must return one compatible type, got {left} and {right}"
                ))
            })?;
            Ok(right)
        }
        Expr::Construct {
            owner,
            constructor,
            payload,
        } => infer_construct(env, locals, owner, constructor, payload),
        Expr::Match { value, arms } => infer_match(env, locals, value, arms),
        Expr::Ascribe { target, value } => {
            let mut vars = HashSet::new();
            for candidate in locals.values().flatten() {
                collect_vars(&candidate.ty, &mut vars);
            }
            let target = resolve_type(env, target, &vars)?;
            match value.as_ref() {
                Expr::Name(name)
                    if locals
                        .get(name.as_str())
                        .is_some_and(|types| types.len() > 1) =>
                {
                    let matches = locals[name.as_str()]
                        .iter()
                        .filter(|candidate| {
                            compatible(env, &candidate.ty, &target, &mut HashMap::new()).is_ok()
                        })
                        .collect::<Vec<_>>();
                    match matches.as_slice() {
                        [_] => {}
                        [] => {
                            return Err(MagError::Type(format!(
                                "no overload {name} matches {target}"
                            )))
                        }
                        _ => {
                            return Err(MagError::Type(format!(
                                "ambiguous overload {name} for {target}"
                            )))
                        }
                    }
                }
                Expr::Name(name) if env.lookup_candidates(name).len() > 1 => {
                    let _ = env.lookup_by_type(name, &target)?;
                }
                _ => {}
            }
            Ok(target)
        }
        Expr::Annotate { target, value } => {
            let mut vars = HashSet::new();
            for candidate in locals.values().flatten() {
                collect_vars(&candidate.ty, &mut vars);
            }
            let target = resolve_type(env, target, &vars)?;
            if matches!(value.as_ref(), Expr::Construct { .. }) {
                return Ok(target);
            }
            let actual = if matches!(value.as_ref(), Expr::Fields(_)) {
                infer_fields_against(env, locals, value, &target)?
            } else {
                infer(env, locals, value)?
            };
            compatible(env, &actual, &target, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(target)
        }
        Expr::TypeTag(target) => {
            let mut vars = HashSet::new();
            for candidate in locals.values().flatten() {
                collect_vars(&candidate.ty, &mut vars);
            }
            Ok(MagType::TypeTag(Box::new(resolve_type(
                env, target, &vars,
            )?)))
        }
        Expr::Function(_) => infer_fn_signature(env, locals, expr),
        Expr::Call {
            callee,
            type_args,
            args,
        } => infer_call(env, locals, callee, type_args.as_deref(), args),
        Expr::Invalid(error) => Err(error.clone().into_mag_error()),
    }
}

fn infer_list(env: &Env, locals: &mut Locals, items: &[Expr]) -> Result<MagType, MagError> {
    if items.is_empty() {
        return Ok(MagType::EmptyList);
    }
    let first = infer(env, locals, &items[0])?;
    for item in &items[1..] {
        let ty = infer(env, locals, item)?;
        compatible(env, &ty, &first, &mut HashMap::new()).map_err(MagError::Type)?;
    }
    Ok(MagType::List(Box::new(first)))
}

fn infer_fields_against(
    env: &Env,
    locals: &mut Locals,
    expression: &Expr,
    expected: &MagType,
) -> Result<MagType, MagError> {
    let Expr::Fields(fields) = expression else {
        return infer(env, locals, expression);
    };
    let expected_fields = named_field_types(env, expected).ok_or_else(|| {
        MagError::Type(format!("named field literal cannot construct {expected}"))
    })?;
    let authored_names = fields.iter().map(|(name, _)| name).collect::<HashSet<_>>();
    let expected_names = expected_fields.keys().collect::<HashSet<_>>();
    if authored_names != expected_names || authored_names.len() != fields.len() {
        return Err(MagError::Type(format!(
            "named field literal must exactly match {expected}"
        )));
    }
    for (name, value) in fields {
        let actual = infer(env, locals, value)?;
        let field = expected_fields
            .get(name)
            .ok_or_else(|| MagError::Type(format!("unexpected field {name} for {expected}")))?;
        compatible(env, &actual, field, &mut HashMap::new()).map_err(MagError::Type)?;
    }
    Ok(expected.clone())
}

fn infer_call(
    env: &Env,
    locals: &mut Locals,
    callee: &Expr,
    explicit_type_args: Option<&[TypeArgument]>,
    args: &[Expr],
) -> Result<MagType, MagError> {
    if let Expr::Name(name) = callee {
        let env_candidates = env.lookup_candidates(name);
        let builtin = env_candidates
            .iter()
            .any(|candidate| matches!(candidate, Value::BuiltinFn(_)));
        let mut signatures = locals
            .get(name.as_str())
            .into_iter()
            .flatten()
            .filter_map(|candidate| match &candidate.ty {
                MagType::Function(params, result) => Some((
                    candidate.generic_binders.is_empty(),
                    candidate.generic_binders.clone(),
                    params.clone(),
                    (**result).clone(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        for signature in env_candidates.iter().filter_map(|candidate| {
            let Value::Fn(function) = candidate else {
                return None;
            };
            Some((
                function.type_params.is_empty(),
                function.type_params.clone(),
                function.param_types.clone(),
                function.return_type.clone(),
            ))
        }) {
            if !signatures.contains(&signature) {
                signatures.push(signature);
            }
        }
        if !signatures.is_empty() {
            if explicit_type_args.is_some()
                && signatures
                    .iter()
                    .all(|(_, binders, _, _)| binders.is_empty())
            {
                return Err(MagError::Type(format!(
                    "{name} is not a generic function and does not accept explicit type arguments"
                )));
            }
            let argument_types = args
                .iter()
                .map(|argument| infer(env, locals, argument))
                .collect::<Result<Vec<_>, _>>()?;
            let mut visible_vars = HashSet::new();
            for candidate in locals.values().flatten() {
                collect_vars(&candidate.ty, &mut visible_vars);
            }
            let mut matching = signatures
                .iter()
                .filter_map(|(concrete, binders, params, result)| {
                    if params.len() != argument_types.len() {
                        return None;
                    }
                    let mut substitution = HashMap::new();
                    if let Some(type_args) = explicit_type_args {
                        if binders.len() != type_args.len() {
                            return None;
                        }
                        for (binder, argument) in binders.iter().zip(type_args) {
                            if let TypeArgument::Explicit(argument) = argument {
                                let ty = resolve_type(env, argument, &visible_vars).ok()?;
                                substitution.insert(binder.clone(), ty);
                            }
                        }
                    }
                    let mut order = (0..params.len()).collect::<Vec<_>>();
                    order.sort_by_key(|index| contains_union(&params[*index]));
                    let bindable = binders.iter().cloned().collect::<HashSet<_>>();
                    for index in order {
                        compatible_with_bindable(
                            env,
                            &argument_types[index],
                            &substitute(&params[index], &substitution),
                            &mut substitution,
                            &bindable,
                        )
                        .ok()?;
                    }
                    Some((*concrete, substitute(result, &substitution)))
                })
                .collect::<Vec<_>>();
            if matching.len() > 1 {
                let concrete = matching
                    .iter()
                    .filter(|(concrete, _)| *concrete)
                    .cloned()
                    .collect::<Vec<_>>();
                if concrete.len() == 1 {
                    matching = concrete;
                }
            }
            return match matching.as_slice() {
                [(_, result)] => Ok(result.clone()),
                [] if builtin && explicit_type_args.is_none() => {
                    infer_builtin(env, locals, name, args)
                }
                [] => Err(MagError::Type(format!("no overload {name} matches call"))),
                _ => Err(MagError::Type(format!(
                    "ambiguous overload {name} for call"
                ))),
            };
        } else if builtin && explicit_type_args.is_none() {
            return infer_builtin(env, locals, name, args);
        }
    }
    if explicit_type_args.is_some() {
        return Err(MagError::Type(
            "explicit type arguments require an immediate call to a named generic function".into(),
        ));
    }
    let callable = infer(env, locals, callee)?;
    let MagType::Function(params, result) = callable else {
        return Err(MagError::Type(format!("cannot call {callable}")));
    };
    if params.len() != args.len() {
        return Err(MagError::Type(format!(
            "call expects {} arguments, got {}",
            params.len(),
            args.len()
        )));
    }
    let argument_types = args
        .iter()
        .map(|argument| infer(env, locals, argument))
        .collect::<Result<Vec<_>, _>>()?;
    let mut substitution = HashMap::new();
    let mut order = (0..params.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| contains_union(&params[*index]));
    for index in order {
        compatible(
            env,
            &argument_types[index],
            &substitute(&params[index], &substitution),
            &mut substitution,
        )
        .map_err(MagError::Type)?;
    }
    Ok(substitute(&result, &substitution))
}

fn infer_construct(
    env: &Env,
    locals: &mut Locals,
    owner: &Type,
    constructor: &str,
    payload: &Expr,
) -> Result<MagType, MagError> {
    let mut vars = HashSet::new();
    for candidate in locals.values().flatten() {
        collect_vars(&candidate.ty, &mut vars);
    }
    let owner = resolve_type(env, owner, &vars)?;
    let (_, payload_type) = instantiated_constructor(env, &owner, constructor)?;
    if matches!(payload, Expr::Fields(_)) {
        infer_fields_against(env, locals, payload, &payload_type)?;
    } else {
        let actual = infer(env, locals, payload)?;
        compatible(env, &actual, &payload_type, &mut HashMap::new()).map_err(MagError::Type)?;
    }
    Ok(owner)
}

fn infer_match(
    env: &Env,
    locals: &Locals,
    value: &Expr,
    arms: &[crate::authored::MatchArm],
) -> Result<MagType, MagError> {
    let owner = infer(env, &mut locals.clone(), value)?;
    let constructors = adt_constructors(env, &owner)?;
    let mut seen = HashSet::new();
    let mut result = None;
    for arm in arms {
        let resolved_pattern_owner = if let Some(pattern_owner) = arm.pattern.owner() {
            let mut vars = HashSet::new();
            for candidate in locals.values().flatten() {
                collect_vars(&candidate.ty, &mut vars);
            }
            Some(resolve_type(env, &Type::Name(pattern_owner), &vars)?)
        } else {
            None
        };
        let constructor_name =
            match_constructor_name(&owner, resolved_pattern_owner.as_ref(), &arm.pattern)?;
        let (constructor, payload_type) = instantiated_constructor(env, &owner, constructor_name)?;
        if !seen.insert(constructor.clone()) {
            return Err(MagError::Type(format!(
                "duplicate match arm for {}",
                arm.pattern
            )));
        }
        let mut arm_locals = locals.clone();
        add_local(&mut arm_locals, arm.binding.clone(), payload_type)?;
        let body_type = infer(env, &mut arm_locals, &arm.body)?;
        if result.as_ref() == Some(&MagType::Never) && body_type != MagType::Never {
            result = Some(body_type);
        } else if let Some(current) = &result {
            compatible(env, &body_type, current, &mut HashMap::new()).map_err(|_| {
                MagError::Type(format!(
                    "match arms must return one compatible type, got {current} and {body_type}"
                ))
            })?;
        } else {
            result = Some(body_type);
        }
    }
    ensure_adt_exhaustive(&constructors, &seen)?;
    Ok(result.unwrap_or(MagType::Unit))
}

fn require_equality_admissible(env: &Env, ty: &MagType) -> Result<(), MagError> {
    let mut variables = HashSet::new();
    equality_admissible_in(env, ty, &mut variables, &mut HashSet::new()).map_err(MagError::Type)?;
    record_equality_variables(variables);
    Ok(())
}

fn equality_admissible_in(
    env: &Env,
    ty: &MagType,
    variables: &mut HashSet<String>,
    visiting: &mut HashSet<MagType>,
) -> Result<(), String> {
    match ty {
        MagType::Var(name) => {
            variables.insert(name.clone());
            Ok(())
        }
        MagType::Unit
        | MagType::Bool
        | MagType::Int
        | MagType::Float
        | MagType::String
        | MagType::JsonValue
        | MagType::Never
        | MagType::EmptyList => Ok(()),
        MagType::List(item) | MagType::Set(item) => {
            equality_admissible_in(env, item, variables, visiting)
        }
        MagType::Map(key, value) => {
            equality_admissible_in(env, key, variables, visiting)?;
            equality_admissible_in(env, value, variables, visiting)
        }
        MagType::Product(items) => items
            .iter()
            .try_for_each(|item| equality_admissible_in(env, item, variables, visiting)),
        MagType::Named(name, arguments) => {
            for argument in arguments {
                equality_admissible_in(env, argument, variables, visiting)?;
            }
            if !visiting.insert(ty.clone()) {
                return Ok(());
            }
            let declaration = env
                .type_decl(name)
                .ok_or_else(|| format!("unknown nominal type {name}"))?;
            let substitution = declaration
                .params
                .iter()
                .cloned()
                .zip(arguments.iter().cloned())
                .collect::<HashMap<_, _>>();
            let result = match declaration.body {
                TypeDeclBody::Fields(crate::ast::FieldTypes(fields)) => {
                    fields.values().try_for_each(|field| {
                        equality_admissible_in(
                            env,
                            &substitute(field, &substitution),
                            variables,
                            visiting,
                        )
                    })
                }
                TypeDeclBody::TransparentAlias(body) | TypeDeclBody::Alias(body) => {
                    equality_admissible_in(
                        env,
                        &substitute(&body, &substitution),
                        variables,
                        visiting,
                    )
                }
                TypeDeclBody::Adt(constructors) => {
                    constructors.iter().try_for_each(|constructor| {
                        equality_admissible_in(
                            env,
                            &substitute(&constructor.payload, &substitution),
                            variables,
                            visiting,
                        )
                    })
                }
                TypeDeclBody::Native => Err(format!(
                    "type {ty} has opaque native behavior and does not support equality"
                )),
            };
            visiting.remove(ty);
            result
        }
        MagType::Function(_, _) => Err(format!("function type {ty} does not support equality")),
        MagType::Artifact
        | MagType::TypeDescriptor
        | MagType::TypeSchema
        | MagType::SemanticTypeId
        | MagType::PackedValue
        | MagType::HostInputs
        | MagType::TypeTag(_) => Err(format!("type {ty} does not support equality")),
    }
}

fn validate_equality_requirements(
    env: &Env,
    requirements: &[MagType],
    substitution: &HashMap<String, MagType>,
) -> Result<(), MagError> {
    for requirement in requirements {
        require_equality_admissible(env, &substitute(requirement, substitution))?;
    }
    Ok(())
}

fn infer_builtin(
    env: &Env,
    locals: &mut Locals,
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
        "__map_empty" => {
            if !(1..=2).contains(&args.len()) {
                return Err(MagError::Type(format!(
                    "__map_empty expects 1-2 arguments, got {}",
                    args.len()
                )));
            }
            let key = match infer(env, locals, &args[0])? {
                MagType::TypeTag(key) => *key,
                actual => {
                    return Err(MagError::Type(format!(
                        "__map_empty expects TypeTag keys, got {actual}"
                    )))
                }
            };
            require_equality_admissible(env, &key)?;
            let value = if let Some(value) = args.get(1) {
                match infer(env, locals, value)? {
                    MagType::TypeTag(value) => *value,
                    actual => {
                        return Err(MagError::Type(format!(
                            "__map_empty expects TypeTag values, got {actual}"
                        )))
                    }
                }
            } else {
                MagType::Var("\0builtin.value".into())
            };
            Ok(MagType::Map(Box::new(key), Box::new(value)))
        }
        "__map_insert" | "__map_put" | "__map_get_or" | "__map_get" | "__map_contains" => {
            exact(
                if matches!(name, "__map_insert" | "__map_put" | "__map_get_or") {
                    3
                } else {
                    2
                },
            )?;
            let target = infer(env, locals, &args[0])?;
            let MagType::Map(key, value) = target else {
                return Err(MagError::Type(format!("{name} expects Map, got {target}")));
            };
            require_equality_admissible(env, &key)?;
            let actual_key = infer(env, locals, &args[1])?;
            compatible(env, &actual_key, &key, &mut HashMap::new()).map_err(MagError::Type)?;
            if matches!(name, "__map_insert" | "__map_put" | "__map_get_or") {
                let actual_value = infer(env, locals, &args[2])?;
                compatible(env, &actual_value, &value, &mut HashMap::new())
                    .map_err(MagError::Type)?;
                if name == "__map_get_or" {
                    Ok(*value)
                } else {
                    Ok(MagType::Map(key, value))
                }
            } else if name == "__map_get" {
                Ok(*value)
            } else {
                Ok(MagType::Bool)
            }
        }
        "__map_union_left" => {
            exact(2)?;
            let left = infer(env, locals, &args[0])?;
            let right = infer(env, locals, &args[1])?;
            compatible(env, &right, &left, &mut HashMap::new()).map_err(MagError::Type)?;
            let MagType::Map(key, value) = left else {
                return Err(MagError::Type(format!(
                    "__map_union_left expects Map, got {left}"
                )));
            };
            require_equality_admissible(env, &key)?;
            Ok(MagType::Map(key, value))
        }
        "__map_count" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Map(key, _) => {
                    require_equality_admissible(env, &key)?;
                    Ok(MagType::Int)
                }
                actual => Err(MagError::Type(format!(
                    "__map_count expects Map, got {actual}"
                ))),
            }
        }
        "__set_empty" => {
            exact(1)?;
            let item = match infer(env, locals, &args[0])? {
                MagType::TypeTag(item) => *item,
                actual => {
                    return Err(MagError::Type(format!(
                        "__set_empty expects TypeTag, got {actual}"
                    )))
                }
            };
            require_equality_admissible(env, &item)?;
            Ok(MagType::Set(Box::new(item)))
        }
        "__set_insert" | "__set_contains" => {
            exact(2)?;
            let target = infer(env, locals, &args[0])?;
            let MagType::Set(item) = target else {
                return Err(MagError::Type(format!("{name} expects Set, got {target}")));
            };
            require_equality_admissible(env, &item)?;
            let actual = infer(env, locals, &args[1])?;
            compatible(env, &actual, &item, &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "__set_insert" {
                Ok(MagType::Set(item))
            } else {
                Ok(MagType::Bool)
            }
        }
        "__set_count" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Set(item) => {
                    require_equality_admissible(env, &item)?;
                    Ok(MagType::Int)
                }
                actual => Err(MagError::Type(format!(
                    "__set_count expects Set, got {actual}"
                ))),
            }
        }
        "get" => {
            exact(2)?;
            let target = infer(env, locals, &args[0])?;
            if matches!(target, MagType::HostInputs | MagType::Map(_, _)) {
                return Err(MagError::Type(format!(
                    "get expects a record, got {target}"
                )));
            }
            let key_type = infer(env, locals, &args[1])?;
            compatible(env, &key_type, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
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
            if matches!(target, MagType::Map(_, _)) {
                return Err(MagError::Type("assoc expects a record".into()));
            }
            let key_type = infer(env, locals, &args[1])?;
            compatible(env, &key_type, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
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
            let actual = infer(env, locals, &args[0])?;
            if matches!(actual, MagType::List(_) | MagType::String)
                || named_field_types(env, &actual).is_some()
            {
                Ok(MagType::Int)
            } else {
                Err(MagError::Type(format!(
                    "count expects a collection, got {actual}"
                )))
            }
        }
        "=" => {
            exact(2)?;
            let left = infer(env, locals, &args[0])?;
            let right = infer(env, locals, &args[1])?;
            compatible(env, &left, &right, &mut HashMap::new()).map_err(MagError::Type)?;
            require_equality_admissible(env, &left)?;
            require_equality_admissible(env, &right)?;
            Ok(MagType::Bool)
        }
        "not" => {
            exact(1)?;
            let actual = infer(env, locals, &args[0])?;
            compatible(env, &actual, &MagType::Bool, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "int_mul" | "int_gt" => {
            exact(2)?;
            for argument in args {
                let actual = infer(env, locals, argument)?;
                compatible(env, &actual, &MagType::Int, &mut HashMap::new())
                    .map_err(MagError::Type)?;
            }
            if name == "int_mul" {
                Ok(MagType::Int)
            } else {
                Ok(MagType::Bool)
            }
        }
        "host_input" => {
            exact(2)?;
            let key = infer(env, locals, &args[0])?;
            compatible(env, &key, &MagType::String, &mut HashMap::new()).map_err(MagError::Type)?;
            match infer(env, locals, &args[1])? {
                MagType::TypeTag(expected) => Ok(*expected),
                actual => Err(MagError::Type(format!(
                    "host_input expects TypeTag, got {actual}"
                ))),
            }
        }
        "type_evidence" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::TypeTag(_) => Ok(MagType::TypeDescriptor),
                actual => Err(MagError::Type(format!(
                    "type_evidence expects TypeTag, got {actual}"
                ))),
            }
        }
        "type_schema" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::TypeTag(_) => Ok(MagType::TypeSchema),
                actual => Err(MagError::Type(format!(
                    "type_schema expects TypeTag, got {actual}"
                ))),
            }
        }
        "type_constructor" => {
            exact(1)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::String)
        }
        "adt_constructor_payload" => {
            exact(2)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            let name = infer(env, locals, &args[1])?;
            compatible(env, &name, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::TypeDescriptor)
        }
        "type_arguments" | "type_components" => {
            exact(1)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::List(Box::new(MagType::TypeDescriptor)))
        }
        "list_type" => {
            exact(1)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::TypeDescriptor)
        }
        "descriptor_schema" => {
            exact(1)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            Ok(MagType::TypeSchema)
        }
        "type_id" => {
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
        "value_type_id" => {
            exact(2)?;
            let actual = infer(env, locals, &args[0])?;
            match infer(env, locals, &args[1])? {
                MagType::TypeTag(expected) => {
                    compatible(env, &expected, &actual, &mut HashMap::new())
                        .map_err(MagError::Type)?;
                }
                MagType::TypeDescriptor => {}
                _ => {
                    return Err(MagError::Type(
                        "value_type_id expects TypeTag or TypeDescriptor evidence".into(),
                    ))
                }
            }
            Ok(MagType::SemanticTypeId)
        }
        "value_type_evidence" => {
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
            Ok(MagType::TypeDescriptor)
        }
        "or" => {
            exact(2)?;
            let a = infer(env, locals, &args[0])?;
            let b = infer(env, locals, &args[1])?;
            compatible(env, &a, &b, &mut HashMap::new()).map_err(|_| {
                MagError::Type(format!(
                    "or operands must return one compatible type, got {a} and {b}"
                ))
            })?;
            Ok(b)
        }
        "str" => Ok(MagType::String),
        "canonical" => {
            exact(1)?;
            let _ = infer(env, locals, &args[0])?;
            Ok(MagType::String)
        }
        "function_name" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::Function(_, _) => Ok(MagType::String),
                actual => Err(MagError::Type(format!(
                    "function_name expects a function, got {actual}"
                ))),
            }
        }
        "conforms" => {
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
        "packed_empty_record" => {
            exact(1)?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::PackedValue, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::Bool)
        }
        "packed_path_strings" => {
            exact(3)?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::PackedValue, &mut HashMap::new())
                .map_err(MagError::Type)?;
            let path = infer(env, locals, &args[1])?;
            compatible(
                env,
                &path,
                &MagType::List(Box::new(MagType::String)),
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            let list = infer(env, locals, &args[2])?;
            compatible(env, &list, &MagType::Bool, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(MagType::List(Box::new(MagType::String)))
        }
        "packed_record_has_only_key" | "packed_record_has_only_keys" | "packed_field_conforms" => {
            exact(if name == "packed_field_conforms" {
                3
            } else {
                2
            })?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::PackedValue, &mut HashMap::new())
                .map_err(MagError::Type)?;
            let key = infer(env, locals, &args[1])?;
            let expected_key = if name == "packed_record_has_only_keys" {
                MagType::List(Box::new(MagType::String))
            } else {
                MagType::String
            };
            compatible(env, &key, &expected_key, &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "packed_field_conforms" {
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
        "descriptor_accepts"
        | "descriptor_accepts_value"
        | "descriptor_input_covered_by"
        | "descriptor_input_assignments"
        | "descriptor_output_covered_by" => {
            exact(2)?;
            let descriptor = infer(env, locals, &args[0])?;
            compatible(
                env,
                &descriptor,
                &MagType::TypeDescriptor,
                &mut HashMap::new(),
            )
            .map_err(MagError::Type)?;
            let expected = if matches!(name, "descriptor_accepts" | "descriptor_accepts_value") {
                MagType::TypeDescriptor
            } else {
                MagType::List(Box::new(MagType::TypeDescriptor))
            };
            let value = infer(env, locals, &args[1])?;
            compatible(env, &value, &expected, &mut HashMap::new()).map_err(MagError::Type)?;
            if name == "descriptor_input_assignments" {
                Ok(MagType::List(Box::new(MagType::Int)))
            } else {
                Ok(MagType::Bool)
            }
        }
        "descriptor_table" => {
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
        "read_json" => {
            exact(1)?;
            let path = infer(env, locals, &args[0])?;
            compatible(env, &path, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::JsonValue)
        }
        "artifact" => {
            exact(1)?;
            Ok(MagType::Artifact)
        }
        "strip_margin" => {
            exact(1)?;
            let value = infer(env, locals, &args[0])?;
            compatible(env, &value, &MagType::String, &mut HashMap::new())
                .map_err(MagError::Type)?;
            Ok(MagType::String)
        }
        "replace" => {
            exact(3)?;
            for argument in args {
                let value = infer(env, locals, argument)?;
                compatible(env, &value, &MagType::String, &mut HashMap::new())
                    .map_err(MagError::Type)?;
            }
            Ok(MagType::String)
        }
        "concat" => {
            exact(2)?;
            let a = infer(env, locals, &args[0])?;
            let b = infer(env, locals, &args[1])?;
            compatible(env, &a, &b, &mut HashMap::new()).map_err(MagError::Type)?;
            Ok(a)
        }
        "remove_at" => {
            exact(2)?;
            let collection = infer(env, locals, &args[0])?;
            let index = infer(env, locals, &args[1])?;
            compatible(env, &index, &MagType::Int, &mut HashMap::new()).map_err(MagError::Type)?;
            match collection {
                MagType::List(_) => Ok(collection),
                actual => Err(MagError::Type(format!(
                    "remove_at expects List, got {actual}"
                ))),
            }
        }
        "keys" => {
            exact(1)?;
            let actual = infer(env, locals, &args[0])?;
            if named_field_types(env, &actual).is_some() {
                Ok(MagType::List(Box::new(MagType::String)))
            } else {
                Err(MagError::Type(format!(
                    "keys expects a named value, got {actual}"
                )))
            }
        }
        "first" => {
            exact(1)?;
            match infer(env, locals, &args[0])? {
                MagType::List(item) => Ok(*item),
                actual => Err(MagError::Type(format!("first expects List, got {actual}"))),
            }
        }
        "map" | "filter" | "flat_map" | "sort_by" | "group_by" => {
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
            } else if name == "sort_by" || name == "group_by" {
                compatible(env, &result, &MagType::String, &mut HashMap::new())
                    .map_err(MagError::Type)?;
                if name == "group_by" {
                    Ok(MagType::Map(
                        Box::new(MagType::String),
                        Box::new(MagType::List(Box::new(item))),
                    ))
                } else {
                    Ok(MagType::List(Box::new(item)))
                }
            } else if name == "flat_map" {
                match *result {
                    MagType::List(_) => Ok(*result),
                    actual => Err(MagError::Type(format!(
                        "flat_map callback must return List, got {actual}"
                    ))),
                }
            } else {
                Ok(MagType::List(result))
            }
        }
        "indexed_map" => {
            exact(2)?;
            let fun = infer(env, locals, &args[0])?;
            let collection = infer(env, locals, &args[1])?;
            let item = match collection {
                MagType::List(t) => *t,
                _ => return Err(MagError::Type("indexed_map expects List".into())),
            };
            let (params, result) = match fun {
                MagType::Function(p, r) => (p, r),
                _ => return Err(MagError::Type("indexed_map expects function".into())),
            };
            if params.len() != 2 {
                return Err(MagError::Type(
                    "indexed_map callback expects 2 parameters".into(),
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

#[derive(Clone)]
struct CheckedCandidate {
    id: BindingId,
    ty: MagType,
    generic_binders: Vec<String>,
    parameter_names: Option<Vec<String>>,
    contributes_type_vars: bool,
}

type CheckedScope = HashMap<String, Vec<CheckedCandidate>>;
const TYPE_BINDER_SCOPE_KEY: &str = "\0type-binders";

pub(crate) const BUILTIN_NAMES: &[&str] = &[
    "__map_empty",
    "__map_insert",
    "__map_put",
    "__map_get",
    "__map_get_or",
    "__map_contains",
    "__map_count",
    "__map_union_left",
    "__set_empty",
    "__set_insert",
    "__set_contains",
    "__set_count",
    "str",
    "strip_margin",
    "replace",
    "map",
    "group_by",
    "indexed_map",
    "filter",
    "flat_map",
    "fold",
    "concat",
    "get",
    "assoc",
    "keys",
    "count",
    "first",
    "canonical",
    "function_name",
    "sort_by",
    "remove_at",
    "conforms",
    "or",
    "not",
    "int_mul",
    "int_gt",
    "=",
    "fail",
    "type_evidence",
    "read",
    "read_json",
    "require",
    "artifact",
    "type_schema",
    "type_constructor",
    "adt_constructor_payload",
    "type_arguments",
    "type_components",
    "list_type",
    "descriptor_schema",
    "type_id",
    "value_type_id",
    "value_type_evidence",
    "pack",
    "packed_empty_record",
    "packed_path_strings",
    "packed_record_has_only_key",
    "packed_record_has_only_keys",
    "packed_field_conforms",
    "descriptor_accepts",
    "descriptor_accepts_value",
    "descriptor_input_covered_by",
    "descriptor_input_assignments",
    "descriptor_output_covered_by",
    "descriptor_table",
    "host_input",
];

fn builtin_overload_types(name: &str, candidate: Option<&MagType>) -> Vec<MagType> {
    let var = |name: &str| MagType::Var(format!("\0builtin.{name}"));
    let function =
        |params: Vec<MagType>, result: MagType| MagType::Function(params, Box::new(result));
    let list = |item: MagType| MagType::List(Box::new(item));
    let set = |item: MagType| MagType::Set(Box::new(item));
    let map = |key: MagType, value: MagType| MagType::Map(Box::new(key), Box::new(value));
    let tag = |value: MagType| MagType::TypeTag(Box::new(value));
    let descriptor = MagType::TypeDescriptor;
    let packed = MagType::PackedValue;
    let mut signatures = match name {
        "__map_empty" => vec![
            function(vec![tag(var("key"))], map(var("key"), var("value"))),
            function(
                vec![tag(var("key")), tag(var("value"))],
                map(var("key"), var("value")),
            ),
        ],
        "__map_insert" => vec![function(
            vec![map(var("key"), var("value")), var("key"), var("value")],
            map(var("key"), var("value")),
        )],
        "__map_put" => vec![function(
            vec![map(var("key"), var("value")), var("key"), var("value")],
            map(var("key"), var("value")),
        )],
        "__map_get_or" => vec![function(
            vec![map(var("key"), var("value")), var("key"), var("value")],
            var("value"),
        )],
        "__map_get" => vec![function(
            vec![map(var("key"), var("value")), var("key")],
            var("value"),
        )],
        "__map_contains" => vec![function(
            vec![map(var("key"), var("value")), var("key")],
            MagType::Bool,
        )],
        "__map_count" => vec![function(vec![map(var("key"), var("value"))], MagType::Int)],
        "__map_union_left" => vec![function(
            vec![map(var("key"), var("value")), map(var("key"), var("value"))],
            map(var("key"), var("value")),
        )],
        "__set_empty" => vec![function(vec![tag(var("item"))], set(var("item")))],
        "__set_insert" => vec![function(
            vec![set(var("item")), var("item")],
            set(var("item")),
        )],
        "__set_contains" => vec![function(vec![set(var("item")), var("item")], MagType::Bool)],
        "__set_count" => vec![function(vec![set(var("item"))], MagType::Int)],
        "str" => candidate
            .and_then(|candidate| match candidate {
                MagType::Function(params, _) => {
                    Some(vec![function(params.clone(), MagType::String)])
                }
                _ => None,
            })
            .unwrap_or_default(),
        "strip_margin" => vec![function(vec![MagType::String], MagType::String)],
        "replace" => vec![function(
            vec![MagType::String, MagType::String, MagType::String],
            MagType::String,
        )],
        "count" => vec![
            function(vec![list(var("item"))], MagType::Int),
            function(vec![MagType::String], MagType::Int),
        ],
        "first" => vec![function(vec![list(var("item"))], var("item"))],
        "remove_at" => vec![function(
            vec![list(var("item")), MagType::Int],
            list(var("item")),
        )],
        "concat" => vec![
            function(
                vec![list(var("item")), list(var("item"))],
                list(var("item")),
            ),
            function(vec![MagType::String, MagType::String], MagType::String),
        ],
        "not" => vec![function(vec![MagType::Bool], MagType::Bool)],
        "int_mul" => vec![function(vec![MagType::Int, MagType::Int], MagType::Int)],
        "int_gt" => vec![function(vec![MagType::Int, MagType::Int], MagType::Bool)],
        "=" => vec![function(vec![var("value"), var("value")], MagType::Bool)],
        "host_input" => vec![function(
            vec![MagType::String, tag(var("value"))],
            var("value"),
        )],
        "type_evidence" => vec![function(vec![tag(var("value"))], descriptor)],
        "type_schema" => vec![function(vec![tag(var("value"))], MagType::TypeSchema)],
        "type_constructor" => vec![function(vec![descriptor.clone()], MagType::String)],
        "adt_constructor_payload" => vec![function(
            vec![descriptor.clone(), MagType::String],
            descriptor,
        )],
        "type_arguments" | "type_components" => {
            vec![function(vec![descriptor], list(MagType::TypeDescriptor))]
        }
        "list_type" => vec![function(vec![descriptor], MagType::TypeDescriptor)],
        "descriptor_schema" => vec![function(vec![descriptor], MagType::TypeSchema)],
        "type_id" => vec![function(vec![descriptor], MagType::SemanticTypeId)],
        "value_type_id" => vec![
            function(
                vec![var("value"), tag(var("value"))],
                MagType::SemanticTypeId,
            ),
            function(
                vec![var("value"), descriptor.clone()],
                MagType::SemanticTypeId,
            ),
        ],
        "value_type_evidence" => vec![function(
            vec![var("value"), descriptor.clone()],
            descriptor.clone(),
        )],
        "canonical" => vec![function(vec![var("value")], MagType::String)],
        "function_name" => vec![function(
            vec![function(vec![var("input")], var("output"))],
            MagType::String,
        )],
        "conforms" => vec![function(vec![var("value"), descriptor], MagType::Bool)],
        "fail" => vec![function(vec![var("value")], MagType::Never)],
        "pack" => vec![function(vec![var("value")], packed.clone())],
        "packed_empty_record" => vec![function(vec![packed.clone()], MagType::Bool)],
        "packed_path_strings" => vec![function(
            vec![packed.clone(), list(MagType::String), MagType::Bool],
            list(MagType::String),
        )],
        "packed_record_has_only_key" => vec![function(
            vec![packed.clone(), MagType::String],
            MagType::Bool,
        )],
        "packed_record_has_only_keys" => vec![function(
            vec![packed.clone(), list(MagType::String)],
            MagType::Bool,
        )],
        "packed_field_conforms" => vec![function(
            vec![packed, MagType::String, descriptor],
            MagType::Bool,
        )],
        "descriptor_accepts" | "descriptor_accepts_value" => vec![function(
            vec![descriptor.clone(), descriptor.clone()],
            MagType::Bool,
        )],
        "descriptor_input_covered_by" | "descriptor_output_covered_by" => vec![function(
            vec![descriptor.clone(), list(descriptor.clone())],
            MagType::Bool,
        )],
        "descriptor_input_assignments" => vec![function(
            vec![descriptor.clone(), list(descriptor.clone())],
            list(MagType::Int),
        )],
        "descriptor_table" => vec![function(
            vec![list(descriptor.clone())],
            map(MagType::String, descriptor.clone()),
        )],
        "read" => vec![
            function(vec![MagType::String], MagType::String),
            function(vec![MagType::String, var("fallback")], MagType::String),
        ],
        "read_json" => vec![function(vec![MagType::String], MagType::JsonValue)],
        "require" => vec![function(vec![MagType::String], MagType::Unit)],
        "artifact" => vec![function(vec![var("value")], MagType::Artifact)],
        "or" => vec![function(vec![var("value"), var("value")], var("value"))],
        "keys" => vec![function(vec![var("named")], list(MagType::String))],
        "get" => vec![function(vec![var("named"), MagType::String], var("field"))],
        "assoc" => vec![function(
            vec![var("named"), MagType::String, var("field")],
            var("named"),
        )],
        "map" => vec![function(
            vec![
                function(vec![var("item")], var("result")),
                list(var("item")),
            ],
            list(var("result")),
        )],
        "group_by" => vec![function(
            vec![
                function(vec![var("item")], MagType::String),
                list(var("item")),
            ],
            map(MagType::String, list(var("item"))),
        )],
        "filter" => vec![function(
            vec![
                function(vec![var("item")], MagType::Bool),
                list(var("item")),
            ],
            list(var("item")),
        )],
        "flat_map" => vec![function(
            vec![
                function(vec![var("item")], list(var("result"))),
                list(var("item")),
            ],
            list(var("result")),
        )],
        "sort_by" => vec![function(
            vec![
                function(vec![var("item")], MagType::String),
                list(var("item")),
            ],
            list(var("item")),
        )],
        "indexed_map" => vec![function(
            vec![
                function(vec![MagType::Int, var("item")], var("result")),
                list(var("item")),
            ],
            list(var("result")),
        )],
        "fold" => vec![function(
            vec![
                function(vec![var("state"), var("item")], var("state")),
                var("state"),
                list(var("item")),
            ],
            var("state"),
        )],
        _ => Vec::new(),
    };
    if let Some(MagType::Function(params, _)) = candidate {
        if name == "or" && params.len() == 2 && params[0] == params[1] {
            signatures.push(function(params.clone(), params[0].clone()));
        }
    }
    signatures
}

fn collides_with_builtin(env: &Env, name: &str, candidate: &MagType) -> bool {
    builtin_overload_types(name, Some(candidate))
        .iter()
        .any(|builtin| {
            let bindable = internal_type_variables(builtin);
            compatible_with_bindable(env, candidate, builtin, &mut HashMap::new(), &bindable)
                .is_ok()
        })
}

// A function's constraints must survive value transport (records, conditionals,
// returned callbacks), not only a direct call through its original name.
fn check_equality_specialization(
    env: &Env,
    expression: &CheckedExpr,
    expected: &MagType,
    outer: &HashMap<String, MagType>,
) -> Result<(), MagError> {
    let mut substitutions = outer.clone();
    let original = substitute(&expression.ty, outer);
    let expected = substitute(expected, outer);
    let mut bindable = HashSet::new();
    collect_vars(&original, &mut bindable);
    // This mapping is local evidence propagation; ordinary checking has already
    // established compatibility and owns diagnostics for mismatched types.
    let _ = compatible_with_bindable(env, &expected, &original, &mut substitutions, &bindable);
    let resolved = substitute(&expression.ty, &substitutions);
    match &expression.kind {
        CheckedExprKind::BindingRef(id) => {
            for requirement in resolved_binding_requirements(env, *id, &resolved) {
                require_equality_admissible(env, &substitute(&requirement, &substitutions))?;
            }
        }
        CheckedExprKind::Function(function) => {
            for parameter in &function.equality_params {
                require_equality_admissible(
                    env,
                    &substitute(&MagType::Var(parameter.clone()), &substitutions),
                )?;
            }
            for binding in &function.body.bindings {
                check_equality_specialization(
                    env,
                    &binding.initializer,
                    &binding.ty,
                    &substitutions,
                )?;
            }
            for expression in &function.body.expressions {
                check_equality_specialization(env, expression, &expression.ty, &substitutions)?;
            }
        }
        CheckedExprKind::Call { callee, args, .. } => {
            let signature = substitute(&callee.ty, &substitutions);
            check_equality_specialization(env, callee, &signature, &substitutions)?;
            if let MagType::Function(params, _) = signature {
                for (argument, parameter) in args.iter().zip(params) {
                    check_equality_specialization(env, argument, &parameter, &substitutions)?;
                }
            }
        }
        CheckedExprKind::Vector(items) => {
            for (index, item) in items.iter().enumerate() {
                let expected = match &resolved {
                    MagType::List(item) => item.as_ref(),
                    MagType::Product(items) => items.get(index).unwrap_or(&item.ty),
                    _ => &item.ty,
                };
                check_equality_specialization(env, item, expected, &substitutions)?;
            }
        }
        CheckedExprKind::Fields(fields) => {
            for (name, field) in fields {
                let expected = named_field_types(env, &resolved)
                    .and_then(|fields| fields.get(name).cloned())
                    .unwrap_or_else(|| field.ty.clone());
                check_equality_specialization(env, field, &expected, &substitutions)?;
            }
        }
        CheckedExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_equality_specialization(env, condition, &MagType::Bool, &substitutions)?;
            check_equality_specialization(env, then_branch, &resolved, &substitutions)?;
            check_equality_specialization(env, else_branch, &resolved, &substitutions)?;
        }
        CheckedExprKind::Ascribe { value, .. } => {
            check_equality_specialization(env, value, &resolved, &substitutions)?
        }
        CheckedExprKind::Construct { payload, .. } => {
            check_equality_specialization(env, payload, &payload.ty, &substitutions)?
        }
        CheckedExprKind::Match { value, arms } => {
            check_equality_specialization(env, value, &value.ty, &substitutions)?;
            for arm in arms {
                check_equality_specialization(env, &arm.body, &resolved, &substitutions)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_called_bindings(expression: &CheckedExpr, calls: &mut Vec<(BindingId, MagType)>) {
    match &expression.kind {
        CheckedExprKind::Call { callee, args, .. } => {
            if let CheckedExprKind::BindingRef(id) = callee.kind {
                calls.push((id, callee.ty.clone()));
            }
            collect_called_bindings(callee, calls);
            for argument in args {
                collect_called_bindings(argument, calls);
            }
        }
        CheckedExprKind::Vector(items) => {
            for item in items {
                collect_called_bindings(item, calls);
            }
        }
        CheckedExprKind::Fields(fields) => {
            for (_, value) in fields {
                collect_called_bindings(value, calls);
            }
        }
        CheckedExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_called_bindings(condition, calls);
            collect_called_bindings(then_branch, calls);
            collect_called_bindings(else_branch, calls);
        }
        CheckedExprKind::Construct { payload, .. }
        | CheckedExprKind::Ascribe { value: payload, .. } => {
            collect_called_bindings(payload, calls);
        }
        CheckedExprKind::Match { value, arms } => {
            collect_called_bindings(value, calls);
            for arm in arms {
                collect_called_bindings(&arm.body, calls);
            }
        }
        CheckedExprKind::Function(function) => {
            collect_called_bindings_in_block(&function.body, calls)
        }
        CheckedExprKind::Unit
        | CheckedExprKind::Str(_)
        | CheckedExprKind::Int(_)
        | CheckedExprKind::Float(_)
        | CheckedExprKind::Bool(_)
        | CheckedExprKind::Keyword(_)
        | CheckedExprKind::BindingRef(_)
        | CheckedExprKind::TypeTag(_) => {}
    }
}

fn collect_called_bindings_in_block(block: &CheckedBlock, calls: &mut Vec<(BindingId, MagType)>) {
    for binding in &block.bindings {
        collect_called_bindings(&binding.initializer, calls);
    }
    for expression in &block.expressions {
        collect_called_bindings(expression, calls);
    }
}

fn resolved_binding_requirements(env: &Env, id: BindingId, resolved: &MagType) -> Vec<MagType> {
    let mut requirements = env.equality_requirements(id);
    if requirements.is_empty() {
        if let Ok(Value::Fn(function)) = env.ready_binding(id) {
            requirements = function
                .equality_params
                .iter()
                .cloned()
                .map(MagType::Var)
                .collect();
        }
    }
    if requirements.is_empty() {
        return requirements;
    }
    let Some(original) = env.binding_metadata(id).and_then(|metadata| metadata.ty) else {
        return requirements;
    };
    let mut variables = HashSet::new();
    collect_vars(&original, &mut variables);
    let mut substitution = HashMap::new();
    if compatible_with_bindable(env, resolved, &original, &mut substitution, &variables).is_err() {
        return requirements;
    }
    requirements
        .iter()
        .map(|requirement| substitute(requirement, &substitution))
        .collect()
}

fn finalize_block_equality_requirements(
    env: &Env,
    bindings: &mut [CheckedBinding],
) -> Result<(), MagError> {
    let mut changed = true;
    while changed {
        changed = false;
        for binding in bindings.iter() {
            let CheckedExprKind::Function(function) = &binding.initializer.kind else {
                let (_, variables) = with_equality_obligation_scope(|| {
                    check_equality_specialization(
                        env,
                        &binding.initializer,
                        &binding.ty,
                        &HashMap::new(),
                    )
                })?;
                let mut requirements = variables.into_iter().map(MagType::Var).collect::<Vec<_>>();
                requirements.sort_by_key(ToString::to_string);
                if requirements != env.equality_requirements(binding.id) {
                    env.set_equality_requirements(binding.id, requirements);
                    changed = true;
                }
                continue;
            };
            let mut required = env
                .equality_requirements(binding.id)
                .into_iter()
                .collect::<HashSet<_>>();
            let (_, variables) = with_equality_obligation_scope(|| {
                check_equality_specialization(
                    env,
                    &binding.initializer,
                    &binding.ty,
                    &HashMap::new(),
                )
            })?;
            required.extend(
                variables
                    .into_iter()
                    .filter(|name| function.type_params.contains(name))
                    .map(MagType::Var),
            );
            let mut calls = Vec::new();
            collect_called_bindings_in_block(&function.body, &mut calls);
            for (callee, resolved) in calls {
                for requirement in resolved_binding_requirements(env, callee, &resolved) {
                    let mut variables = HashSet::new();
                    equality_admissible_in(env, &requirement, &mut variables, &mut HashSet::new())
                        .map_err(MagError::Type)?;
                    required.extend(variables.into_iter().map(MagType::Var));
                }
            }
            let mut required = required.into_iter().collect::<Vec<_>>();
            required.sort_by_key(ToString::to_string);
            if required != env.equality_requirements(binding.id) {
                env.set_equality_requirements(binding.id, required);
                changed = true;
            }
        }
    }
    for binding in bindings {
        let CheckedExprKind::Function(function) = &binding.initializer.kind else {
            continue;
        };
        let mut updated = function.as_ref().clone();
        updated.equality_params = env
            .equality_requirements(binding.id)
            .into_iter()
            .filter_map(|requirement| match requirement {
                MagType::Var(name) if updated.type_params.contains(&name) => Some(name),
                _ => None,
            })
            .collect();
        binding.initializer = Arc::new(checked(
            binding.ty.clone(),
            CheckedExprKind::Function(Arc::new(updated)),
        ));
    }
    Ok(())
}

/// Resolves a source block into typed expressions whose authored references
/// point at stable binding identities. Evaluation never has to repeat name or
/// overload resolution.
pub fn compile_block(env: &Env, expressions: &[BlockItem]) -> Result<CheckedBlock, MagError> {
    compile_block_in(env, &[], expressions, None)
}

fn compile_block_in(
    env: &Env,
    outer: &[CheckedScope],
    expressions: &[BlockItem],
    result_expected: Option<&MagType>,
) -> Result<CheckedBlock, MagError> {
    let mut declarations = Vec::new();
    for expression in expressions {
        if let Some(declaration) = direct_let(expression)? {
            declarations.push(declaration);
        }
    }
    let allocated = declarations
        .iter()
        .map(|(name, initializer)| (*name, *initializer, env.allocate_binding_id(name, None)))
        .collect::<Vec<_>>();
    let declared_names = allocated
        .iter()
        .map(|(name, _, _)| *name)
        .collect::<HashSet<_>>();

    let mut current = CheckedScope::new();
    let scoped_type_vars = visible_type_variables(outer);
    let mut pending = Vec::new();
    for (name, initializer, id) in &allocated {
        if is_fn(initializer) {
            let ty = infer_fn_signature_scoped(env, &scoped_type_vars, initializer)?;
            insert_checked_candidate(
                env,
                outer,
                &mut current,
                name,
                CheckedCandidate {
                    id: *id,
                    ty,
                    generic_binders: function_type_params(initializer)?,
                    parameter_names: function_parameter_names(initializer),
                    contributes_type_vars: false,
                },
            )?;
        } else {
            pending.push((*name, *initializer, *id));
        }
    }

    // Strict initializers are inferred as a dependency fixpoint. Function
    // bodies are intentionally not visited here: their signatures provide the
    // complete peer inventory before any body is checked.
    while !pending.is_empty() {
        let mut next = Vec::new();
        let mut progressed = false;
        for (name, initializer, id) in pending {
            let mut types = visible_types(env, outer, &current);
            let inferred = if let Expr::Name(source) = initializer {
                let candidates = visible_candidates(env, outer, &current, source);
                match candidates.as_slice() {
                    [candidate] if !candidate.generic_binders.is_empty() => {
                        Ok(instantiate_candidate(candidate).0)
                    }
                    _ => infer_shape(env, &mut types, &scoped_type_vars, initializer),
                }
            } else {
                infer_shape(env, &mut types, &scoped_type_vars, initializer)
            };
            match inferred {
                Ok(ty) => {
                    insert_checked_candidate(
                        env,
                        outer,
                        &mut current,
                        name,
                        CheckedCandidate {
                            id,
                            ty,
                            generic_binders: vec![],
                            parameter_names: function_parameter_names(initializer),
                            contributes_type_vars: false,
                        },
                    )?;
                    progressed = true;
                }
                Err(MagError::Unresolved(symbol)) if declared_names.contains(symbol.as_str()) => {
                    next.push((name, initializer, id));
                }
                Err(error @ MagError::Unresolved(_)) => return Err(error),
                Err(error) => return Err(error),
            }
        }
        if !progressed {
            let names = next.iter().map(|(name, _, _)| *name).collect::<Vec<_>>();
            return Err(MagError::Type(format!(
                "cannot infer recursive strict bindings: {}",
                names.join(", ")
            )));
        }
        pending = next;
    }

    let mut scopes = outer.to_vec();
    scopes.push(current.clone());
    let mut bindings = Vec::with_capacity(allocated.len());
    for (name, initializer, id) in allocated {
        let candidate = current
            .get(name)
            .and_then(|candidates| candidates.iter().find(|candidate| candidate.id == id))
            .ok_or_else(|| MagError::Unresolved(name.into()))?;
        let checked = if let Expr::Function(function) = initializer {
            compile_function(
                env,
                &scopes,
                Some(name),
                function,
                Some(&candidate.ty),
                Some(id),
            )?
        } else {
            compile_expr(env, &scopes, initializer, Some(&candidate.ty))?
        };
        bindings.push(CheckedBinding {
            id,
            name: name.into(),
            ty: candidate.ty.clone(),
            initializer: Arc::new(checked),
        });
    }
    finalize_block_equality_requirements(env, &mut bindings)?;

    let last_expression = expressions
        .iter()
        .rposition(|item| matches!(item, BlockItem::Expr(_)));
    let mut checked_expressions = Vec::new();
    for (index, item) in expressions.iter().enumerate() {
        if let Some(expression) = block_expr(item)? {
            let expected = (Some(index) == last_expression)
                .then_some(result_expected)
                .flatten();
            let checked = match compile_expr(env, &scopes, expression, expected) {
                Ok(checked) => checked,
                Err(_) if expected.is_some() => compile_expr(env, &scopes, expression, None)?,
                Err(error) => return Err(error),
            };
            checked_expressions.push(checked);
        }
    }
    env.profile_counters(|counters| {
        counters.checked_bindings = counters
            .checked_bindings
            .saturating_add(bindings.len() as u64);
    });
    Ok(CheckedBlock {
        frame_layout: bindings.iter().map(|binding| binding.id).collect(),
        bindings,
        expressions: checked_expressions,
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_checked_candidate(
    env: &Env,
    outer: &[CheckedScope],
    current: &mut CheckedScope,
    name: &str,
    candidate: CheckedCandidate,
) -> Result<(), MagError> {
    let canonical = canonical_type(&candidate.ty);
    if visible_candidates(env, outer, current, name)
        .iter()
        .any(|candidate| canonical_type(&candidate.ty) == canonical)
        || collides_with_builtin(env, name, &candidate.ty)
    {
        return Err(MagError::Type(format!(
            "duplicate visible overload {name}: {}",
            candidate.ty
        )));
    }
    env.set_binding_type(candidate.id, candidate.ty.clone());
    current.entry(name.to_owned()).or_default().push(candidate);
    Ok(())
}

fn env_candidates(env: &Env, name: &str) -> Vec<CheckedCandidate> {
    env.lookup_candidate_ids(name)
        .into_iter()
        .filter_map(|id| {
            env.binding_metadata(id)
                .and_then(|metadata| metadata.ty)
                .map(|ty| {
                    let (generic_binders, parameter_names) = match env.ready_binding(id) {
                        Ok(Value::Fn(function)) => {
                            (function.type_params.clone(), Some(function.params.clone()))
                        }
                        _ => (vec![], None),
                    };
                    CheckedCandidate {
                        id,
                        ty,
                        generic_binders,
                        parameter_names,
                        contributes_type_vars: false,
                    }
                })
        })
        .collect()
}

fn builtin_id(env: &Env, name: &str) -> Option<BindingId> {
    env.lookup_candidate_ids(name).into_iter().find(
        |id| matches!(env.ready_binding(*id), Ok(Value::BuiltinFn(ref builtin)) if builtin == name),
    )
}

fn visible_candidates(
    env: &Env,
    scopes: &[CheckedScope],
    current: &CheckedScope,
    name: &str,
) -> Vec<CheckedCandidate> {
    let mut candidates = scopes
        .iter()
        .flat_map(|scope| scope.get(name).into_iter().flatten().cloned())
        .chain(current.get(name).into_iter().flatten().cloned())
        .chain(env_candidates(env, name))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.id));
    candidates
}

fn all_candidates(env: &Env, scopes: &[CheckedScope], name: &str) -> Vec<CheckedCandidate> {
    visible_candidates(env, scopes, &CheckedScope::new(), name)
}

fn visible_types(_env: &Env, scopes: &[CheckedScope], current: &CheckedScope) -> Locals {
    let mut types = Locals::new();
    for scope in scopes.iter().chain(std::iter::once(current)) {
        for (name, candidates) in scope {
            let entry = types.entry(name.clone()).or_default();
            for candidate in candidates {
                if !entry
                    .iter()
                    .any(|local: &LocalCandidate| local.ty == candidate.ty)
                {
                    entry.push(LocalCandidate {
                        ty: candidate.ty.clone(),
                        generic_binders: candidate.generic_binders.clone(),
                    });
                }
            }
        }
    }
    types
}

fn infer_shape(
    env: &Env,
    locals: &mut Locals,
    _scoped_type_vars: &HashSet<String>,
    expression: &Expr,
) -> Result<MagType, MagError> {
    infer(env, locals, expression)
}

fn compile_expr(
    env: &Env,
    scopes: &[CheckedScope],
    expression: &Expr,
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let value = match expression {
        Expr::Unit => checked(MagType::Unit, CheckedExprKind::Unit),
        Expr::Bool(value) => checked(MagType::Bool, CheckedExprKind::Bool(*value)),
        Expr::Int(value) => checked(MagType::Int, CheckedExprKind::Int(*value)),
        Expr::Float(value) => checked(MagType::Float, CheckedExprKind::Float(*value)),
        Expr::Str(value) => checked(MagType::String, CheckedExprKind::Str(value.clone())),
        Expr::Keyword(value) => checked(MagType::String, CheckedExprKind::Keyword(value.clone())),
        Expr::Name(name) => compile_symbol(env, scopes, name, expected)?,
        Expr::Vector(items) => compile_vector(env, scopes, items, expected)?,
        Expr::Fields(_) => {
            return Err(MagError::Type(
                "standalone record values are unsupported; use a named type or Map".into(),
            ))
        }
        Expr::If {
            condition,
            then_branch,
            else_branch,
        } => compile_if(env, scopes, condition, then_branch, else_branch, expected)?,
        Expr::Construct {
            owner,
            constructor,
            payload,
        } => compile_construct(env, scopes, owner, constructor, payload)?,
        Expr::Match { value, arms } => compile_match(env, scopes, value, arms, expected)?,
        Expr::Ascribe { target, value } => compile_ascribe(env, scopes, target, value)?,
        Expr::Annotate { target, value } => compile_annotate(env, scopes, target, value)?,
        Expr::TypeTag(target) => compile_type_tag(env, scopes, target)?,
        Expr::Function(function) => compile_function(env, scopes, None, function, expected, None)?,
        Expr::Call {
            callee,
            type_args,
            args,
        } => compile_call(env, scopes, callee, type_args.as_deref(), args, expected)?,
        Expr::Invalid(error) => return Err(error.clone().into_mag_error()),
    };
    if let Some(expected) = expected {
        compatible_static(env, &value.ty, expected, &mut HashMap::new()).map_err(MagError::Type)?;
    }
    check_equality_specialization(env, &value, expected.unwrap_or(&value.ty), &HashMap::new())?;
    env.profile_counters(|counters| {
        counters.checked_expressions = counters.checked_expressions.saturating_add(1);
    });
    Ok(value)
}

fn checked(ty: MagType, kind: CheckedExprKind) -> CheckedExpr {
    CheckedExpr { ty, kind }
}

fn compile_symbol(
    env: &Env,
    scopes: &[CheckedScope],
    name: &str,
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let candidates = all_candidates(env, scopes, name);
    let mut matches = candidates
        .iter()
        .filter_map(|candidate| {
            let (candidate_type, mut bindable) = instantiate_candidate(candidate);
            let resolved = if let Some(expected) = expected {
                bindable.extend(internal_type_variables(expected));
                let mut substitution = HashMap::new();
                if !candidate.generic_binders.is_empty() {
                    compatible_with_bindable(
                        env,
                        expected,
                        &candidate_type,
                        &mut substitution,
                        &bindable,
                    )
                    .ok()?;
                    substitute(&candidate_type, &substitution)
                } else {
                    compatible_with_bindable(
                        env,
                        &candidate_type,
                        expected,
                        &mut substitution,
                        &bindable,
                    )
                    .ok()?;
                    candidate_type
                }
            } else {
                candidate_type
            };
            Some((candidate, resolved))
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        let concrete = matches
            .iter()
            .filter(|(candidate, _)| candidate.generic_binders.is_empty())
            .count();
        if concrete == 1 {
            matches.retain(|(candidate, _)| candidate.generic_binders.is_empty());
        }
    }
    if expected.is_none() && matches.len() > 1 {
        let data = matches
            .iter()
            .filter(|(_, resolved)| !matches!(resolved, MagType::Function(_, _)))
            .cloned()
            .collect::<Vec<_>>();
        if data.len() == 1 {
            matches = data;
        }
    }
    match matches.as_slice() {
        [(candidate, resolved)] => Ok(checked(
            resolved.clone(),
            CheckedExprKind::BindingRef(candidate.id),
        )),
        [] if candidates.is_empty() => Err(MagError::Unresolved(name.into())),
        [] => Err(MagError::Type(format!(
            "no overload {name} matches {}",
            expected
                .map(ToString::to_string)
                .unwrap_or_else(|| "context".into())
        ))),
        _ => Err(MagError::Type(format!(
            "ambiguous overload {name}; candidates: {}",
            matches
                .iter()
                .map(|(_, resolved)| resolved.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn compile_vector(
    env: &Env,
    scopes: &[CheckedScope],
    items: &[Expr],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    if let Some(MagType::Product(_)) = expected {
        let checked_items = items
            .iter()
            .map(|item| compile_expr(env, scopes, item, None))
            .collect::<Result<Vec<_>, _>>()?;
        let actual = checked_items
            .iter()
            .map(|item| item.ty.clone())
            .collect::<Vec<_>>();
        return Ok(checked(
            MagType::Product(actual),
            CheckedExprKind::Vector(checked_items),
        ));
    }
    let expected_item = match expected {
        Some(MagType::List(item)) => Some(item.as_ref()),
        _ => None,
    };
    let mut checked_items = Vec::with_capacity(items.len());
    let mut item_type = expected_item.cloned();
    let mut substitution = HashMap::new();
    for item in items {
        let item_expected = expected_item.map(|ty| substitute(ty, &substitution));
        let value = compile_expr(env, scopes, item, item_expected.as_ref())?;
        if let Some(ty) = &item_expected {
            // Element constraints must survive the contextual list type: callers
            // infer their generic arguments from the vector's resulting type.
            compatible_static(env, &value.ty, ty, &mut substitution).map_err(MagError::Type)?;
            item_type = expected_item.map(|ty| substitute(ty, &substitution));
        } else if let Some(current) = &item_type {
            compatible_static(env, &value.ty, current, &mut HashMap::new()).map_err(|_| {
                MagError::Type(format!(
                    "list elements must have one compatible type, got {current} and {}",
                    value.ty
                ))
            })?;
        } else {
            item_type = Some(value.ty.clone());
        }
        checked_items.push(value);
    }
    let ty = item_type
        .map(|item| MagType::List(Box::new(item)))
        .unwrap_or(MagType::EmptyList);
    Ok(checked(ty, CheckedExprKind::Vector(checked_items)))
}

fn compile_map(
    env: &Env,
    scopes: &[CheckedScope],
    fields: &[(String, Expr)],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let expected = expected.ok_or_else(|| {
        MagError::Type("standalone record values are unsupported; use a named type or Map".into())
    })?;
    let expected_fields = named_field_types(env, expected).ok_or_else(|| {
        MagError::Type(format!("named field literal cannot construct {expected}"))
    })?;
    let mut names = HashSet::new();
    let mut checked_fields = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        if !names.insert(key) {
            return Err(MagError::Type(format!("duplicate named field {key}")));
        }
        let value = match expected_fields.get(key) {
            Some(expected_field) => compile_expr(env, scopes, value, Some(expected_field))?,
            None => compile_expr(env, scopes, value, None)?,
        };
        checked_fields.push((key.clone(), value));
    }
    Ok(checked(
        expected.clone(),
        CheckedExprKind::Fields(checked_fields),
    ))
}

fn compile_call(
    env: &Env,
    scopes: &[CheckedScope],
    callee_expression: &Expr,
    explicit_type_args: Option<&[TypeArgument]>,
    expressions: &[Expr],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    if let Expr::Name(name) = callee_expression {
        let candidates = all_candidates(env, scopes, name);
        let function_candidates = candidates
            .into_iter()
            .filter(|candidate| matches!(candidate.ty, MagType::Function(_, _)))
            .collect::<Vec<_>>();
        if !function_candidates.is_empty() {
            let user_call = compile_overloaded_call(
                env,
                scopes,
                name,
                &function_candidates,
                explicit_type_args,
                expressions,
                expected,
            );
            if user_call.is_ok() || builtin_id(env, name).is_none() {
                return user_call;
            }
        }
        if let Some(id) = builtin_id(env, name) {
            if explicit_type_args.is_some() {
                return Err(MagError::Type(format!(
                    "{name} is not a generic function and does not accept explicit type arguments"
                )));
            }
            return compile_builtin_call(env, scopes, name, id, expressions, expected);
        }
    }
    if explicit_type_args.is_some() {
        return Err(MagError::Type(
            "explicit type arguments require an immediate call to a named generic function".into(),
        ));
    }
    let callee = compile_expr(env, scopes, callee_expression, None)?;
    let MagType::Function(params, result) = &callee.ty else {
        return Err(MagError::Type(format!("cannot call {}", callee.ty)));
    };
    if params.len() != expressions.len() {
        return Err(MagError::Arity {
            expected: params.len(),
            got: expressions.len(),
        });
    }
    let mut substitution = HashMap::new();
    let mut args = Vec::with_capacity(params.len());
    for (expression, parameter) in expressions.iter().zip(params) {
        let parameter = substitute(parameter, &substitution);
        let argument = compile_expr(env, scopes, expression, Some(&parameter))?;
        compatible_static(env, &argument.ty, &parameter, &mut substitution)
            .map_err(MagError::Type)?;
        args.push(argument);
    }
    let result = substitute(result, &substitution);
    if let Some(expected) = expected {
        let bindable = internal_type_variables(&callee.ty);
        constrain_result(env, &result, expected, &mut substitution, &bindable)?;
    }
    let result = substitute(&result, &substitution);
    Ok(checked(
        result,
        CheckedExprKind::Call {
            callee: Box::new(callee),
            args,
            type_bindings: BTreeMap::new(),
        },
    ))
}

fn compile_construct(
    env: &Env,
    scopes: &[CheckedScope],
    authored_owner: &Type,
    constructor_name: &str,
    payload_expression: &Expr,
) -> Result<CheckedExpr, MagError> {
    let owner = parse_checked_type(env, scopes, authored_owner)?;
    let (constructor, payload_type) = instantiated_constructor(env, &owner, constructor_name)?;
    let payload = match payload_expression {
        Expr::Fields(fields) => {
            let expected_fields = named_field_types(env, &payload_type).ok_or_else(|| {
                MagError::Type(format!(
                    "constructor field payload cannot construct {payload_type}"
                ))
            })?;
            let authored_names = fields.iter().map(|(name, _)| name).collect::<HashSet<_>>();
            let expected_names = expected_fields.keys().collect::<HashSet<_>>();
            if authored_names != expected_names || authored_names.len() != fields.len() {
                return Err(MagError::Type(format!(
                    "constructor fields must exactly match {payload_type}"
                )));
            }
            compile_map(env, scopes, fields, Some(&payload_type))?
        }
        expression => compile_expr(env, scopes, expression, Some(&payload_type))?,
    };
    Ok(checked(
        owner.clone(),
        CheckedExprKind::Construct {
            owner,
            constructor,
            payload: Box::new(payload),
        },
    ))
}

fn compile_match(
    env: &Env,
    scopes: &[CheckedScope],
    value_expression: &Expr,
    arm_expressions: &[crate::authored::MatchArm],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let value = compile_expr(env, scopes, value_expression, None)?;
    let constructors = adt_constructors(env, &value.ty)?;
    let mut seen = HashSet::new();
    let mut arms = Vec::with_capacity(arm_expressions.len());
    let mut result = expected.cloned();
    for arm_expression in arm_expressions {
        let resolved_pattern_owner = arm_expression
            .pattern
            .owner()
            .map(|owner| parse_checked_type(env, scopes, &Type::Name(owner)))
            .transpose()?;
        let constructor_name = match_constructor_name(
            &value.ty,
            resolved_pattern_owner.as_ref(),
            &arm_expression.pattern,
        )?;
        let (constructor, payload_type) =
            instantiated_constructor(env, &value.ty, constructor_name)?;
        if !seen.insert(constructor.clone()) {
            return Err(MagError::Type(format!(
                "duplicate match arm for {}",
                arm_expression.pattern
            )));
        }
        let binding_name = &arm_expression.binding;
        let binding_id = env.allocate_binding_id(binding_name, Some(payload_type.clone()));
        let mut arm_scope = CheckedScope::new();
        insert_checked_candidate(
            env,
            scopes,
            &mut arm_scope,
            binding_name,
            CheckedCandidate {
                id: binding_id,
                ty: payload_type.clone(),
                generic_binders: vec![],
                parameter_names: None,
                contributes_type_vars: false,
            },
        )?;
        let mut arm_scopes = scopes.to_vec();
        arm_scopes.push(arm_scope);
        let arm_expected = result.as_ref().filter(|ty| **ty != MagType::Never);
        let body = compile_expr(env, &arm_scopes, &arm_expression.body, arm_expected)?;
        if result.as_ref() == Some(&MagType::Never) && body.ty != MagType::Never {
            result = Some(body.ty.clone());
        } else if let Some(current) = &result {
            compatible_static(env, &body.ty, current, &mut HashMap::new()).map_err(|_| {
                MagError::Type(format!(
                    "match arms must return one compatible type, got {current} and {}",
                    body.ty
                ))
            })?;
        } else {
            result = Some(body.ty.clone());
        }
        arms.push(CheckedMatchArm {
            constructor,
            binding: CheckedParam {
                id: binding_id,
                name: binding_name.clone(),
                ty: payload_type,
            },
            body: Box::new(body),
        });
    }
    ensure_adt_exhaustive(&constructors, &seen)?;
    Ok(checked(
        result.unwrap_or(MagType::Unit),
        CheckedExprKind::Match {
            value: Box::new(value),
            arms,
        },
    ))
}

fn adt_constructors(env: &Env, owner: &MagType) -> Result<Vec<ConstructorDecl>, MagError> {
    let MagType::Named(name, arguments) = owner else {
        return Err(MagError::Type(format!(
            "match expects an ADT value, got {owner}"
        )));
    };
    let declaration = env
        .type_decl(name)
        .ok_or_else(|| MagError::Type(format!("unknown nominal type {name}")))?;
    if declaration.params.len() != arguments.len() {
        return Err(MagError::Type(format!(
            "{name} expects {} type arguments, got {}",
            declaration.params.len(),
            arguments.len()
        )));
    }
    let TypeDeclBody::Adt(constructors) = declaration.body else {
        return Err(MagError::Type(format!(
            "match expects an ADT value, got {owner}"
        )));
    };
    let substitutions = declaration
        .params
        .iter()
        .cloned()
        .zip(arguments.iter().cloned())
        .collect::<HashMap<_, _>>();
    Ok(constructors
        .into_iter()
        .map(|constructor| ConstructorDecl {
            id: constructor.id,
            payload: substitute(&constructor.payload, &substitutions),
        })
        .collect())
}

fn match_constructor_name<'a>(
    owner: &MagType,
    resolved_pattern_owner: Option<&MagType>,
    pattern: &'a NamePath,
) -> Result<&'a str, MagError> {
    if let Some(pattern_owner) = resolved_pattern_owner {
        let same_owner = matches!(
            (owner, pattern_owner),
            (MagType::Named(actual, _), MagType::Named(authored, _)) if actual == authored
        );
        if !same_owner {
            return Err(MagError::Type(format!(
                "pattern owner {pattern_owner} does not match scrutinee owner {owner}"
            )));
        }
    }
    Ok(pattern.last())
}

fn instantiated_constructor(
    env: &Env,
    owner: &MagType,
    name: &str,
) -> Result<(ConstructorDeclarationId, MagType), MagError> {
    let constructors = adt_constructors(env, owner)?;
    constructors
        .into_iter()
        .find(|constructor| constructor.id.name == name)
        .map(|constructor| (constructor.id, constructor.payload))
        .ok_or_else(|| MagError::Type(format!("constructor {name} is not a member of {owner}")))
}

fn ensure_adt_exhaustive(
    constructors: &[ConstructorDecl],
    seen: &HashSet<ConstructorDeclarationId>,
) -> Result<(), MagError> {
    let missing = constructors
        .iter()
        .filter(|constructor| !seen.contains(&constructor.id))
        .map(|constructor| constructor.id.name.clone())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(MagError::Type(format!(
            "non-exhaustive match; missing {}",
            missing.join(", ")
        )))
    }
}

fn compile_if(
    env: &Env,
    scopes: &[CheckedScope],
    condition: &Expr,
    then_expression: &Expr,
    else_expression: &Expr,
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let condition = compile_expr(env, scopes, condition, Some(&MagType::Bool))?;
    let then_branch = compile_expr(env, scopes, then_expression, expected)?;
    let else_branch = compile_expr(env, scopes, else_expression, expected)?;
    let ty = if then_branch.ty == MagType::Never {
        else_branch.ty.clone()
    } else if else_branch.ty == MagType::Never {
        then_branch.ty.clone()
    } else {
        compatible_static(env, &then_branch.ty, &else_branch.ty, &mut HashMap::new()).map_err(
            |_| {
                MagError::Type(format!(
                    "if branches must return one compatible type, got {} and {}",
                    then_branch.ty, else_branch.ty
                ))
            },
        )?;
        else_branch.ty.clone()
    };
    Ok(checked(
        ty,
        CheckedExprKind::If {
            condition: Box::new(condition),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        },
    ))
}

fn compile_ascribe(
    env: &Env,
    scopes: &[CheckedScope],
    authored_target: &Type,
    source: &Expr,
) -> Result<CheckedExpr, MagError> {
    let target = parse_checked_type(env, scopes, authored_target)?;
    let source_expected = match source {
        Expr::Fields(_) => Some(&target),
        Expr::Name(name) if all_candidates(env, scopes, name).len() > 1 => Some(&target),
        _ => None,
    };
    let value = match source {
        Expr::Fields(fields) => compile_map(env, scopes, fields, Some(&target))?,
        Expr::Vector(values) if matches!(target, MagType::Product(_)) => {
            compile_vector(env, scopes, values, Some(&target))?
        }
        // An explicit ascription owns a possible newtype conversion, while
        // ordinary result-only overloads and generics still need target context.
        Expr::Call {
            callee,
            type_args,
            args,
        } if newtype_underlying(env, &target).is_some() => {
            let underlying = newtype_underlying(env, &target).ok_or_else(|| {
                MagError::Type(format!("missing newtype target for ascription to {target}"))
            })?;
            match compile_call(
                env,
                scopes,
                callee,
                type_args.as_deref(),
                args,
                Some(&underlying),
            ) {
                Ok(value) => value,
                Err(context_error) => {
                    match compile_call(env, scopes, callee, type_args.as_deref(), args, None) {
                        Ok(value)
                            if compatible_static(env, &value.ty, &target, &mut HashMap::new())
                                .is_ok()
                                || is_newtype_boundary(env, &value.ty, &target) =>
                        {
                            value
                        }
                        _ => return Err(context_error),
                    }
                }
            }
        }
        Expr::Call {
            callee,
            type_args,
            args,
        } if contains_newtype(env, &target) => {
            compile_call(env, scopes, callee, type_args.as_deref(), args, None)?
        }
        Expr::Call {
            callee,
            type_args,
            args,
        } => compile_call(
            env,
            scopes,
            callee,
            type_args.as_deref(),
            args,
            Some(&target),
        )?,
        source => compile_expr(env, scopes, source, source_expected)?,
    };
    if (contains_newtype(env, &value.ty) || contains_newtype(env, &target))
        && compatible_static(env, &value.ty, &target, &mut HashMap::new()).is_err()
        && !is_newtype_boundary(env, &value.ty, &target)
    {
        return Err(MagError::Type(format!(
            "invalid ascription from {} to {target}",
            value.ty
        )));
    }
    Ok(checked(
        target.clone(),
        CheckedExprKind::Ascribe {
            target,
            value: Box::new(value),
        },
    ))
}

fn compile_annotate(
    env: &Env,
    scopes: &[CheckedScope],
    authored_target: &Type,
    source: &Expr,
) -> Result<CheckedExpr, MagError> {
    let target = parse_checked_type(env, scopes, authored_target)?;
    if matches!(source, Expr::Construct { .. }) {
        return compile_ascribe(env, scopes, authored_target, source);
    }
    let value = match source {
        Expr::Fields(fields) => compile_map(env, scopes, fields, Some(&target))?,
        Expr::Vector(values) if matches!(target, MagType::Product(_)) => {
            compile_vector(env, scopes, values, Some(&target))?
        }
        source => compile_expr(env, scopes, source, Some(&target))?,
    };
    compatible_static(env, &value.ty, &target, &mut HashMap::new()).map_err(MagError::Type)?;
    Ok(checked(
        target.clone(),
        CheckedExprKind::Ascribe {
            target,
            value: Box::new(value),
        },
    ))
}

fn newtype_underlying(env: &Env, ty: &MagType) -> Option<MagType> {
    let MagType::Named(name, arguments) = ty else {
        return None;
    };
    let declaration = env.type_decl(name)?;
    let TypeDeclBody::Alias(body) = declaration.body else {
        return None;
    };
    let substitutions = declaration
        .params
        .into_iter()
        .zip(arguments.iter().cloned())
        .collect();
    Some(substitute(&body, &substitutions))
}

fn is_newtype_boundary(env: &Env, source: &MagType, target: &MagType) -> bool {
    newtype_underlying(env, source).is_some_and(|underlying| {
        compatible_static(env, &underlying, target, &mut HashMap::new()).is_ok()
    }) || newtype_underlying(env, target).is_some_and(|underlying| {
        compatible_static(env, source, &underlying, &mut HashMap::new()).is_ok()
    })
}

fn contains_newtype(env: &Env, ty: &MagType) -> bool {
    if newtype_underlying(env, ty).is_some() {
        return true;
    }
    match ty {
        MagType::List(item) | MagType::Set(item) | MagType::TypeTag(item) => {
            contains_newtype(env, item)
        }
        MagType::Map(key, value) => contains_newtype(env, key) || contains_newtype(env, value),
        MagType::Product(items) => items.iter().any(|item| contains_newtype(env, item)),
        MagType::Function(params, result) => {
            params.iter().any(|param| contains_newtype(env, param)) || contains_newtype(env, result)
        }
        MagType::Named(_, arguments) => arguments
            .iter()
            .any(|argument| contains_newtype(env, argument)),
        _ => false,
    }
}

fn compile_type_tag(
    env: &Env,
    scopes: &[CheckedScope],
    authored_target: &Type,
) -> Result<CheckedExpr, MagError> {
    let target = parse_checked_type(env, scopes, authored_target)?;
    Ok(checked(
        MagType::TypeTag(Box::new(target.clone())),
        CheckedExprKind::TypeTag(target),
    ))
}

#[derive(Clone)]
struct CallCandidateRejection {
    candidate: CheckedCandidate,
    value_arity: usize,
    explicit_type_arity: Option<usize>,
    checked_arguments: usize,
    reason: CallRejectionReason,
    declaration_order: usize,
}

#[derive(Clone)]
enum CallRejectionReason {
    TypeArguments {
        error: String,
    },
    ValueArity {
        expected: usize,
        actual: usize,
    },
    Argument {
        index: usize,
        name: Option<String>,
        expected: MagType,
        actual: Option<MagType>,
        error: String,
    },
    ResultMismatch {
        expected: MagType,
        actual: MagType,
        error: String,
    },
    SubstitutionConflict {
        index: usize,
        name: Option<String>,
        hole_index: usize,
        hole_binder: String,
        expected: MagType,
        actual: MagType,
        error: String,
    },
    HoleConflict {
        error: String,
    },
    Equality {
        error: String,
    },
}

impl CallCandidateRejection {
    fn closeness(&self) -> (usize, usize, usize) {
        let stage = match self.reason {
            CallRejectionReason::TypeArguments { .. } => 0,
            CallRejectionReason::ValueArity { .. } => 1,
            CallRejectionReason::Argument { .. }
            | CallRejectionReason::SubstitutionConflict { .. } => 2,
            CallRejectionReason::ResultMismatch { .. } => 3,
            CallRejectionReason::HoleConflict { .. } => 4,
            CallRejectionReason::Equality { .. } => 5,
        };
        (
            self.checked_arguments,
            stage,
            usize::MAX - self.declaration_order,
        )
    }
}

fn render_call_rejections(name: &str, mut rejections: Vec<CallCandidateRejection>) -> MagError {
    rejections.sort_by_key(|rejection| std::cmp::Reverse(rejection.closeness()));
    let mut lines = vec![format!(
        "no overload {name} matches call; closest candidates:"
    )];
    for rejection in rejections {
        let candidate = &rejection.candidate;
        let signature = match &candidate.ty {
            MagType::Function(params, result) => {
                let params = params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        candidate
                            .parameter_names
                            .as_ref()
                            .and_then(|names| names.get(index))
                            .map_or_else(
                                || ty.to_string(),
                                |parameter| format!("{parameter}: {ty}"),
                            )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let binders = (!candidate.generic_binders.is_empty())
                    .then(|| format!("<{}>", candidate.generic_binders.join(", ")))
                    .unwrap_or_default();
                format!("{name}{binders}({params}) -> {result}")
            }
            ty => format!("{name}: {ty}"),
        };
        let type_arity = rejection.explicit_type_arity.map_or_else(
            || "type args inferred".into(),
            |arity| format!("{arity} explicit type args"),
        );
        let arities = format!(
            "candidate #{}; {} value args; {type_arity}",
            candidate.id.0, rejection.value_arity,
        );
        let reason = match rejection.reason {
            CallRejectionReason::TypeArguments { error }
            | CallRejectionReason::HoleConflict { error }
            | CallRejectionReason::Equality { error } => error,
            CallRejectionReason::ValueArity { expected, actual } => {
                format!("expects {expected} value arguments, got {actual}")
            }
            CallRejectionReason::Argument {
                index,
                name,
                expected,
                actual,
                error,
            } => {
                let label = name.map_or_else(
                    || format!("argument {}", index + 1),
                    |name| format!("argument {} '{name}'", index + 1),
                );
                let actual = actual
                    .map(|actual| format!(", got {actual}"))
                    .unwrap_or_default();
                format!("{label} expected {expected}{actual}: {error}")
            }
            CallRejectionReason::SubstitutionConflict {
                index,
                name: parameter_name,
                hole_index,
                hole_binder,
                expected,
                actual,
                error,
            } => {
                let label = parameter_name.map_or_else(
                    || format!("argument {}", index + 1),
                    |parameter_name| format!("argument {} '{parameter_name}'", index + 1),
                );
                format!(
                    "{label} expected {expected}, got {actual}: conflicting inference for explicit type argument hole in {name} at position {} ({hole_binder}): {error}",
                    hole_index + 1,
                )
            }
            CallRejectionReason::ResultMismatch {
                expected,
                actual,
                error,
            } => {
                format!("result expected {expected}, got {actual}: {error}")
            }
        };
        lines.push(format!("- {signature} [{arities}]: {reason}"));
    }
    MagError::Type(lines.join("\n"))
}

fn compile_overloaded_call(
    env: &Env,
    scopes: &[CheckedScope],
    name: &str,
    candidates: &[CheckedCandidate],
    explicit_type_args: Option<&[TypeArgument]>,
    expressions: &[Expr],
    expected_result: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    let specialized_candidates;
    let candidates = if let Some(arguments) = explicit_type_args {
        if candidates
            .iter()
            .all(|candidate| candidate.generic_binders.is_empty())
        {
            return Err(MagError::Type(format!(
                "{name} is not a generic function and does not accept explicit type arguments"
            )));
        }
        specialized_candidates = candidates
            .iter()
            .filter(|candidate| candidate.generic_binders.len() == arguments.len())
            .cloned()
            .collect::<Vec<_>>();
        if specialized_candidates.is_empty() {
            let mut arities = candidates
                .iter()
                .map(|candidate| candidate.generic_binders.len())
                .collect::<Vec<_>>();
            arities.sort_unstable();
            arities.dedup();
            return Err(MagError::Type(format!(
                "generic call {name} has no overload with exactly {} type arguments; declared arities: {}",
                arguments.len(),
                arities
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        specialized_candidates.as_slice()
    } else {
        candidates
    };

    let mut matches = Vec::new();
    let mut rejections = Vec::new();
    for (declaration_order, candidate) in candidates.iter().enumerate() {
        let reject = |checked_arguments, reason| CallCandidateRejection {
            candidate: candidate.clone(),
            value_arity: expressions.len(),
            explicit_type_arity: explicit_type_args.map(<[TypeArgument]>::len),
            checked_arguments,
            reason,
            declaration_order,
        };
        let (candidate_type, bindable, holes, type_bindings) =
            match instantiate_call_candidate(env, scopes, name, candidate, explicit_type_args) {
                Ok(instantiated) => instantiated,
                Err(error) => {
                    rejections.push(reject(
                        0,
                        CallRejectionReason::TypeArguments {
                            error: error.to_string(),
                        },
                    ));
                    continue;
                }
            };
        let MagType::Function(params, result) = &candidate_type else {
            rejections.push(reject(
                0,
                CallRejectionReason::TypeArguments {
                    error: format!("candidate is not callable: {candidate_type}"),
                },
            ));
            continue;
        };
        if params.len() != expressions.len() {
            rejections.push(reject(
                0,
                CallRejectionReason::ValueArity {
                    expected: params.len(),
                    actual: expressions.len(),
                },
            ));
            continue;
        }
        let (args, mut substitution) = match compile_call_args(
            env,
            scopes,
            expressions,
            params,
            candidate.parameter_names.as_deref(),
            &bindable,
            &holes,
        ) {
            Ok(checked) => checked,
            Err(failure) => {
                let reason = match failure.conflicting_hole {
                    Some((hole_index, hole_binder, actual)) => {
                        CallRejectionReason::SubstitutionConflict {
                            index: failure.index,
                            name: failure.name,
                            hole_index,
                            hole_binder,
                            expected: failure.expected,
                            actual,
                            error: failure.error,
                        }
                    }
                    None => CallRejectionReason::Argument {
                        index: failure.index,
                        name: failure.name,
                        expected: failure.expected,
                        actual: failure.actual,
                        error: failure.error,
                    },
                };
                rejections.push(reject(failure.checked_arguments, reason));
                continue;
            }
        };
        let output = substitute(result, &substitution);
        if let Some(expected) = expected_result {
            if let Err(error) =
                constrain_result(env, &output, expected, &mut substitution, &bindable)
            {
                rejections.push(reject(
                    expressions.len(),
                    CallRejectionReason::ResultMismatch {
                        expected: expected.clone(),
                        actual: output,
                        error: error.to_string(),
                    },
                ));
                continue;
            }
        }
        if let Err(error) = validate_inferred_holes(name, &holes, &substitution) {
            rejections.push(reject(
                expressions.len(),
                CallRejectionReason::HoleConflict {
                    error: error.to_string(),
                },
            ));
            continue;
        }
        if let Err(error) = validate_equality_requirements(
            env,
            &instantiated_equality_requirements(env, candidate),
            &substitution,
        ) {
            rejections.push(reject(
                expressions.len(),
                CallRejectionReason::Equality {
                    error: error.to_string(),
                },
            ));
            continue;
        }
        let output = substitute(result, &substitution);
        let callee_type = MagType::Function(
            params
                .iter()
                .map(|parameter| substitute(parameter, &substitution))
                .collect(),
            Box::new(output.clone()),
        );
        let type_bindings: BTreeMap<String, MagType> = type_bindings
            .into_iter()
            .map(|(name, ty)| (name, substitute(&ty, &substitution)))
            .collect();
        matches.push((candidate, args, output, callee_type, type_bindings));
    }
    if matches.len() > 1 {
        let concrete = matches
            .iter()
            .filter(|(candidate, _, _, _, _)| candidate.generic_binders.is_empty())
            .count();
        if concrete == 1 {
            matches.retain(|(candidate, _, _, _, _)| candidate.generic_binders.is_empty());
        }
    }
    match matches.as_slice() {
        [(candidate, args, result, callee_type, type_bindings)] => {
            let callee = checked(
                callee_type.clone(),
                CheckedExprKind::BindingRef(candidate.id),
            );
            Ok(checked(
                result.clone(),
                CheckedExprKind::Call {
                    callee: Box::new(callee),
                    args: args.clone(),
                    type_bindings: type_bindings.clone(),
                },
            ))
        }
        [] => Err(render_call_rejections(name, rejections)),
        _ => Err(MagError::Type(format!(
            "ambiguous overload {name} for call"
        ))),
    }
}

fn constrain_result(
    env: &Env,
    output: &MagType,
    expected: &MagType,
    substitution: &mut HashMap<String, MagType>,
    bindable: &HashSet<String>,
) -> Result<(), MagError> {
    if has_type_variables(output) {
        compatible_with_bindable(env, expected, output, substitution, bindable)
            .map_err(MagError::Type)
    } else {
        compatible_with_bindable(env, output, expected, substitution, bindable)
            .map_err(MagError::Type)
    }
}

struct CallArgumentFailure {
    index: usize,
    name: Option<String>,
    checked_arguments: usize,
    expected: MagType,
    actual: Option<MagType>,
    conflicting_hole: Option<(usize, String, MagType)>,
    error: String,
}

fn compile_call_args(
    env: &Env,
    scopes: &[CheckedScope],
    expressions: &[Expr],
    params: &[MagType],
    parameter_names: Option<&[String]>,
    bindable: &HashSet<String>,
    holes: &[InferredTypeArgumentHole],
) -> Result<(Vec<CheckedExpr>, HashMap<String, MagType>), CallArgumentFailure> {
    let mut substitution = HashMap::new();
    let mut args = vec![None; params.len()];
    let mut checked_arguments = 0;
    let mut order = (0..params.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| contains_union(&params[*index]));
    for index in order {
        let expression = &expressions[index];
        let parameter = substitute(&params[index], &substitution);
        let name = parameter_names.and_then(|names| names.get(index)).cloned();
        let argument =
            compile_expr(env, scopes, expression, Some(&parameter)).map_err(|error| {
                let mut locals = visible_types(env, scopes, &CheckedScope::new());
                let actual = infer(env, &mut locals, expression).ok();
                let conflicting_hole = actual.as_ref().and_then(|actual| {
                    conflicting_explicit_hole(
                        env,
                        actual,
                        &params[index],
                        &substitution,
                        bindable,
                        holes,
                    )
                    .map(|(hole_index, binder)| (hole_index, binder, actual.clone()))
                });
                CallArgumentFailure {
                    index,
                    name: name.clone(),
                    checked_arguments,
                    expected: parameter.clone(),
                    actual,
                    conflicting_hole,
                    error: error.to_string(),
                }
            })?;
        compatible_with_bindable(env, &argument.ty, &parameter, &mut substitution, bindable)
            .map_err(|error| CallArgumentFailure {
                index,
                name,
                checked_arguments,
                expected: parameter,
                actual: Some(argument.ty.clone()),
                conflicting_hole: conflicting_explicit_hole(
                    env,
                    &argument.ty,
                    &params[index],
                    &substitution,
                    bindable,
                    holes,
                )
                .map(|(hole_index, binder)| (hole_index, binder, argument.ty.clone())),
                error,
            })?;
        args[index] = Some(argument);
        checked_arguments += 1;
    }
    let args = args
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CallArgumentFailure {
            index: 0,
            name: parameter_names.and_then(|names| names.first()).cloned(),
            checked_arguments,
            expected: params.first().cloned().unwrap_or(MagType::Unit),
            actual: None,
            conflicting_hole: None,
            error: "internal error: unchecked call argument".into(),
        })?;
    for (index, (argument, parameter)) in args.iter().zip(params).enumerate() {
        let expected = substitute(parameter, &substitution);
        check_equality_specialization(env, argument, &expected, &substitution).map_err(
            |error| CallArgumentFailure {
                index,
                name: parameter_names.and_then(|names| names.get(index)).cloned(),
                checked_arguments,
                expected,
                actual: Some(argument.ty.clone()),
                conflicting_hole: None,
                error: error.to_string(),
            },
        )?;
    }
    Ok((args, substitution))
}

fn conflicting_explicit_hole(
    env: &Env,
    actual: &MagType,
    parameter: &MagType,
    substitution: &HashMap<String, MagType>,
    bindable: &HashSet<String>,
    holes: &[InferredTypeArgumentHole],
) -> Option<(usize, String)> {
    let parameter_variables = {
        let mut variables = HashSet::new();
        collect_vars(parameter, &mut variables);
        variables
    };
    let relevant = holes
        .iter()
        .filter(|(_, _, variable)| {
            parameter_variables.contains(variable) && substitution.contains_key(variable)
        })
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        return None;
    }
    let mut without_hole_bindings = substitution.clone();
    for (_, _, variable) in &relevant {
        without_hole_bindings.remove(variable);
    }
    compatible_with_bindable(env, actual, parameter, &mut without_hole_bindings, bindable).ok()?;
    relevant
        .first()
        .map(|(index, binder, _)| (*index, binder.clone()))
}

fn compile_artifact_expr(
    env: &Env,
    scopes: &[CheckedScope],
    expression: &Expr,
) -> Result<CheckedExpr, MagError> {
    match expression {
        Expr::Fields(fields) => {
            let mut names = HashSet::new();
            let mut checked_fields = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                if !names.insert(name) {
                    return Err(MagError::Type(format!("duplicate artifact field {name}")));
                }
                checked_fields.push((name.clone(), compile_artifact_expr(env, scopes, value)?));
            }
            Ok(checked(
                MagType::JsonValue,
                CheckedExprKind::Fields(checked_fields),
            ))
        }
        Expr::Vector(items) => Ok(checked(
            MagType::JsonValue,
            CheckedExprKind::Vector(
                items
                    .iter()
                    .map(|item| compile_artifact_expr(env, scopes, item))
                    .collect::<Result<_, _>>()?,
            ),
        )),
        _ => compile_expr(env, scopes, expression, None),
    }
}

fn compile_builtin_call(
    env: &Env,
    scopes: &[CheckedScope],
    name: &str,
    id: BindingId,
    expressions: &[Expr],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    if name == "__map_empty" && expressions.len() == 1 {
        let Some(MagType::Map(key_type, _)) = expected else {
            return Err(MagError::Type(
                "one-argument __map_empty requires an expected Map type".into(),
            ));
        };
        require_equality_admissible(env, key_type)?;
        let key = compile_expr(
            env,
            scopes,
            &expressions[0],
            Some(&MagType::TypeTag(key_type.clone())),
        )?;
        let output = expected.cloned().unwrap_or_else(|| unreachable!());
        return Ok(checked(
            output.clone(),
            CheckedExprKind::Call {
                callee: Box::new(checked(
                    MagType::Function(vec![key.ty.clone()], Box::new(output.clone())),
                    CheckedExprKind::BindingRef(id),
                )),
                args: vec![key],
                type_bindings: BTreeMap::new(),
            },
        ));
    }
    if name == "get" {
        if expressions.len() != 2 {
            return Err(MagError::Arity {
                expected: 2,
                got: expressions.len(),
            });
        }
        let target = compile_expr(env, scopes, &expressions[0], None)?;
        if matches!(target.ty, MagType::HostInputs | MagType::Map(_, _)) {
            return Err(MagError::Type(format!(
                "get expects a record, got {}",
                target.ty
            )));
        }
        let key = compile_expr(env, scopes, &expressions[1], Some(&MagType::String))?;
        let key_name = match &expressions[1] {
            Expr::Str(key) | Expr::Keyword(key) => Some(key.as_str()),
            _ => None,
        };
        let output = field_type(env, &target.ty, key_name)
            .ok_or_else(|| MagError::Type(format!("cannot get {key_name:?} from {}", target.ty)))?;
        return Ok(checked(
            output.clone(),
            CheckedExprKind::Call {
                callee: Box::new(checked(
                    MagType::Function(
                        vec![target.ty.clone(), MagType::String],
                        Box::new(output.clone()),
                    ),
                    CheckedExprKind::BindingRef(id),
                )),
                args: vec![target, key],
                type_bindings: BTreeMap::new(),
            },
        ));
    }
    if name == "canonical" && expressions.len() != 1 {
        return Err(MagError::Arity {
            expected: 1,
            got: expressions.len(),
        });
    }
    if name == "artifact" {
        if expressions.len() != 1 {
            return Err(MagError::Arity {
                expected: 1,
                got: expressions.len(),
            });
        }
        let data = compile_artifact_expr(env, scopes, &expressions[0])?;
        let result = MagType::Artifact;
        return Ok(checked(
            result.clone(),
            CheckedExprKind::Call {
                callee: Box::new(checked(
                    MagType::Function(vec![data.ty.clone()], Box::new(result.clone())),
                    CheckedExprKind::BindingRef(id),
                )),
                args: vec![data],
                type_bindings: BTreeMap::new(),
            },
        ));
    }
    if name == "str" {
        let args = expressions
            .iter()
            .map(|expression| compile_expr(env, scopes, expression, None))
            .collect::<Result<Vec<_>, _>>()?;
        let result = MagType::String;
        return Ok(checked(
            result.clone(),
            CheckedExprKind::Call {
                callee: Box::new(checked(
                    MagType::Function(
                        args.iter().map(|argument| argument.ty.clone()).collect(),
                        Box::new(result.clone()),
                    ),
                    CheckedExprKind::BindingRef(id),
                )),
                args,
                type_bindings: BTreeMap::new(),
            },
        ));
    }
    if matches!(
        name,
        "map" | "filter" | "flat_map" | "sort_by" | "group_by" | "indexed_map"
    ) {
        return compile_collection_builtin(env, scopes, name, id, expressions, expected);
    }
    if name == "fold" {
        return compile_fold_builtin(env, scopes, id, expressions);
    }
    let mut locals = visible_types(env, scopes, &CheckedScope::new());
    let result = infer_builtin(env, &mut locals, name, expressions)?;
    let args = expressions
        .iter()
        .map(|expression| compile_expr(env, scopes, expression, None))
        .collect::<Result<Vec<_>, _>>()?;
    let callee_type = MagType::Function(
        args.iter().map(|argument| argument.ty.clone()).collect(),
        Box::new(result.clone()),
    );
    Ok(checked(
        result,
        CheckedExprKind::Call {
            callee: Box::new(checked(callee_type, CheckedExprKind::BindingRef(id))),
            args,
            type_bindings: BTreeMap::new(),
        },
    ))
}

fn compile_collection_builtin(
    env: &Env,
    scopes: &[CheckedScope],
    name: &str,
    id: BindingId,
    expressions: &[Expr],
    expected: Option<&MagType>,
) -> Result<CheckedExpr, MagError> {
    if expressions.len() != 2 {
        return Err(MagError::Arity {
            expected: 2,
            got: expressions.len(),
        });
    }
    let collection = compile_expr(env, scopes, &expressions[1], None)?;
    let MagType::List(item) = &collection.ty else {
        return Err(MagError::Type(format!("{name} expects List")));
    };
    let callback_result = match name {
        "filter" => MagType::Bool,
        "sort_by" | "group_by" => MagType::String,
        "map" => match expected {
            Some(MagType::List(result)) => (**result).clone(),
            _ => MagType::Var("\0builtin.result".into()),
        },
        "flat_map" => expected
            .cloned()
            .unwrap_or_else(|| MagType::List(Box::new(MagType::Var("\0builtin.result".into())))),
        "indexed_map" => match expected {
            Some(MagType::List(result)) => (**result).clone(),
            _ => MagType::Var("\0builtin.result".into()),
        },
        _ => unreachable!(),
    };
    let callback_params = if name == "indexed_map" {
        vec![MagType::Int, (**item).clone()]
    } else {
        vec![(**item).clone()]
    };
    let callback_expected = MagType::Function(callback_params, Box::new(callback_result));
    let callback =
        compile_expr(env, scopes, &expressions[0], Some(&callback_expected)).map_err(|error| {
            match name {
                "sort_by" | "group_by" => {
                    MagError::Type(format!("{name} callback must return String: {error}"))
                }
                "filter" => MagError::Type(format!("filter callback must return Bool: {error}")),
                _ => error,
            }
        })?;
    let MagType::Function(_, result) = &callback.ty else {
        unreachable!("callback expectation guarantees a function")
    };
    let output = match name {
        "filter" | "sort_by" => collection.ty.clone(),
        "group_by" => MagType::Map(Box::new(MagType::String), Box::new(collection.ty.clone())),
        "flat_map" => (**result).clone(),
        "map" | "indexed_map" => MagType::List(result.clone()),
        _ => unreachable!(),
    };
    let callee_type = MagType::Function(
        vec![callback.ty.clone(), collection.ty.clone()],
        Box::new(output.clone()),
    );
    Ok(checked(
        output,
        CheckedExprKind::Call {
            callee: Box::new(checked(callee_type, CheckedExprKind::BindingRef(id))),
            args: vec![callback, collection],
            type_bindings: BTreeMap::new(),
        },
    ))
}

fn compile_fold_builtin(
    env: &Env,
    scopes: &[CheckedScope],
    id: BindingId,
    expressions: &[Expr],
) -> Result<CheckedExpr, MagError> {
    if expressions.len() != 3 {
        return Err(MagError::Arity {
            expected: 3,
            got: expressions.len(),
        });
    }
    let init = compile_expr(env, scopes, &expressions[1], None)?;
    let collection = compile_expr(env, scopes, &expressions[2], None)?;
    let MagType::List(item) = &collection.ty else {
        return Err(MagError::Type("fold expects List".into()));
    };
    let callback_expected = MagType::Function(
        vec![init.ty.clone(), (**item).clone()],
        Box::new(init.ty.clone()),
    );
    let callback = compile_expr(env, scopes, &expressions[0], Some(&callback_expected))?;
    let output = init.ty.clone();
    let callee_type = MagType::Function(
        vec![callback.ty.clone(), init.ty.clone(), collection.ty.clone()],
        Box::new(output.clone()),
    );
    Ok(checked(
        output,
        CheckedExprKind::Call {
            callee: Box::new(checked(callee_type, CheckedExprKind::BindingRef(id))),
            args: vec![callback, init, collection],
            type_bindings: BTreeMap::new(),
        },
    ))
}

#[derive(Clone)]
struct ParsedFunction {
    type_params: Vec<String>,
    params: Vec<(String, MagType)>,
    result: MagType,
    body: Vec<BlockItem>,
}

fn parse_function(
    env: &Env,
    scopes: &[CheckedScope],
    function: &Function,
) -> Result<ParsedFunction, MagError> {
    let mut vars = visible_type_variables(scopes);
    vars.extend(function.type_params.iter().cloned());
    Ok(ParsedFunction {
        type_params: function.type_params.clone(),
        params: function
            .params
            .iter()
            .map(|parameter| {
                Ok((
                    parameter.name.clone(),
                    resolve_type(env, &parameter.ty, &vars)?,
                ))
            })
            .collect::<Result<Vec<_>, MagError>>()?,
        result: resolve_type(env, &function.result, &vars)?,
        body: function.body.clone(),
    })
}

fn compile_function(
    env: &Env,
    scopes: &[CheckedScope],
    name: Option<&str>,
    authored: &Function,
    expected: Option<&MagType>,
    binding_id: Option<BindingId>,
) -> Result<CheckedExpr, MagError> {
    let function = parse_function(env, scopes, authored)?;
    let signature = MagType::Function(
        function.params.iter().map(|(_, ty)| ty.clone()).collect(),
        Box::new(function.result.clone()),
    );
    if let Some(expected) = expected {
        compatible_static(env, &signature, expected, &mut HashMap::new())
            .map_err(MagError::Type)?;
    }
    let mut parameter_scope = CheckedScope::new();
    if !function.type_params.is_empty() {
        parameter_scope.insert(
            TYPE_BINDER_SCOPE_KEY.into(),
            vec![CheckedCandidate {
                id: BindingId(u64::MAX),
                ty: MagType::Product(
                    function
                        .type_params
                        .iter()
                        .cloned()
                        .map(MagType::Var)
                        .collect(),
                ),
                generic_binders: vec![],
                parameter_names: None,
                contributes_type_vars: true,
            }],
        );
    }
    let mut checked_params = Vec::with_capacity(function.params.len());
    for (parameter_name, ty) in function.params {
        let id = env.allocate_binding_id(&parameter_name, Some(ty.clone()));
        insert_checked_candidate(
            env,
            scopes,
            &mut parameter_scope,
            &parameter_name,
            CheckedCandidate {
                id,
                ty: ty.clone(),
                generic_binders: vec![],
                parameter_names: None,
                contributes_type_vars: false,
            },
        )?;
        checked_params.push(CheckedParam {
            id,
            name: parameter_name,
            ty,
        });
    }
    let mut body_scopes = scopes.to_vec();
    body_scopes.push(parameter_scope);
    let (body, equality_variables) = with_equality_obligation_scope(|| {
        compile_block_in(env, &body_scopes, &function.body, Some(&function.result))
    })?;
    record_equality_variables(
        equality_variables
            .iter()
            .filter(|variable| !function.type_params.contains(variable))
            .cloned(),
    );
    let equality_params = function
        .type_params
        .iter()
        .filter(|parameter| equality_variables.contains(*parameter))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(id) = binding_id {
        env.set_equality_requirements(
            id,
            equality_params.iter().cloned().map(MagType::Var).collect(),
        );
    }
    let actual = body
        .expressions
        .last()
        .map(|expression| expression.ty.clone())
        .unwrap_or(MagType::Unit);
    compatible_static(env, &actual, &function.result, &mut HashMap::new()).map_err(|message| {
        MagError::Type(format!(
            "function {}returns {actual}, declared {}: {message}",
            name.map(|name| format!("{name} ")).unwrap_or_default(),
            function.result
        ))
    })?;
    Ok(checked(
        signature,
        CheckedExprKind::Function(Arc::new(CheckedFn {
            name: name.map(str::to_owned),
            type_params: function.type_params,
            equality_params,
            params: checked_params,
            result: function.result,
            body: Arc::new(body),
        })),
    ))
}

fn parse_checked_type(
    env: &Env,
    scopes: &[CheckedScope],
    expression: &Type,
) -> Result<MagType, MagError> {
    resolve_type(env, expression, &visible_type_variables(scopes))
}

pub(crate) fn resolve_field_types(
    env: &Env,
    authored: &[(String, Type)],
    vars: &HashSet<String>,
) -> Result<BTreeMap<String, MagType>, MagError> {
    let mut resolved = BTreeMap::new();
    for (name, ty) in authored {
        if resolved
            .insert(name.clone(), resolve_type(env, ty, vars)?)
            .is_some()
        {
            return Err(MagError::Type(format!("duplicate named field {name}")));
        }
    }
    Ok(resolved)
}

pub(crate) fn resolve_type(
    env: &Env,
    authored: &Type,
    vars: &HashSet<String>,
) -> Result<MagType, MagError> {
    match authored {
        Type::Name(name) if vars.contains(name.as_str()) => Ok(MagType::Var(name.to_string())),
        Type::Name(name) => {
            let candidates = env.lookup_candidates(name);
            let types = candidates
                .iter()
                .filter_map(|value| match value {
                    Value::Type(ty) => Some(ty.clone()),
                    Value::TypeDecl(decl) => Some(MagType::Named(decl.name.clone(), vec![])),
                    _ => None,
                })
                .collect::<Vec<_>>();
            match types.as_slice() {
                [ty] => expand_transparent_alias(env, ty),
                [] if candidates.is_empty() => Err(MagError::Unresolved(name.to_string())),
                [] => Err(MagError::Type(format!("{name} is not a type"))),
                _ => Err(MagError::Type(format!("ambiguous type name {name}"))),
            }
        }
        Type::Product(types) => types
            .iter()
            .map(|ty| resolve_type(env, ty, vars))
            .collect::<Result<Vec<_>, _>>()
            .map(MagType::Product),
        Type::Tag(ty) => Ok(MagType::TypeTag(Box::new(resolve_type(env, ty, vars)?))),
        Type::Function { params, result } => Ok(MagType::Function(
            params
                .iter()
                .map(|ty| resolve_type(env, ty, vars))
                .collect::<Result<Vec<_>, _>>()?,
            Box::new(resolve_type(env, result, vars)?),
        )),
        Type::Apply {
            constructor,
            arguments,
        } => {
            let declaration = match env.lookup(constructor)? {
                Value::TypeDecl(declaration) => declaration,
                _ => {
                    return Err(MagError::Type(format!(
                        "{constructor} is not a declared type"
                    )))
                }
            };
            if declaration.params.len() != arguments.len() {
                return Err(MagError::Type(format!(
                    "{} expects {} type arguments, got {}",
                    declaration.name,
                    declaration.params.len(),
                    arguments.len()
                )));
            }
            let arguments = arguments
                .iter()
                .map(|ty| resolve_type(env, ty, vars))
                .collect::<Result<Vec<_>, _>>()?;
            match (declaration.body, arguments.as_slice()) {
                (TypeDeclBody::Native, [item]) if declaration.name == "core.List" => {
                    Ok(MagType::List(Box::new(item.clone())))
                }
                (TypeDeclBody::Native, [key, value]) if declaration.name == "core.Map" => {
                    Ok(MagType::Map(Box::new(key.clone()), Box::new(value.clone())))
                }
                (TypeDeclBody::Native, [item]) if declaration.name == "core.Set" => {
                    Ok(MagType::Set(Box::new(item.clone())))
                }
                (TypeDeclBody::Native, _) => Err(MagError::Type(format!(
                    "unsupported native type application {}",
                    declaration.name
                ))),
                (TypeDeclBody::TransparentAlias(body), _) => {
                    let substitutions = declaration.params.iter().cloned().zip(arguments).collect();
                    expand_transparent_alias(env, &substitute(&body, &substitutions))
                }
                _ => Ok(MagType::Named(declaration.name, arguments)),
            }
        }
        Type::Invalid(message) => Err(MagError::Type(message.clone())),
    }
}

fn expand_transparent_alias(env: &Env, ty: &MagType) -> Result<MagType, MagError> {
    let MagType::Named(name, arguments) = ty else {
        return Ok(ty.clone());
    };
    let Some(declaration) = env.type_decl(name) else {
        return Ok(ty.clone());
    };
    let TypeDeclBody::TransparentAlias(body) = declaration.body else {
        return Ok(ty.clone());
    };
    let substitutions = declaration
        .params
        .into_iter()
        .zip(arguments.iter().cloned())
        .collect();
    expand_transparent_alias(env, &substitute(&body, &substitutions))
}

fn visible_type_variables(scopes: &[CheckedScope]) -> HashSet<String> {
    let mut variables = HashSet::new();
    for candidate in scopes.iter().flat_map(|scope| scope.values()).flatten() {
        if candidate.contributes_type_vars {
            collect_vars(&candidate.ty, &mut variables);
        }
    }
    variables
}

fn has_type_variables(ty: &MagType) -> bool {
    let mut variables = HashSet::new();
    collect_vars(ty, &mut variables);
    !variables.is_empty()
}

fn contains_union(ty: &MagType) -> bool {
    match ty {
        MagType::Var(_) => true,
        MagType::Named(_, arguments) | MagType::Product(arguments) => {
            arguments.iter().any(contains_union)
        }
        MagType::TypeTag(value) | MagType::List(value) | MagType::Set(value) => {
            contains_union(value)
        }
        MagType::Map(key, value) => contains_union(key) || contains_union(value),
        MagType::Function(parameters, result) => {
            parameters.iter().any(contains_union) || contains_union(result)
        }
        _ => false,
    }
}

fn instantiated_equality_requirements(env: &Env, candidate: &CheckedCandidate) -> Vec<MagType> {
    let substitutions = candidate
        .generic_binders
        .iter()
        .enumerate()
        .map(|(index, binder)| {
            (
                binder.clone(),
                MagType::Var(format!("\0binding{}.{index}", candidate.id.0)),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut requirements = env.equality_requirements(candidate.id);
    if requirements.is_empty() {
        if let Ok(Value::Fn(function)) = env.ready_binding(candidate.id) {
            requirements = function
                .equality_params
                .iter()
                .cloned()
                .map(MagType::Var)
                .collect();
        }
    }
    requirements
        .iter()
        .map(|requirement| substitute(requirement, &substitutions))
        .collect()
}

type InferredTypeArgumentHole = (usize, String, String);
type InstantiatedCallCandidate = (
    MagType,
    HashSet<String>,
    Vec<InferredTypeArgumentHole>,
    BTreeMap<String, MagType>,
);

fn instantiate_call_candidate(
    env: &Env,
    scopes: &[CheckedScope],
    name: &str,
    candidate: &CheckedCandidate,
    explicit_type_args: Option<&[TypeArgument]>,
) -> Result<InstantiatedCallCandidate, MagError> {
    let Some(arguments) = explicit_type_args else {
        let (ty, bindable) = instantiate_candidate(candidate);
        let bindings = candidate
            .generic_binders
            .iter()
            .enumerate()
            .map(|(index, binder)| {
                (
                    binder.clone(),
                    MagType::Var(format!("\0binding{}.{index}", candidate.id.0)),
                )
            })
            .collect();
        return Ok((ty, bindable, Vec::new(), bindings));
    };
    if arguments.len() != candidate.generic_binders.len() {
        return Err(MagError::Type(format!(
            "generic call {name} expects exactly {} type arguments, got {}",
            candidate.generic_binders.len(),
            arguments.len()
        )));
    }
    let mut substitutions = HashMap::new();
    let mut bindable = HashSet::new();
    let mut holes = Vec::new();
    for (index, (binder, argument)) in candidate.generic_binders.iter().zip(arguments).enumerate() {
        let ty = match argument {
            TypeArgument::Explicit(ty) => parse_checked_type(env, scopes, ty)?,
            TypeArgument::Infer => {
                let variable = format!("\0binding{}.{}", candidate.id.0, index);
                bindable.insert(variable.clone());
                holes.push((index, binder.clone(), variable.clone()));
                MagType::Var(variable)
            }
        };
        substitutions.insert(binder.clone(), ty);
    }
    let bindings = substitutions
        .iter()
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .collect();
    Ok((
        substitute(&candidate.ty, &substitutions),
        bindable,
        holes,
        bindings,
    ))
}

fn validate_inferred_holes(
    name: &str,
    holes: &[InferredTypeArgumentHole],
    substitution: &HashMap<String, MagType>,
) -> Result<(), MagError> {
    let unresolved = holes
        .iter()
        .filter_map(|(index, binder, variable)| {
            let resolved = substitute(&MagType::Var(variable.clone()), substitution);
            (!internal_type_variables(&resolved).is_empty())
                .then(|| format!("position {} ({binder})", index + 1))
        })
        .collect::<Vec<_>>();
    if unresolved.is_empty() {
        Ok(())
    } else {
        Err(MagError::Type(format!(
            "cannot infer explicit type argument hole for {name} at {}",
            unresolved.join(", ")
        )))
    }
}

fn instantiate_candidate(candidate: &CheckedCandidate) -> (MagType, HashSet<String>) {
    let substitutions = candidate
        .generic_binders
        .iter()
        .enumerate()
        .map(|(index, binder)| {
            (
                binder.clone(),
                MagType::Var(format!("\0binding{}.{index}", candidate.id.0)),
            )
        })
        .collect::<HashMap<_, _>>();
    let bindable = substitutions
        .values()
        .filter_map(|ty| match ty {
            MagType::Var(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    (substitute(&candidate.ty, &substitutions), bindable)
}

fn internal_type_variables(ty: &MagType) -> HashSet<String> {
    let mut variables = HashSet::new();
    collect_vars(ty, &mut variables);
    variables.retain(|name| name.starts_with('\0'));
    variables
}

fn canonical_type(ty: &MagType) -> String {
    fn canonicalize(
        ty: &MagType,
        variables: &mut HashMap<String, MagType>,
        next: &mut usize,
    ) -> MagType {
        match ty {
            MagType::Var(name) => variables
                .entry(name.clone())
                .or_insert_with(|| {
                    let variable = MagType::Var(format!("${next}"));
                    *next += 1;
                    variable
                })
                .clone(),
            MagType::Named(name, args) => MagType::Named(
                name.clone(),
                args.iter()
                    .map(|arg| canonicalize(arg, variables, next))
                    .collect(),
            ),
            MagType::TypeTag(item) => {
                MagType::TypeTag(Box::new(canonicalize(item, variables, next)))
            }
            MagType::List(item) => MagType::List(Box::new(canonicalize(item, variables, next))),
            MagType::Set(item) => MagType::Set(Box::new(canonicalize(item, variables, next))),
            MagType::Map(key, value) => MagType::Map(
                Box::new(canonicalize(key, variables, next)),
                Box::new(canonicalize(value, variables, next)),
            ),
            MagType::Product(items) => MagType::Product(
                items
                    .iter()
                    .map(|item| canonicalize(item, variables, next))
                    .collect(),
            ),
            MagType::Function(params, result) => MagType::Function(
                params
                    .iter()
                    .map(|param| canonicalize(param, variables, next))
                    .collect(),
                Box::new(canonicalize(result, variables, next)),
            ),
            _ => ty.clone(),
        }
    }
    canonicalize(ty, &mut HashMap::new(), &mut 0).to_string()
}

pub(crate) fn value_type(value: &Value) -> Option<MagType> {
    match value {
        Value::Unit => Some(MagType::Unit),
        Value::Bool(_) => Some(MagType::Bool),
        Value::Int(_) => Some(MagType::Int),
        Value::Float(_) => Some(MagType::Float),
        Value::Str(_) | Value::Keyword(_) | Value::Symbol(_) => Some(MagType::String),
        Value::List(v) if v.is_empty() => Some(MagType::EmptyList),
        Value::List(v) => Some(MagType::List(Box::new(v.first().and_then(value_type)?))),
        Value::Product(v) => Some(MagType::Product(
            v.iter().map(value_type).collect::<Option<Vec<_>>>()?,
        )),
        Value::Fields(_) | Value::ModuleNamespace(_) => None,
        Value::Map(entries) => {
            let (key, value) = entries.first()?;
            Some(MagType::Map(
                Box::new(value_type(key)?),
                Box::new(value_type(value)?),
            ))
        }
        Value::Set(items) => Some(MagType::Set(Box::new(value_type(items.first()?)?))),
        Value::Fn(f) => Some(MagType::Function(
            f.param_types.clone(),
            Box::new(f.return_type.clone()),
        )),
        Value::Type(t) => Some(t.clone()),
        Value::TypeDecl(d) => Some(MagType::Named(d.name.clone(), vec![])),
        Value::TypeTag(ty) => Some(MagType::TypeTag(Box::new(ty.to_mag_type()))),
        Value::TypeDescriptor(_) => Some(MagType::TypeDescriptor),
        Value::TypeSchema(_) => Some(MagType::TypeSchema),
        Value::SemanticTypeId(_) => Some(MagType::SemanticTypeId),
        Value::PackedValue(_) => Some(MagType::PackedValue),
        Value::JsonValue(_) => Some(MagType::JsonValue),
        Value::HostInputs(_) => Some(MagType::HostInputs),
        Value::Artifact(_) => Some(MagType::Artifact),
        Value::Adt { owner, .. } => Some(owner.clone()),
        Value::Typed(_, ty) => Some(ty.clone()),
        Value::BuiltinFn(_) => None,
    }
}

pub(crate) fn canonical_value_type(value: &Value) -> Option<String> {
    if let Value::Type(ty) = value {
        return Some(format!("Type<{ty}>"));
    }
    if let Value::Fn(function) = value {
        let substitutions = function
            .type_params
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), MagType::Var(format!("${index}"))))
            .collect::<HashMap<_, _>>();
        let params = function
            .param_types
            .iter()
            .map(|ty| substitute(ty, &substitutions).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let result = substitute(&function.return_type, &substitutions);
        return Some(format!(
            "forall[{}].Fn({params})->{result}",
            function.type_params.len()
        ));
    }
    value_type(value).map(|ty| ty.to_string())
}

pub(crate) fn named_field_types(env: &Env, ty: &MagType) -> Option<BTreeMap<String, MagType>> {
    fn resolve(
        env: &Env,
        ty: &MagType,
        visiting: &mut HashSet<MagType>,
    ) -> Option<BTreeMap<String, MagType>> {
        if !visiting.insert(ty.clone()) {
            return None;
        }
        let MagType::Named(name, args) = ty else {
            return None;
        };
        let decl = env.type_decl(name)?;
        let substitutions = decl
            .params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        match decl.body {
            TypeDeclBody::Fields(crate::ast::FieldTypes(fields)) => Some(
                fields
                    .iter()
                    .map(|(name, ty)| (name.clone(), substitute(ty, &substitutions)))
                    .collect(),
            ),
            TypeDeclBody::TransparentAlias(body) | TypeDeclBody::Alias(body) => {
                resolve(env, &substitute(&body, &substitutions), visiting)
            }
            TypeDeclBody::Adt(_) | TypeDeclBody::Native => None,
        }
    }
    resolve(env, ty, &mut HashSet::new())
}

fn field_type(env: &Env, ty: &MagType, key: Option<&str>) -> Option<MagType> {
    match ty {
        MagType::JsonValue => Some(MagType::JsonValue),
        _ => key.and_then(|key| named_field_types(env, ty)?.get(key).cloned()),
    }
}

fn compatible(
    env: &Env,
    actual: &MagType,
    expected: &MagType,
    subst: &mut HashMap<String, MagType>,
) -> Result<(), String> {
    compatible_in(env, actual, expected, subst, None)
}

fn compatible_static(
    env: &Env,
    actual: &MagType,
    expected: &MagType,
    subst: &mut HashMap<String, MagType>,
) -> Result<(), String> {
    let mut bindable = internal_type_variables(actual);
    bindable.extend(internal_type_variables(expected));
    compatible_in(env, actual, expected, subst, Some(&bindable))
}

fn compatible_with_bindable(
    env: &Env,
    actual: &MagType,
    expected: &MagType,
    subst: &mut HashMap<String, MagType>,
    bindable: &HashSet<String>,
) -> Result<(), String> {
    let mut active = bindable.clone();
    active.extend(internal_type_variables(actual));
    active.extend(internal_type_variables(expected));
    compatible_in(env, actual, expected, subst, Some(&active))
}

fn compatible_in(
    env: &Env,
    actual: &MagType,
    expected: &MagType,
    subst: &mut HashMap<String, MagType>,
    bindable: Option<&HashSet<String>>,
) -> Result<(), String> {
    if actual == expected {
        return Ok(());
    }
    if let MagType::Var(name) = expected {
        if bindable.is_none_or(|bindable| bindable.contains(name)) {
            if let Some(bound) = subst.get(name) {
                let bound = bound.clone();
                return compatible_in(env, actual, &bound, subst, bindable)
                    .map_err(|_| format!("{name} was {bound}, got {actual}"));
            }
            subst.insert(name.clone(), actual.clone());
            return Ok(());
        }
        if actual == expected {
            return Ok(());
        }
    }
    if let MagType::Var(name) = actual {
        if bindable.is_some_and(|bindable| bindable.contains(name)) {
            if let Some(bound) = subst.get(name) {
                let bound = bound.clone();
                return compatible_in(env, &bound, expected, subst, bindable)
                    .map_err(|_| format!("{name} was {bound}, expected {expected}"));
            }
            subst.insert(name.clone(), expected.clone());
            return Ok(());
        }
    }
    if matches!(expected, MagType::Var(_)) {
        return Err(format!("expected {expected}, got {actual}"));
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
    match expected {
        MagType::Named(expected_name, expected_args) => match actual {
            MagType::Named(actual_name, actual_args)
                if actual_name == expected_name && actual_args.len() == expected_args.len() =>
            {
                for (actual_arg, expected_arg) in actual_args.iter().zip(expected_args) {
                    compatible_in(env, actual_arg, expected_arg, subst, bindable)?;
                }
                Ok(())
            }
            _ => Err(format!(
                "expected nominal {expected}, got {actual}; use as for explicit refinement"
            )),
        },
        MagType::TypeTag(expected_type) => match actual {
            MagType::TypeTag(actual_type) => {
                compatible_in(env, actual_type, expected_type, subst, bindable)
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::List(e) => match actual {
            MagType::List(a) => compatible_in(env, a, e, subst, bindable),
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Set(e) => match actual {
            MagType::Set(a) => compatible_in(env, a, e, subst, bindable),
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Map(ek, ev) => match actual {
            MagType::Map(ak, av) => {
                compatible_in(env, ak, ek, subst, bindable)?;
                compatible_in(env, av, ev, subst, bindable)
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Product(expected_items) => match actual {
            MagType::Product(actual_items) if actual_items.len() == expected_items.len() => {
                for (actual_item, expected_item) in actual_items.iter().zip(expected_items) {
                    compatible_in(env, actual_item, expected_item, subst, bindable)?;
                }
                Ok(())
            }
            _ => Err(format!("expected {expected}, got {actual}")),
        },
        MagType::Function(ep, er) => match actual {
            MagType::Function(ap, ar) if ap.len() == ep.len() => {
                for (a, e) in ap.iter().zip(ep) {
                    compatible_in(env, a, e, subst, bindable)?;
                }
                compatible_in(env, ar, er, subst, bindable)
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
        MagType::Set(t) => MagType::Set(Box::new(substitute(t, subst))),
        MagType::Map(k, v) => MagType::Map(
            Box::new(substitute(k, subst)),
            Box::new(substitute(v, subst)),
        ),
        MagType::Product(v) => MagType::Product(v.iter().map(|t| substitute(t, subst)).collect()),
        MagType::Function(p, r) => MagType::Function(
            p.iter().map(|t| substitute(t, subst)).collect(),
            Box::new(substitute(r, subst)),
        ),
        _ => ty.clone(),
    }
}

fn collect_vars(ty: &MagType, out: &mut HashSet<String>) {
    match ty {
        MagType::Var(name) => {
            out.insert(name.clone());
        }
        MagType::Named(_, args) | MagType::Product(args) => {
            for arg in args {
                collect_vars(arg, out);
            }
        }
        MagType::List(item) | MagType::Set(item) => collect_vars(item, out),
        MagType::TypeTag(item) => collect_vars(item, out),
        MagType::Map(key, value) => {
            collect_vars(key, out);
            collect_vars(value, out);
        }
        MagType::Function(params, result) => {
            for param in params {
                collect_vars(param, out);
            }
            collect_vars(result, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod builtin_signature_tests {
    use super::*;

    #[test]
    fn every_visible_builtin_has_collision_signatures() {
        let env = Env::new_with_stdlib();
        for &name in BUILTIN_NAMES {
            let signatures = if name == "str" {
                let representative =
                    MagType::Function(vec![MagType::Int], Box::new(MagType::String));
                builtin_overload_types(name, Some(&representative))
            } else {
                builtin_overload_types(name, None)
            };
            assert!(
                !signatures.is_empty(),
                "visible builtin {name} has no signature inventory"
            );
            for signature in signatures {
                assert!(
                    collides_with_builtin(&env, name, &signature),
                    "visible builtin {name} does not recognize signature {signature}"
                );
            }
        }
    }

    #[test]
    fn visible_word_builtins_use_canonical_snake_case() {
        for &name in BUILTIN_NAMES {
            if name.chars().any(char::is_alphanumeric) {
                assert!(
                    !name.contains('-') && !name.contains('?'),
                    "noncanonical builtin {name}"
                );
            }
        }
    }

    #[test]
    fn generic_product_components_are_inferred_positionally() {
        let env = Env::new_with_stdlib();
        let actual = MagType::Product(vec![MagType::String, MagType::String]);
        let expected = MagType::Product(vec![MagType::Var("A".into()), MagType::Var("B".into())]);
        let bindable = HashSet::from(["A".into(), "B".into()]);
        let mut substitution = HashMap::new();
        compatible_in(&env, &actual, &expected, &mut substitution, Some(&bindable))
            .expect("a product should infer each generic occurrence");
        assert_eq!(substitution.get("A"), Some(&MagType::String));
        assert_eq!(substitution.get("B"), Some(&MagType::String));
    }
}
