use crate::authored;
use crate::diagnostic::SourceSnapshot;
use crate::error::MagError;
use crate::profile::CompileProfiler;

/// The source grammar selected for one MAG source snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyntaxMode {
    /// The nominal, expression-oriented MAG syntax.
    #[default]
    New,
    /// The explicit compatibility frontend for prefix Lisp MAG.
    Lisp,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SourceRole {
    Entry,
    Module,
}

/// A MAG source path is a file name followed by `.mag` or `.magl`. A path
/// whose last component is only the suffix (`.mag`, `dir/.magl`) ends in the
/// right letters but names no file, and says so.
pub(crate) fn syntax_for_path(path: &str) -> Result<SyntaxMode, MagError> {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let (stem, syntax) = match file_name.rsplit_once('.') {
        Some((stem, "mag")) => (stem, SyntaxMode::New),
        Some((stem, "magl")) => (stem, SyntaxMode::Lisp),
        _ => {
            return Err(MagError::Eval(format!(
                "MAG source path must end in .mag or .magl: {path}"
            )))
        }
    };
    if stem.is_empty() {
        return Err(MagError::Eval(format!(
            "MAG source path must name a file, like inspect.mag; this one has no name before the extension: {path}"
        )));
    }
    Ok(syntax)
}

pub(crate) fn compile_source(
    syntax: SyntaxMode,
    source: &SourceSnapshot,
    profiler: Option<&CompileProfiler>,
    role: SourceRole,
) -> Result<authored::Module, MagError> {
    let result = match syntax {
        SyntaxMode::New => crate::new_syntax::compile_source(source, profiler, role),
        SyntaxMode::Lisp => crate::lisp::compile_source(source, profiler, role),
    };
    result.map_err(|mut error| {
        if let MagError::Syntax(diagnostic) = &mut error {
            diagnostic.syntax = Some(
                match syntax {
                    SyntaxMode::New => "new",
                    SyntaxMode::Lisp => "lisp",
                }
                .into(),
            );
        }
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(path: &str) -> String {
        match syntax_for_path(path) {
            Err(MagError::Eval(message)) => message,
            other => panic!("{path} should be rejected, got {other:?}"),
        }
    }

    #[test]
    fn a_source_path_selects_its_syntax_by_suffix() {
        for path in [
            "inspect.mag",
            "checks/inspect.mag",
            "a.b.mag",
            ".hidden/inspect.mag",
        ] {
            assert_eq!(syntax_for_path(path).unwrap(), SyntaxMode::New, "{path}");
        }
        for path in ["legacy.magl", "checks/legacy.magl"] {
            assert_eq!(syntax_for_path(path).unwrap(), SyntaxMode::Lisp, "{path}");
        }
    }

    #[test]
    fn a_path_with_another_suffix_is_told_the_suffixes() {
        for path in ["notes.txt", "mag", "inspect.mag.txt", "inspect.mag/", ""] {
            assert_eq!(
                message(path),
                format!("MAG source path must end in .mag or .magl: {path}")
            );
        }
    }

    #[test]
    fn a_path_with_only_the_suffix_is_told_it_has_no_name() {
        for path in [".mag", ".magl", "dir/.mag", "dir/sub/.magl"] {
            assert_eq!(
                message(path),
                format!(
                    "MAG source path must name a file, like inspect.mag; this one has no name before the extension: {path}"
                )
            );
        }
    }
}
