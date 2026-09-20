use crate::authored;
use crate::diagnostic::{ByteSpan, SourceSnapshot, SyntaxDiagnostic};
use crate::error::MagError;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Ident(String),
    Operator(String),
    String(String),
    Int(i64),
    Float(f64),
    Newline,
    Punct(char),
    Arrow,
    FatArrow,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    span: ByteSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Associativity {
    Left,
    Right,
    None,
}

#[derive(Debug, Clone, Copy)]
struct Fixity {
    precedence: u8,
    associativity: Associativity,
    span: ByteSpan,
}

pub(crate) fn compile_source(
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
    let tokens = Lexer::new(source).tokenize()?;
    drop(phase);
    let _phase = profiler.map(|profiler| profiler.start_phase(parse));
    Parser::new(source, tokens)?.parse_module()
}

struct Lexer<'a> {
    source: &'a SourceSnapshot,
    text: &'a str,
    cursor: usize,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a SourceSnapshot) -> Self {
        Self {
            source,
            text: &source.text,
            cursor: 0,
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>, MagError> {
        let mut tokens = Vec::new();
        while self.cursor < self.text.len() {
            let start = self.cursor;
            let byte = self.text.as_bytes()[self.cursor];
            match byte {
                b' ' | b'\t' | b'\r' => self.cursor += 1,
                b'\n' => {
                    self.cursor += 1;
                    tokens.push(self.token(TokenKind::Newline, start));
                }
                b'/' if self.peek_byte(1) == Some(b'/') => {
                    self.cursor += 2;
                    while self.cursor < self.text.len()
                        && self.text.as_bytes()[self.cursor] != b'\n'
                    {
                        self.cursor += 1;
                    }
                }
                b'"' => tokens.push(self.lex_string()?),
                b'`' => tokens.push(self.lex_backtick()?),
                b'0'..=b'9' => tokens.push(self.lex_number(false)?),
                b'-' if self.peek_byte(1).is_some_and(|next| next.is_ascii_digit())
                    && !tokens
                        .last()
                        .is_some_and(|token| token_can_end_expression(&token.kind)) =>
                {
                    tokens.push(self.lex_number(true)?)
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => tokens.push(self.lex_ident()),
                b'-' if self.peek_byte(1) == Some(b'>') => {
                    self.cursor += 2;
                    tokens.push(self.token(TokenKind::Arrow, start));
                }
                b'=' if self.peek_byte(1) == Some(b'>') => {
                    self.cursor += 2;
                    tokens.push(self.token(TokenKind::FatArrow, start));
                }
                b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b':' | b'.' => {
                    self.cursor += 1;
                    tokens.push(self.token(TokenKind::Punct(byte as char), start));
                }
                _ if is_operator_byte(byte) => tokens.push(self.lex_operator()),
                _ => {
                    let end = self.cursor
                        + self.text[self.cursor..]
                            .chars()
                            .next()
                            .map_or(1, char::len_utf8);
                    return Err(self.error(
                        "MAG1001",
                        "lexer",
                        "unexpected character",
                        ByteSpan::new(start, end),
                        None,
                    ));
                }
            }
        }
        tokens.push(Token {
            kind: TokenKind::Eof,
            span: ByteSpan::new(self.cursor, self.cursor),
        });
        Ok(tokens)
    }

    fn lex_string(&mut self) -> Result<Token, MagError> {
        let start = self.cursor;
        if self.text[self.cursor..].starts_with("\"\"\"") {
            self.cursor += 3;
            let body_start = self.cursor;
            if let Some(offset) = self.text[self.cursor..].find("\"\"\"") {
                let value = self.text[body_start..self.cursor + offset].to_owned();
                self.cursor += offset + 3;
                return Ok(self.token(TokenKind::String(value), start));
            }
            return Err(self.error(
                "MAG1002",
                "lexer",
                "unterminated raw string",
                ByteSpan::new(start, self.text.len()),
                None,
            ));
        }
        self.cursor += 1;
        let mut value = String::new();
        while self.cursor < self.text.len() {
            let ch_start = self.cursor;
            let ch = self.next_char();
            match ch {
                '"' => return Ok(self.token(TokenKind::String(value), start)),
                '\n' => {
                    return Err(self.error(
                        "MAG1003",
                        "lexer",
                        "newline in quoted string",
                        ByteSpan::new(ch_start, self.cursor),
                        None,
                    ))
                }
                '\\' => {
                    let escape_start = ch_start;
                    if self.cursor >= self.text.len() {
                        break;
                    }
                    let escaped = self.next_char();
                    match escaped {
                        'n' => value.push('\n'),
                        't' => value.push('\t'),
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        _ => {
                            return Err(self.error(
                                "MAG1004",
                                "lexer",
                                format!("unsupported string escape \\{escaped}"),
                                ByteSpan::new(escape_start, self.cursor),
                                None,
                            ))
                        }
                    }
                }
                other => value.push(other),
            }
        }
        Err(self.error(
            "MAG1002",
            "lexer",
            "unterminated string",
            ByteSpan::new(start, self.text.len()),
            None,
        ))
    }

    fn lex_backtick(&mut self) -> Result<Token, MagError> {
        let start = self.cursor;
        self.cursor += 1;
        let body = self.cursor;
        while self.cursor < self.text.len() && self.text.as_bytes()[self.cursor] != b'`' {
            if self.text.as_bytes()[self.cursor] == b'\n' {
                return Err(self.error(
                    "MAG1005",
                    "lexer",
                    "newline in backtick identifier",
                    ByteSpan::new(start, self.cursor),
                    None,
                ));
            }
            self.cursor += self.text[self.cursor..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
        }
        if self.cursor == self.text.len() {
            return Err(self.error(
                "MAG1005",
                "lexer",
                "unterminated backtick identifier",
                ByteSpan::new(start, self.cursor),
                None,
            ));
        }
        if self.cursor == body {
            return Err(self.error(
                "MAG1005",
                "lexer",
                "backtick identifier cannot be empty",
                ByteSpan::new(start, self.cursor + 1),
                None,
            ));
        }
        let value = self.text[body..self.cursor].to_owned();
        self.cursor += 1;
        Ok(self.token(TokenKind::Ident(value), start))
    }

    fn lex_number(&mut self, signed: bool) -> Result<Token, MagError> {
        let start = self.cursor;
        if signed {
            self.cursor += 1;
        }
        while self.cursor < self.text.len() && self.text.as_bytes()[self.cursor].is_ascii_digit() {
            self.cursor += 1;
        }
        let mut float = false;
        if self.text.as_bytes().get(self.cursor) == Some(&b'.')
            && self
                .text
                .as_bytes()
                .get(self.cursor + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            float = true;
            self.cursor += 1;
            while self.cursor < self.text.len()
                && self.text.as_bytes()[self.cursor].is_ascii_digit()
            {
                self.cursor += 1;
            }
        }
        let text = &self.text[start..self.cursor];
        let kind = if float {
            TokenKind::Float(text.parse().map_err(|_| {
                self.error(
                    "MAG1006",
                    "lexer",
                    "invalid float",
                    ByteSpan::new(start, self.cursor),
                    None,
                )
            })?)
        } else {
            TokenKind::Int(text.parse().map_err(|_| {
                self.error(
                    "MAG1006",
                    "lexer",
                    "integer is outside the Int range",
                    ByteSpan::new(start, self.cursor),
                    None,
                )
            })?)
        };
        Ok(self.token(kind, start))
    }

    fn lex_ident(&mut self) -> Token {
        let start = self.cursor;
        self.cursor += 1;
        while self.cursor < self.text.len() {
            let byte = self.text.as_bytes()[self.cursor];
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'\'') {
                self.cursor += 1;
            } else {
                break;
            }
        }
        self.token(
            TokenKind::Ident(self.text[start..self.cursor].to_owned()),
            start,
        )
    }

    fn lex_operator(&mut self) -> Token {
        let start = self.cursor;
        while self.cursor < self.text.len() && is_operator_byte(self.text.as_bytes()[self.cursor]) {
            if self.text[self.cursor..].starts_with("//")
                || self.text[self.cursor..].starts_with("->")
                || self.text[self.cursor..].starts_with("=>")
            {
                break;
            }
            self.cursor += 1;
        }
        self.token(
            TokenKind::Operator(self.text[start..self.cursor].to_owned()),
            start,
        )
    }

    fn next_char(&mut self) -> char {
        let ch = self.text[self.cursor..].chars().next().unwrap_or('\0');
        self.cursor += ch.len_utf8();
        ch
    }
    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.text.as_bytes().get(self.cursor + offset).copied()
    }
    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            span: ByteSpan::new(start, self.cursor),
        }
    }
    fn error(
        &self,
        code: &'static str,
        stage: &'static str,
        message: impl Into<String>,
        span: ByteSpan,
        related: Option<(String, ByteSpan)>,
    ) -> MagError {
        MagError::Syntax(Box::new(SyntaxDiagnostic::new(
            code,
            stage,
            message.into(),
            self.source,
            span,
            related,
        )))
    }
}

fn token_can_end_expression(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident(_)
            | TokenKind::String(_)
            | TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Punct(')' | ']' | '}')
    )
}

fn is_infix_separator_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_operator_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'*'
            | b'+'
            | b'-'
            | b'/'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'@'
            | b'\\'
            | b'^'
            | b'|'
            | b'~'
    )
}

#[derive(Debug, Clone)]
struct VariantMetadata {
    owner: String,
    owner_params: Vec<String>,
    payload: authored::Type,
    field_order: Vec<String>,
}

#[derive(Clone)]
struct Parser<'a> {
    source: &'a SourceSnapshot,
    tokens: Vec<Token>,
    cursor: usize,
    fixities: HashMap<String, Fixity>,
    aliases: HashMap<String, authored::NamePath>,
    variants: HashMap<String, VariantMetadata>,
    requires: Vec<authored::Require>,
    namespace_aliases: HashSet<String>,
    bound_names: HashSet<String>,
    local_types: HashSet<String>,
    allow_brace_construct: bool,
    specialized_constructors: HashMap<String, (authored::Type, String)>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a SourceSnapshot, tokens: Vec<Token>) -> Result<Self, MagError> {
        let fixities = scan_fixities(source, &tokens)?;
        let bound_names = scan_top_level_lets(&tokens);
        Ok(Self {
            source,
            tokens,
            cursor: 0,
            fixities,
            aliases: HashMap::new(),
            variants: HashMap::new(),
            requires: Vec::new(),
            namespace_aliases: HashSet::new(),
            bound_names,
            local_types: HashSet::new(),
            allow_brace_construct: true,
            specialized_constructors: HashMap::new(),
        })
    }

    fn parse_module(mut self) -> Result<authored::Module, MagError> {
        let mut forms = Vec::new();
        self.separators();
        while !self.at_eof() {
            if self.at_word("import") {
                self.parse_import()?;
            } else if self.at_word("fixity")
                || self.at_word("infixl")
                || self.at_word("infixr")
                || self.at_word("infix")
            {
                self.skip_fixity()?;
            } else if self.at_word("type") || self.at_word("newtype") {
                forms.extend(self.parse_type_declaration()?);
            } else if self.at_word("let") {
                forms.push(authored::Form::Block(self.parse_let()?));
            } else {
                forms.push(authored::Form::Block(authored::BlockItem::Expr(
                    self.parse_expr(None)?,
                )));
            }
            self.require_separator()?;
        }
        let mut result = self
            .requires
            .into_iter()
            .map(authored::Form::Require)
            .collect::<Vec<_>>();
        result.extend(forms);
        Ok(authored::Module { forms: result })
    }

    fn parse_import(&mut self) -> Result<(), MagError> {
        self.expect_word("import")?;
        let mut path_segments = vec![self.expect_name()?];
        while self.at_punct('.') && !matches!(self.peek_n(1).kind, TokenKind::Punct('{')) {
            self.bump();
            if self.at_operator("*") {
                return Err(self.here("wildcard imports are unsupported"));
            }
            path_segments.push(self.expect_name()?);
        }
        let path = authored::NamePath::from_segments(path_segments);

        let exposure = if self.eat_word("as") {
            let alias = self.expect_binding_name()?;
            if alias == "_" {
                return Err(self.here("'_' is only valid after 'as' in an import selector"));
            }
            self.namespace_aliases.insert(alias.clone());
            self.aliases
                .insert(alias.clone(), authored::NamePath::single(alias.clone()));
            authored::ImportExposure::NamespaceAlias(alias)
        } else if self.eat_punct('.') {
            self.expect_punct('{')?;
            let mut selectors = Vec::new();
            if !self.eat_punct('}') {
                loop {
                    let export = self.expect_name()?;
                    let local = if self.eat_word("as") {
                        let name = self.expect_binding_name()?;
                        (name != "_").then_some(name)
                    } else {
                        Some(export.clone())
                    };
                    if let Some(local_name) = &local {
                        let mut segments = path.segments().to_vec();
                        segments.push(export.clone());
                        let canonical = authored::NamePath::from_segments(segments);
                        if let Some(fixity) = self.fixities.get(local_name).copied() {
                            self.fixities.insert(canonical.to_string(), fixity);
                        }
                        self.aliases.insert(local_name.clone(), canonical);
                    }
                    selectors.push(authored::ImportSelector { export, local });
                    if self.eat_punct('}') {
                        break;
                    }
                    self.expect_punct(',')?;
                    if self.eat_punct('}') {
                        break;
                    }
                }
            }
            if selectors.is_empty() {
                authored::ImportExposure::Qualified
            } else {
                authored::ImportExposure::Selective(selectors)
            }
        } else {
            authored::ImportExposure::Open
        };
        self.requires.push(authored::Require {
            module: path.to_string(),
            exposure,
        });
        Ok(())
    }

    fn skip_fixity(&mut self) -> Result<(), MagError> {
        while !self.at_separator() && !self.at_eof() {
            self.bump();
        }
        Ok(())
    }

    fn parse_type_declaration(&mut self) -> Result<Vec<authored::Form>, MagError> {
        let is_newtype = self.eat_word("newtype");
        if !is_newtype {
            self.expect_word("type")?;
        }
        let name = self.expect_name()?;
        self.local_types.insert(name.clone());
        let params = self.parse_generic_names()?;
        if is_newtype && self.at_punct('{') {
            return Err(self.here("newtype requires '= TypeExpr'"));
        }
        if self.eat_punct('{') {
            let body = authored::TypeDeclarationBody::Fields(self.parse_type_fields('}')?);
            return Ok(vec![authored::Form::Type(authored::TypeDeclaration {
                name,
                params,
                body,
            })]);
        }
        self.expect_operator("=")?;
        self.newlines();
        if is_newtype {
            if self.at_punct('{') || self.peek_variant_declaration() {
                return Err(self.here("newtype requires '= TypeExpr' and declares no constructors"));
            }
            let body = authored::TypeDeclarationBody::Newtype(self.parse_type()?);
            return Ok(vec![authored::Form::Type(authored::TypeDeclaration {
                name,
                params,
                body,
            })]);
        }
        if !self.peek_variant_declaration() {
            let body = if self.eat_punct('{') {
                authored::TypeDeclarationBody::Fields(self.parse_type_fields('}')?)
            } else {
                authored::TypeDeclarationBody::TransparentAlias(self.parse_type()?)
            };
            return Ok(vec![authored::Form::Type(authored::TypeDeclaration {
                name,
                params,
                body,
            })]);
        }

        let mut variants = Vec::new();
        let mut helper_forms = Vec::new();
        loop {
            self.eat_operator("|");
            self.newlines();
            let constructor = self.expect_name()?;
            let mut field_order = Vec::new();
            let payload = if self.eat_punct('(') {
                let items = self.parse_type_list(')')?;
                match items.as_slice() {
                    [] => authored::Type::Name("Unit".into()),
                    [item] => item.clone(),
                    _ => authored::Type::Product(items),
                }
            } else if self.eat_punct('{') {
                let fields = self.parse_type_fields('}')?;
                field_order = fields.iter().map(|(field, _)| field.clone()).collect();
                let helper = format!("{name}.{constructor}");
                helper_forms.push(authored::Form::Type(authored::TypeDeclaration {
                    name: helper.clone(),
                    params: params.clone(),
                    body: authored::TypeDeclarationBody::Fields(fields),
                }));
                let arguments = params
                    .iter()
                    .cloned()
                    .map(|name| authored::Type::Name(name.into()))
                    .collect::<Vec<_>>();
                if arguments.is_empty() {
                    authored::Type::Name(helper.into())
                } else {
                    authored::Type::Apply {
                        constructor: helper.into(),
                        arguments,
                    }
                }
            } else {
                return Err(self.here("constructor requires a positional or named payload"));
            };
            self.variants.insert(
                format!("{name}.{constructor}"),
                VariantMetadata {
                    owner: name.clone(),
                    owner_params: params.clone(),
                    payload: payload.clone(),
                    field_order,
                },
            );
            variants.push(authored::ConstructorDeclaration {
                name: constructor,
                payload,
            });
            let separator = self.cursor;
            self.newlines();
            if !self.at_operator("|") {
                self.cursor = separator;
                break;
            }
        }
        helper_forms.push(authored::Form::Type(authored::TypeDeclaration {
            name,
            params,
            body: authored::TypeDeclarationBody::Adt(variants),
        }));
        Ok(helper_forms)
    }

    fn parse_type_fields(&mut self, end: char) -> Result<Vec<(String, authored::Type)>, MagError> {
        let mut fields = Vec::new();
        self.separators();
        while !self.eat_punct(end) {
            let name = self.expect_name()?;
            self.expect_punct(':')?;
            fields.push((name, self.parse_type()?));
            if self.eat_punct(end) {
                break;
            }
            if !self.eat_punct(',') && !self.at_newline() {
                return Err(self.here("expected ',' or newline between fields"));
            }
            self.separators();
        }
        Ok(fields)
    }

    fn parse_let(&mut self) -> Result<authored::BlockItem, MagError> {
        self.expect_word("let")?;
        let name = self.expect_binding_name()?;
        self.bound_names.insert(name.clone());
        let type_params = self.parse_generic_names()?;
        let expected = if self.eat_punct(':') {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect_operator("=")?;
        let mut value = self.parse_expr(expected.as_ref())?;
        if let Some(target) = expected {
            let generic_function = if let authored::Expr::Function(function) = &mut value {
                function.type_params = type_params;
                !function.type_params.is_empty()
            } else {
                false
            };
            if !generic_function {
                value = authored::Expr::Annotate {
                    target,
                    value: Box::new(value),
                };
            }
        } else if !type_params.is_empty() {
            return Err(self.here("generic let parameters require a complete type annotation"));
        }
        Ok(authored::BlockItem::Let { name, value })
    }

    fn parse_expr(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let first = self.parse_ascription(expected)?;
        let mut operands = vec![first];
        let mut operators = Vec::new();
        while let Some((operator, span)) = self.take_infix_operator()? {
            self.newlines();
            operators.push((operator, span));
            operands.push(self.parse_ascription(None)?);
        }
        self.reassociate(operands, operators)
    }

    fn parse_ascription(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let mut value = self.parse_postfix(expected)?;
        if self.eat_punct(':') {
            let target = self.parse_type()?;
            if matches!(value, authored::Expr::Invalid(_)) {
                return Ok(value);
            }
            if let authored::Expr::Function(_) = value {
                // Reparse is unnecessary: lambdas without annotations are diagnosed while parsed,
                // so inline ascriptions pass their type through a small lookahead in parse_postfix.
            }
            value = authored::Expr::Ascribe {
                target,
                value: Box::new(value),
            };
        }
        Ok(value)
    }

    fn parse_postfix(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let inline_expected = if self.at_punct('(') {
            self.lookahead_ascribed_function_type()
        } else {
            None
        };
        let mut value = self.parse_primary(expected.or(inline_expected.as_ref()))?;
        loop {
            if self.eat_operator("<") {
                let authored::Expr::Name(owner) = value else {
                    return Err(self.here("generic application requires a callable name"));
                };
                let arguments = self.parse_call_type_arguments()?;
                if self.eat_punct('(') {
                    let args = self.parse_arguments()?;
                    if args
                        .iter()
                        .any(|argument| matches!(argument, CallArgument::Named(_, _)))
                    {
                        return Err(self.here(
                            "named arguments are only valid for named constructor payloads",
                        ));
                    }
                    value = authored::Expr::Call {
                        callee: Box::new(authored::Expr::Name(owner)),
                        type_args: Some(arguments),
                        args: args
                            .into_iter()
                            .map(|argument| match argument {
                                CallArgument::Positional(value) => value,
                                CallArgument::Named(_, _) => unreachable!(),
                            })
                            .collect(),
                    };
                } else {
                    let concrete = arguments
                        .into_iter()
                        .map(|argument| match argument {
                            authored::TypeArgument::Explicit(ty) => Ok(ty),
                            authored::TypeArgument::Infer => Err(self.here(
                                "'_' inference holes are only valid in explicit function calls",
                            )),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let owner_type = authored::Type::Apply {
                        constructor: owner.clone(),
                        arguments: concrete,
                    };
                    if self.eat_punct('.') {
                        let constructor = self.expect_name()?;
                        let marker = format!("#constructor{}", self.specialized_constructors.len());
                        self.specialized_constructors
                            .insert(marker.clone(), (owner_type, constructor));
                        value = authored::Expr::Name(marker.into());
                    } else if self.allow_brace_construct && self.eat_punct('{') {
                        value = authored::Expr::Ascribe {
                            target: owner_type,
                            value: Box::new(authored::Expr::Fields(self.parse_value_fields('}')?)),
                        };
                    } else {
                        return Err(self.here(
                            "explicit type arguments must be followed by a call or constructor",
                        ));
                    }
                }
            } else if self.eat_punct('(') {
                let args = self.parse_arguments()?;
                value = self.lower_call(value, args, expected)?;
            } else if self.eat_punct('.') {
                let field = self.expect_name()?;
                value = authored::Expr::Call {
                    callee: Box::new(authored::Expr::Name("get".into())),
                    type_args: None,
                    args: vec![value, authored::Expr::Str(field)],
                };
            } else if self.allow_brace_construct && self.at_punct('{') {
                let owner = match &value {
                    authored::Expr::Name(name) => name.clone(),
                    _ => return Err(self.here("only a named type can construct braced fields")),
                };
                self.bump();
                let fields = self.parse_value_fields('}')?;
                value = if owner == "artifact" {
                    authored::Expr::Call {
                        callee: Box::new(authored::Expr::Name(owner)),
                        type_args: None,
                        args: vec![authored::Expr::Fields(fields)],
                    }
                } else if let Some(owner_prefix) = owner.owner() {
                    let constructor = owner.last();
                    if let Some(metadata) = self.variants.get(owner.as_str()) {
                        if metadata.owner != owner_prefix.as_str()
                            || metadata.field_order.is_empty()
                        {
                            return Err(
                                self.here("named constructor owner or payload does not match")
                            );
                        }
                        let (sum_type, _) =
                            constructor_types(&metadata.owner, constructor, expected);
                        let payload_type = specialize_variant_payload(metadata, &sum_type);
                        let payload = authored::Expr::Ascribe {
                            target: payload_type,
                            value: Box::new(authored::Expr::Fields(fields)),
                        };
                        authored::Expr::Construct {
                            owner: sum_type,
                            constructor: constructor.to_owned(),
                            payload: Box::new(payload),
                        }
                    } else if expected_owner_name(expected) == Some(owner_prefix.as_str()) {
                        authored::Expr::Construct {
                            owner: expected
                                .cloned()
                                .unwrap_or(authored::Type::Name(owner_prefix)),
                            constructor: constructor.to_owned(),
                            payload: Box::new(authored::Expr::Fields(fields)),
                        }
                    } else {
                        authored::Expr::Ascribe {
                            target: authored::Type::Name(owner),
                            value: Box::new(authored::Expr::Fields(fields)),
                        }
                    }
                } else {
                    authored::Expr::Ascribe {
                        target: authored::Type::Name(owner),
                        value: Box::new(authored::Expr::Fields(fields)),
                    }
                };
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_primary(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        self.newlines();
        let token = self.bump().clone();
        match token.kind {
            TokenKind::String(value) => Ok(authored::Expr::Str(value)),
            TokenKind::Int(value) => Ok(authored::Expr::Int(value)),
            TokenKind::Float(value) => Ok(authored::Expr::Float(value)),
            TokenKind::Ident(word) if word == "nil" => Ok(authored::Expr::Unit),
            TokenKind::Ident(word) if word == "true" => Ok(authored::Expr::Bool(true)),
            TokenKind::Ident(word) if word == "false" => Ok(authored::Expr::Bool(false)),
            TokenKind::Ident(word) if word == "if" => self.parse_if(expected),
            TokenKind::Ident(word) if word == "match" => self.parse_match(expected),
            TokenKind::Ident(word) if word == "type_tag" => self.parse_type_tag(),
            TokenKind::Ident(word) if word == "named" => self.parse_named(),
            TokenKind::Ident(word) => self.parse_name_expression(word, expected),
            TokenKind::Operator(word) if word == "|" => self.parse_lambda(expected, token.span),
            TokenKind::Operator(word) => Ok(authored::Expr::Name(self.resolve_name(&word).into())),
            TokenKind::Punct('[') => self.parse_list(),
            TokenKind::Punct('{') => self.parse_block_after_open(expected),
            TokenKind::Punct('(') => self.parse_parens(expected),
            _ => Err(self.parse_error("expected expression", token.span)),
        }
    }

    fn parse_name_expression(
        &mut self,
        word: String,
        _expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let mut segments = if self.bound_names.contains(&word) {
            vec![word.clone()]
        } else {
            self.aliases
                .get(&word)
                .map(|path| path.segments().to_vec())
                .unwrap_or_else(|| vec![word.clone()])
        };
        let imported_alias = self.aliases.contains_key(&word);
        // Bound values and selectively imported values own dot access. An import
        // alias is consumed as a qualified owner only when the next segment is
        // immediately constructed or called.
        let alias_qualifies = self.namespace_aliases.contains(&word)
            || (imported_alias
                && self.at_punct('.')
                && matches!(
                    self.peek_n(1).kind,
                    TokenKind::Ident(_) | TokenKind::Operator(_)
                )
                && (matches!(self.peek_n(2).kind, TokenKind::Punct('(' | '{'))
                    || matches!(
                        self.peek_n(2).kind,
                        TokenKind::Operator(ref operator) if operator == "<"
                    )));
        if !self.bound_names.contains(&word)
            && self.at_punct('.')
            && (!imported_alias || alias_qualifies)
        {
            while self.eat_punct('.') {
                segments.push(self.expect_name()?);
            }
        }
        Ok(authored::Expr::Name(authored::NamePath::from_segments(
            segments,
        )))
    }

    fn parse_lambda(
        &mut self,
        expected: Option<&authored::Type>,
        start: ByteSpan,
    ) -> Result<authored::Expr, MagError> {
        let authored::Type::Function { params: expected_params, result } = expected.ok_or_else(|| self.parse_error("lambda requires a complete expected function type from a let annotation or ascription", start))? else {
            return Err(self.parse_error("lambda expected type must be a function type", start));
        };
        let mut names = Vec::new();
        if !self.eat_operator("|") {
            loop {
                names.push(self.expect_name()?);
                if self.eat_operator("|") {
                    break;
                }
                self.expect_punct(',')?;
            }
        }
        if names.len() != expected_params.len() {
            return Err(self.parse_error(
                format!(
                    "lambda has {} parameters but its expected type has {}",
                    names.len(),
                    expected_params.len()
                ),
                start,
            ));
        }
        self.expect_fat_arrow()?;
        let outer_names = self.bound_names.clone();
        self.bound_names.extend(names.iter().cloned());
        let body_result = if self.eat_punct('{') {
            self.parse_function_block()
        } else {
            self.parse_expr(Some(result))
                .map(|value| vec![authored::BlockItem::Expr(value)])
        };
        self.bound_names = outer_names;
        let body = body_result?;
        Ok(authored::Expr::Function(authored::Function {
            type_params: vec![],
            params: names
                .into_iter()
                .zip(expected_params.iter().cloned())
                .map(|(name, ty)| authored::Parameter { name, ty })
                .collect(),
            result: (**result).clone(),
            body,
        }))
    }

    fn parse_function_block(&mut self) -> Result<Vec<authored::BlockItem>, MagError> {
        let outer_names = self.bound_names.clone();
        self.bound_names
            .extend(scan_direct_block_lets(&self.tokens, self.cursor));
        let mut items = Vec::new();
        self.separators();
        while !self.eat_punct('}') {
            items.push(if self.at_word("let") {
                self.parse_let()?
            } else {
                authored::BlockItem::Expr(self.parse_expr(None)?)
            });
            if !self.at_punct('}') {
                self.require_separator()?;
            }
        }
        if items.is_empty() {
            items.push(authored::BlockItem::Expr(authored::Expr::Unit));
        }
        self.bound_names = outer_names;
        Ok(items)
    }

    fn parse_if(&mut self, expected: Option<&authored::Type>) -> Result<authored::Expr, MagError> {
        let condition = self.parse_expr(None)?;
        self.expect_word("then")?;
        self.newlines();
        let then_branch = self.parse_expr(expected)?;
        self.expect_word("else")?;
        self.newlines();
        let else_branch = self.parse_expr(expected)?;
        Ok(authored::Expr::If {
            condition: Box::new(condition),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }

    fn parse_expr_before_block(&mut self) -> Result<authored::Expr, MagError> {
        let previous = self.allow_brace_construct;
        self.allow_brace_construct = false;
        let result = self.parse_expr(None);
        self.allow_brace_construct = previous;
        result
    }

    fn parse_block_after_open(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let outer_names = self.bound_names.clone();
        self.bound_names
            .extend(scan_direct_block_lets(&self.tokens, self.cursor));
        let mut body = Vec::new();
        self.separators();
        while self.at_word("let") {
            body.push(self.parse_let()?);
            self.require_separator()?;
        }
        if self.at_punct('}') {
            return Err(self.here("empty block is invalid; write { () }"));
        }
        let result = self.parse_expr(expected)?;
        let result_type = expected
            .cloned()
            .or_else(|| obvious_expr_type(&result))
            .ok_or_else(|| self.here("block result type requires an annotation"))?;
        body.push(authored::BlockItem::Expr(result));
        self.separators();
        self.expect_punct('}')?;
        self.bound_names = outer_names;
        Ok(authored::Expr::Call {
            callee: Box::new(authored::Expr::Function(authored::Function {
                type_params: Vec::new(),
                params: Vec::new(),
                result: result_type,
                body,
            })),
            type_args: None,
            args: Vec::new(),
        })
    }

    fn parse_match(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        let value = self.parse_expr_before_block()?;
        self.expect_punct('{')?;
        self.separators();
        let mut arms = Vec::new();
        while !self.eat_punct('}') {
            self.expect_word("case")?;
            let pattern = self.expect_name_path()?;
            self.expect_punct('(')?;
            let binding = self.expect_name()?;
            self.expect_punct(')')?;
            self.expect_fat_arrow()?;
            let body = self.parse_expr(expected)?;
            arms.push(authored::MatchArm {
                pattern,
                binding,
                body: Box::new(body),
            });
            if self.eat_punct(',') {
                self.separators();
            } else if !self.at_punct('}') {
                self.require_separator()?;
            }
        }
        if arms.is_empty() {
            return Err(self.here("match requires at least one arm"));
        }
        Ok(authored::Expr::Match {
            value: Box::new(value),
            arms,
        })
    }

    fn parse_type_tag(&mut self) -> Result<authored::Expr, MagError> {
        self.expect_operator("<")?;
        let ty = self.parse_type()?;
        self.expect_operator(">")?;
        self.expect_punct('(')?;
        self.expect_punct(')')?;
        Ok(authored::Expr::TypeTag(ty))
    }

    fn parse_named(&mut self) -> Result<authored::Expr, MagError> {
        self.expect_punct('(')?;
        let owner = self.parse_type()?;
        self.expect_punct(',')?;
        let constructor = self.expect_name()?;
        self.expect_punct(',')?;
        let payload = self.parse_expr(None)?;
        self.expect_punct(')')?;
        Ok(authored::Expr::Construct {
            owner,
            constructor,
            payload: Box::new(payload),
        })
    }

    fn parse_list(&mut self) -> Result<authored::Expr, MagError> {
        let mut items = Vec::new();
        self.separators();
        while !self.eat_punct(']') {
            items.push(self.parse_expr(None)?);
            if self.eat_punct(']') {
                break;
            }
            self.expect_punct(',')?;
            self.separators();
        }
        Ok(authored::Expr::Vector(items))
    }

    fn parse_parens(
        &mut self,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        self.newlines();
        if self.eat_punct(')') {
            return Ok(authored::Expr::Unit);
        }
        if self.peek_n(1).kind == TokenKind::Punct(')') {
            let name = match self.peek().kind.clone() {
                TokenKind::Operator(name) => Some(name),
                TokenKind::Arrow => Some("->".into()),
                TokenKind::FatArrow => Some("=>".into()),
                _ => None,
            };
            if let Some(name) = name {
                self.bump();
                self.bump();
                return Ok(authored::Expr::Name(name.into()));
            }
        }
        // An ascribed lambda must receive the annotation before its body is lowered.
        if self.at_operator("|") {
            let value = self.parse_primary(expected)?;
            self.newlines();
            self.expect_punct(')')?;
            return Ok(value);
        }
        let first = self.parse_expr(None)?;
        if !self.eat_punct(',') {
            self.expect_punct(')')?;
            return Ok(first);
        }
        let mut items = vec![first];
        loop {
            self.newlines();
            if self.eat_punct(')') {
                break;
            }
            items.push(self.parse_expr(None)?);
            if self.eat_punct(')') {
                break;
            }
            self.expect_punct(',')?;
        }
        let target = match expected {
            Some(authored::Type::Product(types)) if types.len() == items.len() => {
                authored::Type::Product(types.clone())
            }
            _ => authored::Type::Product(
                items
                    .iter()
                    .map(obvious_expr_type)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| self.here("tuple element types require an annotation"))?,
            ),
        };
        Ok(authored::Expr::Ascribe {
            target,
            value: Box::new(authored::Expr::Vector(items)),
        })
    }

    fn parse_arguments(&mut self) -> Result<Vec<CallArgument>, MagError> {
        let mut args = Vec::new();
        self.separators();
        while !self.eat_punct(')') {
            if matches!(self.peek().kind, TokenKind::Ident(_))
                && self.peek_n(1).kind == TokenKind::Punct(':')
            {
                let name = self.expect_name()?;
                self.expect_punct(':')?;
                args.push(CallArgument::Named(name, self.parse_expr(None)?));
            } else {
                args.push(CallArgument::Positional(self.parse_expr(None)?));
            }
            if self.eat_punct(')') {
                break;
            }
            self.expect_punct(',')?;
            self.separators();
        }
        Ok(args)
    }

    fn lower_call(
        &mut self,
        callee: authored::Expr,
        args: Vec<CallArgument>,
        expected: Option<&authored::Type>,
    ) -> Result<authored::Expr, MagError> {
        if let authored::Expr::Name(constructor_reference) = &callee {
            let specialized = self
                .specialized_constructors
                .get(constructor_reference.as_str())
                .cloned();
            let constructor = specialized
                .as_ref()
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| constructor_reference.last().to_owned());
            let owner_hint = specialized
                .as_ref()
                .and_then(|(owner, _)| match owner {
                    authored::Type::Name(name) => Some(name.clone()),
                    authored::Type::Apply { constructor, .. } => Some(constructor.clone()),
                    _ => None,
                })
                .or_else(|| constructor_reference.owner());
            let variant = owner_hint
                .as_ref()
                .and_then(|owner| self.variants.get(&format!("{owner}.{constructor}")))
                .cloned()
                .or_else(|| {
                    let mut matching = self
                        .variants
                        .iter()
                        .filter(|(name, _)| name.ends_with(&format!(".{constructor}")))
                        .map(|(_, metadata)| metadata.clone());
                    let first = matching.next()?;
                    matching.next().is_none().then_some(first)
                });
            if let Some(metadata) = variant {
                let owner_type = if let Some((owner_type, _)) = specialized.clone() {
                    owner_type
                } else {
                    constructor_types(&metadata.owner, &constructor, expected).0
                };
                let payload_type = specialize_variant_payload(&metadata, &owner_type);
                let payload = if metadata.field_order.is_empty()
                    && args
                        .iter()
                        .all(|arg| matches!(arg, CallArgument::Positional(_)))
                {
                    let values = args
                        .into_iter()
                        .map(|arg| match arg {
                            CallArgument::Positional(value) => value,
                            CallArgument::Named(_, _) => unreachable!(),
                        })
                        .collect::<Vec<_>>();
                    match values.as_slice() {
                        [] => authored::Expr::Unit,
                        [value] => value.clone(),
                        _ => authored::Expr::Ascribe {
                            target: payload_type,
                            value: Box::new(authored::Expr::Vector(values)),
                        },
                    }
                } else {
                    let fields = args
                        .into_iter()
                        .enumerate()
                        .map(|(index, arg)| match arg {
                            CallArgument::Named(name, value) => (name, value),
                            CallArgument::Positional(value) => (
                                metadata
                                    .field_order
                                    .get(index)
                                    .cloned()
                                    .unwrap_or_else(|| index.to_string()),
                                value,
                            ),
                        })
                        .collect();
                    authored::Expr::Ascribe {
                        target: payload_type,
                        value: Box::new(authored::Expr::Fields(fields)),
                    }
                };
                return Ok(authored::Expr::Construct {
                    owner: owner_type,
                    constructor: constructor.to_owned(),
                    payload: Box::new(payload),
                });
            }
            let imported_owner = specialized
                .as_ref()
                .map(|(owner, _)| owner.clone())
                .or_else(|| {
                    owner_hint.as_ref().and_then(|owner| {
                        (expected_owner_name(expected) == Some(owner.as_str()))
                            .then(|| expected.cloned())
                            .flatten()
                    })
                });
            if let Some(owner) = imported_owner {
                let all_positional = args
                    .iter()
                    .all(|argument| matches!(argument, CallArgument::Positional(_)));
                let all_named = args
                    .iter()
                    .all(|argument| matches!(argument, CallArgument::Named(_, _)));
                let payload = if all_positional {
                    let values = args
                        .into_iter()
                        .map(|argument| match argument {
                            CallArgument::Positional(value) => value,
                            CallArgument::Named(_, _) => unreachable!(),
                        })
                        .collect::<Vec<_>>();
                    match values.as_slice() {
                        [] => authored::Expr::Unit,
                        [value] => value.clone(),
                        _ => authored::Expr::Vector(values),
                    }
                } else if all_named {
                    authored::Expr::Fields(
                        args.into_iter()
                            .map(|argument| match argument {
                                CallArgument::Named(name, value) => (name, value),
                                CallArgument::Positional(_) => unreachable!(),
                            })
                            .collect(),
                    )
                } else {
                    return Err(self
                        .here("constructor arguments must be either all positional or all named"));
                };
                return Ok(authored::Expr::Construct {
                    owner,
                    constructor,
                    payload: Box::new(payload),
                });
            }
        }
        if args
            .iter()
            .any(|argument| matches!(argument, CallArgument::Named(_, _)))
        {
            return Err(self.here("named arguments are only valid for named constructor payloads"));
        }
        Ok(authored::Expr::Call {
            callee: Box::new(callee),
            type_args: None,
            args: args
                .into_iter()
                .map(|arg| match arg {
                    CallArgument::Positional(value) => value,
                    CallArgument::Named(_, _) => unreachable!(),
                })
                .collect(),
        })
    }

    fn parse_value_fields(&mut self, end: char) -> Result<Vec<(String, authored::Expr)>, MagError> {
        let mut fields = Vec::new();
        self.separators();
        while !self.eat_punct(end) {
            let name = self.expect_name()?;
            self.expect_punct(':')?;
            fields.push((name, self.parse_expr(None)?));
            if self.eat_punct(end) {
                break;
            }
            if !self.eat_punct(',') && !self.at_newline() {
                return Err(self.here("expected ',' or newline between fields"));
            }
            self.separators();
        }
        Ok(fields)
    }

    fn parse_type(&mut self) -> Result<authored::Type, MagError> {
        self.parse_arrow_type()
    }

    fn parse_arrow_type(&mut self) -> Result<authored::Type, MagError> {
        let left = self.parse_sum_type()?;
        if self.eat_arrow() {
            let result = self.parse_arrow_type()?;
            let params = if matches!(&left, authored::Type::Product(items) if items.is_empty()) {
                Vec::new()
            } else {
                vec![left]
            };
            Ok(authored::Type::Function {
                params,
                result: Box::new(result),
            })
        } else {
            Ok(left)
        }
    }

    fn parse_sum_type(&mut self) -> Result<authored::Type, MagError> {
        let value = self.parse_type_atom()?;
        if self.at_operator("|") || self.at_operator("+") {
            return Err(
                self.here("infix sum and product types are unsupported; use an ADT or tuple type")
            );
        }
        Ok(value)
    }

    fn parse_type_atom(&mut self) -> Result<authored::Type, MagError> {
        self.newlines();
        if self.eat_word("fn") {
            self.expect_punct('(')?;
            let params = self.parse_type_list(')')?;
            self.expect_arrow()?;
            return Ok(authored::Type::Function {
                params,
                result: Box::new(self.parse_arrow_type()?),
            });
        }
        if self.eat_punct('(') {
            let mut params = self.parse_type_list(')')?;
            if params.len() == 1 {
                return Ok(params.remove(0));
            }
            return Ok(authored::Type::Product(params));
        }
        let name = self.expect_name_path()?;
        if self.eat_operator("<") {
            let mut arguments = self.parse_type_arguments()?;
            if name.as_str() == "TypeTag" && arguments.len() == 1 {
                return Ok(authored::Type::Tag(Box::new(arguments.remove(0))));
            }
            Ok(authored::Type::Apply {
                constructor: name,
                arguments,
            })
        } else {
            Ok(authored::Type::Name(name))
        }
    }

    fn parse_call_type_arguments(&mut self) -> Result<Vec<authored::TypeArgument>, MagError> {
        let mut values = Vec::new();
        self.newlines();
        if self.eat_operator(">") {
            return Ok(values);
        }
        loop {
            if self.at_word("_") {
                self.bump();
                values.push(authored::TypeArgument::Infer);
            } else {
                values.push(authored::TypeArgument::Explicit(self.parse_type()?));
            }
            if self.eat_operator(">") {
                break;
            }
            self.expect_punct(',')?;
            self.newlines();
        }
        Ok(values)
    }

    fn parse_type_arguments(&mut self) -> Result<Vec<authored::Type>, MagError> {
        let mut values = Vec::new();
        self.newlines();
        if self.eat_operator(">") {
            return Ok(values);
        }
        loop {
            values.push(self.parse_type()?);
            if self.eat_operator(">") {
                break;
            }
            self.expect_punct(',')?;
            self.newlines();
        }
        Ok(values)
    }

    fn parse_type_list(&mut self, end: char) -> Result<Vec<authored::Type>, MagError> {
        let mut values = Vec::new();
        self.newlines();
        if self.eat_punct(end) {
            return Ok(values);
        }
        loop {
            values.push(self.parse_type()?);
            if self.eat_punct(end) {
                break;
            }
            self.expect_punct(',')?;
            self.newlines();
        }
        Ok(values)
    }

    fn reassociate(
        &self,
        operands: Vec<authored::Expr>,
        operators: Vec<(String, ByteSpan)>,
    ) -> Result<authored::Expr, MagError> {
        if operators.is_empty() {
            return operands
                .into_iter()
                .next()
                .ok_or_else(|| self.here("internal parser error: expression has no operand"));
        }
        let first = operands
            .first()
            .cloned()
            .ok_or_else(|| self.here("internal parser error: infix expression has no operand"))?;
        let mut values = vec![first];
        let mut stack: Vec<(String, Fixity)> = Vec::new();
        for (index, (operator, span)) in operators.iter().enumerate() {
            let fixity = self.fixities.get(operator).copied().unwrap_or(Fixity {
                precedence: 9,
                associativity: Associativity::Left,
                span: *span,
            });
            while let Some((top_name, top)) = stack.last().cloned() {
                if top.precedence == fixity.precedence
                    && (top.associativity != fixity.associativity
                        || top.associativity == Associativity::None)
                {
                    return Err(self.error("MAG2010", format!("conflicting fixities for '{top_name}' and '{operator}' at precedence {}", fixity.precedence), *span, Some((format!("'{top_name}' fixity declared here"), top.span))));
                }
                let reduce = top.precedence > fixity.precedence
                    || (top.precedence == fixity.precedence
                        && fixity.associativity == Associativity::Left);
                if !reduce {
                    break;
                }
                stack.pop();
                reduce_operator(&mut values, top_name).ok_or_else(|| {
                    self.here("internal parser error: incomplete infix expression")
                })?;
            }
            stack.push((operator.clone(), fixity));
            let operand = operands
                .get(index + 1)
                .cloned()
                .ok_or_else(|| self.here("internal parser error: missing right operand"))?;
            values.push(operand);
        }
        while let Some((operator, _)) = stack.pop() {
            reduce_operator(&mut values, operator)
                .ok_or_else(|| self.here("internal parser error: incomplete infix expression"))?;
        }
        values
            .pop()
            .ok_or_else(|| self.here("internal parser error: infix expression produced no value"))
    }

    fn take_infix_operator(&mut self) -> Result<Option<(String, ByteSpan)>, MagError> {
        let token = self.peek().clone();
        let (name, symbolic) = match &token.kind {
            TokenKind::Operator(op) if !matches!(op.as_str(), "=" | "|" | ">" | "<") => {
                (self.resolve_name(op), true)
            }
            TokenKind::Arrow => (self.resolve_name("->"), true),
            TokenKind::Ident(name)
                if !matches!(
                    name.as_str(),
                    "then" | "else" | "case" | "let" | "type" | "import" | "as"
                ) =>
            {
                (self.resolve_name(name), name.bytes().all(is_operator_byte))
            }
            _ => return Ok(None),
        };
        if symbolic {
            self.require_symbolic_infix_spacing(&token)?;
        }
        self.bump();
        Ok(Some((name, token.span)))
    }

    fn require_symbolic_infix_spacing(&self, token: &Token) -> Result<(), MagError> {
        let separated_left = token.span.start > 0
            && is_infix_separator_byte(self.source.text.as_bytes()[token.span.start - 1]);
        let separated_right = token.span.end < self.source.text.len()
            && is_infix_separator_byte(self.source.text.as_bytes()[token.span.end]);
        if separated_left && separated_right {
            return Ok(());
        }
        let side = match (separated_left, separated_right) {
            (false, false) => "before and after",
            (false, true) => "before",
            (true, false) => "after",
            (true, true) => unreachable!(),
        };
        let operator = &self.source.text[token.span.start..token.span.end];
        Err(self.error(
            "MAG2012",
            format!(
                "symbolic infix operator '{operator}' requires whitespace {side} it; write infix operators with spaces on both sides"
            ),
            token.span,
            None,
        ))
    }

    fn parse_generic_names(&mut self) -> Result<Vec<String>, MagError> {
        if !self.eat_operator("<") {
            return Ok(vec![]);
        }
        let mut names = Vec::new();
        if self.eat_operator(">") {
            return Ok(names);
        }
        loop {
            names.push(self.expect_name()?);
            if self.eat_operator(">") {
                break;
            }
            self.expect_punct(',')?;
        }
        let mut seen = HashSet::new();
        if let Some(duplicate) = names.iter().find(|name| !seen.insert((*name).clone())) {
            return Err(self.here(format!("duplicate generic parameter {duplicate}")));
        }
        Ok(names)
    }

    fn peek_variant_declaration(&self) -> bool {
        let mut index = self.cursor;
        while matches!(self.tokens[index].kind, TokenKind::Newline) {
            index += 1;
        }
        if matches!(&self.tokens[index].kind, TokenKind::Operator(op) if op == "|") {
            index += 1;
        }
        matches!(self.tokens[index].kind, TokenKind::Ident(_))
            && self.tokens.get(index + 1).is_some_and(|token| {
                matches!(token.kind, TokenKind::Punct('(') | TokenKind::Punct('{'))
            })
    }

    fn resolve_name(&mut self, name: &str) -> String {
        if self.bound_names.contains(name) {
            return name.to_owned();
        }
        self.aliases
            .get(name)
            .map(ToString::to_string)
            .unwrap_or_else(|| name.to_owned())
    }

    fn expect_name_path(&mut self) -> Result<authored::NamePath, MagError> {
        let first = self.expect_name()?;
        let mut segments = self
            .aliases
            .get(&first)
            .map(|path| path.segments().to_vec())
            .unwrap_or_else(|| vec![first]);
        while self.eat_punct('.') {
            segments.push(self.expect_name()?);
        }
        Ok(authored::NamePath::from_segments(segments))
    }

    fn lookahead_ascribed_function_type(&self) -> Option<authored::Type> {
        if !self.at_punct('(')
            || !matches!(self.peek_n(1).kind, TokenKind::Operator(ref op) if op == "|")
        {
            return None;
        }
        let mut depth = 0usize;
        let mut index = self.cursor;
        loop {
            match self.tokens.get(index)?.kind {
                TokenKind::Punct('(') => depth += 1,
                TokenKind::Punct(')') => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Eof => return None,
                _ => {}
            }
            index += 1;
        }
        index += 1;
        while matches!(self.tokens.get(index)?.kind, TokenKind::Newline) {
            index += 1;
        }
        if self.tokens.get(index)?.kind != TokenKind::Punct(':') {
            return None;
        }
        let mut probe = self.clone();
        probe.cursor = index + 1;
        probe.parse_type().ok()
    }

    fn require_separator(&mut self) -> Result<(), MagError> {
        if self.at_newline() {
            self.separators();
            return Ok(());
        }
        if self.at_eof() || self.at_punct('}') {
            return Ok(());
        }
        Err(self.here("expected newline"))
    }
    fn separators(&mut self) {
        while self.at_newline() {
            self.bump();
        }
    }
    fn newlines(&mut self) {
        while self.at_newline() {
            self.bump();
        }
    }
    fn at_separator(&self) -> bool {
        self.at_newline() || self.at_punct(';')
    }
    fn at_newline(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Newline)
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }
    fn at_word(&self, word: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(value) if value == word)
    }
    fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, word: &str) -> Result<(), MagError> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.here(format!("expected '{word}'")))
        }
    }
    fn at_operator(&self, operator: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Operator(value) if value == operator)
    }
    fn eat_operator(&mut self, operator: &str) -> bool {
        if self.at_operator(operator) {
            self.bump();
            return true;
        }
        if operator == ">" {
            let token = self.peek().clone();
            if let TokenKind::Operator(value) = token.kind {
                if let Some(rest) = value.strip_prefix('>') {
                    if rest.is_empty() {
                        self.bump();
                    } else {
                        self.tokens[self.cursor] = Token {
                            kind: TokenKind::Operator(rest.to_owned()),
                            span: ByteSpan::new(token.span.start + 1, token.span.end),
                        };
                    }
                    return true;
                }
            }
        }
        false
    }
    fn expect_operator(&mut self, operator: &str) -> Result<(), MagError> {
        if self.eat_operator(operator) {
            Ok(())
        } else {
            Err(self.here(format!("expected '{operator}'")))
        }
    }
    fn eat_arrow(&mut self) -> bool {
        if matches!(self.peek().kind, TokenKind::Arrow) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_arrow(&mut self) -> Result<(), MagError> {
        if self.eat_arrow() {
            Ok(())
        } else {
            Err(self.here("expected '->'"))
        }
    }
    fn expect_fat_arrow(&mut self) -> Result<(), MagError> {
        if matches!(self.peek().kind, TokenKind::FatArrow) {
            self.bump();
            Ok(())
        } else {
            Err(self.here("expected '=>'"))
        }
    }
    fn at_punct(&self, punct: char) -> bool {
        self.peek().kind == TokenKind::Punct(punct)
    }
    fn eat_punct(&mut self, punct: char) -> bool {
        if self.at_punct(punct) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_punct(&mut self, punct: char) -> Result<(), MagError> {
        if self.eat_punct(punct) {
            Ok(())
        } else {
            Err(self.here(format!("expected '{punct}'")))
        }
    }
    fn expect_binding_name(&mut self) -> Result<String, MagError> {
        if self.eat_punct('(') {
            let token = self.bump().clone();
            let name = match token.kind {
                TokenKind::Ident(name) | TokenKind::Operator(name) => name,
                TokenKind::Arrow => "->".into(),
                TokenKind::FatArrow => "=>".into(),
                _ => return Err(self.parse_error("expected operator binding name", token.span)),
            };
            self.expect_punct(')')?;
            Ok(name)
        } else {
            self.expect_name()
        }
    }
    fn expect_name(&mut self) -> Result<String, MagError> {
        let token = self.bump().clone();
        match token.kind {
            TokenKind::Ident(name) | TokenKind::Operator(name) => Ok(name),
            _ => Err(self.parse_error("expected name", token.span)),
        }
    }
    fn peek(&self) -> &Token {
        &self.tokens[self.cursor]
    }
    fn peek_n(&self, offset: usize) -> &Token {
        let last = self.tokens.len().saturating_sub(1);
        &self.tokens[(self.cursor + offset).min(last)]
    }
    fn bump(&mut self) -> &Token {
        let index = self.cursor;
        if !self.at_eof() {
            self.cursor += 1;
        }
        &self.tokens[index]
    }
    fn here(&self, message: impl Into<String>) -> MagError {
        self.parse_error(message, self.peek().span)
    }
    fn parse_error(&self, message: impl Into<String>, span: ByteSpan) -> MagError {
        self.error("MAG2001", message, span, None)
    }
    fn error(
        &self,
        code: &'static str,
        message: impl Into<String>,
        span: ByteSpan,
        related: Option<(String, ByteSpan)>,
    ) -> MagError {
        MagError::Syntax(Box::new(SyntaxDiagnostic::new(
            code,
            "parser",
            message.into(),
            self.source,
            span,
            related,
        )))
    }
}

#[derive(Debug)]
enum CallArgument {
    Positional(authored::Expr),
    Named(String, authored::Expr),
}

fn expected_owner_name(expected: Option<&authored::Type>) -> Option<&str> {
    match expected {
        Some(authored::Type::Name(name)) => Some(name),
        Some(authored::Type::Apply { constructor, .. }) => Some(constructor),
        _ => None,
    }
}

fn constructor_types(
    owner: &str,
    constructor: &str,
    expected: Option<&authored::Type>,
) -> (authored::Type, authored::Type) {
    match expected {
        Some(authored::Type::Apply {
            constructor: expected_owner,
            arguments,
        }) if expected_owner == owner => (
            authored::Type::Apply {
                constructor: expected_owner.clone(),
                arguments: arguments.clone(),
            },
            authored::Type::Apply {
                constructor: constructor.into(),
                arguments: arguments.clone(),
            },
        ),
        Some(authored::Type::Name(expected_owner)) if expected_owner == owner => (
            authored::Type::Name(owner.into()),
            authored::Type::Name(constructor.into()),
        ),
        _ => (
            authored::Type::Name(owner.into()),
            authored::Type::Name(constructor.into()),
        ),
    }
}

fn specialize_variant_payload(
    metadata: &VariantMetadata,
    owner: &authored::Type,
) -> authored::Type {
    let arguments = match owner {
        authored::Type::Apply { arguments, .. } => arguments,
        _ => return metadata.payload.clone(),
    };
    let substitutions = metadata
        .owner_params
        .iter()
        .map(String::as_str)
        .zip(arguments)
        .collect::<HashMap<_, _>>();
    substitute_authored_type(&metadata.payload, &substitutions)
}

fn substitute_authored_type(
    ty: &authored::Type,
    substitutions: &HashMap<&str, &authored::Type>,
) -> authored::Type {
    match ty {
        authored::Type::Name(name) => substitutions
            .get(name.as_str())
            .map_or_else(|| ty.clone(), |replacement| (*replacement).clone()),
        authored::Type::Product(items) => authored::Type::Product(
            items
                .iter()
                .map(|item| substitute_authored_type(item, substitutions))
                .collect(),
        ),
        authored::Type::Tag(item) => {
            authored::Type::Tag(Box::new(substitute_authored_type(item, substitutions)))
        }
        authored::Type::Function { params, result } => authored::Type::Function {
            params: params
                .iter()
                .map(|parameter| substitute_authored_type(parameter, substitutions))
                .collect(),
            result: Box::new(substitute_authored_type(result, substitutions)),
        },
        authored::Type::Apply {
            constructor,
            arguments,
        } => authored::Type::Apply {
            constructor: constructor.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute_authored_type(argument, substitutions))
                .collect(),
        },
        authored::Type::Invalid(_) => ty.clone(),
    }
}

fn obvious_expr_type(expression: &authored::Expr) -> Option<authored::Type> {
    match expression {
        authored::Expr::Unit => Some(authored::Type::Name("Unit".into())),
        authored::Expr::Str(_) => Some(authored::Type::Name("String".into())),
        authored::Expr::Int(_) => Some(authored::Type::Name("Int".into())),
        authored::Expr::Float(_) => Some(authored::Type::Name("Float".into())),
        authored::Expr::Bool(_) => Some(authored::Type::Name("Bool".into())),
        authored::Expr::Ascribe { target, .. } => Some(target.clone()),
        _ => None,
    }
}

fn reduce_operator(values: &mut Vec<authored::Expr>, operator: String) -> Option<()> {
    let right = values.pop()?;
    let left = values.pop()?;
    values.push(authored::Expr::Call {
        callee: Box::new(authored::Expr::Name(operator.into())),
        type_args: None,
        args: vec![left, right],
    });
    Some(())
}

fn scan_direct_block_lets(tokens: &[Token], cursor: usize) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut depth = 0usize;
    let mut index = cursor;
    while index < tokens.len() {
        match &tokens[index].kind {
            TokenKind::Punct('}') if depth == 0 => break,
            TokenKind::Punct('{') => depth += 1,
            TokenKind::Punct('}') => depth = depth.saturating_sub(1),
            TokenKind::Ident(word) if word == "let" && depth == 0 => {
                match tokens.get(index + 1).map(|token| &token.kind) {
                    Some(TokenKind::Ident(name) | TokenKind::Operator(name)) => {
                        names.insert(name.clone());
                    }
                    Some(TokenKind::Punct('(')) => {
                        if let Some(TokenKind::Operator(name)) =
                            tokens.get(index + 2).map(|token| &token.kind)
                        {
                            names.insert(name.clone());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        index += 1;
    }
    names
}

fn scan_top_level_lets(tokens: &[Token]) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index + 1 < tokens.len() {
        match tokens[index].kind {
            TokenKind::Punct('(' | '[' | '{') => depth += 1,
            TokenKind::Punct(')' | ']' | '}') => depth = depth.saturating_sub(1),
            TokenKind::Ident(ref word) if word == "let" && depth == 0 => {
                match tokens.get(index + 1).map(|token| &token.kind) {
                    Some(TokenKind::Ident(name) | TokenKind::Operator(name)) => {
                        names.insert(name.clone());
                    }
                    Some(TokenKind::Punct('(')) => {
                        if let Some(TokenKind::Operator(name)) =
                            tokens.get(index + 2).map(|token| &token.kind)
                        {
                            names.insert(name.clone());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        index += 1;
    }
    names
}

fn fixity_name_at(tokens: &[Token], cursor: usize) -> Option<(String, ByteSpan, usize)> {
    let token = tokens.get(cursor)?;
    match &token.kind {
        TokenKind::Ident(name) | TokenKind::Operator(name) => {
            Some((name.clone(), token.span, cursor + 1))
        }
        TokenKind::Punct('(') => {
            let name_token = tokens.get(cursor + 1)?;
            let name = match &name_token.kind {
                TokenKind::Ident(name) | TokenKind::Operator(name) => name.clone(),
                TokenKind::Arrow => "->".into(),
                TokenKind::FatArrow => "=>".into(),
                _ => return None,
            };
            matches!(tokens.get(cursor + 2)?.kind, TokenKind::Punct(')')).then_some((
                name,
                name_token.span,
                cursor + 3,
            ))
        }
        _ => None,
    }
}

fn scan_fixities(
    source: &SourceSnapshot,
    tokens: &[Token],
) -> Result<HashMap<String, Fixity>, MagError> {
    let mut fixities = HashMap::new();
    let mut index = 0;
    while index < tokens.len() {
        while matches!(
            tokens[index].kind,
            TokenKind::Newline | TokenKind::Punct(';')
        ) {
            index += 1;
        }
        let start = tokens.get(index).map_or(
            ByteSpan::new(source.text.len(), source.text.len()),
            |token| token.span,
        );
        let (associativity, mut cursor) = match tokens.get(index).map(|token| &token.kind) {
            Some(TokenKind::Ident(word)) if word == "infixl" => (Associativity::Left, index + 1),
            Some(TokenKind::Ident(word)) if word == "infixr" => (Associativity::Right, index + 1),
            Some(TokenKind::Ident(word)) if word == "infix" => (Associativity::None, index + 1),
            Some(TokenKind::Ident(word)) if word == "fixity" => {
                let assoc = match tokens.get(index + 1).map(|token| &token.kind) {
                    Some(TokenKind::Ident(word)) if word == "left" => Associativity::Left,
                    Some(TokenKind::Ident(word)) if word == "right" => Associativity::Right,
                    Some(TokenKind::Ident(word)) if word == "none" => Associativity::None,
                    _ => {
                        index += 1;
                        continue;
                    }
                };
                (assoc, index + 2)
            }
            _ => {
                while !matches!(tokens[index].kind, TokenKind::Newline | TokenKind::Eof) {
                    index += 1;
                }
                if matches!(tokens[index].kind, TokenKind::Eof) {
                    break;
                }
                continue;
            }
        };
        let Some(Token {
            kind: TokenKind::Int(precedence),
            ..
        }) = tokens.get(cursor)
        else {
            return Err(MagError::Syntax(Box::new(SyntaxDiagnostic::new(
                "MAG2002",
                "parser",
                "fixity declaration requires precedence 0..9".into(),
                source,
                start,
                None,
            ))));
        };
        let precedence = u8::try_from(*precedence)
            .ok()
            .filter(|value| *value <= 9)
            .ok_or_else(|| {
                MagError::Syntax(Box::new(SyntaxDiagnostic::new(
                    "MAG2002",
                    "parser",
                    "fixity precedence must be 0..9".into(),
                    source,
                    tokens[cursor].span,
                    None,
                )))
            })?;
        cursor += 1;
        loop {
            let (operator, span, next) = fixity_name_at(tokens, cursor).ok_or_else(|| {
                MagError::Syntax(Box::new(SyntaxDiagnostic::new(
                    "MAG2002",
                    "parser",
                    "fixity declaration requires a term name".into(),
                    source,
                    tokens.get(cursor).map_or(start, |token| token.span),
                    None,
                )))
            })?;
            let fixity = Fixity {
                precedence,
                associativity,
                span,
            };
            if let Some(previous) = fixities.insert(operator.clone(), fixity) {
                return Err(MagError::Syntax(Box::new(SyntaxDiagnostic::new(
                    "MAG2003",
                    "parser",
                    format!("duplicate fixity declaration for '{operator}'"),
                    source,
                    span,
                    Some(("previous declaration".into(), previous.span)),
                ))));
            }
            cursor = next;
            if tokens
                .get(cursor)
                .is_some_and(|token| token.kind == TokenKind::Punct(','))
            {
                cursor += 1;
                continue;
            }
            index = cursor;
            break;
        }
    }
    Ok(fixities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::SourceRole;

    fn parse(text: &str) -> Result<authored::Module, MagError> {
        compile_source(
            &SourceSnapshot::named("surface.mag", text),
            None,
            SourceRole::Entry,
        )
    }

    #[test]
    fn lowers_imports_types_calls_and_constructors() {
        let module = parse(
            r#"
            import nefor.graph.{edge, identity as id}
            import support.{}
            import nefor.node.{}
            type Score { label: String, accepted: Bool }
            type Decision = Accepted(Score) | Rejected {score: Score, reason: String}
            let score: Score = Score { label: "ok", accepted: true }
            let decision: Decision = Decision.Accepted(score)
            let result = edge(id("x"), nefor.node.output("y"))
        "#,
        )
        .unwrap();
        assert!(
            matches!(&module.forms[0], authored::Form::Require(value) if value.module == "nefor.graph")
        );
        assert!(
            matches!(&module.forms[1], authored::Form::Require(value) if value.module == "support")
        );
        assert!(module.forms.iter().any(
            |form| matches!(form, authored::Form::Require(value) if value.module == "nefor.node")
        ));
        assert!(module.forms.iter().any(
            |form| matches!(form, authored::Form::Type(value) if value.name == "Decision.Rejected")
        ));
        let authored::Form::Block(authored::BlockItem::Let { value, .. }) =
            module.forms.last().unwrap()
        else {
            panic!("let")
        };
        let authored::Expr::Call { callee, .. } = value else {
            panic!("call")
        };
        assert!(
            matches!(callee.as_ref(), authored::Expr::Name(name) if name == "nefor.graph.edge")
        );
    }

    #[test]
    fn generic_constructor_uses_expected_owner_arguments() {
        let module = parse(
            "type Option<T> = Some(T) | None()\nlet value: Option<Int> = Option<Int>.Some(1)\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = module.forms.last().unwrap()
        else {
            panic!("constructor binding")
        };
        let authored::Expr::Construct { owner, payload, .. } = value.as_ref() else {
            panic!("constructor")
        };
        assert!(
            matches!(owner, authored::Type::Apply { constructor, arguments } if constructor == "Option" && arguments == &[authored::Type::Name("Int".into())])
        );
        assert!(matches!(payload.as_ref(), authored::Expr::Int(1)));
    }

    #[test]
    fn preserves_nary_calls_and_lowers_infix_to_binary_calls() {
        let module = parse("fixity right 5 ++\nlet x = f(a, b, c)\nlet y = a ++ b ++ c\n").unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Call { args, .. },
            ..
        }) = &module.forms[0]
        else {
            panic!("nary call")
        };
        assert_eq!(args.len(), 3);

        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Call { callee, args, .. },
            ..
        }) = &module.forms[1]
        else {
            panic!("infix")
        };
        assert!(matches!(callee.as_ref(), authored::Expr::Name(name) if name == "++"));
        assert!(
            matches!(args.as_slice(), [authored::Expr::Name(name), authored::Expr::Call { .. }] if name == "a")
        );
        let authored::Expr::Call {
            callee: nested_callee,
            args: nested_args,
            ..
        } = &args[1]
        else {
            panic!("right-associated infix call")
        };
        assert!(matches!(nested_callee.as_ref(), authored::Expr::Name(name) if name == "++"));
        assert!(
            matches!(nested_args.as_slice(), [authored::Expr::Name(left), authored::Expr::Name(right)] if left == "b" && right == "c")
        );
    }

    #[test]
    fn complete_let_type_supplies_lambda_annotations() {
        let module = parse("let f: fn(Int, String) -> Bool = |number, text| => true\n").unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = &module.forms[0]
        else {
            panic!("ascribed let")
        };
        let authored::Expr::Function(function) = value.as_ref() else {
            panic!("function")
        };
        assert_eq!(function.params[0].ty, authored::Type::Name("Int".into()));
        assert_eq!(function.result, authored::Type::Name("Bool".into()));
    }

    #[test]
    fn inline_ascription_supplies_lambda_annotations() {
        let module = parse("let f = (|value| => value): (Int) -> Int\n").unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Ascribe { value, .. },
            ..
        }) = &module.forms[0]
        else {
            panic!("ascribed lambda")
        };
        assert!(
            matches!(value.as_ref(), authored::Expr::Function(function) if function.params[0].ty == authored::Type::Name("Int".into()))
        );
    }

    #[test]
    fn tuple_argument_and_nary_function_types_remain_distinct() {
        let module = parse(
            "let tupled: (Int, String) -> Bool = |pair| => true\nlet nary: fn(Int, String) -> Bool = |number, text| => true\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { target: tupled, .. },
            ..
        }) = &module.forms[0]
        else {
            panic!("tuple function")
        };
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { target: nary, .. },
            ..
        }) = &module.forms[1]
        else {
            panic!("nary function")
        };
        assert_ne!(tupled, nary);
        assert!(
            matches!(tupled, authored::Type::Function { params, .. } if matches!(params.as_slice(), [authored::Type::Product(_)]))
        );
        assert!(matches!(nary, authored::Type::Function { params, .. } if params.len() == 2));
    }

    #[test]
    fn tuple_list_and_grouping_have_distinct_authored_shapes() {
        let module =
            parse("let unit = ()\nlet grouped = (1)\nlet tuple = (1,)\nlet list = [1]\n").unwrap();
        let values = module
            .forms
            .iter()
            .map(|form| match form {
                authored::Form::Block(authored::BlockItem::Let { value, .. }) => value,
                _ => panic!("let"),
            })
            .collect::<Vec<_>>();
        assert!(matches!(values[0], authored::Expr::Unit));
        assert!(matches!(values[1], authored::Expr::Int(1)));
        assert!(
            matches!(values[2], authored::Expr::Ascribe { value, .. } if matches!(value.as_ref(), authored::Expr::Vector(items) if items.len() == 1))
        );
        assert!(matches!(values[3], authored::Expr::Vector(items) if items.len() == 1));
    }

    #[test]
    fn legacy_forms_semicolons_and_leading_operator_continuations_are_rejected() {
        assert!(parse("require \"support\"\n").is_err());
        assert!(parse("let x = 1; let y = 2\n").is_err());
        assert!(parse("let x = 1\n+ 2\n").is_err());
        assert!(parse("artifact(type-tag<String>())\n").is_err());
    }

    #[test]
    fn parenthesized_symbolic_bindings_and_fixities_use_maximal_munch() {
        let module = parse(
            "infixr 4 (>>>), (->)\nlet (>>>): fn(Int, Int) -> Int = |left, right| => left\nlet value = 1 >>> 2\n",
        )
        .unwrap();
        assert!(module.forms.iter().any(|form| matches!(form, authored::Form::Block(authored::BlockItem::Let { name, .. }) if name == ">>>")));
    }

    #[test]
    fn arrow_is_type_syntax_and_can_be_an_imported_term_operator() {
        let module =
            parse("import operators.{`->`}\ninfixr 4 (->)\nlet value: Result = left -> right\n")
                .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = module.forms.last().unwrap()
        else {
            panic!("arrow infix binding")
        };
        let authored::Expr::Call { callee, args, .. } = value.as_ref() else {
            panic!("arrow infix application")
        };
        assert!(matches!(callee.as_ref(), authored::Expr::Name(name) if name == "operators.->"));
        assert_eq!(args.len(), 2);
        assert!(parse("let f: Int -> Int = |value| => value\n").is_ok());
        assert!(parse("let arrow = (->)\n").is_ok());
    }

    #[test]
    fn lexical_binders_win_over_import_aliases_and_named_calls_do_not_erase_labels() {
        let module = parse(
            "import support.{value as imported}\nlet f: Int -> Int = |imported| => imported\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = module.forms.last().unwrap()
        else {
            panic!("function")
        };
        let authored::Expr::Function(function) = value.as_ref() else {
            panic!("function")
        };
        assert!(
            matches!(&function.body[0], authored::BlockItem::Expr(authored::Expr::Name(name)) if name == "imported")
        );
        assert!(parse("let x = f(right: 1, left: 2)\n").is_err());
    }

    #[test]
    fn unannotated_lambda_has_located_diagnostic() {
        let error = parse("let f = |x| => x\n").unwrap_err();
        let MagError::Syntax(diagnostic) = error else {
            panic!("syntax diagnostic")
        };
        assert!(diagnostic
            .message
            .contains("complete expected function type"));
        assert_eq!(diagnostic.location.start.line, 1);
        assert_eq!(diagnostic.location.start.column, 9);
    }

    #[test]
    fn conflicting_same_precedence_fixities_are_rejected() {
        let error = parse("infixl 5 +\ninfixr 5 *\nlet x = a + b * c\n").unwrap_err();
        let MagError::Syntax(diagnostic) = error else {
            panic!("syntax diagnostic")
        };
        assert_eq!(diagnostic.code, "MAG2010");
        assert!(diagnostic.related.is_some());
    }

    #[test]
    fn lexer_keeps_symbolic_operator_boundaries_for_spacing_diagnostics() {
        let source = SourceSnapshot::named("surface.mag", "1+2 3 + 4");
        let tokens = Lexer::new(&source).tokenize().unwrap();
        let operators = tokens
            .iter()
            .filter(|token| matches!(&token.kind, TokenKind::Operator(name) if name == "+"))
            .map(|token| token.span)
            .collect::<Vec<_>>();
        assert_eq!(operators, [ByteSpan::new(1, 2), ByteSpan::new(6, 7)]);
    }

    #[test]
    fn lexes_raw_strings_signed_numbers_comments_and_newlines() {
        let module =
            parse("// comment\nlet a = -12\nlet b = -2.5\nlet s = \"\"\"x\\ny\"\"\"\n").unwrap();
        assert_eq!(module.forms.len(), 3);
        assert!(matches!(
            &module.forms[0],
            authored::Form::Block(authored::BlockItem::Let {
                value: authored::Expr::Int(-12),
                ..
            })
        ));
        assert!(
            matches!(&module.forms[1], authored::Form::Block(authored::BlockItem::Let { value: authored::Expr::Float(value), .. }) if *value == -2.5)
        );
    }

    #[test]
    fn symbolic_infix_operators_require_whitespace_on_both_sides() {
        for (source, side, column) in [
            ("let value = 1- 2\n", "before", 14),
            ("let value = 1 -2\n", "after", 15),
            ("let value = 1-2\n", "before and after", 14),
            ("let value = (1)+ 2\n", "before", 16),
            ("let value = 1 +(2)\n", "after", 15),
        ] {
            let MagError::Syntax(diagnostic) = parse(source).unwrap_err() else {
                panic!("expected syntax diagnostic for {source:?}")
            };
            assert_eq!(diagnostic.code, "MAG2012");
            assert!(diagnostic.message.contains(side), "{}", diagnostic.message);
            assert!(diagnostic.message.contains("spaces on both sides"));
            assert_eq!(diagnostic.location.start.column, column);
            assert_eq!(diagnostic.span.end - diagnostic.span.start, 1);
        }
    }

    #[test]
    fn infix_spacing_accepts_horizontal_and_trailing_operator_newline_separation() {
        assert!(parse("let value = 1 - -2\n").is_ok());
        assert!(parse("let value = f(-2)\n").is_ok());
        assert!(parse("let value = 1 +\n  2\n").is_ok());
        assert!(parse("let value = 1 + // right operand follows the comment\n  2\n").is_ok());
        assert!(parse("let value = 1\n  + 2\n").is_err());
        assert!(parse("let value = 1 +// comment\n  2\n").is_err());
    }

    #[test]
    fn authored_name_paths_preserve_quoted_punctuation_as_one_segment() {
        let module = parse("let value = tools.deep.`call.with.dots`<String>(\"ok\")\n").unwrap();
        let authored::Form::Block(authored::BlockItem::Let { value, .. }) = &module.forms[0] else {
            panic!("let")
        };
        let authored::Expr::Call {
            callee, type_args, ..
        } = value
        else {
            panic!("call")
        };
        let authored::Expr::Name(path) = callee.as_ref() else {
            panic!("name path")
        };
        assert_eq!(path.segments(), ["tools", "deep", "call.with.dots"]);
        assert_eq!(type_args.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn alphabetic_infix_and_symbolic_prefix_forms_keep_their_meaning() {
        assert!(parse(
            "infixl 5 add\nlet add: fn(Int, Int) -> Int = |left, right| => left\nlet value = 1 add 2\n",
        )
        .is_ok());
        assert!(parse("let value = (+)(1, 2)\n").is_ok());
        assert!(parse("import nefor.node.{}\nlet value = nefor.node.`>>>`(left, right)\n").is_ok());
    }

    #[test]
    fn function_block_peer_bindings_shadow_import_aliases_before_declaration() {
        let module = parse(
            "import support.{value}\nlet f: fn() -> Int = | | => {\n  let result = value\n  let value = 1\n  result\n}\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = module.forms.last().unwrap()
        else {
            panic!("function")
        };
        let authored::Expr::Function(function) = value.as_ref() else {
            panic!("function")
        };
        assert!(matches!(
            &function.body[0],
            authored::BlockItem::Let {
                value: authored::Expr::Name(name),
                ..
            } if name == "value"
        ));
    }

    #[test]
    fn constructor_product_uses_its_declared_payload_type() {
        let module = parse(
            "type PairResult = Pair(Int, String)\nlet number = 1\nlet text = \"x\"\nlet result: PairResult = PairResult.Pair(number, text)\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let {
            value: authored::Expr::Annotate { value, .. },
            ..
        }) = module.forms.last().unwrap()
        else {
            panic!("constructor")
        };
        let authored::Expr::Construct { payload, .. } = value.as_ref() else {
            panic!("constructor")
        };
        assert!(matches!(
            payload.as_ref(),
            authored::Expr::Ascribe {
                target: authored::Type::Product(items),
                ..
            } if items == &[authored::Type::Name("Int".into()), authored::Type::Name("String".into())]
        ));
    }

    #[test]
    fn expected_generic_owner_reaches_if_and_match_arms() {
        assert!(parse(
            "type Option<T> = Some(T) | None(Unit)\nlet value: Option<Int> = if true then Option.Some(1) else Option.None(nil)\n",
        )
        .is_ok());
        assert!(parse(
            "type Option<T> = Some(T) | None(Unit)\ntype Flag = Yes(Unit) | No(Unit)\nlet flag: Flag = Flag.Yes(nil)\nlet value: Option<Int> = match flag { case Yes(unit) => Option.Some(1), case No(unit) => Option.None(nil) }\n",
        )
        .is_ok());
    }

    #[test]
    fn local_fixity_applies_to_an_imported_alias() {
        let module = parse(
            "import operators.{append as plus}\ninfixr 5 plus\nlet value = a plus b plus c\n",
        )
        .unwrap();
        let authored::Form::Block(authored::BlockItem::Let { value, .. }) =
            module.forms.last().unwrap()
        else {
            panic!("value")
        };
        let authored::Expr::Call { callee, args, .. } = value else {
            panic!("right-associated call")
        };
        assert!(
            matches!(callee.as_ref(), authored::Expr::Name(name) if name == "operators.append")
        );
        assert!(
            matches!(args.as_slice(), [authored::Expr::Name(left), authored::Expr::Call { .. }] if left == "a")
        );
    }
}
