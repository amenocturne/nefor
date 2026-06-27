use crate::error::MagError;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
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

pub fn tokenize(input: &str) -> Result<Vec<Token>, MagError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            // whitespace and commas
            ' ' | '\t' | '\n' | '\r' | ',' => {
                chars.next();
            }

            // comments: ;; to end of line
            ';' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }

            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '[' => {
                chars.next();
                tokens.push(Token::LBracket);
            }
            ']' => {
                chars.next();
                tokens.push(Token::RBracket);
            }
            '{' => {
                chars.next();
                tokens.push(Token::LBrace);
            }
            '}' => {
                chars.next();
                tokens.push(Token::RBrace);
            }

            // arrow or minus (negative number or symbol part)
            '-' => {
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::Arrow);
                } else if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    // negative number
                    let num = read_number(&mut chars, true);
                    tokens.push(num);
                } else {
                    // symbol starting with -
                    let rest = read_symbol_rest(&mut chars);
                    tokens.push(Token::Symbol(format!("-{}", rest)));
                }
            }

            '|' => {
                chars.next();
                tokens.push(Token::Pipe);
            }
            '+' => {
                chars.next();
                tokens.push(Token::Plus);
            }

            // colon: keyword or bare colon
            ':' => {
                chars.next();
                if chars.peek().is_some_and(|c| is_symbol_start(*c)) {
                    let name = read_symbol_chars(&mut chars);
                    tokens.push(Token::Keyword(name));
                } else {
                    tokens.push(Token::Colon);
                }
            }

            // strings
            '"' => {
                chars.next();
                let s = read_string(&mut chars)?;
                tokens.push(Token::Str(s));
            }

            // numbers
            c if c.is_ascii_digit() => {
                let num = read_number(&mut chars, false);
                tokens.push(num);
            }

            // symbols (including true, false, nil)
            c if is_symbol_start(c) => {
                let sym = read_symbol_chars(&mut chars);
                match sym.as_str() {
                    "true" => tokens.push(Token::Bool(true)),
                    "false" => tokens.push(Token::Bool(false)),
                    "nil" => tokens.push(Token::Nil),
                    _ => tokens.push(Token::Symbol(sym)),
                }
            }

            other => {
                return Err(MagError::Lex(format!("unexpected character: '{}'", other)));
            }
        }
    }

    Ok(tokens)
}

fn is_symbol_start(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '_' | '?' | '!' | '*' | '=' | '<' | '>')
}

fn is_symbol_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '?' | '!' | '*' | '=' | '<' | '>')
}

fn read_symbol_chars(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = chars.peek() {
        if is_symbol_char(c) {
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }
    s
}

/// Read the rest of a symbol after a leading `-` was already consumed.
fn read_symbol_rest(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    read_symbol_chars(chars)
}

fn read_number(chars: &mut std::iter::Peekable<std::str::Chars>, negative: bool) -> Token {
    let mut s = String::new();
    if negative {
        s.push('-');
    }
    let mut is_float = false;

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            chars.next();
        } else if c == '.' && !is_float {
            is_float = true;
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }

    if is_float {
        Token::Float(s.parse().unwrap_or(0.0))
    } else {
        Token::Int(s.parse().unwrap_or(0))
    }
}

fn read_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, MagError> {
    let mut s = String::new();
    loop {
        match chars.next() {
            Some('"') => return Ok(s),
            Some('\\') => match chars.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('\\') => s.push('\\'),
                Some('"') => s.push('"'),
                Some(c) => s.push(c),
                None => return Err(MagError::Lex("unterminated escape in string".into())),
            },
            Some(c) => s.push(c),
            None => return Err(MagError::Lex("unterminated string".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parens_brackets_braces() {
        let tokens = tokenize("( ) [ ] { }").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::LParen,
                Token::RParen,
                Token::LBracket,
                Token::RBracket,
                Token::LBrace,
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn arrow_pipe_plus() {
        let tokens = tokenize("-> | +").unwrap();
        assert_eq!(tokens, vec![Token::Arrow, Token::Pipe, Token::Plus]);
    }

    #[test]
    fn string_literals() {
        let tokens = tokenize(r#""hello world""#).unwrap();
        assert_eq!(tokens, vec![Token::Str("hello world".into())]);
    }

    #[test]
    fn string_with_escapes() {
        let tokens = tokenize(r#""line\none\ttwo\\end\"""#).unwrap();
        assert_eq!(tokens, vec![Token::Str("line\none\ttwo\\end\"".into())]);
    }

    #[test]
    fn integers() {
        let tokens = tokenize("42 -7 0").unwrap();
        assert_eq!(tokens, vec![Token::Int(42), Token::Int(-7), Token::Int(0)]);
    }

    #[test]
    fn floats() {
        let tokens = tokenize("1.25 -0.5").unwrap();
        assert_eq!(tokens, vec![Token::Float(1.25), Token::Float(-0.5)]);
    }

    #[test]
    fn booleans_and_nil() {
        let tokens = tokenize("true false nil").unwrap();
        assert_eq!(
            tokens,
            vec![Token::Bool(true), Token::Bool(false), Token::Nil]
        );
    }

    #[test]
    fn keywords() {
        let tokens = tokenize(":keyword :another-key").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Keyword("keyword".into()),
                Token::Keyword("another-key".into()),
            ]
        );
    }

    #[test]
    fn symbols() {
        let tokens = tokenize("def fn my-var fs/read").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Symbol("def".into()),
                Token::Symbol("fn".into()),
                Token::Symbol("my-var".into()),
                Token::Symbol("fs/read".into()),
            ]
        );
    }

    #[test]
    fn comments_skipped() {
        let tokens = tokenize(";; this is a comment\n42").unwrap();
        assert_eq!(tokens, vec![Token::Int(42)]);
    }

    #[test]
    fn commas_as_whitespace() {
        let tokens = tokenize("1, 2, 3").unwrap();
        assert_eq!(tokens, vec![Token::Int(1), Token::Int(2), Token::Int(3)]);
    }

    #[test]
    fn bare_colon() {
        let tokens = tokenize("x : Int").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Symbol("x".into()),
                Token::Colon,
                Token::Symbol("Int".into()),
            ]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(tokenize(r#""no end"#).is_err());
    }
}
