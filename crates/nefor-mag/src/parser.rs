use crate::ast::Expr;
use crate::error::MagError;
use crate::lexer::Token;

pub fn parse(tokens: &[Token]) -> Result<Vec<Expr>, MagError> {
    let mut pos = 0;
    let mut exprs = Vec::new();
    while pos < tokens.len() {
        let (expr, next) = parse_expr(tokens, pos)?;
        exprs.push(expr);
        pos = next;
    }
    Ok(exprs)
}

fn parse_expr(tokens: &[Token], pos: usize) -> Result<(Expr, usize), MagError> {
    let token = tokens
        .get(pos)
        .ok_or_else(|| MagError::Parse("unexpected end of input".into()))?;

    match token {
        Token::LParen => parse_list(tokens, pos + 1),
        Token::LBracket => parse_vector(tokens, pos + 1),
        Token::LBrace => parse_map(tokens, pos + 1),

        Token::RParen | Token::RBracket | Token::RBrace => Err(MagError::Parse(format!(
            "unexpected closing delimiter at position {}",
            pos
        ))),

        // operator tokens become symbols
        Token::Arrow => Ok((Expr::Symbol("->".into()), pos + 1)),
        Token::Pipe => Ok((Expr::Symbol("|".into()), pos + 1)),
        Token::Plus => Ok((Expr::Symbol("+".into()), pos + 1)),
        Token::Colon => Ok((Expr::Symbol(":".into()), pos + 1)),

        // atoms
        Token::Symbol(s) => Ok((Expr::Symbol(s.clone()), pos + 1)),
        Token::Keyword(k) => Ok((Expr::Keyword(k.clone()), pos + 1)),
        Token::Str(s) => Ok((Expr::Str(s.clone()), pos + 1)),
        Token::Int(n) => Ok((Expr::Int(*n), pos + 1)),
        Token::Float(f) => Ok((Expr::Float(*f), pos + 1)),
        Token::Bool(b) => Ok((Expr::Bool(*b), pos + 1)),
        Token::Nil => Ok((Expr::Nil, pos + 1)),
    }
}

fn parse_list(tokens: &[Token], start: usize) -> Result<(Expr, usize), MagError> {
    let mut items = Vec::new();
    let mut pos = start;
    loop {
        match tokens.get(pos) {
            Some(Token::RParen) => return Ok((Expr::List(items), pos + 1)),
            Some(_) => {
                let (expr, next) = parse_expr(tokens, pos)?;
                items.push(expr);
                pos = next;
            }
            None => return Err(MagError::Parse("unclosed '('".into())),
        }
    }
}

fn parse_vector(tokens: &[Token], start: usize) -> Result<(Expr, usize), MagError> {
    let mut items = Vec::new();
    let mut pos = start;
    loop {
        match tokens.get(pos) {
            Some(Token::RBracket) => return Ok((Expr::Vector(items), pos + 1)),
            Some(_) => {
                let (expr, next) = parse_expr(tokens, pos)?;
                items.push(expr);
                pos = next;
            }
            None => return Err(MagError::Parse("unclosed '['".into())),
        }
    }
}

fn parse_map(tokens: &[Token], start: usize) -> Result<(Expr, usize), MagError> {
    let mut pairs = Vec::new();
    let mut pos = start;
    loop {
        match tokens.get(pos) {
            Some(Token::RBrace) => return Ok((Expr::Map(pairs), pos + 1)),
            Some(_) => {
                let (key, mid) = parse_expr(tokens, pos)?;
                let (val, next) = parse_expr(tokens, mid)?;
                pairs.push((key, val));
                pos = next;
            }
            None => return Err(MagError::Parse("unclosed '{'".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_str(input: &str) -> Vec<Expr> {
        let tokens = tokenize(input).unwrap();
        parse(&tokens).unwrap()
    }

    #[test]
    fn simple_list() {
        let exprs = parse_str("(def x 42)");
        assert_eq!(
            exprs,
            vec![Expr::List(vec![
                Expr::Symbol("def".into()),
                Expr::Symbol("x".into()),
                Expr::Int(42),
            ])]
        );
    }

    #[test]
    fn nested_lists() {
        let exprs = parse_str("(let [x 1] (f x))");
        assert_eq!(
            exprs,
            vec![Expr::List(vec![
                Expr::Symbol("let".into()),
                Expr::Vector(vec![Expr::Symbol("x".into()), Expr::Int(1)]),
                Expr::List(vec![Expr::Symbol("f".into()), Expr::Symbol("x".into())]),
            ])]
        );
    }

    #[test]
    fn vector() {
        let exprs = parse_str("[1 2 3]");
        assert_eq!(
            exprs,
            vec![Expr::Vector(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)])]
        );
    }

    #[test]
    fn map() {
        let exprs = parse_str(r#"{:key "val"}"#);
        assert_eq!(
            exprs,
            vec![Expr::Map(vec![(
                Expr::Keyword("key".into()),
                Expr::Str("val".into()),
            )])]
        );
    }

    #[test]
    fn arrow_in_context() {
        let exprs = parse_str("a -> b");
        assert_eq!(
            exprs,
            vec![
                Expr::Symbol("a".into()),
                Expr::Symbol("->".into()),
                Expr::Symbol("b".into()),
            ]
        );
    }

    #[test]
    fn empty_parens() {
        let exprs = parse_str("()");
        assert_eq!(exprs, vec![Expr::List(vec![])]);
    }

    #[test]
    fn unclosed_paren_errors() {
        let tokens = tokenize("(def x").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn unexpected_closing_errors() {
        let tokens = tokenize(")").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn map_with_multiple_pairs() {
        let exprs = parse_str(r#"{:a 1 :b 2}"#);
        assert_eq!(
            exprs,
            vec![Expr::Map(vec![
                (Expr::Keyword("a".into()), Expr::Int(1)),
                (Expr::Keyword("b".into()), Expr::Int(2)),
            ])]
        );
    }

    #[test]
    fn booleans_and_nil_parse() {
        let exprs = parse_str("(if true nil false)");
        assert_eq!(
            exprs,
            vec![Expr::List(vec![
                Expr::Symbol("if".into()),
                Expr::Bool(true),
                Expr::Nil,
                Expr::Bool(false),
            ])]
        );
    }

    #[test]
    fn pipe_and_plus_as_symbols() {
        let exprs = parse_str("(| A B)");
        assert_eq!(
            exprs,
            vec![Expr::List(vec![
                Expr::Symbol("|".into()),
                Expr::Symbol("A".into()),
                Expr::Symbol("B".into()),
            ])]
        );
    }
}
