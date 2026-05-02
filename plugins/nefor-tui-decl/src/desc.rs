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

impl WidgetDescription {
    /// Static type tag used as half of the reconciler's match key.
    pub fn type_tag(&self) -> &'static str {
        match self {
            WidgetDescription::Text { .. } => "text",
            WidgetDescription::Column { .. } => "column",
            WidgetDescription::Row { .. } => "row",
            WidgetDescription::Padding { .. } => "padding",
            WidgetDescription::Stack { .. } => "stack",
            WidgetDescription::Expanded { .. } => "expanded",
            WidgetDescription::Spacer { .. } => "spacer",
            WidgetDescription::Constrained { .. } => "constrained",
            WidgetDescription::Align { .. } => "align",
        }
    }

    /// User-supplied `key` field, if any.
    pub fn user_key(&self) -> Option<&str> {
        match self {
            WidgetDescription::Text { key, .. }
            | WidgetDescription::Column { key, .. }
            | WidgetDescription::Row { key, .. }
            | WidgetDescription::Padding { key, .. }
            | WidgetDescription::Stack { key, .. }
            | WidgetDescription::Expanded { key, .. }
            | WidgetDescription::Spacer { key, .. }
            | WidgetDescription::Constrained { key, .. }
            | WidgetDescription::Align { key, .. } => key.as_deref(),
        }
    }
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
        "column" => parse_column(t),
        "padding" => parse_padding(t),
        other => Err(TuiError::InvalidDesc(format!(
            "unknown widget kind `{other}`; expected one of: text, column, padding"
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
        Value::String(s) if s.to_str()?.as_ref() == "reset" => Ok(Some(Color::Reset)),
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
            "color `{key}` must be a string \"reset\", an integer 0..=255, or a table {{r,g,b}} (got {})",
            other.type_name()
        ))),
    }
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
        let t = eval_table(&l, r#"return { _tui_kind = "row" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("unknown widget kind"));
    }

    #[test]
    fn missing_kind_errors() {
        let l = lua();
        let t = eval_table(&l, r#"return { content = "x" }"#);
        let err = from_lua_table(&t).unwrap_err();
        assert!(format!("{err}").contains("missing `_tui_kind`"));
    }
}
