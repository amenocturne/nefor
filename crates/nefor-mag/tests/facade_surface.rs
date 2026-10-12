//! The `nefor` facade is the one Nefor module shown to workflow authors. Its
//! names are aliases, so each carries a comment restating the declaration it
//! points at. This test keeps those comments equal to the real declarations.

use std::path::PathBuf;

const OPAQUE: &[&str] = &["Node", "DynamicList"];

fn lib() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mag/lib")
}

/// Drops module qualifiers (`nefor.graph.Node` -> `Node`) and all whitespace.
fn normalized(text: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(word.rsplit('.').next().unwrap_or(""));
        word.clear();
    };
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' || character == '.' {
            word.push(character);
        } else {
            flush(&mut word, &mut out);
            if !character.is_whitespace() {
                out.push(character);
            }
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Everything after the declared name: generics and signature, or type body.
fn shape<'a>(declaration: &'a str, keyword: &str, name: &str) -> Option<&'a str> {
    let rest = declaration.strip_prefix(keyword)?.strip_prefix(' ')?;
    let bare = name.trim_matches('`');
    [name.to_owned(), format!("({bare})"), bare.to_owned()]
        .iter()
        .find_map(|spelling| rest.strip_prefix(spelling.as_str()))
        .filter(|rest| rest.starts_with(['<', ':', ' ', '{']))
}

fn declared(module: &str, name: &str) -> (String, String) {
    let path = lib().join(format!("{}.mag", module.replace('.', "/")));
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    for line in source.lines() {
        for keyword in ["type", "newtype"] {
            if let Some(rest) = shape(line, keyword, name) {
                return (keyword.to_owned(), rest.to_owned());
            }
        }
        if let Some(rest) = shape(line, "let", name) {
            let signature = if rest.starts_with([':', '<']) {
                rest.split(" = ").next().unwrap_or(rest)
            } else {
                ""
            };
            return ("let".to_owned(), signature.to_owned());
        }
    }
    panic!("{module} declares no {name}");
}

#[test]
fn facade_comments_restate_the_real_declarations() {
    let facade = std::fs::read_to_string(lib().join("nefor.mag")).expect("read facade");
    let lines: Vec<&str> = facade.lines().collect();
    let mut checked = 0;
    for (index, line) in lines.iter().enumerate() {
        let (keyword, rest) = match line.split_once(' ') {
            Some((keyword @ ("let" | "type"), rest)) => (keyword, rest),
            _ => continue,
        };
        let (left, target) = rest.split_once(" = ").expect("facade names are aliases");
        let operator = left.starts_with('`');
        let name = if operator {
            left
        } else {
            left.split('<').next().unwrap()
        };
        let target = if operator {
            target
        } else {
            target.split('<').next().unwrap()
        };
        let (module, target_name) = match target.find('`') {
            Some(at) => (&target[..at - 1], &target[at..]),
            None => target.rsplit_once('.').expect("qualified alias target"),
        };
        let (real_keyword, real_shape) = declared(module, target_name);
        let restated = index
            .checked_sub(1)
            .and_then(|above| lines[above].strip_prefix("// "))
            .and_then(|comment| {
                ["let", "type", "newtype"]
                    .iter()
                    .find_map(|k| shape(comment, k, name).map(|rest| (*k, rest)))
            });
        if OPAQUE.contains(&name) || (keyword == "let" && real_shape.is_empty()) {
            assert!(restated.is_none(), "{name} has no declaration to restate");
            continue;
        }
        let (stated_keyword, stated_shape) =
            restated.unwrap_or_else(|| panic!("{name} needs its declaration restated above it"));
        assert_eq!(stated_keyword, real_keyword, "{name} declaration kind");
        assert_eq!(
            normalized(stated_shape),
            normalized(&real_shape),
            "{name} restates {module}.{target_name}"
        );
        checked += 1;
    }
    assert!(checked > 60, "facade declarations were checked: {checked}");
}
