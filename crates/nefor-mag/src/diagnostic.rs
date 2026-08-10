use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub name: String,
    pub path: Option<String>,
    pub text: Arc<str>,
}

impl SourceSnapshot {
    pub fn named(name: impl Into<String>, text: &str) -> Self {
        Self {
            name: name.into(),
            path: None,
            text: Arc::from(text),
        }
    }

    pub fn file(path: &Path, text: &str) -> Self {
        let display = path.display().to_string();
        Self {
            name: display.clone(),
            path: Some(display),
            text: Arc::from(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Position {
    pub byte: usize,
    pub line: usize,
    pub column: usize,
    pub display_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocatedSpan {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedDiagnostic {
    pub message: String,
    pub span: ByteSpan,
    pub location: LocatedSpan,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyntaxDiagnostic {
    pub code: &'static str,
    pub stage: &'static str,
    pub message: String,
    pub source_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(serialize_with = "serialize_source")]
    pub source: Arc<str>,
    pub span: ByteSpan,
    pub location: LocatedSpan,
    pub excerpt: String,
    pub caret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related: Option<RelatedDiagnostic>,
}

impl SyntaxDiagnostic {
    pub fn new(
        code: &'static str,
        stage: &'static str,
        message: String,
        source: &SourceSnapshot,
        span: ByteSpan,
        related: Option<(String, ByteSpan)>,
    ) -> Self {
        let span = clamp_span(&source.text, span);
        let location = locate(&source.text, span);
        let (excerpt, caret) = render(&source.text, span, &location);
        let related = related.map(|(message, related_span)| {
            let related_span = clamp_span(&source.text, related_span);
            RelatedDiagnostic {
                message,
                span: related_span,
                location: locate(&source.text, related_span),
            }
        });
        Self {
            code,
            stage,
            message,
            source_name: source.name.clone(),
            path: source.path.clone(),
            source: source.text.clone(),
            span,
            location,
            excerpt,
            caret,
            related,
        }
    }
}

impl std::fmt::Display for SyntaxDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}\n --> {}:{}:{}\n  |\n{:>2} | {}\n  | {}",
            self.message,
            self.source_name,
            self.location.start.line,
            self.location.start.display_column,
            self.location.start.line,
            self.excerpt,
            self.caret
        )?;
        if let Some(related) = &self.related {
            write!(
                formatter,
                "\n  = {} at {}:{}:{}",
                related.message,
                self.source_name,
                related.location.start.line,
                related.location.start.display_column
            )?;
        }
        Ok(())
    }
}

fn serialize_source<S>(source: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(source)
}

fn clamp_span(source: &str, span: ByteSpan) -> ByteSpan {
    ByteSpan::new(
        span.start.min(source.len()),
        span.end.max(span.start).min(source.len()),
    )
}

fn position(source: &str, byte: usize) -> Position {
    let byte = byte.min(source.len());
    let before = &source[..byte];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let mut column = 1;
    let mut display_column = 1;
    for ch in source[line_start..byte].chars() {
        column += 1;
        display_column = if ch == '\t' {
            ((display_column - 1) / 4 + 1) * 4 + 1
        } else {
            display_column + 1
        };
    }
    Position {
        byte,
        line,
        column,
        display_column,
    }
}

fn locate(source: &str, span: ByteSpan) -> LocatedSpan {
    LocatedSpan {
        start: position(source, span.start),
        end: position(source, span.end),
    }
}

fn render(source: &str, span: ByteSpan, location: &LocatedSpan) -> (String, String) {
    let line_start = source[..span.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_end = source[span.start..]
        .find('\n')
        .map_or(source.len(), |index| span.start + index);
    let excerpt = source[line_start..line_end]
        .trim_end_matches('\r')
        .replace('\t', "    ");
    let width = if span.end == span.start || location.end.line != location.start.line {
        1
    } else {
        location
            .end
            .display_column
            .saturating_sub(location.start.display_column)
            .max(1)
    };
    let caret = format!(
        "{}{}",
        " ".repeat(location.start.display_column - 1),
        "^".repeat(width)
    );
    (excerpt, caret)
}
