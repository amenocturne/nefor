use crate::ast;
use crate::authored::{self, AuthoringError};
use crate::diagnostic::SourceSnapshot;
use crate::error::MagError;

pub fn compile_source(
    source: &SourceSnapshot,
    profiler: Option<&crate::profile::CompileProfiler>,
    role: crate::frontend::SourceRole,
) -> Result<authored::Module, MagError> {
    let (lex, parse) = match role {
        crate::frontend::SourceRole::Entry => (
            crate::profile::Phase::EntryLex,
            crate::profile::Phase::EntryParse,
        ),
        crate::frontend::SourceRole::Module => (
            crate::profile::Phase::ModuleLex,
            crate::profile::Phase::ModuleParse,
        ),
    };
    let phase = profiler.map(|profiler| profiler.start_phase(lex));
    let tokens = crate::lexer::tokenize_source(source)?;
    drop(phase);
    let phase = profiler.map(|profiler| profiler.start_phase(parse));
    let expressions = crate::parser::parse_source(&tokens, source)?;
    drop(phase);
    Ok(lower_module(&expressions))
}

pub fn lower_module(expressions: &[ast::Expr]) -> authored::Module {
    authored::Module {
        forms: expressions.iter().map(lower_top_level).collect(),
    }
}

fn lower_top_level(expression: &ast::Expr) -> authored::Form {
    let ast::Expr::List(items) = expression else {
        return authored::Form::Block(lower_block_item(expression));
    };
    match items.first().and_then(ast::Expr::as_symbol) {
        Some("require") => lower_require(items),
        Some("type") => lower_type_declaration(items),
        _ => authored::Form::Block(lower_block_item(expression)),
    }
}

fn lower_require(items: &[ast::Expr]) -> authored::Form {
    if items.len() != 2 {
        return authored::Form::Invalid(AuthoringError::Arity {
            expected: 1,
            got: items.len() - 1,
        });
    }
    match &items[1] {
        ast::Expr::Str(module) => authored::Form::Require(authored::Require {
            module: module.clone(),
            exposure: authored::ImportExposure::Qualified,
        }),
        _ => authored::Form::Invalid(AuthoringError::Eval(
            "require expects a module string".into(),
        )),
    }
}

fn lower_type_declaration(items: &[ast::Expr]) -> authored::Form {
    let args = &items[1..];
    if args.is_empty() || args.len() > 3 {
        return authored::Form::Invalid(AuthoringError::Eval(
            "type requires a name, optional generic parameters, and a body".into(),
        ));
    }
    let Some(name) = args[0].as_symbol() else {
        return authored::Form::Invalid(AuthoringError::Eval("type name must be a symbol".into()));
    };
    let (params, body) = match args {
        [_, ast::Expr::Vector(params), body] => {
            let params = params
                .iter()
                .map(|parameter| {
                    parameter.as_symbol().map(str::to_owned).ok_or_else(|| {
                        AuthoringError::Type("generic parameters must be symbols".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>();
            match params {
                Ok(params) => {
                    let mut seen = std::collections::HashSet::new();
                    if let Some(duplicate) =
                        params.iter().find(|parameter| !seen.insert(*parameter))
                    {
                        return authored::Form::Invalid(AuthoringError::Type(format!(
                            "duplicate generic parameter {duplicate}"
                        )));
                    }
                    (params, lower_declaration_body(body))
                }
                Err(error) => return authored::Form::Invalid(error),
            }
        }
        [_, body] => (vec![], lower_declaration_body(body)),
        [_] => {
            return authored::Form::Invalid(AuthoringError::Type(
                "type declaration requires a body".into(),
            ))
        }
        _ => unreachable!(),
    };
    authored::Form::Type(authored::TypeDeclaration {
        name: name.to_owned(),
        params,
        body,
    })
}

fn lower_declaration_body(expression: &ast::Expr) -> authored::TypeDeclarationBody {
    if let ast::Expr::Map(fields) = expression {
        let mut lowered = Vec::with_capacity(fields.len());
        for (key, value) in fields {
            let Some(key) = record_key(key) else {
                return authored::TypeDeclarationBody::Newtype(authored::Type::Invalid(
                    "named field names must be symbols or keywords".into(),
                ));
            };
            lowered.push((key, lower_type(value)));
        }
        return authored::TypeDeclarationBody::Fields(lowered);
    }
    let ast::Expr::List(items) = expression else {
        return authored::TypeDeclarationBody::Newtype(lower_type(expression));
    };
    if items.first().and_then(ast::Expr::as_symbol) != Some("adt") {
        return authored::TypeDeclarationBody::Newtype(lower_type(expression));
    }
    let mut constructors = Vec::with_capacity(items.len().saturating_sub(1));
    for constructor in &items[1..] {
        let ast::Expr::Vector(parts) = constructor else {
            return authored::TypeDeclarationBody::Newtype(authored::Type::Invalid(
                "ADT constructor must be [Name PayloadType]".into(),
            ));
        };
        let [name, payload] = parts.as_slice() else {
            return authored::TypeDeclarationBody::Newtype(authored::Type::Invalid(
                "ADT constructor must be [Name PayloadType]".into(),
            ));
        };
        let Some(name) = name.as_symbol() else {
            return authored::TypeDeclarationBody::Newtype(authored::Type::Invalid(
                "ADT constructor name must be a symbol".into(),
            ));
        };
        constructors.push(authored::ConstructorDeclaration {
            name: name.to_owned(),
            payload: lower_type(payload),
        });
    }
    authored::TypeDeclarationBody::Adt(constructors)
}

pub fn lower_block_item(expression: &ast::Expr) -> authored::BlockItem {
    if let ast::Expr::List(items) = expression {
        if matches!(items.first().and_then(ast::Expr::as_symbol), Some("let")) {
            return match items.as_slice() {
                [_, ast::Expr::Symbol(name), value] => authored::BlockItem::Let {
                    name: name.clone(),
                    value: lower_expr(value),
                },
                [_, _, _] => authored::BlockItem::Invalid(AuthoringError::Type(
                    "let name must be a symbol".into(),
                )),
                _ => authored::BlockItem::Invalid(AuthoringError::Type(
                    "let expects a name and value".into(),
                )),
            };
        }
    }
    authored::BlockItem::Expr(lower_expr(expression))
}

fn lower_expr(expression: &ast::Expr) -> authored::Expr {
    match expression {
        ast::Expr::Nil => authored::Expr::Unit,
        ast::Expr::Str(value) => authored::Expr::Str(value.clone()),
        ast::Expr::Int(value) => authored::Expr::Int(*value),
        ast::Expr::Float(value) => authored::Expr::Float(*value),
        ast::Expr::Bool(value) => authored::Expr::Bool(*value),
        ast::Expr::Keyword(value) => authored::Expr::Keyword(value.clone()),
        ast::Expr::Symbol(value) => authored::Expr::Name(value.clone().into()),
        ast::Expr::Vector(items) => authored::Expr::Vector(items.iter().map(lower_expr).collect()),
        ast::Expr::Map(fields) => {
            let mut lowered = Vec::with_capacity(fields.len());
            for (key, value) in fields {
                let Some(key) = record_key(key) else {
                    return authored::Expr::Invalid(AuthoringError::Type(
                        "record key must be a keyword, symbol, or string".into(),
                    ));
                };
                lowered.push((key, lower_expr(value)));
            }
            authored::Expr::Fields(lowered)
        }
        ast::Expr::List(items) => lower_list(items),
    }
}

fn lower_list(items: &[ast::Expr]) -> authored::Expr {
    if items.is_empty() {
        return authored::Expr::Unit;
    }
    match items.first().and_then(ast::Expr::as_symbol) {
        Some("let") => authored::Expr::Invalid(AuthoringError::Type(
            "let is only valid directly in a source or function block".into(),
        )),
        Some("if") => lower_if(items),
        Some("match") => lower_match(items),
        Some("construct") => lower_construct(items),
        Some("as") => lower_ascribe(items),
        Some("type-tag") => lower_type_tag(items),
        Some("fn") => lower_function(items),
        _ => authored::Expr::Call {
            callee: Box::new(lower_expr(&items[0])),
            type_args: None,
            args: items[1..].iter().map(lower_expr).collect(),
        },
    }
}

fn lower_if(items: &[ast::Expr]) -> authored::Expr {
    if !(3..=4).contains(&items.len()) {
        return authored::Expr::Invalid(AuthoringError::Type("if expects 2-3 arguments".into()));
    }
    authored::Expr::If {
        condition: Box::new(lower_expr(&items[1])),
        then_branch: Box::new(lower_expr(&items[2])),
        else_branch: Box::new(items.get(3).map(lower_expr).unwrap_or(authored::Expr::Unit)),
    }
}

fn lower_construct(items: &[ast::Expr]) -> authored::Expr {
    let [_, owner, constructor, payload] = items else {
        return authored::Expr::Invalid(AuthoringError::Type(
            "construct expects an ADT type, constructor, and payload".into(),
        ));
    };
    let Some(constructor) = constructor.as_symbol() else {
        return authored::Expr::Invalid(AuthoringError::Type(
            "construct constructor must be a symbol".into(),
        ));
    };
    authored::Expr::Construct {
        owner: lower_type(owner),
        constructor: constructor.to_owned(),
        arguments: authored::ConstructorArguments::Positional(vec![lower_expr(payload)]),
    }
}

fn lower_match(items: &[ast::Expr]) -> authored::Expr {
    if items.len() < 3 {
        return authored::Expr::Invalid(AuthoringError::Type(
            "match expects a value and at least one arm".into(),
        ));
    }
    let mut arms = Vec::with_capacity(items.len() - 2);
    for arm in &items[2..] {
        let ast::Expr::Vector(values) = arm else {
            return authored::Expr::Invalid(AuthoringError::Type(
                "match arm must be [Constructor binding expression]".into(),
            ));
        };
        let [constructor, binding, body] = values.as_slice() else {
            return authored::Expr::Invalid(AuthoringError::Type(
                "match arm must be [Constructor binding expression]".into(),
            ));
        };
        let Some(constructor) = constructor.as_symbol() else {
            return authored::Expr::Invalid(AuthoringError::Type(
                "match constructor must be a symbol".into(),
            ));
        };
        let Some(binding) = binding.as_symbol() else {
            return authored::Expr::Invalid(AuthoringError::Type(
                "match binding must be a symbol".into(),
            ));
        };
        arms.push(authored::MatchArm {
            pattern: authored::NamePath::single(constructor.to_owned()),
            binding: binding.to_owned(),
            body: Box::new(lower_expr(body)),
        });
    }
    authored::Expr::Match {
        value: Box::new(lower_expr(&items[1])),
        arms,
    }
}

fn lower_ascribe(items: &[ast::Expr]) -> authored::Expr {
    if items.len() != 3 {
        return authored::Expr::Invalid(AuthoringError::Type("as expects a type and value".into()));
    }
    authored::Expr::Ascribe {
        target: lower_type(&items[1]),
        value: Box::new(lower_expr(&items[2])),
    }
}

fn lower_type_tag(items: &[ast::Expr]) -> authored::Expr {
    if items.len() != 2 {
        return authored::Expr::Invalid(AuthoringError::Type("type-tag expects one type".into()));
    }
    authored::Expr::TypeTag(lower_type(&items[1]))
}

fn lower_function(items: &[ast::Expr]) -> authored::Expr {
    let args = &items[1..];
    if args.len() < 4 {
        return authored::Expr::Invalid(AuthoringError::Type("typed fn signature required".into()));
    }
    let (type_params, params, arrow, result, body) =
        if matches!(args.get(1), Some(ast::Expr::Vector(_))) {
            (&args[0], &args[1], &args[2], &args[3], &args[4..])
        } else {
            (
                &ast::Expr::Vector(vec![]),
                &args[0],
                &args[1],
                &args[2],
                &args[3..],
            )
        };
    if !matches!(arrow, ast::Expr::Symbol(value) if value == "->") {
        return authored::Expr::Invalid(AuthoringError::Type("fn signature requires ->".into()));
    }
    let ast::Expr::Vector(type_params) = type_params else {
        unreachable!()
    };
    let type_params = match type_params
        .iter()
        .map(|parameter| {
            parameter
                .as_symbol()
                .map(str::to_owned)
                .ok_or_else(|| AuthoringError::Type("generic binder must be a symbol".into()))
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(params) => params,
        Err(error) => return authored::Expr::Invalid(error),
    };
    let mut seen = std::collections::HashSet::new();
    if let Some(duplicate) = type_params
        .iter()
        .find(|parameter| !seen.insert(*parameter))
    {
        return authored::Expr::Invalid(AuthoringError::Type(format!(
            "duplicate generic parameter {duplicate}"
        )));
    }
    let ast::Expr::Vector(params) = params else {
        return authored::Expr::Invalid(AuthoringError::Type(
            "fn parameters must be a vector".into(),
        ));
    };
    let mut lowered_params = Vec::with_capacity(params.len());
    for parameter in params {
        let ast::Expr::Vector(pair) = parameter else {
            return authored::Expr::Invalid(AuthoringError::Type(
                "parameter must be [name Type]".into(),
            ));
        };
        let [ast::Expr::Symbol(name), ty] = pair.as_slice() else {
            let message = if pair.len() == 2 {
                "parameter name must be a symbol"
            } else {
                "parameter must be [name Type]"
            };
            return authored::Expr::Invalid(AuthoringError::Type(message.into()));
        };
        lowered_params.push(authored::Parameter {
            name: name.clone(),
            ty: lower_type(ty),
        });
    }
    authored::Expr::Function(authored::Function {
        type_params,
        params: lowered_params,
        result: lower_type(result),
        body: body.iter().map(lower_block_item).collect(),
    })
}

fn lower_type(expression: &ast::Expr) -> authored::Type {
    match expression {
        ast::Expr::Symbol(name) => authored::Type::Name(name.clone().into()),
        ast::Expr::Map(_) => authored::Type::Invalid(
            "anonymous record types are unsupported; declare a named type".into(),
        ),
        ast::Expr::List(items) if !items.is_empty() => {
            let Some(head) = items[0].as_symbol() else {
                return authored::Type::Invalid("type application head must be a symbol".into());
            };
            match head {
                "|" => authored::Type::Invalid(
                    "authored structural unions are unsupported; declare an ADT".into(),
                ),
                "+" => authored::Type::Product(items[1..].iter().map(lower_type).collect()),
                "TypeTag" if items.len() == 2 => {
                    authored::Type::Tag(Box::new(lower_type(&items[1])))
                }
                "Fn" if items.len() >= 2 => {
                    let mut types = items[1..].iter().map(lower_type).collect::<Vec<_>>();
                    let result = types.pop().expect("nonempty Fn type arguments");
                    authored::Type::Function {
                        params: types,
                        result: Box::new(result),
                    }
                }
                _ => authored::Type::Apply {
                    constructor: head.to_owned().into(),
                    arguments: items[1..].iter().map(lower_type).collect(),
                },
            }
        }
        _ => authored::Type::Invalid("invalid type expression".into()),
    }
}

fn record_key(expression: &ast::Expr) -> Option<String> {
    match expression {
        ast::Expr::Keyword(key) | ast::Expr::Symbol(key) | ast::Expr::Str(key) => Some(key.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lisp_names_use_the_same_segment_preserving_path_representation() {
        let lowered = lower_expr(&ast::Expr::Symbol("call.with.dots".into()));
        let authored::Expr::Name(path) = lowered else {
            panic!("name")
        };
        assert_eq!(path.segments(), ["call.with.dots"]);
    }

    #[test]
    fn lowers_lisp_conventions_to_semantic_authored_forms() {
        let source = SourceSnapshot::named(
            "test.mag",
            r#"
                (require "support")
                (type Choice [T] (adt [Selected T] [Empty Unit]))
                (let choose (fn [T] [[value T]] -> (Choice T)
                  (construct (Choice T) Selected value)))
                (artifact {:answer (type-tag (Map String (Fn Int String)))})
            "#,
        );
        let module = compile_source(&source, None, crate::frontend::SourceRole::Entry).unwrap();

        assert!(matches!(module.forms[0], authored::Form::Require(_)));
        let authored::Form::Type(declaration) = &module.forms[1] else {
            panic!("type declaration")
        };
        assert_eq!(declaration.params, ["T"]);
        assert!(matches!(
            declaration.body,
            authored::TypeDeclarationBody::Adt(_)
        ));

        let authored::Form::Block(authored::BlockItem::Let { value, .. }) = &module.forms[2] else {
            panic!("let declaration")
        };
        let authored::Expr::Function(function) = value else {
            panic!("semantic function")
        };
        assert!(matches!(
            function.body[0],
            authored::BlockItem::Expr(authored::Expr::Construct { .. })
        ));

        let authored::Form::Block(authored::BlockItem::Expr(authored::Expr::Call { args, .. })) =
            &module.forms[3]
        else {
            panic!("artifact call")
        };
        let authored::Expr::Fields(fields) = &args[0] else {
            panic!("record")
        };
        assert!(matches!(fields[0].1, authored::Expr::TypeTag(_)));
    }
}
