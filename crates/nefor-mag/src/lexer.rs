use crate::diagnostic::{ByteSpan, SourceSnapshot, SyntaxDiagnostic};
use crate::error::MagError;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Symbol(String),
    Keyword(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Arrow,
    Pipe,
    Plus,
    Colon,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: ByteSpan,
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, MagError> {
    tokenize_source(&SourceSnapshot::named("<input>", input))
}

pub fn tokenize_source(source: &SourceSnapshot) -> Result<Vec<Token>, MagError> {
    let input = source.text.as_ref();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let ch = input[pos..].chars().next().unwrap_or('\0');
        let width = ch.len_utf8();
        match ch {
            ' ' | '\t' | '\n' | '\r' | ',' => pos += width,
            ';' => {
                pos += width;
                while pos < input.len() && input.as_bytes()[pos] != b'\n' {
                    pos += 1;
                }
            }
            '(' => push(&mut tokens, TokenKind::LParen, pos, pos + 1, &mut pos),
            ')' => push(&mut tokens, TokenKind::RParen, pos, pos + 1, &mut pos),
            '[' => push(&mut tokens, TokenKind::LBracket, pos, pos + 1, &mut pos),
            ']' => push(&mut tokens, TokenKind::RBracket, pos, pos + 1, &mut pos),
            '{' => push(&mut tokens, TokenKind::LBrace, pos, pos + 1, &mut pos),
            '}' => push(&mut tokens, TokenKind::RBrace, pos, pos + 1, &mut pos),
            '|' => push(&mut tokens, TokenKind::Pipe, pos, pos + 1, &mut pos),
            '+' => push(&mut tokens, TokenKind::Plus, pos, pos + 1, &mut pos),
            ':' => {
                let start = pos;
                pos += 1;
                if next_char(input, pos).is_some_and(is_symbol_start) {
                    let value = read_symbol(input, &mut pos);
                    tokens.push(Token {
                        kind: TokenKind::Keyword(value),
                        span: ByteSpan::new(start, pos),
                    });
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Colon,
                        span: ByteSpan::new(start, pos),
                    });
                }
            }
            '-' => {
                let start = pos;
                pos += 1;
                if next_char(input, pos) == Some('>') {
                    pos += 1;
                    tokens.push(Token {
                        kind: TokenKind::Arrow,
                        span: ByteSpan::new(start, pos),
                    });
                } else if next_char(input, pos).is_some_and(|c| c.is_ascii_digit()) {
                    let kind = read_number(input, &mut pos, true);
                    tokens.push(Token {
                        kind,
                        span: ByteSpan::new(start, pos),
                    });
                } else {
                    let rest = read_symbol(input, &mut pos);
                    tokens.push(Token {
                        kind: TokenKind::Symbol(format!("-{rest}")),
                        span: ByteSpan::new(start, pos),
                    });
                }
            }
            '"' => {
                let start = pos;
                pos += 1;
                let value = read_string(input, &mut pos, start, source)?;
                tokens.push(Token {
                    kind: TokenKind::Str(value),
                    span: ByteSpan::new(start, pos),
                });
            }
            c if c.is_ascii_digit() => {
                let start = pos;
                let kind = read_number(input, &mut pos, false);
                tokens.push(Token {
                    kind,
                    span: ByteSpan::new(start, pos),
                });
            }
            c if is_symbol_start(c) => {
                let start = pos;
                let symbol = read_symbol(input, &mut pos);
                let kind = match symbol.as_str() {
                    "true" => TokenKind::Bool(true),
                    "false" => TokenKind::Bool(false),
                    "nil" => TokenKind::Nil,
                    _ => TokenKind::Symbol(symbol),
                };
                tokens.push(Token {
                    kind,
                    span: ByteSpan::new(start, pos),
                });
            }
            other => {
                let message = format!("unexpected character: '{other}'");
                return Err(MagError::Syntax(SyntaxDiagnostic::new(
                    "syntax_lex",
                    "lex",
                    format!("lexer: {message}"),
                    source,
                    ByteSpan::new(pos, pos + width),
                    None,
                )));
            }
        }
    }
    Ok(tokens)
}

fn push(tokens: &mut Vec<Token>, kind: TokenKind, start: usize, end: usize, pos: &mut usize) {
    tokens.push(Token {
        kind,
        span: ByteSpan::new(start, end),
    });
    *pos = end;
}
fn next_char(input: &str, pos: usize) -> Option<char> {
    input.get(pos..)?.chars().next()
}
fn is_symbol_start(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '_' | '?' | '!' | '*' | '=' | '<' | '>')
}
fn is_symbol_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '-' | '_' | '/' | '.' | '?' | '!' | '*' | '=' | '<' | '>')
}
fn read_symbol(input: &str, pos: &mut usize) -> String {
    let start = *pos;
    while let Some(c) = next_char(input, *pos) {
        if !is_symbol_char(c) {
            break;
        }
        *pos += c.len_utf8();
    }
    input[start..*pos].to_owned()
}
fn read_number(input: &str, pos: &mut usize, negative: bool) -> TokenKind {
    let mut value = if negative {
        "-".to_owned()
    } else {
        String::new()
    };
    let mut float = false;
    while let Some(c) = next_char(input, *pos) {
        if c.is_ascii_digit() {
            value.push(c);
            *pos += 1;
        } else if c == '.' && !float {
            float = true;
            value.push(c);
            *pos += 1;
        } else {
            break;
        }
    }
    if float {
        TokenKind::Float(value.parse().unwrap_or(0.0))
    } else {
        TokenKind::Int(value.parse().unwrap_or(0))
    }
}
fn read_string(
    input: &str,
    pos: &mut usize,
    opener: usize,
    source: &SourceSnapshot,
) -> Result<String, MagError> {
    let mut value = String::new();
    loop {
        let Some(c) = next_char(input, *pos) else {
            let message = "unterminated string";
            return Err(MagError::Syntax(SyntaxDiagnostic::new(
                "syntax_lex",
                "lex",
                format!("lexer: {message}"),
                source,
                ByteSpan::new(input.len(), input.len()),
                Some((
                    "string opened here".into(),
                    ByteSpan::new(opener, opener + 1),
                )),
            )));
        };
        let at = *pos;
        *pos += c.len_utf8();
        match c {
            '"' => return Ok(value),
            '\\' => {
                let Some(escaped) = next_char(input, *pos) else {
                    return Err(MagError::Syntax(SyntaxDiagnostic::new(
                        "syntax_lex",
                        "lex",
                        "lexer: unterminated escape in string".into(),
                        source,
                        ByteSpan::new(input.len(), input.len()),
                        Some((
                            "string opened here".into(),
                            ByteSpan::new(opener, opener + 1),
                        )),
                    )));
                };
                let escape_at = *pos;
                *pos += escaped.len_utf8();
                match escaped {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    '\\' => value.push('\\'),
                    '"' => value.push('"'),
                    other => {
                        let message = format!(
                            "unknown string escape \\{other}; use \\\\ for a literal backslash"
                        );
                        return Err(MagError::Syntax(SyntaxDiagnostic::new(
                            "syntax_lex",
                            "lex",
                            format!("lexer: {message}"),
                            source,
                            ByteSpan::new(at, escape_at + escaped.len_utf8()),
                            Some((
                                "string opened here".into(),
                                ByteSpan::new(opener, opener + 1),
                            )),
                        )));
                    }
                }
            }
            other => value.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn kinds(input: &str) -> Vec<TokenKind> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }
    #[test]
    fn preserves_language_tokens_and_byte_spans() {
        assert_eq!(
            kinds(";; c\n(1, -2.5 :key true nil -> | + fs/read)"),
            vec![
                TokenKind::LParen,
                TokenKind::Int(1),
                TokenKind::Float(-2.5),
                TokenKind::Keyword("key".into()),
                TokenKind::Bool(true),
                TokenKind::Nil,
                TokenKind::Arrow,
                TokenKind::Pipe,
                TokenKind::Plus,
                TokenKind::Symbol("fs/read".into()),
                TokenKind::RParen
            ]
        );
        let tokens = tokenize("é (x)").unwrap_err();
        let MagError::Syntax(d) = tokens else {
            panic!("syntax")
        };
        assert_eq!(d.span, ByteSpan::new(0, 2));
        let tokens = tokenize(";; λ\n(x)").unwrap();
        assert_eq!(tokens[0].span, ByteSpan::new(6, 7));
    }
    #[test]
    fn string_failures_preserve_opener_and_exact_escape() {
        let MagError::Syntax(d) = tokenize("\"x\\q\"").unwrap_err() else {
            panic!("syntax")
        };
        assert_eq!(d.span, ByteSpan::new(2, 4));
        assert_eq!(d.related.unwrap().span, ByteSpan::new(0, 1));
        let MagError::Syntax(d) = tokenize("\"λ").unwrap_err() else {
            panic!("syntax")
        };
        assert_eq!(d.span, ByteSpan::new(3, 3));
        assert_eq!(d.source.as_ref(), "\"λ");
    }
}
