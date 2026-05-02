//! Widget descriptions — the pure data Lua produces from `tui.text /
//! tui.column / tui.padding`. Descriptions are converted from Lua tables
//! once per render; the reconciler diffs descriptions against the prior
//! instance tree to decide create / reuse / drop.

use mlua::{Table, Value};

use crate::error::TuiError;

/// Sentinel field every primitive's table carries. `desc::from_lua_table`
/// dispatches on its value.
pub const KIND_FIELD: &str = "_tui_kind";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetDescription {
    Text {
        content: String,
        style: Option<Style>,
        wrap: WrapMode,
        key: Option<String>,
    },
    /// Inline styled runs — a single logical text block with multiple
    /// per-segment styles. Wrapping operates on the concatenated logical
    /// text; span boundaries do not force line breaks.
    Spans {
        spans: Vec<Span>,
        wrap: WrapMode,
        key: Option<String>,
    },
    /// Markdown source rendered through `pulldown-cmark`. The widget
    /// walks the parser's events, emits a flat list of styled spans
    /// (with internal newlines between blocks), and wraps the result.
    /// `theme = None` (or any missing entry) renders that element as
    /// neutral plain text. **No bundled defaults.**
    Markdown {
        source: String,
        theme: Option<MarkdownTheme>,
        wrap: WrapMode,
        key: Option<String>,
    },
    Column {
        children: Vec<WidgetDescription>,
        gap: u16,
        key: Option<String>,
    },
    Row {
        children: Vec<WidgetDescription>,
        gap: u16,
        key: Option<String>,
    },
    Padding {
        top: u16,
        right: u16,
        bottom: u16,
        left: u16,
        child: Box<WidgetDescription>,
        key: Option<String>,
    },
    Stack {
        children: Vec<WidgetDescription>,
        key: Option<String>,
    },
    Expanded {
        flex: u16,
        child: Box<WidgetDescription>,
        key: Option<String>,
    },
    Spacer {
        flex: u16,
        key: Option<String>,
    },
    Constrained {
        min_width: Option<u16>,
        max_width: Option<u16>,
        min_height: Option<u16>,
        max_height: Option<u16>,
        child: Box<WidgetDescription>,
        key: Option<String>,
    },
    Align {
        alignment: Alignment,
        child: Box<WidgetDescription>,
        key: Option<String>,
    },
    Anchored {
        anchor: Anchor,
        offset_x: i16,
        offset_y: i16,
        width: Dimension,
        height: Dimension,
        child: Box<WidgetDescription>,
        key: Option<String>,
    },
    TextInput {
        /// User key — required for text_input so Lua can reference it
        /// across re-renders. Stored as `Option<String>` to keep the
        /// generic key-handling helpers untouched; semantically
        /// `text_input` always has one.
        key: Option<String>,
        /// Controlled-component value. Lua holds the source of truth and
        /// passes it back each render; the engine compares against the
        /// instance's stored `last_value` to decide whether to reset
        /// internal cursor state.
        value: String,
        /// Lua-controlled focus prop. When `true` the input router
        /// absorbs editing keys. Multiple focused text_inputs in a tree
        /// are user error: first-by-tree-order wins; the rest emit a
        /// `tracing::warn!` once per render.
        focused: bool,
        /// Stable msg-kind identifier (constraint #1: callbacks are
        /// strings, never function refs). Fired with `value = <new>`.
        on_change: Option<String>,
        /// Stable msg-kind identifier. Fired by Enter (no Shift) with
        /// `value = <current>`. Does not modify the value itself.
        on_submit: Option<String>,
        /// Lower bound on visible rows. Width comes from parent
        /// constraints.
        min_lines: u16,
        /// Upper bound on visible rows. When `min_lines == max_lines`
        /// the input is fixed-size; else it grows up to `max_lines` and
        /// then scrolls vertically.
        max_lines: u16,
        /// Optional placeholder text painted when `value` is empty.
        /// `None` = no placeholder (and no opinion about fade colour).
        placeholder: Option<String>,
        /// Engine-level cursor blink hint. Phase 4 does not implement a
        /// blinker (no internal clock); the field is parsed and stored
        /// for forward compatibility with the animation primitive.
        cursor_blink: bool,
        /// Style record. `None` = neutral terminal fg/bg, no attrs.
        style: Option<TextInputStyle>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Anchor positions for `tui.anchored`. Mirrors [`Alignment`] but stays a
/// distinct type so future divergence (e.g. anchor-only "follow-cursor"
/// variants) doesn't leak into the alignment switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Width / height value for `tui.anchored`. `Intrinsic` lays the child out
/// against the parent's loose bounds and uses its measured size; `Cells`
/// pins to an absolute cell count; `Percent` resolves against the parent's
/// max on that axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Intrinsic,
    Cells(u16),
    Percent(u8),
}

impl WidgetDescription {
    /// Static type tag used as half of the reconciler's match key.
    pub fn type_tag(&self) -> &'static str {
        match self {
            WidgetDescription::Text { .. } => "text",
            WidgetDescription::Spans { .. } => "spans",
            WidgetDescription::Markdown { .. } => "markdown",
            WidgetDescription::Column { .. } => "column",
            WidgetDescription::Row { .. } => "row",
            WidgetDescription::Padding { .. } => "padding",
            WidgetDescription::Stack { .. } => "stack",
            WidgetDescription::Expanded { .. } => "expanded",
            WidgetDescription::Spacer { .. } => "spacer",
            WidgetDescription::Constrained { .. } => "constrained",
            WidgetDescription::Align { .. } => "align",
            WidgetDescription::Anchored { .. } => "anchored",
            WidgetDescription::TextInput { .. } => "text_input",
        }
    }

    /// User-supplied `key` field, if any.
    pub fn user_key(&self) -> Option<&str> {
        match self {
            WidgetDescription::Text { key, .. }
            | WidgetDescription::Spans { key, .. }
            | WidgetDescription::Markdown { key, .. }
            | WidgetDescription::Column { key, .. }
            | WidgetDescription::Row { key, .. }
            | WidgetDescription::Padding { key, .. }
            | WidgetDescription::Stack { key, .. }
            | WidgetDescription::Expanded { key, .. }
            | WidgetDescription::Spacer { key, .. }
            | WidgetDescription::Constrained { key, .. }
            | WidgetDescription::Align { key, .. }
            | WidgetDescription::Anchored { key, .. }
            | WidgetDescription::TextInput { key, .. } => key.as_deref(),
        }
    }
}

/// Style record for `tui.text_input`. Mirrors the spec's
/// `{ fg, bg, cursor, selection_bg, placeholder }` shape; each entry is
/// optional so Lua can override piecemeal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextInputStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub cursor: Option<Color>,
    pub selection_bg: Option<Color>,
    pub placeholder: Option<Color>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Reset,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    Word,
    Char,
    None,
}

/// One styled run inside a `tui.spans`. The markdown walker (phase 5b)
/// produces these too; future primitives that emit pre-styled inline
/// content can reuse the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// Per-element style overrides for `tui.markdown`. Each entry is `None`
/// when Lua omits it; the renderer falls back to neutral styling for
/// any missing entry. **No bundled defaults** — `theme = nil` renders
/// every element as plain text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkdownTheme {
    pub bold: Option<Style>,
    pub italic: Option<Style>,
    pub code: Option<Style>,
    pub code_block: Option<Style>,
    pub h1: Option<Style>,
    pub h2: Option<Style>,
    pub h3: Option<Style>,
    pub h4: Option<Style>,
    pub h5: Option<Style>,
    pub h6: Option<Style>,
    pub link: Option<Style>,
    pub blockquote: Option<Style>,
    pub list_marker: Option<Style>,
}

/// Convert a Lua table (output of `tui.text/column/padding`) into a
/// [`WidgetDescription`]. The conversion is recursive; each child of a
/// column is also dispatched through here.
pub fn from_lua_table(t: &Table) -> Result<WidgetDescription, TuiError> {
    let kind: String = match t.get::<Value>(KIND_FIELD)? {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Nil => {
            return Err(TuiError::InvalidDesc(format!(
                "missing `{KIND_FIELD}` field; was this table built via tui.text/column/padding?"
            )));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "`{KIND_FIELD}` must be a string (got {})",
                other.type_name()
            )));
        }
    };

    match kind.as_str() {
        "text" => parse_text(t),
        "spans" => parse_spans(t),
        "markdown" => parse_markdown(t),
        "column" => parse_column(t),
        "row" => parse_row(t),
        "padding" => parse_padding(t),
        "stack" => parse_stack(t),
        "expanded" => parse_expanded(t),
        "spacer" => parse_spacer(t),
        "constrained" => parse_constrained(t),
        "align" => parse_align(t),
        "anchored" => parse_anchored(t),
        "text_input" => parse_text_input(t),
        other => Err(TuiError::InvalidDesc(format!(
            "unknown widget kind `{other}`; expected one of: text, spans, markdown, column, row, padding, stack, expanded, spacer, constrained, align, anchored, text_input"
        ))),
    }
}

fn parse_text(t: &Table) -> Result<WidgetDescription, TuiError> {
    let content: String = match t.get::<Value>("content")? {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.text: `content` is required (got nil)".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.text: `content` must be a string (got {})",
                other.type_name()
            )));
        }
    };
    let key = parse_key(t)?;
    let wrap = parse_wrap(t)?;
    let style = parse_style(t)?;
    Ok(WidgetDescription::Text {
        content,
        style,
        wrap,
        key,
    })
}

fn parse_spans(t: &Table) -> Result<WidgetDescription, TuiError> {
    let spans_val: Value = t.get("spans")?;
    let spans_tbl = match spans_val {
        Value::Table(arr) => arr,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.spans: `spans` is required (got nil)".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.spans: `spans` must be an array of span tables (got {})",
                other.type_name()
            )));
        }
    };
    let spans = parse_span_array(&spans_tbl, "tui.spans")?;
    let wrap = parse_wrap(t)?;
    let key = parse_key(t)?;
    Ok(WidgetDescription::Spans { spans, wrap, key })
}

/// Parse a Lua array of span tables `{ text=, fg=, bg=, bold=, italic=,
/// underline=, reverse= }`. Used by `tui.spans` and (in later phases)
/// `tui.animation` and the markdown walker.
pub(crate) fn parse_span_array(arr: &Table, ctx: &str) -> Result<Vec<Span>, TuiError> {
    let mut out = Vec::new();
    let len = arr.raw_len();
    for i in 1..=len {
        let v: Value = arr.get(i)?;
        let entry = match v {
            Value::Nil => continue,
            Value::Table(t) => t,
            other => {
                return Err(TuiError::InvalidDesc(format!(
                    "{ctx}: span #{i} must be a table (got {})",
                    other.type_name()
                )));
            }
        };
        out.push(parse_one_span(&entry, ctx, i)?);
    }
    Ok(out)
}

fn parse_one_span(t: &Table, ctx: &str, i: usize) -> Result<Span, TuiError> {
    let text: String = match t.get::<Value>("text")? {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Nil => {
            return Err(TuiError::InvalidDesc(format!(
                "{ctx}: span #{i} requires `text`"
            )));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "{ctx}: span #{i} `text` must be a string (got {})",
                other.type_name()
            )));
        }
    };
    let fg = parse_color(t, "fg")?;
    let bg = parse_color(t, "bg")?;
    let bold = parse_bool(t, "bold")?;
    let italic = parse_bool(t, "italic")?;
    let underline = parse_bool(t, "underline")?;
    let reverse = parse_bool(t, "reverse")?;
    Ok(Span {
        text,
        style: Style {
            fg,
            bg,
            bold,
            italic,
            underline,
            reverse,
        },
    })
}

fn parse_markdown(t: &Table) -> Result<WidgetDescription, TuiError> {
    let source: String = match t.get::<Value>("source")? {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.markdown: `source` is required (got nil)".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.markdown: `source` must be a string (got {})",
                other.type_name()
            )));
        }
    };
    let theme = parse_markdown_theme(t)?;
    let wrap = parse_wrap(t)?;
    let key = parse_key(t)?;
    Ok(WidgetDescription::Markdown {
        source,
        theme,
        wrap,
        key,
    })
}

fn parse_markdown_theme(t: &Table) -> Result<Option<MarkdownTheme>, TuiError> {
    match t.get::<Value>("theme")? {
        Value::Nil => Ok(None),
        Value::Table(theme_t) => {
            let bold = parse_theme_entry(&theme_t, "bold")?;
            let italic = parse_theme_entry(&theme_t, "italic")?;
            let code = parse_theme_entry(&theme_t, "code")?;
            let code_block = parse_theme_entry(&theme_t, "code_block")?;
            let h1 = parse_theme_entry(&theme_t, "h1")?;
            let h2 = parse_theme_entry(&theme_t, "h2")?;
            let h3 = parse_theme_entry(&theme_t, "h3")?;
            let h4 = parse_theme_entry(&theme_t, "h4")?;
            let h5 = parse_theme_entry(&theme_t, "h5")?;
            let h6 = parse_theme_entry(&theme_t, "h6")?;
            let link = parse_theme_entry(&theme_t, "link")?;
            let blockquote = parse_theme_entry(&theme_t, "blockquote")?;
            let list_marker = parse_theme_entry(&theme_t, "list_marker")?;
            Ok(Some(MarkdownTheme {
                bold,
                italic,
                code,
                code_block,
                h1,
                h2,
                h3,
                h4,
                h5,
                h6,
                link,
                blockquote,
                list_marker,
            }))
        }
        other => Err(TuiError::InvalidDesc(format!(
            "tui.markdown: `theme` must be a table or nil (got {})",
            other.type_name()
        ))),
    }
}

/// One theme entry table → `Style`. Same shape as `tui.text`'s `style`.
fn parse_theme_entry(t: &Table, key: &str) -> Result<Option<Style>, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Table(st) => {
            let fg = parse_color(&st, "fg")?;
            let bg = parse_color(&st, "bg")?;
            let bold = parse_bool(&st, "bold")?;
            let italic = parse_bool(&st, "italic")?;
            let underline = parse_bool(&st, "underline")?;
            let reverse = parse_bool(&st, "reverse")?;
            Ok(Some(Style {
                fg,
                bg,
                bold,
                italic,
                underline,
                reverse,
            }))
        }
        other => Err(TuiError::InvalidDesc(format!(
            "tui.markdown.theme: `{key}` must be a table or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_column(t: &Table) -> Result<WidgetDescription, TuiError> {
    let children_val: Value = t.get("children")?;
    let children = match children_val {
        Value::Table(arr) => parse_children(&arr)?,
        Value::Nil => Vec::new(),
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.column: `children` must be an array (got {})",
                other.type_name()
            )));
        }
    };
    let gap = parse_u16(t, "gap", 0, "tui.column")?;
    let key = parse_key(t)?;
    Ok(WidgetDescription::Column { children, gap, key })
}

fn parse_row(t: &Table) -> Result<WidgetDescription, TuiError> {
    let children_val: Value = t.get("children")?;
    let children = match children_val {
        Value::Table(arr) => parse_children(&arr)?,
        Value::Nil => Vec::new(),
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.row: `children` must be an array (got {})",
                other.type_name()
            )));
        }
    };
    let gap = parse_u16(t, "gap", 0, "tui.row")?;
    let key = parse_key(t)?;
    Ok(WidgetDescription::Row { children, gap, key })
}

fn parse_stack(t: &Table) -> Result<WidgetDescription, TuiError> {
    let children_val: Value = t.get("children")?;
    let children = match children_val {
        Value::Table(arr) => parse_children(&arr)?,
        Value::Nil => Vec::new(),
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.stack: `children` must be an array (got {})",
                other.type_name()
            )));
        }
    };
    let key = parse_key(t)?;
    Ok(WidgetDescription::Stack { children, key })
}

fn parse_expanded(t: &Table) -> Result<WidgetDescription, TuiError> {
    let flex = parse_u16(t, "flex", 1, "tui.expanded")?;
    let child_val: Value = t.get("child")?;
    let child_tbl = match child_val {
        Value::Table(t) => t,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.expanded: `child` is required".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.expanded: `child` must be a widget table (got {})",
                other.type_name()
            )));
        }
    };
    let child = Box::new(from_lua_table(&child_tbl)?);
    let key = parse_key(t)?;
    Ok(WidgetDescription::Expanded { flex, child, key })
}

fn parse_spacer(t: &Table) -> Result<WidgetDescription, TuiError> {
    let flex = parse_u16(t, "flex", 1, "tui.spacer")?;
    let key = parse_key(t)?;
    Ok(WidgetDescription::Spacer { flex, key })
}

fn parse_constrained(t: &Table) -> Result<WidgetDescription, TuiError> {
    let min_width = parse_optional_u16(t, "min_width", "tui.constrained")?;
    let max_width = parse_optional_u16(t, "max_width", "tui.constrained")?;
    let min_height = parse_optional_u16(t, "min_height", "tui.constrained")?;
    let max_height = parse_optional_u16(t, "max_height", "tui.constrained")?;
    let child_val: Value = t.get("child")?;
    let child_tbl = match child_val {
        Value::Table(t) => t,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.constrained: `child` is required".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.constrained: `child` must be a widget table (got {})",
                other.type_name()
            )));
        }
    };
    let child = Box::new(from_lua_table(&child_tbl)?);
    let key = parse_key(t)?;
    Ok(WidgetDescription::Constrained {
        min_width,
        max_width,
        min_height,
        max_height,
        child,
        key,
    })
}

fn parse_align(t: &Table) -> Result<WidgetDescription, TuiError> {
    let alignment = match t.get::<Value>("alignment")? {
        Value::Nil => Alignment::Center,
        Value::String(s) => parse_alignment_str(&s.to_str()?)?,
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.align: `alignment` must be a string (got {})",
                other.type_name()
            )));
        }
    };
    let child_val: Value = t.get("child")?;
    let child_tbl = match child_val {
        Value::Table(t) => t,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.align: `child` is required".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.align: `child` must be a widget table (got {})",
                other.type_name()
            )));
        }
    };
    let child = Box::new(from_lua_table(&child_tbl)?);
    let key = parse_key(t)?;
    Ok(WidgetDescription::Align {
        alignment,
        child,
        key,
    })
}

fn parse_anchored(t: &Table) -> Result<WidgetDescription, TuiError> {
    let anchor = match t.get::<Value>("anchor")? {
        Value::Nil => Anchor::Center,
        Value::String(s) => parse_anchor_str(&s.to_str()?)?,
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.anchored: `anchor` must be a string (got {})",
                other.type_name()
            )));
        }
    };
    let offset_x = parse_i16(t, "offset_x", 0, "tui.anchored")?;
    let offset_y = parse_i16(t, "offset_y", 0, "tui.anchored")?;
    let width = parse_dimension(t, "width")?;
    let height = parse_dimension(t, "height")?;
    let child_val: Value = t.get("child")?;
    let child_tbl = match child_val {
        Value::Table(t) => t,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.anchored: `child` is required".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.anchored: `child` must be a widget table (got {})",
                other.type_name()
            )));
        }
    };
    let child = Box::new(from_lua_table(&child_tbl)?);
    let key = parse_key(t)?;
    Ok(WidgetDescription::Anchored {
        anchor,
        offset_x,
        offset_y,
        width,
        height,
        child,
        key,
    })
}

fn parse_text_input(t: &Table) -> Result<WidgetDescription, TuiError> {
    let key = parse_key(t)?;
    let value: String = match t.get::<Value>("value")? {
        Value::Nil => String::new(),
        Value::String(s) => s.to_str()?.to_string(),
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.text_input: `value` must be a string or nil (got {})",
                other.type_name()
            )));
        }
    };
    let focused = parse_optional_bool(t, "focused", "tui.text_input")?.unwrap_or(false);
    let on_change = parse_optional_string(t, "on_change", "tui.text_input")?;
    let on_submit = parse_optional_string(t, "on_submit", "tui.text_input")?;
    let min_lines = parse_u16(t, "min_lines", 1, "tui.text_input")?;
    let max_lines = parse_u16(t, "max_lines", min_lines.max(1), "tui.text_input")?;
    if min_lines == 0 {
        return Err(TuiError::InvalidDesc(
            "tui.text_input: `min_lines` must be ≥ 1".into(),
        ));
    }
    if max_lines < min_lines {
        return Err(TuiError::InvalidDesc(format!(
            "tui.text_input: `max_lines` ({max_lines}) must be ≥ `min_lines` ({min_lines})"
        )));
    }
    let placeholder = parse_optional_string(t, "placeholder", "tui.text_input")?;
    let cursor_blink = parse_optional_bool(t, "cursor_blink", "tui.text_input")?.unwrap_or(false);
    let style = parse_text_input_style(t)?;
    Ok(WidgetDescription::TextInput {
        key,
        value,
        focused,
        on_change,
        on_submit,
        min_lines,
        max_lines,
        placeholder,
        cursor_blink,
        style,
    })
}

fn parse_optional_bool(t: &Table, key: &str, ctx: &str) -> Result<Option<bool>, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Boolean(b) => Ok(Some(b)),
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be a boolean (got {})",
            other.type_name()
        ))),
    }
}

fn parse_optional_string(t: &Table, key: &str, ctx: &str) -> Result<Option<String>, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be a string or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_text_input_style(t: &Table) -> Result<Option<TextInputStyle>, TuiError> {
    match t.get::<Value>("style")? {
        Value::Nil => Ok(None),
        Value::Table(st) => {
            let fg = parse_color(&st, "fg")?;
            let bg = parse_color(&st, "bg")?;
            let cursor = parse_color(&st, "cursor")?;
            let selection_bg = parse_color(&st, "selection_bg")?;
            let placeholder = parse_color(&st, "placeholder")?;
            Ok(Some(TextInputStyle {
                fg,
                bg,
                cursor,
                selection_bg,
                placeholder,
            }))
        }
        other => Err(TuiError::InvalidDesc(format!(
            "tui.text_input: `style` must be a table or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_anchor_str(s: &str) -> Result<Anchor, TuiError> {
    match s {
        "top-left" => Ok(Anchor::TopLeft),
        "top" => Ok(Anchor::Top),
        "top-right" => Ok(Anchor::TopRight),
        "left" => Ok(Anchor::Left),
        "center" => Ok(Anchor::Center),
        "right" => Ok(Anchor::Right),
        "bottom-left" => Ok(Anchor::BottomLeft),
        "bottom" => Ok(Anchor::Bottom),
        "bottom-right" => Ok(Anchor::BottomRight),
        other => Err(TuiError::InvalidDesc(format!(
            "tui.anchored: `anchor` must be one of top-left|top|top-right|left|center|right|bottom-left|bottom|bottom-right (got `{other}`)"
        ))),
    }
}

fn parse_dimension(t: &Table, key: &str) -> Result<Dimension, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(Dimension::Intrinsic),
        Value::Integer(n) => clamp_u16(n, &format!("tui.anchored.{key}")).map(Dimension::Cells),
        Value::Number(n) => clamp_u16_f(n, &format!("tui.anchored.{key}")).map(Dimension::Cells),
        Value::String(s) => parse_percent(&s.to_str()?, key).map(Dimension::Percent),
        other => Err(TuiError::InvalidDesc(format!(
            "tui.anchored: `{key}` must be nil, an integer, or a percent string like \"50%\" (got {})",
            other.type_name()
        ))),
    }
}

fn parse_percent(s: &str, key: &str) -> Result<u8, TuiError> {
    let trimmed = s.trim();
    let body = trimmed.strip_suffix('%').ok_or_else(|| {
        TuiError::InvalidDesc(format!(
            "tui.anchored: `{key}` string must end with `%` (got `{trimmed}`)"
        ))
    })?;
    let n: u32 = body.trim().parse().map_err(|_| {
        TuiError::InvalidDesc(format!(
            "tui.anchored: `{key}` must be `N%` where N is an integer (got `{trimmed}`)"
        ))
    })?;
    if n > 100 {
        return Err(TuiError::InvalidDesc(format!(
            "tui.anchored: `{key}` must be 0%..=100% (got `{trimmed}`)"
        )));
    }
    Ok(n as u8)
}

fn parse_i16(t: &Table, key: &str, default: i16, ctx: &str) -> Result<i16, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(n) => clamp_i16(n, &format!("{ctx}.{key}")),
        Value::Number(n) => clamp_i16_f(n, &format!("{ctx}.{key}")),
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be a number (got {})",
            other.type_name()
        ))),
    }
}

fn clamp_i16(n: i64, ctx: &str) -> Result<i16, TuiError> {
    if !(i16::MIN as i64..=i16::MAX as i64).contains(&n) {
        return Err(TuiError::InvalidDesc(format!(
            "{ctx}: must be in {}..={} (got {n})",
            i16::MIN,
            i16::MAX
        )));
    }
    Ok(n as i16)
}

fn clamp_i16_f(n: f64, ctx: &str) -> Result<i16, TuiError> {
    if !n.is_finite() || !(i16::MIN as f64..=i16::MAX as f64).contains(&n) {
        return Err(TuiError::InvalidDesc(format!(
            "{ctx}: must be in {}..={} (got {n})",
            i16::MIN,
            i16::MAX
        )));
    }
    Ok(n as i16)
}

fn parse_alignment_str(s: &str) -> Result<Alignment, TuiError> {
    match s {
        "top-left" => Ok(Alignment::TopLeft),
        "top" => Ok(Alignment::Top),
        "top-right" => Ok(Alignment::TopRight),
        "left" => Ok(Alignment::Left),
        "center" => Ok(Alignment::Center),
        "right" => Ok(Alignment::Right),
        "bottom-left" => Ok(Alignment::BottomLeft),
        "bottom" => Ok(Alignment::Bottom),
        "bottom-right" => Ok(Alignment::BottomRight),
        other => Err(TuiError::InvalidDesc(format!(
            "tui.align: `alignment` must be one of top-left|top|top-right|left|center|right|bottom-left|bottom|bottom-right (got `{other}`)"
        ))),
    }
}

fn parse_optional_u16(t: &Table, key: &str, ctx: &str) -> Result<Option<u16>, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Integer(n) => clamp_u16(n, &format!("{ctx}.{key}")).map(Some),
        Value::Number(n) => clamp_u16_f(n, &format!("{ctx}.{key}")).map(Some),
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be a number or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_padding(t: &Table) -> Result<WidgetDescription, TuiError> {
    let (top, right, bottom, left) = match t.get::<Value>("value")? {
        Value::Integer(n) => {
            let v = clamp_u16(n, "tui.padding.value")?;
            (v, v, v, v)
        }
        Value::Number(n) => {
            let v = clamp_u16_f(n, "tui.padding.value")?;
            (v, v, v, v)
        }
        Value::Table(t) => {
            let top = parse_u16(&t, "top", 0, "tui.padding.value")?;
            let right = parse_u16(&t, "right", 0, "tui.padding.value")?;
            let bottom = parse_u16(&t, "bottom", 0, "tui.padding.value")?;
            let left = parse_u16(&t, "left", 0, "tui.padding.value")?;
            (top, right, bottom, left)
        }
        Value::Nil => (0, 0, 0, 0),
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.padding: `value` must be a number or a table {{top,right,bottom,left}} (got {})",
                other.type_name()
            )));
        }
    };
    let child_val: Value = t.get("child")?;
    let child_tbl = match child_val {
        Value::Table(t) => t,
        Value::Nil => {
            return Err(TuiError::InvalidDesc(
                "tui.padding: `child` is required".into(),
            ));
        }
        other => {
            return Err(TuiError::InvalidDesc(format!(
                "tui.padding: `child` must be a widget table (got {})",
                other.type_name()
            )));
        }
    };
    let child = Box::new(from_lua_table(&child_tbl)?);
    let key = parse_key(t)?;
    Ok(WidgetDescription::Padding {
        top,
        right,
        bottom,
        left,
        child,
        key,
    })
}

fn parse_children(arr: &Table) -> Result<Vec<WidgetDescription>, TuiError> {
    let mut out = Vec::new();
    let len = arr.raw_len();
    for i in 1..=len {
        let v: Value = arr.get(i)?;
        match v {
            Value::Nil => continue,
            Value::Table(t) => out.push(from_lua_table(&t)?),
            other => {
                return Err(TuiError::InvalidDesc(format!(
                    "child #{i} must be a widget table or nil (got {})",
                    other.type_name()
                )));
            }
        }
    }
    Ok(out)
}

fn parse_key(t: &Table) -> Result<Option<String>, TuiError> {
    match t.get::<Value>("key")? {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        other => Err(TuiError::InvalidDesc(format!(
            "`key` must be a string or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_wrap(t: &Table) -> Result<WrapMode, TuiError> {
    match t.get::<Value>("wrap")? {
        Value::Nil => Ok(WrapMode::Word),
        Value::String(s) => match s.to_str()?.as_ref() {
            "word" => Ok(WrapMode::Word),
            "char" => Ok(WrapMode::Char),
            "none" => Ok(WrapMode::None),
            other => Err(TuiError::InvalidDesc(format!(
                "tui.text: `wrap` must be one of word|char|none (got `{other}`)"
            ))),
        },
        other => Err(TuiError::InvalidDesc(format!(
            "tui.text: `wrap` must be a string (got {})",
            other.type_name()
        ))),
    }
}

fn parse_style(t: &Table) -> Result<Option<Style>, TuiError> {
    match t.get::<Value>("style")? {
        Value::Nil => Ok(None),
        Value::Table(st) => {
            let fg = parse_color(&st, "fg")?;
            let bg = parse_color(&st, "bg")?;
            let bold = parse_bool(&st, "bold")?;
            let italic = parse_bool(&st, "italic")?;
            let underline = parse_bool(&st, "underline")?;
            let reverse = parse_bool(&st, "reverse")?;
            Ok(Some(Style {
                fg,
                bg,
                bold,
                italic,
                underline,
                reverse,
            }))
        }
        other => Err(TuiError::InvalidDesc(format!(
            "`style` must be a table or nil (got {})",
            other.type_name()
        ))),
    }
}

fn parse_color(t: &Table, key: &str) -> Result<Option<Color>, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(s) => {
            let raw = s.to_str()?;
            let val: &str = raw.as_ref();
            if val == "reset" {
                Ok(Some(Color::Reset))
            } else if let Some(hex) = val.strip_prefix('#') {
                parse_hex_rgb(hex, key).map(Some)
            } else {
                Err(TuiError::InvalidDesc(format!(
                    "color `{key}`: string must be \"reset\" or `#rrggbb` (got `{val}`)"
                )))
            }
        }
        Value::Integer(n) => {
            if !(0..=255).contains(&n) {
                return Err(TuiError::InvalidDesc(format!(
                    "color `{key}`: indexed must be 0..=255 (got {n})"
                )));
            }
            Ok(Some(Color::Indexed(n as u8)))
        }
        Value::Table(rgb) => {
            let r = parse_u8(&rgb, "r", "rgb color")?;
            let g = parse_u8(&rgb, "g", "rgb color")?;
            let b = parse_u8(&rgb, "b", "rgb color")?;
            Ok(Some(Color::Rgb(r, g, b)))
        }
        other => Err(TuiError::InvalidDesc(format!(
            "color `{key}` must be a string \"reset\" or `#rrggbb`, an integer 0..=255, or a table {{r,g,b}} (got {})",
            other.type_name()
        ))),
    }
}

fn parse_hex_rgb(hex: &str, key: &str) -> Result<Color, TuiError> {
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(TuiError::InvalidDesc(format!(
            "color `{key}`: hex must be `#rrggbb` (got `#{hex}`)"
        )));
    }
    let parse_pair = |i: usize| -> u8 {
        u8::from_str_radix(&hex[i..i + 2], 16).expect("validated as ascii hex above")
    };
    Ok(Color::Rgb(parse_pair(0), parse_pair(2), parse_pair(4)))
}

fn parse_bool(t: &Table, key: &str) -> Result<bool, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(false),
        Value::Boolean(b) => Ok(b),
        other => Err(TuiError::InvalidDesc(format!(
            "`{key}` must be a boolean (got {})",
            other.type_name()
        ))),
    }
}

fn parse_u16(t: &Table, key: &str, default: u16, ctx: &str) -> Result<u16, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(n) => clamp_u16(n, &format!("{ctx}.{key}")),
        Value::Number(n) => clamp_u16_f(n, &format!("{ctx}.{key}")),
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be a number (got {})",
            other.type_name()
        ))),
    }
}

fn parse_u8(t: &Table, key: &str, ctx: &str) -> Result<u8, TuiError> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(0),
        Value::Integer(n) => {
            if !(0..=255).contains(&n) {
                return Err(TuiError::InvalidDesc(format!(
                    "{ctx}: `{key}` must be 0..=255 (got {n})"
                )));
            }
            Ok(n as u8)
        }
        other => Err(TuiError::InvalidDesc(format!(
            "{ctx}: `{key}` must be an integer 0..=255 (got {})",
            other.type_name()
        ))),
    }
}

fn clamp_u16(n: i64, ctx: &str) -> Result<u16, TuiError> {
    if !(0..=65535).contains(&n) {
        return Err(TuiError::InvalidDesc(format!(
            "{ctx}: must be in 0..=65535 (got {n})"
        )));
    }
    Ok(n as u16)
}

fn clamp_u16_f(n: f64, ctx: &str) -> Result<u16, TuiError> {
    if !n.is_finite() || !(0.0..=65535.0).contains(&n) {
        return Err(TuiError::InvalidDesc(format!(
            "{ctx}: must be in 0..=65535 (got {n})"
        )));
    }
    Ok(n as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn lua() -> Lua {
        Lua::new()
    }

    fn eval_table(lua: &Lua, src: &str) -> Table {
        lua.load(src).eval().expect("eval table")
    }

    #[test]
    fn text_table_parses_minimal() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "text", content = "hello" }"#);
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Text {
                content,
                style,
                wrap,
                key,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(style, None);
                assert!(matches!(wrap, WrapMode::Word));
                assert_eq!(key, None);
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn text_requires_content() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "text" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("`content` is required"));
    }

    #[test]
    fn text_wrap_modes_parse() {
        let l = lua();
        for (s, expected) in [
            ("word", WrapMode::Word),
            ("char", WrapMode::Char),
            ("none", WrapMode::None),
        ] {
            let src = format!(r#"return {{ _tui_kind = "text", content = "x", wrap = "{s}" }}"#);
            let t = eval_table(&l, &src);
            let d = from_lua_table(&t).expect("parse");
            match d {
                WidgetDescription::Text { wrap, .. } => assert_eq!(wrap, expected),
                _ => panic!("expected text"),
            }
        }
    }

    #[test]
    fn column_skips_nil_children() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "column",
              children = {
                { _tui_kind = "text", content = "a" },
                nil,
                { _tui_kind = "text", content = "c" },
              },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Column { children, .. } => {
                // Lua arrays stop at first nil for `#t`, so length-based
                // iteration yields only the prefix run before the nil.
                // The semantics we promise: holes / nils are skipped.
                assert!(!children.is_empty());
                assert!(
                    matches!(&children[0], WidgetDescription::Text { content, .. } if content == "a")
                );
            }
            _ => panic!("expected column"),
        }
    }

    #[test]
    fn padding_value_shorthand_expands() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "padding",
              value = 2,
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Padding {
                top,
                right,
                bottom,
                left,
                ..
            } => {
                assert_eq!((top, right, bottom, left), (2, 2, 2, 2));
            }
            _ => panic!("expected padding"),
        }
    }

    #[test]
    fn padding_value_table_explicit() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "padding",
              value = { top = 1, right = 2, bottom = 3, left = 4 },
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Padding {
                top,
                right,
                bottom,
                left,
                ..
            } => {
                assert_eq!((top, right, bottom, left), (1, 2, 3, 4));
            }
            _ => panic!("expected padding"),
        }
    }

    #[test]
    fn padding_requires_child() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "padding", value = 1 }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("`child` is required"));
    }

    #[test]
    fn unknown_kind_errors() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "marquee" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("unknown widget kind"));
    }

    #[test]
    fn row_table_parses() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "row",
              gap = 2,
              children = {
                { _tui_kind = "text", content = "a" },
                { _tui_kind = "text", content = "b" },
              },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Row { children, gap, .. } => {
                assert_eq!(gap, 2);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected row"),
        }
    }

    #[test]
    fn expanded_table_parses_with_default_flex() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "expanded",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Expanded { flex, .. } => assert_eq!(flex, 1),
            _ => panic!("expected expanded"),
        }
    }

    #[test]
    fn expanded_table_parses_explicit_flex() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "expanded",
              flex = 3,
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Expanded { flex, .. } => assert_eq!(flex, 3),
            _ => panic!("expected expanded"),
        }
    }

    #[test]
    fn expanded_requires_child() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "expanded" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("`child` is required"));
    }

    #[test]
    fn spacer_table_parses() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "spacer" }"#);
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Spacer { flex, .. } => assert_eq!(flex, 1),
            _ => panic!("expected spacer"),
        }
    }

    #[test]
    fn constrained_table_parses() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "constrained",
              max_width = 30,
              min_height = 2,
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Constrained {
                min_width,
                max_width,
                min_height,
                max_height,
                ..
            } => {
                assert_eq!(min_width, None);
                assert_eq!(max_width, Some(30));
                assert_eq!(min_height, Some(2));
                assert_eq!(max_height, None);
            }
            _ => panic!("expected constrained"),
        }
    }

    #[test]
    fn align_table_parses_each_alignment() {
        let l = lua();
        for (s, expected) in [
            ("top-left", Alignment::TopLeft),
            ("top", Alignment::Top),
            ("top-right", Alignment::TopRight),
            ("left", Alignment::Left),
            ("center", Alignment::Center),
            ("right", Alignment::Right),
            ("bottom-left", Alignment::BottomLeft),
            ("bottom", Alignment::Bottom),
            ("bottom-right", Alignment::BottomRight),
        ] {
            let src = format!(
                r#"return {{ _tui_kind = "align", alignment = "{s}", child = {{ _tui_kind = "text", content = "x" }} }}"#
            );
            let t = eval_table(&l, &src);
            let d = from_lua_table(&t).expect("parse");
            match d {
                WidgetDescription::Align { alignment, .. } => assert_eq!(alignment, expected),
                _ => panic!("expected align for {s}"),
            }
        }
    }

    #[test]
    fn align_default_is_center() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "align",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Align { alignment, .. } => assert_eq!(alignment, Alignment::Center),
            _ => panic!("expected align"),
        }
    }

    #[test]
    fn align_unknown_value_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "align",
              alignment = "diagonal",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("alignment"));
    }

    #[test]
    fn stack_table_parses() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "stack",
              children = {
                { _tui_kind = "text", content = "bg" },
                { _tui_kind = "text", content = "fg" },
              },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Stack { children, .. } => assert_eq!(children.len(), 2),
            _ => panic!("expected stack"),
        }
    }

    #[test]
    fn missing_kind_errors() {
        let l = lua();
        let t = eval_table(&l, r#"return { content = "x" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("missing `_tui_kind`"));
    }

    #[test]
    fn anchored_table_parses_with_defaults() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "anchored",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Anchored {
                anchor,
                offset_x,
                offset_y,
                width,
                height,
                ..
            } => {
                assert_eq!(anchor, Anchor::Center);
                assert_eq!(offset_x, 0);
                assert_eq!(offset_y, 0);
                assert_eq!(width, Dimension::Intrinsic);
                assert_eq!(height, Dimension::Intrinsic);
            }
            _ => panic!("expected anchored"),
        }
    }

    #[test]
    fn anchored_table_parses_percent_and_offsets() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "anchored",
              anchor = "top-right",
              offset_x = -2,
              offset_y = 3,
              width = "60%",
              height = 4,
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Anchored {
                anchor,
                offset_x,
                offset_y,
                width,
                height,
                ..
            } => {
                assert_eq!(anchor, Anchor::TopRight);
                assert_eq!(offset_x, -2);
                assert_eq!(offset_y, 3);
                assert_eq!(width, Dimension::Percent(60));
                assert_eq!(height, Dimension::Cells(4));
            }
            _ => panic!("expected anchored"),
        }
    }

    #[test]
    fn anchored_unknown_anchor_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "anchored",
              anchor = "northwest",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("anchor"));
    }

    #[test]
    fn anchored_invalid_percent_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "anchored",
              width = "fifty",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("`%`") || format!("{err}").contains("integer"));
    }

    #[test]
    fn anchored_percent_over_100_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "anchored",
              width = "150%",
              child = { _tui_kind = "text", content = "x" },
            }
        "#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("0%..=100%"));
    }

    #[test]
    fn anchored_requires_child() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "anchored" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("`child` is required"));
    }

    #[test]
    fn text_input_table_parses_with_defaults() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "text_input", key = "input" }"#);
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::TextInput {
                key,
                value,
                focused,
                on_change,
                on_submit,
                min_lines,
                max_lines,
                placeholder,
                cursor_blink,
                style,
            } => {
                assert_eq!(key.as_deref(), Some("input"));
                assert!(value.is_empty());
                assert!(!focused);
                assert!(on_change.is_none());
                assert!(on_submit.is_none());
                assert_eq!(min_lines, 1);
                assert_eq!(max_lines, 1);
                assert!(placeholder.is_none());
                assert!(!cursor_blink);
                assert!(style.is_none());
            }
            _ => panic!("expected text_input"),
        }
    }

    #[test]
    fn text_input_parses_full_props() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"
            return {
              _tui_kind = "text_input",
              key = "input",
              value = "hello",
              focused = true,
              on_change = "input.changed",
              on_submit = "input.submit",
              min_lines = 2,
              max_lines = 5,
              placeholder = "type here",
              cursor_blink = true,
            }
        "#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::TextInput {
                value,
                focused,
                on_change,
                on_submit,
                min_lines,
                max_lines,
                placeholder,
                cursor_blink,
                ..
            } => {
                assert_eq!(value, "hello");
                assert!(focused);
                assert_eq!(on_change.as_deref(), Some("input.changed"));
                assert_eq!(on_submit.as_deref(), Some("input.submit"));
                assert_eq!(min_lines, 2);
                assert_eq!(max_lines, 5);
                assert_eq!(placeholder.as_deref(), Some("type here"));
                assert!(cursor_blink);
            }
            _ => panic!("expected text_input"),
        }
    }

    #[test]
    fn text_input_rejects_max_lines_below_min() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"return { _tui_kind = "text_input", min_lines = 5, max_lines = 2 }"#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("max_lines"));
    }

    #[test]
    fn text_input_rejects_min_lines_zero() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"return { _tui_kind = "text_input", min_lines = 0, max_lines = 1 }"#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("min_lines"));
    }

    #[test]
    fn spans_table_parses_minimal() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"return { _tui_kind = "spans", spans = { { text = "hi" } } }"#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Spans { spans, wrap, key } => {
                assert_eq!(spans.len(), 1);
                assert_eq!(spans[0].text, "hi");
                assert_eq!(spans[0].style, Style::default());
                assert!(matches!(wrap, WrapMode::Word));
                assert!(key.is_none());
            }
            _ => panic!("expected spans"),
        }
    }

    #[test]
    fn spans_table_parses_full_attributes() {
        let l = lua();
        let t = eval_table(
            &l,
            r##"return {
              _tui_kind = "spans",
              spans = {
                { text = "a", fg = "#ff0000", bold = true },
                { text = "b", fg = 196, italic = true },
                { text = "c", bg = { r = 1, g = 2, b = 3 }, underline = true, reverse = true },
                { text = "d", fg = "reset" },
              },
              wrap = "char",
            }"##,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Spans { spans, wrap, .. } => {
                assert!(matches!(wrap, WrapMode::Char));
                assert_eq!(spans[0].style.fg, Some(Color::Rgb(0xff, 0, 0)));
                assert!(spans[0].style.bold);
                assert_eq!(spans[1].style.fg, Some(Color::Indexed(196)));
                assert!(spans[1].style.italic);
                assert_eq!(spans[2].style.bg, Some(Color::Rgb(1, 2, 3)));
                assert!(spans[2].style.underline);
                assert!(spans[2].style.reverse);
                assert_eq!(spans[3].style.fg, Some(Color::Reset));
            }
            _ => panic!("expected spans"),
        }
    }

    #[test]
    fn spans_requires_spans_field() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "spans" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("spans"));
    }

    #[test]
    fn spans_each_entry_requires_text() {
        let l = lua();
        let t = eval_table(
            &l,
            r##"return { _tui_kind = "spans", spans = { { fg = "#ffffff" } } }"##,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("text"));
    }

    #[test]
    fn spans_invalid_hex_color_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r##"return { _tui_kind = "spans", spans = { { text = "x", fg = "#zzzzzz" } } }"##,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("hex"));
    }

    #[test]
    fn spans_unknown_string_color_errors() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"return { _tui_kind = "spans", spans = { { text = "x", fg = "rebeccapurple" } } }"#,
        );
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("rrggbb"));
    }

    #[test]
    fn markdown_table_parses_minimal() {
        let l = lua();
        let t = eval_table(
            &l,
            r#"return { _tui_kind = "markdown", source = "**hi** _world_" }"#,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Markdown {
                source,
                theme,
                wrap,
                ..
            } => {
                assert_eq!(source, "**hi** _world_");
                assert!(theme.is_none());
                assert!(matches!(wrap, WrapMode::Word));
            }
            _ => panic!("expected markdown"),
        }
    }

    #[test]
    fn markdown_requires_source() {
        let l = lua();
        let t = eval_table(&l, r#"return { _tui_kind = "markdown" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("source"));
    }

    #[test]
    fn markdown_theme_table_parses() {
        let l = lua();
        let t = eval_table(
            &l,
            r##"return {
              _tui_kind = "markdown",
              source = "x",
              theme = {
                bold = { bold = true },
                italic = { italic = true },
                code = { fg = "#ff8800" },
                code_block = { fg = "#888888" },
                h1 = { fg = "#ff00ff", bold = true },
                h2 = { fg = "#00ffff" },
                h3 = { fg = "#ffff00" },
                h4 = { fg = "#00ff00" },
                h5 = { fg = "#0000ff" },
                h6 = { fg = "#888888" },
                link = { underline = true },
                blockquote = { italic = true },
                list_marker = { fg = "#888888" },
              },
            }"##,
        );
        let d = from_lua_table(&t).expect("parse");
        match d {
            WidgetDescription::Markdown { theme, .. } => {
                let t = theme.expect("theme set");
                assert!(t.bold.unwrap().bold);
                assert!(t.italic.unwrap().italic);
                assert_eq!(t.code.unwrap().fg, Some(Color::Rgb(0xff, 0x88, 0x00)));
                assert_eq!(t.h1.unwrap().fg, Some(Color::Rgb(0xff, 0x00, 0xff)));
                assert!(t.h1.unwrap().bold);
                assert!(t.link.unwrap().underline);
            }
            _ => panic!("expected markdown"),
        }
    }
}
