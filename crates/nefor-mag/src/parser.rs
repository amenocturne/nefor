use crate::ast::Expr;
use crate::diagnostic::{ByteSpan, SourceSnapshot, SyntaxDiagnostic};
use crate::error::MagError;
use crate::lexer::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Vec<Expr>, MagError> {
    parse_source(tokens, &SourceSnapshot::named("<input>", ""))
}

pub fn parse_source(tokens: &[Token], source: &SourceSnapshot) -> Result<Vec<Expr>, MagError> {
    let mut pos = 0;
    let mut exprs = Vec::new();
    while pos < tokens.len() {
        let (expr, next) = parse_expr(tokens, pos, 0, source)?;
        exprs.push(expr);
        pos = next;
    }
    Ok(exprs)
}

fn parse_expr(
    tokens: &[Token],
    pos: usize,
    depth: u16,
    source: &SourceSnapshot,
) -> Result<(Expr, usize), MagError> {
    if depth >= 128 {
        return Err(MagError::Parse("expression nesting limit reached".into()));
    }
    let token = tokens
        .get(pos)
        .ok_or_else(|| MagError::Parse("unexpected end of input".into()))?;
    match &token.kind {
        TokenKind::LParen => parse_collection(tokens, pos, depth, source, Delimiter::Paren),
        TokenKind::LBracket => parse_collection(tokens, pos, depth, source, Delimiter::Bracket),
        TokenKind::LBrace => parse_map(tokens, pos, depth, source),
        kind if closer(kind).is_some() => Err(unexpected_closer(source, token)),
        TokenKind::Arrow => Ok((Expr::Symbol("->".into()), pos + 1)),
        TokenKind::Pipe => Ok((Expr::Symbol("|".into()), pos + 1)),
        TokenKind::Plus => Ok((Expr::Symbol("+".into()), pos + 1)),
        TokenKind::Colon => Ok((Expr::Symbol(":".into()), pos + 1)),
        TokenKind::Symbol(s) => Ok((Expr::Symbol(s.clone()), pos + 1)),
        TokenKind::Keyword(k) => Ok((Expr::Keyword(k.clone()), pos + 1)),
        TokenKind::Str(s) => Ok((Expr::Str(s.clone()), pos + 1)),
        TokenKind::Int(n) => Ok((Expr::Int(*n), pos + 1)),
        TokenKind::Float(f) => Ok((Expr::Float(*f), pos + 1)),
        TokenKind::Bool(b) => Ok((Expr::Bool(*b), pos + 1)),
        TokenKind::Nil => Ok((Expr::Nil, pos + 1)),
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy)]
enum Delimiter {
    Paren,
    Bracket,
    Brace,
}
impl Delimiter {
    fn open(self) -> char {
        match self {
            Self::Paren => '(',
            Self::Bracket => '[',
            Self::Brace => '{',
        }
    }
    fn close(self) -> char {
        match self {
            Self::Paren => ')',
            Self::Bracket => ']',
            Self::Brace => '}',
        }
    }
    fn matches(self, kind: &TokenKind) -> bool {
        matches!(
            (self, kind),
            (Self::Paren, TokenKind::RParen)
                | (Self::Bracket, TokenKind::RBracket)
                | (Self::Brace, TokenKind::RBrace)
        )
    }
}
fn closer(kind: &TokenKind) -> Option<char> {
    match kind {
        TokenKind::RParen => Some(')'),
        TokenKind::RBracket => Some(']'),
        TokenKind::RBrace => Some('}'),
        _ => None,
    }
}

fn parse_collection(
    tokens: &[Token],
    opener_pos: usize,
    depth: u16,
    source: &SourceSnapshot,
    delimiter: Delimiter,
) -> Result<(Expr, usize), MagError> {
    let opener = &tokens[opener_pos];
    let mut pos = opener_pos + 1;
    let mut items = Vec::new();
    loop {
        match tokens.get(pos) {
            Some(token) if delimiter.matches(&token.kind) => {
                let expr = match delimiter {
                    Delimiter::Paren => Expr::List(items),
                    Delimiter::Bracket => Expr::Vector(items),
                    Delimiter::Brace => unreachable!(),
                };
                return Ok((expr, pos + 1));
            }
            Some(token) if closer(&token.kind).is_some() => {
                return Err(mismatched(source, token, opener, delimiter))
            }
            Some(_) => {
                let (expr, next) = parse_expr(tokens, pos, depth + 1, source)?;
                items.push(expr);
                pos = next;
            }
            None => return Err(unclosed(source, opener, delimiter)),
        }
    }
}
fn parse_map(
    tokens: &[Token],
    opener_pos: usize,
    depth: u16,
    source: &SourceSnapshot,
) -> Result<(Expr, usize), MagError> {
    let opener = &tokens[opener_pos];
    let delimiter = Delimiter::Brace;
    let mut pos = opener_pos + 1;
    let mut pairs = Vec::new();
    loop {
        match tokens.get(pos) {
            Some(token) if delimiter.matches(&token.kind) => {
                return Ok((Expr::Map(pairs), pos + 1))
            }
            Some(token) if closer(&token.kind).is_some() => {
                return Err(mismatched(source, token, opener, delimiter))
            }
            Some(_) => {
                let (key, mid) = parse_expr(tokens, pos, depth + 1, source)?;
                match tokens.get(mid) {
                    Some(token) if delimiter.matches(&token.kind) => {
                        return Err(syntax(
                            source,
                            "parse: map requires a value for its final key".into(),
                            token.span,
                            Some(("map opened here".into(), opener.span)),
                        ));
                    }
                    Some(token) if closer(&token.kind).is_some() => {
                        return Err(mismatched(source, token, opener, delimiter))
                    }
                    None => return Err(unclosed(source, opener, delimiter)),
                    Some(_) => {}
                }
                let (value, next) = parse_expr(tokens, mid, depth + 1, source)?;
                pairs.push((key, value));
                pos = next;
            }
            None => return Err(unclosed(source, opener, delimiter)),
        }
    }
}
fn unexpected_closer(source: &SourceSnapshot, token: &Token) -> MagError {
    let found = closer(&token.kind).unwrap_or('?');
    syntax(
        source,
        format!("parse: unexpected closing delimiter '{found}'"),
        token.span,
        None,
    )
}
fn mismatched(
    source: &SourceSnapshot,
    token: &Token,
    opener: &Token,
    expected: Delimiter,
) -> MagError {
    let found = closer(&token.kind).unwrap_or('?');
    syntax(
        source,
        format!(
            "parse: mismatched closing delimiter: expected '{}', found '{found}'",
            expected.close()
        ),
        token.span,
        Some((format!("'{}' opened here", expected.open()), opener.span)),
    )
}
fn unclosed(source: &SourceSnapshot, opener: &Token, delimiter: Delimiter) -> MagError {
    let eof = ByteSpan::new(source.text.len(), source.text.len());
    syntax(
        source,
        format!("parse: unclosed '{}'", delimiter.open()),
        eof,
        Some((format!("'{}' opened here", delimiter.open()), opener.span)),
    )
}
fn syntax(
    source: &SourceSnapshot,
    message: String,
    span: ByteSpan,
    related: Option<(String, ByteSpan)>,
) -> MagError {
    MagError::Syntax(SyntaxDiagnostic::new(
        "syntax_parse",
        "parse",
        message,
        source,
        span,
        related,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize_source;
    fn error(source_text: &str) -> SyntaxDiagnostic {
        let source = SourceSnapshot::named("test.mag", source_text);
        let tokens = tokenize_source(&source).unwrap();
        let MagError::Syntax(d) = parse_source(&tokens, &source).unwrap_err() else {
            panic!("syntax")
        };
        d
    }
    #[test]
    fn distinguishes_closer_failures() {
        let d = error(")");
        assert_eq!(d.span, ByteSpan::new(0, 1));
        assert!(d.message.contains("unexpected"));
        assert!(d.related.is_none());
        let d = error("(]\r\n");
        assert_eq!(d.location.start.line, 1);
        assert!(d.message.contains("expected ')'"));
        assert_eq!(d.related.unwrap().span, ByteSpan::new(0, 1));
    }
    #[test]
    fn eof_is_zero_width_and_opener_is_related() {
        let d = error(";;λ\r\n\t[");
        assert_eq!(d.span, ByteSpan::new(8, 8));
        assert_eq!(d.location.start.line, 2);
        assert_eq!(d.location.start.display_column, 6);
        assert_eq!(d.related.unwrap().span, ByteSpan::new(7, 8));
    }
}
