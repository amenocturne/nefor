use crate::ast::{
    BindingId, CheckedBlock, CheckedExpr, CheckedExprKind, ConstructorDecl,
    ConstructorDeclarationId, FnValue, FrameId, TypeDecl, TypeDeclBody, Value,
};
use crate::authored::{Form, Module, TypeDeclaration, TypeDeclarationBody};
use crate::env::{BindingForce, BindingHandle, Env};
use crate::error::MagError;
use crate::profile::Phase;
use crate::resolver::resolve_workspace_path;
use crate::types::{ConcreteType, MagType};
use std::collections::{BTreeMap, BTreeSet, HashSet};

thread_local! {
    static FORCE_STACK: std::cell::RefCell<Vec<(FrameId, BindingId, String, bool)>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct ForceStackGuard;

impl Drop for ForceStackGuard {
    fn drop(&mut self) {
        FORCE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

fn enter_force(env: &Env, handle: &BindingHandle) -> Result<ForceStackGuard, MagError> {
    let id = handle.id;
    let frame = handle.frame;
    let name = env
        .binding_metadata(id)
        .map(|binding| binding.name)
        .unwrap_or_else(|| format!("binding#{}", id.0));
    FORCE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(start) = stack
            .iter()
            .position(|(active_frame, active, _, initializing)| {
                *active_frame == frame && *active == id && *initializing
            })
        {
            let mut cycle = stack[start..]
                .iter()
                .map(|(_, _, name, _)| name.clone())
                .collect::<Vec<_>>();
            cycle.push(name);
            return Err(MagError::Eval(format!(
                "binding initialization cycle: {}",
                cycle.join(" -> ")
            )));
        }
        stack.push((frame, id, name, true));
        Ok(ForceStackGuard)
    })
}

fn enter_call_binding(env: &Env, id: BindingId) -> Result<ForceStackGuard, MagError> {
    let handle = env.binding_handle(id)?;
    let name = env
        .binding_metadata(id)
        .map(|binding| binding.name)
        .unwrap_or_else(|| format!("binding#{}", id.0));
    FORCE_STACK.with(|stack| stack.borrow_mut().push((handle.frame, id, name, false)));
    Ok(ForceStackGuard)
}

fn force_stack_active() -> bool {
    FORCE_STACK.with(|stack| !stack.borrow().is_empty())
}

pub mod fuel {
    use crate::error::MagError;
    use std::cell::Cell;

    thread_local! {
        static REMAINING: Cell<Option<u64>> = const { Cell::new(None) };
        static CALL_DEPTH: Cell<u16> = const { Cell::new(0) };
        static EXPR_DEPTH: Cell<u16> = const { Cell::new(0) };
        static CALL_LIMIT: Cell<u16> = const { Cell::new(64) };
        static EXPR_LIMIT: Cell<u16> = const { Cell::new(128) };
    }

    pub struct Guard {
        previous: Option<u64>,
        previous_call_limit: u16,
        previous_expr_limit: u16,
        active: bool,
    }
    pub fn install(limits: impl Into<crate::CompilerLimits>) -> Guard {
        let limits = limits.into();
        let previous = REMAINING.with(|remaining| remaining.replace(Some(limits.evaluation_steps)));
        let previous_call_limit = CALL_LIMIT.with(|limit| limit.replace(limits.call_depth));
        let previous_expr_limit = EXPR_LIMIT.with(|limit| limit.replace(limits.expression_depth));
        Guard {
            previous,
            previous_call_limit,
            previous_expr_limit,
            active: true,
        }
    }
    pub fn ensure(limits: impl Into<crate::CompilerLimits>) -> Guard {
        let limits = limits.into();
        REMAINING.with(|remaining| {
            if remaining.get().is_some() {
                Guard {
                    previous: None,
                    previous_call_limit: CALL_LIMIT.with(Cell::get),
                    previous_expr_limit: EXPR_LIMIT.with(Cell::get),
                    active: false,
                }
            } else {
                remaining.set(Some(limits.evaluation_steps));
                let previous_call_limit = CALL_LIMIT.with(|limit| limit.replace(limits.call_depth));
                let previous_expr_limit =
                    EXPR_LIMIT.with(|limit| limit.replace(limits.expression_depth));
                Guard {
                    previous: None,
                    previous_call_limit,
                    previous_expr_limit,
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
                CALL_LIMIT.with(|limit| limit.set(self.previous_call_limit));
                EXPR_LIMIT.with(|limit| limit.set(self.previous_expr_limit));
            }
        }
    }

    pub struct CallGuard;
    pub fn enter_call() -> Result<CallGuard, MagError> {
        CALL_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= CALL_LIMIT.with(Cell::get) {
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
            if current >= EXPR_LIMIT.with(Cell::get) {
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

pub(crate) fn eval_program(env: &mut Env, module: &Module) -> Result<Value, MagError> {
    let _fuel = fuel::ensure(env.compiler_limits());
    let requires = module
        .forms
        .iter()
        .filter_map(|form| match form {
            Form::Require(require) => Some(require.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    eval_imports(env, &requires, module)?;
    let mut source = Vec::new();
    for form in &module.forms {
        let _declaration_depth = if matches!(form, Form::Block(_)) {
            None
        } else {
            let depth = fuel::enter_expr()?;
            fuel::step()?;
            env.profile_counters(|counters| {
                counters.evaluator_steps = counters.evaluator_steps.saturating_add(1);
            });
            Some(depth)
        };
        match form {
            Form::Require(_) => {}
            Form::Type(declaration) => {
                eval_type_declaration(env, declaration)
                    .map_err(|error| enrich_unresolved_import(env, error))?;
            }
            Form::Block(item) => source.push(item.clone()),
            Form::Invalid(error) => return Err(error.clone().into_mag_error()),
        }
    }
    let checking_phase = env.profile_phase(Phase::Checking);
    let checked = crate::checker::compile_block(env, &source)
        .map_err(|error| enrich_unresolved_import(env, error))?;
    drop(checking_phase);
    let evaluated = eval_checked_block(env, &checked);
    match &evaluated {
        Ok(result) => {
            env.collect_frames(std::slice::from_ref(result));
        }
        Err(_) => {
            env.collect_frames(&[]);
        }
    }
    evaluated
}

fn enrich_unresolved_import(env: &Env, error: MagError) -> MagError {
    let MagError::Unresolved(name) = error else {
        return error;
    };
    let suggestions = crate::resolver::import_suggestions(env.module_roots(), &name);
    if suggestions.is_empty() {
        MagError::Unresolved(name)
    } else {
        MagError::Unresolved(format!(
            "{name}; possible imports: {}",
            suggestions.join(", ")
        ))
    }
}

fn eval_type_declaration(env: &mut Env, authored: &TypeDeclaration) -> Result<Value, MagError> {
    let vars = authored.params.iter().cloned().collect();
    let qualified = env.qualify(&authored.name);
    let body = match &authored.body {
        TypeDeclarationBody::Fields(fields) => TypeDeclBody::Fields(crate::ast::FieldTypes(
            crate::checker::resolve_field_types(env, fields, &vars)?,
        )),
        TypeDeclarationBody::TransparentAlias(body) => {
            TypeDeclBody::TransparentAlias(crate::checker::resolve_type(env, body, &vars)?)
        }
        TypeDeclarationBody::Newtype(body) => {
            TypeDeclBody::Alias(crate::checker::resolve_type(env, body, &vars)?)
        }
        TypeDeclarationBody::Adt(constructors) => {
            if constructors.is_empty() {
                return Err(MagError::Type(format!(
                    "ADT {} must declare at least one constructor",
                    authored.name
                )));
            }
            let mut seen = HashSet::new();
            let constructors = constructors
                .iter()
                .map(|constructor| {
                    if !seen.insert(constructor.name.clone()) {
                        return Err(MagError::Type(format!(
                            "duplicate constructor {} in {}",
                            constructor.name, authored.name
                        )));
                    }
                    Ok(ConstructorDecl {
                        id: ConstructorDeclarationId {
                            owner: qualified.clone(),
                            name: constructor.name.clone(),
                        },
                        payload: crate::checker::resolve_type(env, &constructor.payload, &vars)?,
                    })
                })
                .collect::<Result<Vec<_>, MagError>>()?;
            TypeDeclBody::Adt(constructors)
        }
    };
    let declaration = TypeDecl {
        name: qualified,
        params: authored.params.clone(),
        body,
    };
    if let Some(existing) = env.type_decl(&declaration.name) {
        if existing != declaration {
            return Err(MagError::Type(format!(
                "conflicting semantic type declaration {}",
                declaration.name
            )));
        }
    }
    let value = Value::TypeDecl(declaration);
    env.define(&authored.name, value.clone());
    Ok(value)
}

fn eval_checked_block(env: &mut Env, block: &CheckedBlock) -> Result<Value, MagError> {
    for binding in &block.bindings {
        env.declare_binding_slot(binding.id, &binding.name, binding.initializer.clone())?;
    }
    for binding in &block.bindings {
        force_binding(env, binding.id)
            .map_err(|error| binding_initialization_error(&binding.name, error))?;
    }
    let mut result = Value::Unit;
    for expression in &block.expressions {
        result = eval_checked_expr(env, expression)?;
    }
    Ok(result)
}

fn force_binding(env: &mut Env, id: crate::ast::BindingId) -> Result<Value, MagError> {
    let handle = env.binding_handle(id)?;
    let _force = match enter_force(env, &handle) {
        Ok(force) => force,
        Err(error) => {
            Env::profile_force_cycle(&handle);
            return Err(error);
        }
    };
    match Env::begin_handle_force(&handle)? {
        BindingForce::Ready(value) => Ok(value),
        BindingForce::Initialize {
            handle,
            initializer,
        } => match eval_checked_expr(env, &initializer) {
            Ok(value) => {
                Env::complete_handle_force(&handle, value.clone())?;
                Ok(value)
            }
            Err(error) => {
                Env::reset_handle_force(&handle, initializer)?;
                Err(error)
            }
        },
    }
}

fn eval_checked_expr(env: &mut Env, expression: &CheckedExpr) -> Result<Value, MagError> {
    let _depth = fuel::enter_expr()?;
    fuel::step()?;
    env.profile_counters(|counters| {
        counters.evaluator_steps = counters.evaluator_steps.saturating_add(1);
    });
    match &expression.kind {
        CheckedExprKind::Unit => Ok(Value::Unit),
        CheckedExprKind::Str(value) => Ok(Value::Str(value.clone())),
        CheckedExprKind::Int(value) => Ok(Value::Int(*value)),
        CheckedExprKind::Float(value) => Ok(Value::Float(*value)),
        CheckedExprKind::Bool(value) => Ok(Value::Bool(*value)),
        CheckedExprKind::Keyword(value) => Ok(Value::Str(format!(":{value}"))),
        CheckedExprKind::BindingRef(id) => force_binding(env, *id),
        CheckedExprKind::Vector(items) => Ok(Value::List(std::sync::Arc::new(
            items
                .iter()
                .map(|item| eval_checked_expr(env, item))
                .collect::<Result<_, _>>()?,
        ))),
        CheckedExprKind::Fields(fields) => {
            let fields = fields
                .iter()
                .map(|(name, value)| Ok((name.clone(), eval_checked_expr(env, value)?)))
                .collect::<Result<BTreeMap<_, _>, MagError>>()?;
            Ok(Value::Fields(std::sync::Arc::new(fields)))
        }
        CheckedExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            if truthy(&eval_checked_expr(env, condition)?) {
                eval_checked_expr(env, then_branch)
            } else {
                eval_checked_expr(env, else_branch)
            }
        }
        CheckedExprKind::Construct {
            owner,
            constructor,
            payload,
        } => Ok(Value::Adt {
            owner: runtime_type(env, owner),
            constructor: constructor.clone(),
            payload: std::sync::Arc::new(eval_checked_expr(env, payload)?),
        }),
        CheckedExprKind::Match { value, arms } => {
            let value = eval_checked_expr(env, value)?;
            let Value::Adt {
                constructor,
                payload,
                ..
            } = raw(&value)
            else {
                return Err(MagError::Type("match value is not an ADT".into()));
            };
            let arm = arms
                .iter()
                .find(|arm| &arm.constructor == constructor)
                .ok_or_else(|| {
                    MagError::Type(format!(
                        "match has no arm for selected constructor {}",
                        constructor.name
                    ))
                })?;
            let payload = payload.as_ref().clone();
            env.push_scope();
            env.define_ready(arm.binding.id, &arm.binding.name, payload);
            let result = eval_checked_expr(env, &arm.body);
            env.pop_scope();
            result
        }
        CheckedExprKind::Call {
            callee,
            args,
            type_bindings,
        } => {
            let function = eval_checked_expr(env, callee)?;
            let args = args
                .iter()
                .map(|argument| eval_checked_expr(env, argument))
                .collect::<Result<Vec<_>, _>>()?;
            let type_bindings = type_bindings
                .iter()
                .map(|(name, ty)| (name.clone(), runtime_type(env, ty)))
                .collect::<BTreeMap<_, _>>();
            let _call_binding = match &callee.kind {
                CheckedExprKind::BindingRef(id) if force_stack_active() => {
                    Some(enter_call_binding(env, *id)?)
                }
                _ => None,
            };
            let resolved_signature = runtime_type(env, &callee.ty);
            let value = apply_resolved(env, &function, &args, &resolved_signature, &type_bindings)?;
            let ty = runtime_type(env, &expression.ty);
            if matches!(ty, MagType::Map(_, _) | MagType::Set(_)) {
                Ok(Value::Typed(std::sync::Arc::new(value), ty))
            } else {
                Ok(value)
            }
        }
        CheckedExprKind::Function(function) => Ok(Value::Fn(std::sync::Arc::new(FnValue {
            name: function.name.clone(),
            type_params: function.type_params.clone(),
            equality_params: function.equality_params.clone(),
            params: function
                .params
                .iter()
                .map(|parameter| parameter.name.clone())
                .collect(),
            param_types: function
                .params
                .iter()
                .map(|parameter| parameter.ty.clone())
                .collect(),
            return_type: function.result.clone(),
            checked: function.clone(),
            closure: env.snapshot(),
        }))),
        CheckedExprKind::Ascribe { target, value } => {
            let value = eval_checked_expr(env, value)?;
            checked_typed_value(env, value, runtime_type(env, target))
        }
        CheckedExprKind::TypeTag(ty) => Ok(Value::TypeTag(ConcreteType::resolve(
            env,
            &runtime_type(env, ty),
        )?)),
    }
}

fn runtime_type(env: &Env, ty: &MagType) -> MagType {
    match ty {
        MagType::Var(name) => match env.lookup(name) {
            Ok(Value::Type(ty)) => ty,
            _ => ty.clone(),
        },
        MagType::Named(name, args) => MagType::Named(
            name.clone(),
            args.iter().map(|ty| runtime_type(env, ty)).collect(),
        ),
        MagType::TypeTag(ty) => MagType::TypeTag(Box::new(runtime_type(env, ty))),
        MagType::List(ty) => MagType::List(Box::new(runtime_type(env, ty))),
        MagType::Set(ty) => MagType::Set(Box::new(runtime_type(env, ty))),
        MagType::Map(key, value) => MagType::Map(
            Box::new(runtime_type(env, key)),
            Box::new(runtime_type(env, value)),
        ),
        MagType::Product(types) => {
            MagType::Product(types.iter().map(|ty| runtime_type(env, ty)).collect())
        }
        MagType::Function(params, result) => MagType::Function(
            params.iter().map(|ty| runtime_type(env, ty)).collect(),
            Box::new(runtime_type(env, result)),
        ),
        other => other.clone(),
    }
}

fn binding_initialization_error(name: &str, error: MagError) -> MagError {
    match error {
        MagError::Type(message) => MagError::Type(format!("initializing {name}: {message}")),
        MagError::Eval(message) => MagError::Eval(format!("initializing {name}: {message}")),
        other => other,
    }
}

fn record_fields(env: &Env, ty: &MagType) -> Option<BTreeMap<String, MagType>> {
    crate::checker::named_field_types(env, ty)
}

fn record_field_diff(env: &Env, value: &Value, ty: &MagType) -> Option<String> {
    let Value::Fields(actual) = raw(value) else {
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
    let invalid = expected
        .iter()
        .filter_map(|(key, ty)| {
            let value = actual.get(key)?;
            validate_value(env, value, ty).is_err().then(|| {
                format!(
                    "{key}: expected {ty}, got {}",
                    crate::checker::value_type(value)
                        .map(|actual| actual.to_string())
                        .unwrap_or_else(|| value.type_name().to_owned())
                )
            })
        })
        .collect::<Vec<_>>();
    if missing.is_empty() && unexpected.is_empty() && invalid.is_empty() {
        return None;
    }
    let mut details = Vec::new();
    if !missing.is_empty() {
        details.push(format!("missing fields: {}", missing.join(", ")));
    }
    if !unexpected.is_empty() {
        details.push(format!("unexpected fields: {}", unexpected.join(", ")));
    }
    if !invalid.is_empty() {
        details.push(format!("invalid fields: {}", invalid.join(", ")));
    }
    Some(details.join("; "))
}

fn checked_typed_value(env: &Env, value: Value, ty: MagType) -> Result<Value, MagError> {
    if let MagType::Product(components) = &ty {
        let values = match raw(&value) {
            Value::List(values) | Value::Product(values) => values,
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
    Ok(Value::Typed(std::sync::Arc::new(value), ty))
}

fn validate_value(env: &Env, value: &Value, ty: &MagType) -> Result<(), MagError> {
    env.profile_counters(|counters| {
        counters.runtime_value_validation_visits =
            counters.runtime_value_validation_visits.saturating_add(1);
        let boundary = match ty {
            MagType::Named(name, _) => format!("nominal:{name}"),
            MagType::Function(_, _) => "function".to_owned(),
            other => format!("type:{other}"),
        };
        *counters
            .runtime_validation_visits_by_boundary
            .entry(boundary)
            .or_default() += 1;
    });
    if let Value::Typed(_, evidence) = value {
        if evidence == ty {
            return Ok(());
        }
        if let (Ok(expected), Ok(actual)) = (
            crate::types::ConcreteType::resolve(env, ty),
            crate::types::ConcreteType::resolve(env, evidence),
        ) {
            if expected.accepts(&actual) {
                return Ok(());
            }
        }
    }
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
            Value::List(xs) => xs.iter().all(|x| validate_value(env, x, item).is_ok()),
            _ => false,
        },
        MagType::EmptyList => {
            matches!(value, Value::List(items) if items.is_empty())
        }
        MagType::Map(key, item) => match value {
            Value::Map(map) => map.iter().all(|(k, v)| {
                validate_value(env, k, key).is_ok() && validate_value(env, v, item).is_ok()
            }),
            _ => false,
        },
        MagType::Set(item) => match value {
            Value::Set(items) => items.iter().all(|v| validate_value(env, v, item).is_ok()),
            _ => false,
        },
        MagType::Named(name, args) => env.type_decl(name).is_some_and(|decl| {
            let substitutions = decl
                .params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            match decl.body {
                TypeDeclBody::Fields(crate::ast::FieldTypes(fields)) => match value {
                    Value::Fields(map) => {
                        map.len() == fields.len()
                            && fields.iter().all(|(key, field)| {
                                map.get(key).is_some_and(|value| {
                                    validate_value(
                                        env,
                                        value,
                                        &crate::checker::substitute(field, &substitutions),
                                    )
                                    .is_ok()
                                })
                            })
                    }
                    _ => false,
                },
                TypeDeclBody::TransparentAlias(body) | TypeDeclBody::Alias(body) => validate_value(
                    env,
                    value,
                    &crate::checker::substitute(&body, &substitutions),
                )
                .is_ok(),
                TypeDeclBody::Adt(_) => matches!(
                    value,
                    Value::Adt { owner, .. } if owner == ty
                ),
                TypeDeclBody::Native => false,
            }
        }),
        MagType::TypeTag(expected) => matches!(
            value,
            Value::TypeTag(actual)
                if crate::types::ConcreteType::resolve(env, expected)
                    .is_ok_and(|expected| actual == &expected)
        ),
        MagType::Product(types) => match value {
            Value::List(values) | Value::Product(values) => {
                values.len() == types.len()
                    && values
                        .iter()
                        .zip(types)
                        .all(|(position, ty)| validate_value(env, position, ty).is_ok())
            }
            _ => false,
        },
        MagType::Function(_, _) => matches!(value, Value::Fn(_)),
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
    apply_with_signature(caller, f, args, None, &BTreeMap::new())
}

fn apply_resolved(
    caller: &Env,
    f: &Value,
    args: &[Value],
    resolved_signature: &MagType,
    type_bindings: &BTreeMap<String, MagType>,
) -> Result<Value, MagError> {
    apply_with_signature(caller, f, args, Some(resolved_signature), type_bindings)
}

fn apply_with_signature(
    caller: &Env,
    f: &Value,
    args: &[Value],
    resolved_signature: Option<&MagType>,
    type_bindings: &BTreeMap<String, MagType>,
) -> Result<Value, MagError> {
    caller.profile_counters(|counters| {
        counters.function_calls = counters.function_calls.saturating_add(1);
        if matches!(f, Value::BuiltinFn(_)) {
            counters.builtin_calls = counters.builtin_calls.saturating_add(1);
        } else if let Value::Fn(function) = f {
            counters.user_function_calls = counters.user_function_calls.saturating_add(1);
            *counters
                .user_function_calls_by_name
                .entry(function.name.as_deref().unwrap_or("<anonymous>").to_owned())
                .or_default() += 1;
        }
    });
    match raw(f) {
        Value::Fn(fun) => {
            let _call_depth = fuel::enter_call()?;
            if args.len() != fun.params.len() {
                return Err(MagError::Arity {
                    expected: fun.params.len(),
                    got: args.len(),
                });
            }
            let (expected_return, type_bindings) = match resolved_signature {
                Some(signature) => {
                    crate::checker::check_resolved_call(caller, fun, signature, type_bindings)
                }
                None => crate::checker::check_call(caller, fun, args),
            }
            .map_err(|error| match &fun.name {
                Some(name) => MagError::Type(format!("calling {name}: {error}")),
                None => error,
            })?;
            let memo_type_bindings = type_bindings
                .iter()
                .map(|(name, ty)| (name.clone(), ty.clone()))
                .collect::<BTreeMap<_, _>>();
            if let Some(result) =
                caller.memoized_call(fun, resolved_signature, &memo_type_bindings, args)
            {
                return Ok(result);
            }
            caller.profile_counters(|counters| {
                *counters
                    .user_function_executions_by_name
                    .entry(fun.name.as_deref().unwrap_or("<anonymous>").to_owned())
                    .or_default() += 1;
            });
            let mut env = caller.child_for_call();
            env.replace_scopes(fun.closure.clone());
            env.push_scope();
            env.define_type_declarations_from(caller);
            for (name, ty) in type_bindings {
                env.define(&name, Value::Type(ty));
            }
            let evaluated = (|| {
                for (parameter, value) in fun.checked.params.iter().zip(args) {
                    env.define_ready(parameter.id, &parameter.name, value.clone());
                }
                let out = eval_checked_block(&mut env, &fun.checked.body)?;
                validate_value(caller, &out, &expected_return).map_err(|error| {
                    match &fun.name {
                        Some(name) => MagError::Type(format!("returning from {name}: {error}")),
                        None => error,
                    }
                })?;
                checked_typed_value(caller, out, expected_return)
            })();
            drop(env);
            match evaluated {
                Ok(result) => {
                    caller.memoize_call(
                        fun,
                        resolved_signature,
                        &memo_type_bindings,
                        args,
                        &result,
                    );
                    if caller.frame_collection_due() {
                        let mut roots = Vec::with_capacity(args.len() + 2);
                        roots.push(Value::Fn(fun.clone()));
                        roots.extend_from_slice(args);
                        roots.push(result.clone());
                        caller.collect_frames(&roots);
                    }
                    Ok(result)
                }
                Err(error) => {
                    if caller.frame_collection_due() {
                        let mut roots = Vec::with_capacity(args.len() + 1);
                        roots.push(Value::Fn(fun.clone()));
                        roots.extend_from_slice(args);
                        caller.collect_frames(&roots);
                    }
                    Err(error)
                }
            }
        }
        Value::BuiltinFn(name) => builtin(caller, name, args),
        _ => Err(MagError::Eval(format!("cannot call {}", f.type_name()))),
    }
}

pub fn apply_named(env: &Env, name: &str, arg: Value) -> Result<Value, MagError> {
    let matching = env
        .lookup_candidates(name)
        .into_iter()
        .filter(|candidate| match candidate {
            Value::Fn(function) => {
                crate::checker::check_call(env, function, std::slice::from_ref(&arg)).is_ok()
            }
            Value::BuiltinFn(_) => true,
            _ => false,
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [function] => apply(env, function, &[arg]),
        [] => Err(MagError::Type(format!("no overload {name} matches call"))),
        _ => Err(MagError::Type(format!(
            "ambiguous overload {name} for call"
        ))),
    }
}

fn collection_len(value: &Value) -> Option<u64> {
    match raw(value) {
        Value::List(values) | Value::Product(values) => Some(values.len() as u64),
        Value::Fields(values) => Some(values.len() as u64),
        _ => None,
    }
}

fn builtin(env: &Env, name: &str, args: &[Value]) -> Result<Value, MagError> {
    let input_items = match name {
        "concat" => args.iter().filter_map(collection_len).sum(),
        "remove_at" | "descriptor_table" => args.first().and_then(collection_len).unwrap_or(0),
        "descriptor_input_assignments" => args.get(1).and_then(collection_len).unwrap_or(0),
        "fold" => args.get(2).and_then(collection_len).unwrap_or(0),
        "map" | "group_by" | "indexed_map" | "filter" | "flat_map" | "sort_by" => {
            args.get(1).and_then(collection_len).unwrap_or(0)
        }
        _ => 0,
    };
    let remove_at_index = args.get(1).and_then(|value| match raw(value) {
        Value::Int(index) if *index >= 0 => Some(*index as u64),
        _ => None,
    });
    env.profile_counters(|counters| {
        *counters
            .builtin_calls_by_name
            .entry(name.to_owned())
            .or_default() += 1;
        if input_items > 0
            || matches!(
                name,
                "concat"
                    | "remove_at"
                    | "descriptor_table"
                    | "descriptor_input_assignments"
                    | "fold"
                    | "map"
                    | "group_by"
                    | "indexed_map"
                    | "filter"
                    | "flat_map"
                    | "sort_by"
            )
        {
            *counters
                .builtin_input_items_by_name
                .entry(name.to_owned())
                .or_default() += input_items;
        }
        if name == "concat" {
            *counters
                .builtin_copied_items_by_name
                .entry(name.to_owned())
                .or_default() += input_items;
        } else if name == "remove_at" {
            *counters
                .builtin_cloned_items_by_name
                .entry(name.to_owned())
                .or_default() += input_items;
            let shifted = remove_at_index
                .map(|index| input_items.saturating_sub(index.saturating_add(1)))
                .unwrap_or(0);
            *counters
                .builtin_shifted_items_by_name
                .entry(name.to_owned())
                .or_default() += shifted;
        }
    });
    match name {
        "__map_empty" => {
            if !(1..=2).contains(&args.len()) {
                return Err(MagError::Arity {
                    expected: 1,
                    got: args.len(),
                });
            }
            Ok(Value::Map(std::sync::Arc::new(Vec::new())))
        }
        "__set_empty" => {
            arity(args, 1)?;
            Ok(Value::Set(std::sync::Arc::new(Vec::new())))
        }
        "__map_insert" => {
            arity(args, 3)?;
            let Value::Map(entries) = raw(&args[0]) else {
                return Err(MagError::Type("__map_insert expects Map".into()));
            };
            if entries.iter().any(|(key, _)| equal(env, key, &args[1])) {
                return Err(MagError::Eval("duplicate Map key".into()));
            }
            let mut entries = entries.as_ref().clone();
            entries.push((args[1].clone(), args[2].clone()));
            Ok(Value::Map(std::sync::Arc::new(entries)))
        }
        "__map_put" => {
            arity(args, 3)?;
            let Value::Map(entries) = raw(&args[0]) else {
                return Err(MagError::Type("__map_put expects Map".into()));
            };
            let mut entries = entries.as_ref().clone();
            if let Some((_, value)) = entries
                .iter_mut()
                .find(|(key, _)| equal(env, key, &args[1]))
            {
                *value = args[2].clone();
            } else {
                entries.push((args[1].clone(), args[2].clone()));
            }
            Ok(Value::Map(std::sync::Arc::new(entries)))
        }
        "__set_insert" => {
            arity(args, 2)?;
            let Value::Set(items) = raw(&args[0]) else {
                return Err(MagError::Type("__set_insert expects Set".into()));
            };
            if items.iter().any(|item| equal(env, item, &args[1])) {
                return Err(MagError::Eval("duplicate Set member".into()));
            }
            let mut items = items.as_ref().clone();
            items.push(args[1].clone());
            Ok(Value::Set(std::sync::Arc::new(items)))
        }
        "__map_get_or" => {
            arity(args, 3)?;
            let Value::Map(entries) = raw(&args[0]) else {
                return Err(MagError::Type("__map_get_or expects Map".into()));
            };
            Ok(entries
                .iter()
                .find(|(key, _)| equal(env, key, &args[1]))
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| args[2].clone()))
        }
        "__map_get" | "__map_contains" => {
            arity(args, 2)?;
            let Value::Map(entries) = raw(&args[0]) else {
                return Err(MagError::Type(format!("{name} expects Map")));
            };
            let found = entries.iter().find(|(key, _)| equal(env, key, &args[1]));
            if name == "__map_contains" {
                Ok(Value::Bool(found.is_some()))
            } else {
                found
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| MagError::Eval("Map key not found".into()))
            }
        }
        "__set_contains" => {
            arity(args, 2)?;
            let Value::Set(items) = raw(&args[0]) else {
                return Err(MagError::Type("__set_contains expects Set".into()));
            };
            Ok(Value::Bool(
                items.iter().any(|item| equal(env, item, &args[1])),
            ))
        }
        "__map_union_left" => {
            arity(args, 2)?;
            let (Value::Map(left), Value::Map(right)) = (raw(&args[0]), raw(&args[1])) else {
                return Err(MagError::Type(
                    "__map_union_left expects Map arguments".into(),
                ));
            };
            let mut entries = left.as_ref().clone();
            for (key, value) in right.iter() {
                if !entries
                    .iter()
                    .any(|(existing, _)| equal(env, existing, key))
                {
                    entries.push((key.clone(), value.clone()));
                }
            }
            Ok(Value::Map(std::sync::Arc::new(entries)))
        }
        "__map_count" | "__set_count" => {
            arity(args, 1)?;
            let count = match (name, raw(&args[0])) {
                ("__map_count", Value::Map(entries)) => entries.len(),
                ("__set_count", Value::Set(items)) => items.len(),
                _ => return Err(MagError::Type(format!("invalid collection for {name}"))),
            };
            Ok(Value::Int(count as i64))
        }
        "artifact" => {
            arity(args, 1)?;
            Ok(Value::Artifact(crate::json::value_to_json(env, &args[0])?))
        }
        "str" => Ok(Value::Str(
            args.iter().map(value_string).collect::<Vec<_>>().join(""),
        )),
        "strip_margin" => {
            arity(args, 1)?;
            let value = raw(&args[0])
                .as_str()
                .ok_or_else(|| MagError::Type("strip_margin expects a String".into()))?;
            Ok(Value::Str(strip_margin(value)))
        }
        "replace" => {
            arity(args, 3)?;
            let value = raw(&args[0])
                .as_str()
                .ok_or_else(|| MagError::Type("replace expects a String".into()))?;
            let from = raw(&args[1])
                .as_str()
                .ok_or_else(|| MagError::Type("replace expects a String pattern".into()))?;
            let to = raw(&args[2])
                .as_str()
                .ok_or_else(|| MagError::Type("replace expects a String replacement".into()))?;
            Ok(Value::Str(value.replace(from, to)))
        }
        "canonical" => {
            arity(args, 1)?;
            let json = canonical_json(crate::json::value_to_json(env, &args[0])?);
            let visits = json_recursive_visits(&json);
            let encoded = serde_json::to_string(&json).map_err(|error| {
                MagError::Eval(format!("canonical serialization failed: {error}"))
            })?;
            env.profile_counters(|counters| {
                counters.canonicalization_recursive_visits = counters
                    .canonicalization_recursive_visits
                    .saturating_add(visits);
                counters.canonicalization_serialized_bytes = counters
                    .canonicalization_serialized_bytes
                    .saturating_add(encoded.len() as u64);
            });
            Ok(Value::Str(encoded))
        }
        "function_name" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Fn(function) => function.name.clone().map(Value::Str).ok_or_else(|| {
                    MagError::Eval(
                        "function_name requires a function bound by let; anonymous closures have no authored identity"
                            .into(),
                    )
                }),
                _ => Err(MagError::Type("function_name expects a function".into())),
            }
        }
        "conforms" => {
            arity(args, 2)?;
            let ty = match raw(&args[1]) {
                Value::TypeDescriptor(ty) => ty.to_mag_type(),
                _ => return Err(MagError::Type("conforms expects a TypeDescriptor".into())),
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
                Value::List(v) => v.len(),
                Value::Fields(v) => v.len(),
                Value::Str(v) => v.chars().count(),
                _ => return Err(MagError::Eval("count expects a collection".into())),
            };
            Ok(Value::Int(n as i64))
        }
        "remove_at" => {
            arity(args, 2)?;
            let mut values = match raw(&args[0]) {
                Value::List(values) => values.as_ref().clone(),
                _ => return Err(MagError::Eval("remove_at expects List".into())),
            };
            let index = match raw(&args[1]) {
                Value::Int(index) if *index >= 0 => *index as usize,
                _ => {
                    return Err(MagError::Eval(
                        "remove_at index must be non-negative Int".into(),
                    ))
                }
            };
            if index >= values.len() {
                return Err(MagError::Eval(format!(
                    "remove_at index {index} is out of bounds for {} values",
                    values.len()
                )));
            }
            values.remove(index);
            Ok(Value::List(std::sync::Arc::new(values)))
        }
        "get" => {
            arity(args, 2)?;
            let key = value_string(&args[1]);
            match raw(&args[0]) {
                Value::Fields(m) => Ok(m
                    .get(key.trim_start_matches(':'))
                    .cloned()
                    .unwrap_or(Value::Unit)),
                Value::JsonValue(serde_json::Value::Object(fields)) => {
                    Ok(crate::json::project_json_value(
                        fields
                            .get(key.trim_start_matches(':'))
                            .unwrap_or(&serde_json::Value::Null),
                    ))
                }
                _ => Err(MagError::Eval(
                    "get expects named fields or a JSON object".into(),
                )),
            }
        }
        "assoc" => {
            arity(args, 3)?;
            let mut m = match raw(&args[0]) {
                Value::Fields(m) => m.as_ref().clone(),
                _ => return Err(MagError::Eval("assoc expects a map".into())),
            };
            m.insert(
                value_string(&args[1]).trim_start_matches(':').into(),
                args[2].clone(),
            );
            Ok(Value::Fields(std::sync::Arc::new(m)))
        }
        "keys" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::Fields(m) => Ok(Value::List(std::sync::Arc::new(
                    m.keys().cloned().map(Value::Str).collect(),
                ))),
                _ => Err(MagError::Eval("keys expects a map".into())),
            }
        }
        "first" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::List(values) => values
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
                (Value::List(a), Value::List(b)) => Ok(Value::List(std::sync::Arc::new(
                    a.iter().chain(b.iter()).cloned().collect(),
                ))),
                _ => Err(MagError::Eval(
                    "concat expects matching strings or collections".into(),
                )),
            }
        }
        "int_mul" | "int_gt" => {
            arity(args, 2)?;
            let (Value::Int(left), Value::Int(right)) = (raw(&args[0]), raw(&args[1])) else {
                return Err(MagError::Type(format!("{name} expects Int arguments")));
            };
            if name == "int_mul" {
                left.checked_mul(*right)
                    .map(Value::Int)
                    .ok_or_else(|| MagError::Eval(format!("int_mul overflow: {left} * {right}")))
            } else {
                Ok(Value::Bool(left > right))
            }
        }
        "=" => {
            arity(args, 2)?;
            Ok(Value::Bool(equal(env, &args[0], &args[1])))
        }
        "fail" => {
            arity(args, 1)?;
            let diagnostic = crate::json::value_to_json(env, &args[0])?;
            Err(MagError::Eval(format!("validation failed: {diagnostic}")))
        }
        "host_input" => {
            arity(args, 2)?;
            let key = raw(&args[0])
                .as_str()
                .ok_or_else(|| MagError::Type("host_input key must be a String".into()))?;
            let Value::TypeTag(expected) = raw(&args[1]) else {
                return Err(MagError::Type("host_input expects a TypeTag".into()));
            };
            let host_inputs = env.lookup_by_type("inputs", &MagType::HostInputs)?;
            let Value::HostInputs(inputs) = raw(&host_inputs) else {
                return Err(MagError::Type(
                    "host_input requires compiler host inputs".into(),
                ));
            };
            let value = inputs
                .get(key)
                .ok_or_else(|| MagError::Type(format!("host input {key:?} is not present")))?;
            crate::json::project_typed_value(env, value, &expected.to_mag_type())
                .map_err(|error| MagError::Type(format!("host input {key:?}: {error}")))
        }
        "type_evidence" => {
            arity(args, 1)?;
            match raw(&args[0]) {
                Value::TypeTag(ty) => Ok(Value::TypeDescriptor(ty.clone())),
                other => Err(MagError::Type(format!(
                    "type_evidence expects TypeTag, got {}",
                    other.type_name()
                ))),
            }
        }
        "type_schema" => {
            arity(args, 1)?;
            let ty = match raw(&args[0]) {
                Value::TypeTag(ty) => ty,
                other => {
                    return Err(MagError::Type(format!(
                        "type_schema expects TypeTag, got {}",
                        other.type_name()
                    )))
                }
            };
            let schema = crate::schema::TypeSchema::reify(env, &ty.to_mag_type())?;
            Ok(Value::TypeSchema(schema))
        }
        "type_constructor" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "type_constructor expects a TypeDescriptor".into(),
                ));
            };
            let name = match ty {
                ConcreteType::Named { name, .. } | ConcreteType::Adt { name, .. } => name.clone(),
                _ => String::new(),
            };
            Ok(Value::Str(name))
        }
        "adt_constructor_payload" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "adt_constructor_payload expects a TypeDescriptor".into(),
                ));
            };
            let Value::Str(name) = raw(&args[1]) else {
                return Err(MagError::Type(
                    "adt_constructor_payload expects a constructor name".into(),
                ));
            };
            let ConcreteType::Adt { constructors, .. } = ty else {
                return Err(MagError::Type(
                    "adt_constructor_payload requires an ADT owner".into(),
                ));
            };
            let payload = constructors
                .iter()
                .find(|constructor| constructor.name == name.as_ref())
                .ok_or_else(|| {
                    MagError::Type(format!("constructor {name} is not a member of this ADT"))
                })?;
            Ok(Value::TypeDescriptor(payload.payload.clone()))
        }
        "type_arguments" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "type_arguments expects a TypeDescriptor".into(),
                ));
            };
            let arguments: &[ConcreteType] = match ty {
                ConcreteType::Named { arguments, .. } | ConcreteType::Adt { arguments, .. } => {
                    arguments
                }
                _ => &[],
            };
            Ok(Value::List(std::sync::Arc::new(
                arguments
                    .iter()
                    .cloned()
                    .map(Value::TypeDescriptor)
                    .collect(),
            )))
        }
        "type_components" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "type_components expects a TypeDescriptor".into(),
                ));
            };
            use crate::types::ConcreteNamedBody;
            let components: Vec<&ConcreteType> = match ty {
                ConcreteType::Named {
                    arguments, body, ..
                } => arguments
                    .iter()
                    .chain(match body {
                        ConcreteNamedBody::Fields { fields } => fields.values().collect::<Vec<_>>(),
                        ConcreteNamedBody::Alias { ty } => vec![ty.as_ref()],
                    })
                    .collect(),
                ConcreteType::Adt {
                    arguments,
                    constructors,
                    ..
                } => arguments
                    .iter()
                    .chain(constructors.iter().map(|c| &c.payload))
                    .collect(),
                ConcreteType::List { item } | ConcreteType::Set { item } => vec![item.as_ref()],
                ConcreteType::Map { key, value } => vec![key.as_ref(), value.as_ref()],
                ConcreteType::Product { items } => items.iter().collect(),
                _ => vec![],
            };
            Ok(Value::List(std::sync::Arc::new(
                components
                    .into_iter()
                    .cloned()
                    .map(Value::TypeDescriptor)
                    .collect(),
            )))
        }
        "list_type" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(item) = raw(&args[0]) else {
                return Err(MagError::Type("list_type expects a TypeDescriptor".into()));
            };
            Ok(Value::TypeDescriptor(ConcreteType::List {
                item: Box::new(item.clone()),
            }))
        }
        "descriptor_schema" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor_schema expects a TypeDescriptor".into(),
                ));
            };
            Ok(Value::TypeSchema(crate::schema::TypeSchema::from_concrete(
                ty,
            )?))
        }
        "type_id" => {
            arity(args, 1)?;
            let Value::TypeDescriptor(ty) = raw(&args[0]) else {
                return Err(MagError::Type("type_id expects a TypeDescriptor".into()));
            };
            Ok(Value::SemanticTypeId(ty.stable_id()))
        }
        "value_type_id" => {
            arity(args, 2)?;
            let declared = declared_value_type(&args[1])?;
            let selected = selected_value_type(env, &args[0], declared)?;
            Ok(Value::SemanticTypeId(selected.stable_id()))
        }
        "value_type_evidence" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(declared) = raw(&args[1]) else {
                return Err(MagError::Type(
                    "value_type_evidence expects a TypeDescriptor".into(),
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
        "packed_empty_record" => {
            arity(args, 1)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed_empty_record expects PackedValue".into(),
                ));
            };
            Ok(Value::Bool(
                matches!(raw(value), Value::Fields(fields) if fields.is_empty()),
            ))
        }
        "packed_path_strings" => {
            arity(args, 3)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed_path_strings expects PackedValue".into(),
                ));
            };
            let Value::List(path) = raw(&args[1]) else {
                return Err(MagError::Type(
                    "packed_path_strings expects a String path".into(),
                ));
            };
            let path = path
                .iter()
                .map(|segment| {
                    segment.as_str().ok_or_else(|| {
                        MagError::Type("packed_path_strings expects a String path".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let Value::Bool(expect_list) = raw(&args[2]) else {
                return Err(MagError::Type(
                    "packed_path_strings expects Bool as its third argument".into(),
                ));
            };

            let mut selected = value.as_ref();
            for (index, segment) in path.iter().enumerate() {
                let Value::Fields(fields) = raw(selected) else {
                    let parent = if index == 0 {
                        "<root>".to_owned()
                    } else {
                        path[..index].join(".")
                    };
                    return Err(MagError::Eval(format!(
                        "packed_path_strings cannot traverse {parent}: expected record, got {}",
                        raw(selected).type_name()
                    )));
                };
                selected = fields.get(*segment).ok_or_else(|| {
                    MagError::Eval(format!(
                        "packed_path_strings path {} has no field {segment:?}",
                        if index == 0 {
                            "<root>".to_owned()
                        } else {
                            path[..index].join(".")
                        }
                    ))
                })?;
            }

            let selected_path = if path.is_empty() {
                "<root>".to_owned()
            } else {
                path.join(".")
            };
            let strings = if *expect_list {
                let Value::List(values) = raw(selected) else {
                    return Err(MagError::Eval(format!(
                        "packed_path_strings expected List<String> at {selected_path}, got {}",
                        raw(selected).type_name()
                    )));
                };
                values
                    .iter()
                    .map(|value| {
                        value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                            MagError::Eval(format!(
                                "packed_path_strings expected List<String> at {selected_path}, got list containing {}",
                                raw(value).type_name()
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                vec![selected.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    MagError::Eval(format!(
                        "packed_path_strings expected String at {selected_path}, got {}",
                        raw(selected).type_name()
                    ))
                })?]
            };
            Ok(Value::List(std::sync::Arc::new(
                strings.into_iter().map(Value::Str).collect(),
            )))
        }
        "packed_record_has_only_key" => {
            arity(args, 2)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed_record_has_only_key expects PackedValue".into(),
                ));
            };
            let key = args[1]
                .as_str()
                .ok_or_else(|| MagError::Type("packed record key must be String".into()))?;
            Ok(Value::Bool(matches!(
                raw(value),
                Value::Fields(fields) if fields.len() == 1 && fields.contains_key(key)
            )))
        }
        "packed_record_has_only_keys" => {
            arity(args, 2)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed_record_has_only_keys expects PackedValue".into(),
                ));
            };
            let values = match raw(&args[1]) {
                Value::List(values) => values,
                _ => {
                    return Err(MagError::Type(
                        "packed_record_has_only_keys expects a String list".into(),
                    ))
                }
            };
            let keys = values
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        MagError::Type("packed_record_has_only_keys expects a String list".into())
                    })
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            Ok(Value::Bool(matches!(
                raw(value),
                Value::Fields(fields)
                    if fields.len() == keys.len()
                        && fields.keys().all(|key| keys.contains(key.as_str()))
            )))
        }
        "packed_field_conforms" => {
            arity(args, 3)?;
            let Value::PackedValue(value) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "packed_field_conforms expects PackedValue".into(),
                ));
            };
            let key = args[1]
                .as_str()
                .ok_or_else(|| MagError::Type("packed record key must be String".into()))?;
            let Value::TypeDescriptor(ty) = raw(&args[2]) else {
                return Err(MagError::Type(
                    "packed_field_conforms expects TypeDescriptor".into(),
                ));
            };
            let valid = match raw(value) {
                Value::Fields(fields) => fields
                    .get(key)
                    .is_some_and(|field| validate_value(env, field, &ty.to_mag_type()).is_ok()),
                _ => false,
            };
            Ok(Value::Bool(valid))
        }
        "descriptor_accepts" | "descriptor_accepts_value" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(format!(
                    "{name} expects TypeDescriptor arguments"
                )));
            };
            let Value::TypeDescriptor(source) = raw(&args[1]) else {
                return Err(MagError::Type(format!(
                    "{name} expects TypeDescriptor arguments"
                )));
            };
            Ok(Value::Bool(if name == "descriptor_accepts_value" {
                target.accepts(source)
            } else {
                target.accepts_edge_source(source)
            }))
        }
        "descriptor_input_covered_by" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor_input_covered_by expects a TypeDescriptor target".into(),
                ));
            };
            let sources = match raw(&args[1]) {
                Value::List(sources) => sources,
                _ => {
                    return Err(MagError::Type(
                        "descriptor_input_covered_by expects a descriptor list".into(),
                    ))
                }
            };
            let sources = sources
                .iter()
                .map(|source| match raw(source) {
                    Value::TypeDescriptor(source) => Ok(source.clone()),
                    _ => Err(MagError::Type(
                        "descriptor_input_covered_by expects a descriptor list".into(),
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Bool(target.input_is_covered_by(&sources)))
        }
        "descriptor_input_assignments" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor_input_assignments expects a TypeDescriptor target".into(),
                ));
            };
            let sources = descriptor_list(
                &args[1],
                "descriptor_input_assignments expects a descriptor list",
            )?;
            let result = target.assign_input_sources(&sources);
            let (compatibility_checks, search_branches) = if env.profiling_enabled() {
                descriptor_assignment_search_profile(target, &sources)
            } else {
                (0, 0)
            };
            env.profile_counters(|counters| {
                counters.descriptor_assignment_sources_examined = counters
                    .descriptor_assignment_sources_examined
                    .saturating_add(sources.len() as u64);
                if let ConcreteType::Product { items } = target {
                    counters.descriptor_assignment_target_product_occurrences = counters
                        .descriptor_assignment_target_product_occurrences
                        .saturating_add(items.len() as u64);
                }
                counters.descriptor_assignment_compatibility_checks = counters
                    .descriptor_assignment_compatibility_checks
                    .saturating_add(compatibility_checks);
                counters.descriptor_assignment_search_branches = counters
                    .descriptor_assignment_search_branches
                    .saturating_add(search_branches);
                match &result {
                    Ok(assignments) => {
                        counters.descriptor_assignment_assignments_produced = counters
                            .descriptor_assignment_assignments_produced
                            .saturating_add(assignments.len() as u64);
                    }
                    Err(crate::types::InputAssignmentError::IncompleteCoverage) => {
                        counters.descriptor_assignment_incomplete_outcomes = counters
                            .descriptor_assignment_incomplete_outcomes
                            .saturating_add(1);
                    }
                    Err(crate::types::InputAssignmentError::AmbiguousCoverage) => {
                        counters.descriptor_assignment_ambiguous_outcomes = counters
                            .descriptor_assignment_ambiguous_outcomes
                            .saturating_add(1);
                    }
                }
            });
            result
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
        "descriptor_output_covered_by" => {
            arity(args, 2)?;
            let Value::TypeDescriptor(target) = raw(&args[0]) else {
                return Err(MagError::Type(
                    "descriptor_output_covered_by expects a TypeDescriptor target".into(),
                ));
            };
            let handlers = descriptor_list(
                &args[1],
                "descriptor_output_covered_by expects a descriptor list",
            )?;
            Ok(Value::Bool(target.output_is_covered_by(&handlers)))
        }
        "descriptor_table" => {
            arity(args, 1)?;
            let descriptors =
                descriptor_list(&args[0], "descriptor_table expects a descriptor list")?;
            let top_level = descriptors.len() as u64;
            let recursive_nodes = descriptors.iter().map(descriptor_node_count).sum::<u64>();
            let hashed_bytes = descriptors.iter().map(descriptor_hashed_bytes).sum::<u64>();
            let mut declarations = BTreeMap::new();
            let mut inserts = 0_u64;
            let mut duplicates = 0_u64;
            for descriptor in descriptors {
                for (id, declaration) in descriptor.declarations()? {
                    if let Some(existing) = declarations.insert(id.clone(), declaration.clone()) {
                        duplicates = duplicates.saturating_add(1);
                        if existing != declaration {
                            return Err(MagError::Type(format!(
                                "semantic type identity collision at {id}"
                            )));
                        }
                    } else {
                        inserts = inserts.saturating_add(1);
                    }
                }
            }
            env.profile_counters(|counters| {
                counters.descriptor_table_top_level_descriptors = counters
                    .descriptor_table_top_level_descriptors
                    .saturating_add(top_level);
                counters.descriptor_table_recursive_nodes = counters
                    .descriptor_table_recursive_nodes
                    .saturating_add(recursive_nodes);
                counters.descriptor_table_declaration_inserts = counters
                    .descriptor_table_declaration_inserts
                    .saturating_add(inserts);
                counters.descriptor_table_duplicate_declaration_hits = counters
                    .descriptor_table_duplicate_declaration_hits
                    .saturating_add(duplicates);
                counters.descriptor_table_stable_id_invocations = counters
                    .descriptor_table_stable_id_invocations
                    .saturating_add(recursive_nodes);
                counters.descriptor_table_hashed_bytes = counters
                    .descriptor_table_hashed_bytes
                    .saturating_add(hashed_bytes);
            });
            Ok(Value::Map(std::sync::Arc::new(
                declarations
                    .into_iter()
                    .map(|(id, descriptor)| (Value::Str(id), Value::TypeDescriptor(descriptor)))
                    .collect(),
            )))
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
        "map" | "group_by" | "indexed_map" | "filter" | "flat_map" | "fold" | "sort_by" => {
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
            env.observe(
                || crate::observation::Query::read(env.source_dir(), path),
                &full,
                &s,
            );
            if let Some(Value::Fields(m)) = args.get(1) {
                for (k, v) in m.iter() {
                    s = s.replace(&format!("{{{{{k}}}}}"), &value_string(v));
                }
            }
            Ok(Value::Str(s))
        }
        "read_json" => {
            arity(args, 1)?;
            let path = args[0]
                .as_str()
                .ok_or_else(|| MagError::Eval("read_json path must be a string".into()))?;
            let roots = std::iter::once(env.source_dir().to_path_buf())
                .chain(env.module_roots().iter().cloned())
                .collect::<Vec<_>>();
            let full = crate::resolver::resolve_json(&roots, path)?;
            let source = env.read_file(&full, path)?;
            env.observe(
                || crate::observation::Query::json(roots, path),
                &full,
                &source,
            );
            let value = serde_json::from_str(&source)
                .map_err(|error| MagError::Eval(format!("cannot parse JSON {path}: {error}")))?;
            Ok(Value::JsonValue(value))
        }
        "require" => Err(MagError::Eval("require is a special form".into())),
        _ => Err(MagError::Eval(format!("unknown builtin {name}"))),
    }
}

fn json_recursive_visits(value: &serde_json::Value) -> u64 {
    1 + match value {
        serde_json::Value::Array(items) => items.iter().map(json_recursive_visits).sum(),
        serde_json::Value::Object(fields) => fields.values().map(json_recursive_visits).sum(),
        _ => 0,
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
        Value::List(v) => Ok(v.clone()),
        _ => Err(MagError::Eval(format!("{name} expects a collection"))),
    };
    match name {
        "map" => {
            arity(args, 2)?;
            Ok(Value::List(std::sync::Arc::new(
                seq(&args[1])?
                    .iter()
                    .map(|v| apply(env, &args[0], std::slice::from_ref(v)))
                    .collect::<Result<_, _>>()?,
            )))
        }
        "group_by" => {
            arity(args, 2)?;
            let mut groups = BTreeMap::<String, Vec<Value>>::new();
            for value in seq(&args[1])?.iter() {
                let key = apply(env, &args[0], std::slice::from_ref(value))?;
                let key = key
                    .as_str()
                    .ok_or_else(|| MagError::Eval("group_by callback must return String".into()))?;
                groups
                    .entry(key.to_owned())
                    .or_default()
                    .push(value.clone());
            }
            Ok(Value::Map(std::sync::Arc::new(
                groups
                    .into_iter()
                    .map(|(key, values)| {
                        (Value::Str(key), Value::List(std::sync::Arc::new(values)))
                    })
                    .collect(),
            )))
        }
        "indexed_map" => {
            arity(args, 2)?;
            Ok(Value::List(std::sync::Arc::new(
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
            Ok(Value::List(std::sync::Arc::new(out)))
        }
        "flat_map" => {
            arity(args, 2)?;
            let mut out = vec![];
            for v in seq(&args[1])?.iter().cloned() {
                out.extend(seq(&apply(env, &args[0], &[v])?)?.iter().cloned())
            }
            Ok(Value::List(std::sync::Arc::new(out)))
        }
        "fold" => {
            arity(args, 3)?;
            let mut acc = args[1].clone();
            for v in seq(&args[2])?.iter().cloned() {
                acc = apply(env, &args[0], &[acc, v])?;
            }
            Ok(acc)
        }
        "sort_by" => {
            arity(args, 2)?;
            let mut keyed = seq(&args[1])?
                .iter()
                .cloned()
                .map(|value| {
                    let key = apply(env, &args[0], std::slice::from_ref(&value))?;
                    let key = key
                        .as_str()
                        .ok_or_else(|| {
                            MagError::Eval("sort_by callback must return String".into())
                        })?
                        .to_owned();
                    Ok((key, value))
                })
                .collect::<Result<Vec<_>, MagError>>()?;
            keyed.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(Value::List(std::sync::Arc::new(
                keyed.into_iter().map(|(_, value)| value).collect(),
            )))
        }
        _ => unreachable!(),
    }
}

fn descriptor_assignment_search_profile(
    target: &ConcreteType,
    sources: &[ConcreteType],
) -> (u64, u64) {
    let ConcreteType::Product { items } = target else {
        let mut checks = 0_u64;
        for source in sources {
            checks = checks.saturating_add(1);
            if !target.accepts_edge_source(source) {
                break;
            }
        }
        return (checks, 0);
    };
    let mut checks = 0_u64;
    let component_sources = sources
        .iter()
        .filter(|source| {
            checks = checks.saturating_add(1);
            !target.accepts(source)
        })
        .collect::<Vec<_>>();
    if component_sources.is_empty() {
        for source in sources {
            checks = checks.saturating_add(1);
            if !target.accepts(source) {
                break;
            }
        }
        return (checks, 0);
    }
    if component_sources.len() != items.len() {
        return (checks, 0);
    }
    let mut capacities = BTreeMap::<ConcreteType, usize>::new();
    for item in items {
        *capacities.entry(item.clone()).or_default() += 1;
    }
    fn walk(
        sources: &[&ConcreteType],
        index: usize,
        capacities: &mut BTreeMap<ConcreteType, usize>,
        checks: &mut u64,
        branches: &mut u64,
    ) {
        let Some(source) = sources.get(index) else {
            return;
        };
        let candidates = capacities
            .iter()
            .filter(|(component, remaining)| {
                if **remaining == 0 {
                    return false;
                }
                *checks = checks.saturating_add(1);
                component.accepts(source)
            })
            .map(|(component, _)| component.clone())
            .collect::<Vec<_>>();
        for component in candidates {
            *branches = branches.saturating_add(1);
            if let Some(remaining) = capacities.get_mut(&component) {
                *remaining -= 1;
            }
            walk(sources, index + 1, capacities, checks, branches);
            if let Some(remaining) = capacities.get_mut(&component) {
                *remaining += 1;
            }
        }
    }
    let mut branches = 0_u64;
    walk(
        &component_sources,
        0,
        &mut capacities,
        &mut checks,
        &mut branches,
    );
    (checks, branches)
}

fn descriptor_node_count(descriptor: &ConcreteType) -> u64 {
    1 + match descriptor {
        ConcreteType::Named {
            arguments, body, ..
        } => {
            arguments.iter().map(descriptor_node_count).sum::<u64>()
                + match body {
                    crate::types::ConcreteNamedBody::Fields { fields } => {
                        fields.values().map(descriptor_node_count).sum()
                    }
                    crate::types::ConcreteNamedBody::Alias { ty } => descriptor_node_count(ty),
                }
        }
        ConcreteType::Adt {
            arguments,
            constructors,
            ..
        } => {
            arguments.iter().map(descriptor_node_count).sum::<u64>()
                + constructors
                    .iter()
                    .map(|constructor| descriptor_node_count(&constructor.payload))
                    .sum::<u64>()
        }
        ConcreteType::List { item } | ConcreteType::Set { item } => descriptor_node_count(item),
        ConcreteType::Map { key, value } => {
            descriptor_node_count(key) + descriptor_node_count(value)
        }
        ConcreteType::Product { items } => items.iter().map(descriptor_node_count).sum(),
        ConcreteType::JsonValue
        | ConcreteType::Unit
        | ConcreteType::Bool
        | ConcreteType::Int
        | ConcreteType::Float
        | ConcreteType::String => 0,
    }
}

fn descriptor_hashed_bytes(descriptor: &ConcreteType) -> u64 {
    let own = serde_json::to_vec(descriptor).map_or(0, |bytes| bytes.len() as u64);
    own + match descriptor {
        ConcreteType::Named {
            arguments, body, ..
        } => {
            arguments.iter().map(descriptor_hashed_bytes).sum::<u64>()
                + match body {
                    crate::types::ConcreteNamedBody::Fields { fields } => {
                        fields.values().map(descriptor_hashed_bytes).sum()
                    }
                    crate::types::ConcreteNamedBody::Alias { ty } => descriptor_hashed_bytes(ty),
                }
        }
        ConcreteType::Adt {
            arguments,
            constructors,
            ..
        } => {
            arguments.iter().map(descriptor_hashed_bytes).sum::<u64>()
                + constructors
                    .iter()
                    .map(|constructor| descriptor_hashed_bytes(&constructor.payload))
                    .sum::<u64>()
        }
        ConcreteType::List { item } | ConcreteType::Set { item } => descriptor_hashed_bytes(item),
        ConcreteType::Map { key, value } => {
            descriptor_hashed_bytes(key) + descriptor_hashed_bytes(value)
        }
        ConcreteType::Product { items } => items.iter().map(descriptor_hashed_bytes).sum(),
        ConcreteType::JsonValue
        | ConcreteType::Unit
        | ConcreteType::Bool
        | ConcreteType::Int
        | ConcreteType::Float
        | ConcreteType::String => 0,
    }
}

fn descriptor_list(value: &Value, error: &str) -> Result<Vec<ConcreteType>, MagError> {
    let values = match raw(value) {
        Value::List(values) => values,
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
        _ => format!("<{:?}>", v.type_name()),
    }
}

fn strip_margin(value: &str) -> String {
    value
        .split_inclusive('\n')
        .map(|line| {
            let margin = line.char_indices().find_map(|(index, character)| {
                if character == '|' {
                    Some(Some(index + character.len_utf8()))
                } else if character.is_whitespace() {
                    None
                } else {
                    Some(None)
                }
            });
            match margin.flatten() {
                Some(content_start) => &line[content_start..],
                None => line,
            }
        })
        .collect()
}
pub(crate) fn equal(env: &Env, a: &Value, b: &Value) -> bool {
    env.profile_counters(|counters| {
        counters.value_equality_visits = counters.value_equality_visits.saturating_add(1);
    });
    fn nominal<'a>(env: &Env, value: &'a Value) -> Option<(ConcreteType, &'a Value)> {
        match value {
            Value::Typed(inner, ty) => {
                if let Ok(concrete @ ConcreteType::Named { .. }) = ConcreteType::resolve(env, ty) {
                    Some((concrete, inner))
                } else {
                    nominal(env, inner)
                }
            }
            _ => None,
        }
    }
    match (nominal(env, a), nominal(env, b)) {
        (Some((left, a)), Some((right, b))) => return left == right && equal(env, a, b),
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    match (raw(a), raw(b)) {
        (Value::Unit, Value::Unit) => true,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        // Equality is bit-exact, including the sign of zero and NaN payloads.
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Keyword(a), Value::Keyword(b)) => a == b,
        (Value::Symbol(a), Value::Symbol(b)) => a == b,
        (Value::List(a), Value::List(b)) | (Value::Product(a), Value::Product(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(left, right)| equal(env, left, right))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.iter().any(|(other_key, other_value)| {
                        equal(env, key, other_key) && equal(env, value, other_value)
                    })
                })
        }
        (Value::Set(a), Value::Set(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|item| b.iter().any(|other| equal(env, item, other)))
        }
        (Value::Fields(a), Value::Fields(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| equal(env, value, other)))
        }
        (
            Value::Adt {
                owner: left_owner,
                constructor: left_constructor,
                payload: left_payload,
            },
            Value::Adt {
                owner: right_owner,
                constructor: right_constructor,
                payload: right_payload,
            },
        ) => {
            ConcreteType::resolve(env, left_owner)
                .ok()
                .zip(ConcreteType::resolve(env, right_owner).ok())
                .is_some_and(|(left, right)| left == right)
                && left_constructor == right_constructor
                && equal(env, left_payload, right_payload)
        }
        (Value::JsonValue(a), Value::JsonValue(b)) => json_equal(a, b),
        _ => false,
    }
}

pub(crate) fn json_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as Json;
    match (a, b) {
        (Json::Number(a), Json::Number(b)) if a.is_f64() && b.is_f64() => a
            .as_f64()
            .zip(b.as_f64())
            .is_some_and(|(a, b)| a.to_bits() == b.to_bits()),
        (Json::Array(a), Json::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_equal(a, b))
        }
        (Json::Object(a), Json::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| json_equal(a, b)))
        }
        _ => a == b,
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
    _env: &Env,
    _value: &Value,
    declared: &ConcreteType,
) -> Result<ConcreteType, MagError> {
    Ok(declared.clone())
}

fn direct_module_exports(env: &Env, name: &str) -> Result<BTreeSet<String>, MagError> {
    let resolve_phase = env.profile_phase(Phase::ModuleResolve);
    let resolved = crate::resolver::resolve_module(env.module_roots(), name)?;
    drop(resolve_phase);
    let read_phase = env.profile_phase(Phase::ModuleRead);
    let source = std::fs::read_to_string(&resolved.path)
        .map_err(|error| MagError::Eval(format!("cannot read module {name}: {error}")))?;
    drop(read_phase);
    let snapshot = crate::diagnostic::SourceSnapshot::file(&resolved.path, &source);
    let profiler = env.profiler();
    let authored = crate::frontend::compile_source(
        resolved.syntax,
        &snapshot,
        profiler.as_ref(),
        crate::frontend::SourceRole::Module,
    )?;
    Ok(authored
        .forms
        .iter()
        .filter_map(|form| match form {
            Form::Type(declaration) if !declaration.name.contains('.') => {
                Some(declaration.name.clone())
            }
            Form::Block(crate::authored::BlockItem::Let { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .collect())
}

fn eval_imports(
    env: &mut Env,
    requires: &[crate::authored::Require],
    authored: &Module,
) -> Result<(), MagError> {
    use crate::authored::ImportExposure;

    let mut direct_exports = BTreeMap::new();
    for require in requires {
        if !direct_exports.contains_key(&require.module) {
            direct_exports.insert(
                require.module.clone(),
                direct_module_exports(env, &require.module)?,
            );
        }
    }

    let mut bare = BTreeSet::new();
    let mut suppressions = BTreeMap::<String, BTreeSet<String>>::new();
    let mut positive = BTreeSet::<(String, String, String)>::new();
    let mut canonical_modules = BTreeSet::new();
    let mut aliases = BTreeMap::<String, BTreeSet<String>>::new();
    for require in requires {
        let exports = direct_exports.get(&require.module).ok_or_else(|| {
            MagError::Eval(format!("resolved import {} is unavailable", require.module))
        })?;
        match &require.exposure {
            ImportExposure::Open => {
                bare.insert(require.module.clone());
                canonical_modules.insert(require.module.clone());
            }
            ImportExposure::Qualified => {
                canonical_modules.insert(require.module.clone());
            }
            ImportExposure::NamespaceAlias(alias) => {
                aliases
                    .entry(alias.clone())
                    .or_default()
                    .insert(require.module.clone());
            }
            ImportExposure::Selective(selectors) => {
                canonical_modules.insert(require.module.clone());
                for selector in selectors {
                    if !exports.contains(&selector.export) {
                        return Err(MagError::Unresolved(format!(
                            "{}.{} (import selector)",
                            require.module, selector.export
                        )));
                    }
                    if let Some(local) = &selector.local {
                        positive.insert((
                            require.module.clone(),
                            selector.export.clone(),
                            local.clone(),
                        ));
                    } else {
                        suppressions
                            .entry(require.module.clone())
                            .or_default()
                            .insert(selector.export.clone());
                    }
                }
            }
        }
    }

    // Local spellings map to canonical declaration identities. Repeated
    // exposure of one declaration is idempotent; overload members remain
    // grouped under that declaration instead of becoming collision candidates.
    let mut opened = BTreeMap::<String, BTreeSet<(String, String)>>::new();
    for module in &bare {
        let suppressed = suppressions.get(module);
        for export in &direct_exports[module] {
            if !suppressed.is_some_and(|names| names.contains(export)) {
                opened
                    .entry(export.clone())
                    .or_default()
                    .insert((module.clone(), export.clone()));
            }
        }
    }
    for (module, export, local) in positive {
        opened.entry(local).or_default().insert((module, export));
    }

    let mut candidates = BTreeMap::<String, BTreeSet<String>>::new();
    for form in &authored.forms {
        match form {
            Form::Type(declaration) if !declaration.name.contains('.') => {
                candidates
                    .entry(declaration.name.clone())
                    .or_default()
                    .insert(format!("local type {}", declaration.name));
            }
            Form::Block(crate::authored::BlockItem::Let { name, .. }) => {
                candidates
                    .entry(name.clone())
                    .or_default()
                    .insert(format!("local let {name}"));
            }
            _ => {}
        }
    }
    for builtin in crate::checker::BUILTIN_NAMES.iter().copied().chain([
        "List",
        "Map",
        "Set",
        "Artifact",
        "JsonValue",
        "TypeDescriptor",
        "TypeSchema",
        "SemanticTypeId",
        "PackedValue",
        "Unit",
        "Bool",
        "Int",
        "Float",
        "String",
    ]) {
        candidates
            .entry(builtin.to_owned())
            .or_default()
            .insert(format!("builtin {builtin}"));
    }
    for module in &canonical_modules {
        if let Some(root) = module.split('.').next() {
            candidates
                .entry(root.to_owned())
                .or_default()
                .insert(format!("canonical namespace {root}"));
        }
    }
    for (alias, modules) in &aliases {
        for module in modules {
            candidates
                .entry(alias.clone())
                .or_default()
                .insert(format!("namespace alias {alias} for {module}"));
        }
    }
    for (local, declarations) in &opened {
        for (module, export) in declarations {
            candidates
                .entry(local.clone())
                .or_default()
                .insert(format!("imported export {module}.{export}"));
        }
    }

    // A directly imported descendant is a namespace segment, not a field of
    // the imported prefix module.
    for descendant in &canonical_modules {
        let segments = descendant.split('.').collect::<Vec<_>>();
        for index in 1..segments.len() {
            let prefix = segments[..index].join(".");
            let segment = segments[index];
            if canonical_modules.contains(&prefix)
                && direct_exports
                    .get(&prefix)
                    .is_some_and(|exports| exports.contains(segment))
            {
                let spelling = format!("{prefix}.{segment}");
                let set = candidates.entry(spelling).or_default();
                set.insert(format!(
                    "canonical namespace {}",
                    segments[..=index].join(".")
                ));
                set.insert(format!("imported export {prefix}.{segment}"));
            }
        }
    }

    if let Some((spelling, conflicting)) = candidates.iter().find(|(_, set)| {
        set.len() > 1
            && set.iter().any(|candidate| {
                candidate.starts_with("imported export ")
                    || candidate.starts_with("canonical namespace ")
                    || candidate.starts_with("namespace alias ")
            })
    }) {
        let mut repairs = BTreeSet::new();
        for require in requires {
            match &require.exposure {
                ImportExposure::Open
                    if direct_exports[&require.module].contains(spelling.as_str()) =>
                {
                    repairs.insert(format!(
                        "replace `import {0}` with `import {0}.{{}}`, or add `import {0}.{{{1} as _}}`",
                        require.module, spelling
                    ));
                }
                ImportExposure::Selective(selectors) => {
                    for selector in selectors
                        .iter()
                        .filter(|selector| selector.local.as_deref() == Some(spelling.as_str()))
                    {
                        repairs.insert(format!(
                            "remove or rename selector `{}.{{{}}}`",
                            require.module, selector.export
                        ));
                    }
                }
                ImportExposure::Qualified if require.module.split('.').next() == Some(spelling) => {
                    repairs.insert(format!(
                        "replace `import {0}.{{}}` with `import {0} as alias` and qualify its uses through `alias`",
                        require.module
                    ));
                }
                _ => {}
            }
        }
        let repairs = if repairs.is_empty() {
            String::new()
        } else {
            format!(
                "; repairs: {}",
                repairs.into_iter().collect::<Vec<_>>().join("; ")
            )
        };
        return Err(MagError::Type(format!(
            "import collision at '{spelling}'; candidates: {}{repairs}",
            conflicting.iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let mut modules = BTreeMap::new();
    for require in requires {
        if !modules.contains_key(&require.module) {
            eval_require(env, &require.module)?;
            let defs = env.module_defs(&require.module).ok_or_else(|| {
                MagError::Eval(format!("module {} did not enter the cache", require.module))
            })?;
            modules.insert(require.module.clone(), defs);
        }
    }

    for module in &canonical_modules {
        env.install_module(module, modules[module].clone());
    }
    for (alias, targets) in &aliases {
        if let Some(module) = targets.iter().next() {
            env.install_module(alias, modules[module].clone());
        }
    }
    for (local, declarations) in opened {
        let Some((module, export)) = declarations.into_iter().next() else {
            continue;
        };
        for value in &modules[&module][&export] {
            env.define(&local, value.clone());
        }
    }
    Ok(())
}

fn eval_require(env: &mut Env, name: &str) -> Result<Value, MagError> {
    if let Some(defs) = env.module_cached(name) {
        return Ok(Value::ModuleNamespace(std::sync::Arc::new(
            module_value_map(&defs),
        )));
    }
    env.begin_module(name)?;
    let resolve_phase = env.profile_phase(Phase::ModuleResolve);
    let resolved = crate::resolver::resolve_module(env.module_roots(), name)?;
    let syntax = resolved.syntax;
    let path = resolved.path;
    drop(resolve_phase);
    let read_phase = env.profile_phase(Phase::ModuleRead);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| MagError::Eval(format!("cannot read module {name}: {e}")))?;
    env.observe(
        || crate::observation::Query::module(env.module_roots(), name),
        &path,
        &content,
    );
    drop(read_phase);
    let source_snapshot = crate::diagnostic::SourceSnapshot::file(&path, &content);
    let profiler = env.profiler();
    let authored = crate::frontend::compile_source(
        syntax,
        &source_snapshot,
        profiler.as_ref(),
        crate::frontend::SourceRole::Module,
    )?;
    let mut module = env.module_env(name);
    let eval_phase = env.profile_phase(Phase::ModuleEvaluate);
    let result = eval_program(&mut module, &authored);
    drop(eval_phase);
    match result {
        Ok(_) => {
            let direct_names = authored
                .forms
                .iter()
                .filter_map(|form| match form {
                    Form::Type(declaration) if !declaration.name.contains('.') => {
                        Some(declaration.name.clone())
                    }
                    Form::Block(crate::authored::BlockItem::Let { name, .. }) => Some(name.clone()),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let defs = module.user_defs(&direct_names);
            env.finish_module(name, defs.clone());
            Ok(Value::ModuleNamespace(std::sync::Arc::new(
                module_value_map(&defs),
            )))
        }
        Err(e) => Err(e),
    }
}

fn module_value_map(defs: &BTreeMap<String, Vec<Value>>) -> BTreeMap<String, Value> {
    defs.iter()
        .filter_map(|(name, values)| values.first().cloned().map(|value| (name.clone(), value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant_function(
        env: &Env,
        name: &str,
        closure: Vec<(String, Value)>,
    ) -> Result<Value, MagError> {
        let mut captured = env.child_for_call();
        for (name, value) in closure {
            captured.define(&name, value);
        }
        let body = vec![crate::authored::BlockItem::Expr(
            crate::authored::Expr::Name(name.into()),
        )];
        let checked = crate::checker::compile_resolved_function(
            &captured,
            None,
            &[],
            &[],
            &[],
            &MagType::Int,
            &body,
        )?;
        let closure = captured.snapshot();
        Ok(Value::Fn(std::sync::Arc::new(FnValue {
            name: None,
            type_params: vec![],
            equality_params: vec![],
            params: vec![],
            param_types: vec![],
            return_type: MagType::Int,
            checked: std::sync::Arc::new(checked),
            closure,
        })))
    }

    fn typed_int(value: Value) -> i64 {
        match value {
            Value::Typed(value, _) => match value.as_ref() {
                Value::Int(value) => *value,
                other => panic!("expected typed Int, got {other:?}"),
            },
            other => panic!("expected typed Int, got {other:?}"),
        }
    }

    #[test]
    fn float_equality_preserves_every_bit() {
        let env = Env::new();
        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let other_nan = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(equal(&env, &Value::Float(nan), &Value::Float(nan)));
        assert!(!equal(&env, &Value::Float(nan), &Value::Float(other_nan)));
        assert!(!equal(&env, &Value::Float(0.0), &Value::Float(-0.0)));
    }

    #[test]
    fn group_by_defensively_rejects_a_runtime_non_string_key() {
        let env = Env::new_with_stdlib();
        let values = Value::List(std::sync::Arc::new(vec![Value::List(std::sync::Arc::new(
            vec![Value::Int(1)],
        ))]));
        let error = collection_builtin(
            &env,
            "group_by",
            &[Value::BuiltinFn("count".into()), values],
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "eval: group_by callback must return String"
        );
    }

    #[test]
    fn default_evaluation_budget_allows_exactly_1_000_000_steps() {
        let limit = crate::CompilerLimits::default().evaluation_steps;
        assert_eq!(limit, 1_000_000);
        let _fuel = fuel::install(limit);

        for _ in 0..limit {
            fuel::step().unwrap();
        }

        assert_eq!(fuel::remaining(), Some(0));
        assert!(matches!(fuel::step(), Err(MagError::Budget(_))));
    }

    #[test]
    fn lexical_closure_overrides_colliding_caller_binding() {
        let mut caller = Env::new();
        caller.define("value", Value::Int(99));
        let function =
            constant_function(&caller, "value", vec![("value".into(), Value::Int(1))]).unwrap();
        let _fuel = fuel::install(100);

        assert_eq!(typed_int(apply(&caller, &function, &[]).unwrap()), 1);
    }

    #[test]
    fn caller_only_late_binding_is_invisible() {
        let mut caller = Env::new();
        caller.define("late", Value::Int(42));
        let result = constant_function(&caller, "late", vec![]);

        assert!(matches!(result, Err(MagError::Unresolved(name)) if name == "late"));
    }

    #[test]
    fn checked_same_type_closure_duplicates_are_ambiguous() {
        let caller = Env::new();
        let result = constant_function(
            &caller,
            "value",
            vec![
                ("value".into(), Value::Int(1)),
                ("value".into(), Value::Int(2)),
            ],
        );

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ambiguous overload value"));
    }

    #[test]
    fn value_parameters_override_same_named_generic_bindings() {
        let env = Env::new();
        let type_params = vec!["T".into()];
        let params = vec!["T".into()];
        let param_types = vec![MagType::Var("T".into())];
        let result = MagType::Var("T".into());
        let body = vec![crate::authored::BlockItem::Expr(
            crate::authored::Expr::Name("T".into()),
        )];
        let checked = crate::checker::compile_resolved_function(
            &env,
            None,
            &type_params,
            &params,
            &param_types,
            &result,
            &body,
        )
        .unwrap();
        let function = Value::Fn(std::sync::Arc::new(FnValue {
            name: None,
            type_params,
            equality_params: vec![],
            params,
            param_types,
            return_type: result,
            checked: std::sync::Arc::new(checked),
            closure: vec![],
        }));
        let _fuel = fuel::install(100);

        assert_eq!(
            typed_int(apply(&Env::new(), &function, &[Value::Int(7)]).unwrap()),
            7
        );
    }

    #[test]
    fn memoized_result_is_independent_of_caller_only_bindings() {
        let mut first_caller = Env::new();
        let function = constant_function(
            &first_caller,
            "value",
            vec![("value".into(), Value::Int(1))],
        )
        .unwrap();
        first_caller.define("value", Value::Int(99));
        let mut second_caller = first_caller.child_for_call();
        second_caller.define("value", Value::Int(100));
        let _fuel = fuel::install(100);

        assert_eq!(typed_int(apply(&first_caller, &function, &[]).unwrap()), 1);
        assert_eq!(typed_int(apply(&second_caller, &function, &[]).unwrap()), 1);
    }

    #[test]
    fn repeated_call_reuses_the_cached_shared_result_without_spending_fuel() {
        let source = r#"
            let values = [1, 2, 3]
            let copy: fn(List<Int>) -> List<Int> = |items| =>
              map(((|item| => item): fn(Int) -> Int), items)
            let copied = copy(values)
            artifact(())
        "#;
        let source = crate::diagnostic::SourceSnapshot::named("test.mag", source);
        let module =
            crate::new_syntax::compile_source(&source, None, crate::frontend::SourceRole::Entry)
                .unwrap();
        let mut env = Env::new();
        let _fuel = fuel::install(1_000);
        eval_program(&mut env, &module).unwrap();
        let argument = env.lookup("values").unwrap().clone();
        let function = env.lookup("copy").unwrap().clone();

        let first = apply(&env, &function, std::slice::from_ref(&argument)).unwrap();
        let after_first = fuel::remaining().unwrap();
        let second = apply(&env, &function, &[argument]).unwrap();

        assert_eq!(fuel::remaining(), Some(after_first));
        match (first, second) {
            (Value::Typed(left, _), Value::Typed(right, _)) => {
                assert!(std::sync::Arc::ptr_eq(&left, &right));
            }
            values => panic!("expected typed results, got {values:?}"),
        }
    }
}
