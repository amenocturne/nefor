//! `nefor.ui.*` bindings.
//!
//! # MVP scope
//!
//! - `nefor.ui.register_widget(region, renderer) -> WidgetHandle` where
//!   `renderer` is a Lua function returning an array of strings. Rust wraps
//!   it in a [`LuaWidget`] that, on `Widget::render`, locks the Lua VM,
//!   invokes the renderer, and draws the returned lines as a ratatui
//!   `Paragraph`. Rich ratatui access from Lua (frame handle, widget
//!   builders) is not year-one scope — returning lines covers every widget
//!   the starter bundle needs (chat transcript, statusline, input prompt).
//! - `nefor.ui.invalidate(handle)` — no-op for MVP because the event loop
//!   redraws on every tick anyway. API is present so plugins can call it
//!   and stay forward-compatible.
//! - `nefor.ui.subscribe_key(pattern, handler)` — sugar over
//!   `nefor.events.on("key", ...)` with a simple pattern matcher.
//! - `nefor.ui.subscribe_resize(handler)` — sugar over
//!   `nefor.events.on("resize", ...)`.
//!
//! # Frame-borrow dance
//!
//! ratatui's `terminal.draw(|frame| ...)` passes a `&mut Frame` with a
//! short lifetime; the closure can't hold onto it. [`LuaWidget::render`]
//! runs *inside* that closure, so the Lua call is synchronous from the
//! frame's POV. mlua serializes concurrent VM access internally (the `send`
//! feature's reentrant mutex), so even if another tokio task has a Lua
//! call in flight, this one waits rather than racing.

use std::sync::Arc;

use mlua::{Function, Lua, RegistryKey, Table, Value};
use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::events::{EventBus, EventName, EventPayload, SubscriptionId, KEY, RESIZE};
use crate::ui::{Region, SharedRegistry, Widget};

/// A registered Lua-backed widget.
///
/// Holds a [`RegistryKey`] for the Lua renderer function and a cloned
/// [`mlua::Lua`] handle used to call it on each frame. On `render`, locks
/// the VM (via mlua's internal mutex), calls the function, collects the
/// returned string-array, and draws as a ratatui `Paragraph`.
struct LuaWidget {
    lua: Lua,
    renderer: Arc<RegistryKey>,
}

impl Widget for LuaWidget {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = match self.invoke_renderer() {
            Some(v) => v,
            None => return,
        };
        // Tail-clip: walk lines from the end, accumulate wrapped row counts,
        // stop when adding the next line would exceed area.height. Renders
        // only the tail slice — no Paragraph::scroll offset, no wrap/scroll
        // math discrepancy. Newest content stays pinned to the bottom of the
        // area; older content scrolls off the top. A line that partially
        // overflows is excluded entirely (small blank gap at top), which the
        // user tolerates more readily than newest content clipped at bottom.
        let width = area.width as usize;
        let height = area.height as usize;
        let mut rows_used = 0usize;
        let mut start = lines.len();
        for (i, line) in lines.iter().enumerate().rev() {
            let r = wrapped_row_count(line, width);
            if rows_used + r > height {
                break;
            }
            rows_used += r;
            start = i;
        }
        let tail = &lines[start..];
        let text = tail.join("\n");
        let paragraph = Paragraph::new(text)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn measure(&self, width: u16) -> u16 {
        let lines = match self.invoke_renderer() {
            Some(v) => v,
            None => return 1,
        };
        if lines.is_empty() {
            return 1;
        }
        let w = width as usize;
        let total: usize = lines.iter().map(|l| wrapped_row_count(l, w)).sum();
        u16::try_from(total.max(1)).unwrap_or(u16::MAX)
    }
}

impl LuaWidget {
    fn invoke_renderer(&self) -> Option<Vec<String>> {
        let func: Function = match self.lua.registry_value(&self.renderer) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "Lua widget renderer missing from registry");
                return None;
            }
        };
        match func.call::<Vec<String>>(()) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!(error = %e, "Lua widget renderer raised");
                None
            }
        }
    }
}

/// Approximate row count for a line rendered under ratatui's
/// `Wrap { trim: false }` (word-wrap). For most ASCII input this matches
/// ratatui's output; for very wide graphemes it under-counts. `width == 0`
/// degrades to one row per input line — safe, non-panicking.
fn wrapped_row_count(line: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    if line.is_empty() {
        return 1;
    }
    // Mirror ratatui's word-wrap: pack words into the current row, break when
    // the next word wouldn't fit (counting one separator cell). A word wider
    // than `width` spans ceil(len / width) rows on its own.
    let mut rows = 1usize;
    let mut col = 0usize;
    for word in line.split_whitespace() {
        let wlen = word.chars().count();
        if col == 0 {
            if wlen <= width {
                col = wlen;
            } else {
                rows += (wlen - 1) / width;
                col = wlen % width;
                if col == 0 {
                    col = width;
                }
            }
        } else {
            let needed = 1 + wlen;
            if col + needed <= width {
                col += needed;
            } else {
                rows += 1;
                if wlen <= width {
                    col = wlen;
                } else {
                    rows += (wlen - 1) / width;
                    col = wlen % width;
                    if col == 0 {
                        col = width;
                    }
                }
            }
        }
    }
    rows
}

/// Install `nefor.ui.*` onto `nefor_tbl`.
pub fn install_ui(
    lua: &Lua,
    nefor_tbl: &Table,
    bus: Arc<EventBus>,
    registry: SharedRegistry,
) -> mlua::Result<()> {
    let ui = lua.create_table()?;

    // nefor.ui.register_widget(region, renderer) -> integer handle
    let register_lua = lua.clone();
    let register_registry = Arc::clone(&registry);
    let register_fn = lua.create_function(move |_, (region_val, renderer): (Value, Value)| {
        let region = parse_region(&region_val)?;
        let func = match renderer {
            Value::Function(f) => f,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.ui.register_widget: renderer must be a function (got {})",
                    other.type_name(),
                )));
            }
        };
        let key = register_lua.create_registry_value(func)?;
        let widget = LuaWidget {
            lua: register_lua.clone(),
            renderer: Arc::new(key),
        };
        let mut reg = match register_registry.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let handle = reg
            .register(region, Box::new(widget))
            .map_err(|e| mlua::Error::runtime(format!("nefor.ui.register_widget: {e}",)))?;
        Ok(handle.as_u64())
    })?;
    ui.set("register_widget", register_fn)?;

    // nefor.ui.invalidate(handle) — no-op for MVP.
    let invalidate_fn = lua.create_function(|_, _handle: u64| Ok(()))?;
    ui.set("invalidate", invalidate_fn)?;

    // nefor.ui.subscribe_key(pattern, handler) -> sub_id
    let subscribe_key_bus = Arc::clone(&bus);
    let subscribe_key_lua = lua.clone();
    let subscribe_key_fn = lua.create_function(move |_, (pattern, handler): (String, Value)| {
        let parsed = KeyPattern::parse(&pattern).map_err(|reason| {
            mlua::Error::runtime(format!(
                "nefor.ui.subscribe_key: invalid pattern {pattern:?}: {reason}"
            ))
        })?;
        let func = match handler {
            Value::Function(f) => f,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.ui.subscribe_key: handler must be a function (got {})",
                    other.type_name(),
                )));
            }
        };
        let key = subscribe_key_lua.create_registry_value(func)?;
        let key = Arc::new(key);
        let lua_for_cb = subscribe_key_lua.clone();
        let id = subscribe_key_bus.on(
            EventName::from(KEY),
            Box::new(move |payload| {
                let EventPayload::Key(ke) = payload else {
                    return;
                };
                if !parsed.matches(ke) {
                    return;
                }
                let func: Function = match lua_for_cb.registry_value(&key) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "subscribe_key handler missing from registry");
                        return;
                    }
                };
                let arg = match key_event_to_table(&lua_for_cb, ke) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build key event table");
                        return;
                    }
                };
                if let Err(e) = func.call::<()>(arg) {
                    tracing::error!(error = %e, "subscribe_key handler raised");
                }
            }),
        );
        Ok(id.as_u64())
    })?;
    ui.set("subscribe_key", subscribe_key_fn)?;

    // nefor.ui.subscribe_resize(handler) -> sub_id
    let subscribe_resize_bus = Arc::clone(&bus);
    let subscribe_resize_lua = lua.clone();
    let subscribe_resize_fn = lua.create_function(move |_, handler: Value| {
        let func = match handler {
            Value::Function(f) => f,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.ui.subscribe_resize: handler must be a function (got {})",
                    other.type_name(),
                )));
            }
        };
        let key = subscribe_resize_lua.create_registry_value(func)?;
        let key = Arc::new(key);
        let lua_for_cb = subscribe_resize_lua.clone();
        let id = subscribe_resize_bus.on(
            EventName::from(RESIZE),
            Box::new(move |payload| {
                let EventPayload::Resize { cols, rows } = payload else { return };
                let func: Function = match lua_for_cb.registry_value(&key) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "subscribe_resize handler missing from registry");
                        return;
                    }
                };
                let t = match lua_for_cb.create_table() {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build resize event table");
                        return;
                    }
                };
                if let Err(e) = t.set("cols", *cols).and_then(|_| t.set("rows", *rows)) {
                    tracing::error!(error = %e, "failed to populate resize event table");
                    return;
                }
                if let Err(e) = func.call::<()>(t) {
                    tracing::error!(error = %e, "subscribe_resize handler raised");
                }
            }),
        );
        Ok(id.as_u64())
    })?;
    ui.set("subscribe_resize", subscribe_resize_fn)?;

    // Re-export off via nefor.ui.off for symmetry with subscribe_* returning
    // ids; users can still use nefor.events.off.
    let off_bus = Arc::clone(&bus);
    let off_fn = lua.create_function(move |_, id: u64| {
        off_bus.off(SubscriptionId::from_u64(id));
        Ok(())
    })?;
    ui.set("off", off_fn)?;

    nefor_tbl.set("ui", ui)?;
    Ok(())
}

/// Convert a key event into a Lua table matching the `key` event payload shape
/// that `nefor.events.on("key", ...)` would deliver.
fn key_event_to_table(lua: &Lua, ke: &crossterm::event::KeyEvent) -> mlua::Result<Value> {
    use crossterm::event::KeyModifiers;
    let t = lua.create_table()?;
    let (code_str, ch, fnum) = describe_key(ke.code);
    t.set("code", code_str)?;
    if let Some(c) = ch {
        t.set("char", c.to_string())?;
    }
    if let Some(n) = fnum {
        t.set("f", n)?;
    }
    let mods = lua.create_table()?;
    mods.set("ctrl", ke.modifiers.contains(KeyModifiers::CONTROL))?;
    mods.set("shift", ke.modifiers.contains(KeyModifiers::SHIFT))?;
    mods.set("alt", ke.modifiers.contains(KeyModifiers::ALT))?;
    t.set("modifiers", mods)?;
    Ok(Value::Table(t))
}

fn describe_key(code: crossterm::event::KeyCode) -> (&'static str, Option<char>, Option<u8>) {
    use crossterm::event::KeyCode as K;
    match code {
        K::Backspace => ("Backspace", None, None),
        K::Enter => ("Enter", None, None),
        K::Left => ("Left", None, None),
        K::Right => ("Right", None, None),
        K::Up => ("Up", None, None),
        K::Down => ("Down", None, None),
        K::Home => ("Home", None, None),
        K::End => ("End", None, None),
        K::PageUp => ("PageUp", None, None),
        K::PageDown => ("PageDown", None, None),
        K::Tab => ("Tab", None, None),
        K::BackTab => ("BackTab", None, None),
        K::Delete => ("Delete", None, None),
        K::Insert => ("Insert", None, None),
        K::F(n) => ("F", None, Some(n)),
        K::Char(c) => ("Char", Some(c), None),
        K::Null => ("Null", None, None),
        K::Esc => ("Esc", None, None),
        K::CapsLock => ("CapsLock", None, None),
        K::ScrollLock => ("ScrollLock", None, None),
        K::NumLock => ("NumLock", None, None),
        K::PrintScreen => ("PrintScreen", None, None),
        K::Pause => ("Pause", None, None),
        K::Menu => ("Menu", None, None),
        K::KeypadBegin => ("KeypadBegin", None, None),
        K::Media(_) => ("Media", None, None),
        K::Modifier(_) => ("Modifier", None, None),
    }
}

/// A parsed `nefor.ui.subscribe_key` pattern.
///
/// Grammar (MVP — intentionally small):
/// - `"q"` / `"a"` / `"1"` → a single character
/// - `"Enter"` / `"Esc"` / `"Backspace"` / `"Tab"` / `"Up"` / `"Down"` /
///   `"Left"` / `"Right"` → the corresponding special key
/// - `"C-x"` / `"S-x"` / `"M-x"` → ctrl/shift/alt + named key, where `x` is
///   any of the above. Prefixes can chain (`"C-S-Enter"`).
#[derive(Debug, PartialEq, Eq)]
struct KeyPattern {
    code: PatternCode,
    ctrl: bool,
    shift: bool,
    alt: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum PatternCode {
    Char(char),
    Named(&'static str),
}

impl KeyPattern {
    fn parse(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("empty pattern".into());
        }
        let mut rest = s;
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        // Peel off modifier prefixes.
        while let Some((prefix, tail)) = rest.split_once('-') {
            match prefix {
                "C" => ctrl = true,
                "S" => shift = true,
                "M" | "A" => alt = true,
                _ => break,
            }
            rest = tail;
        }
        if rest.is_empty() {
            return Err("missing key after modifier".into());
        }
        let code = match rest {
            "Enter" => PatternCode::Named("Enter"),
            "Esc" | "Escape" => PatternCode::Named("Esc"),
            "Backspace" => PatternCode::Named("Backspace"),
            "Tab" => PatternCode::Named("Tab"),
            "Up" => PatternCode::Named("Up"),
            "Down" => PatternCode::Named("Down"),
            "Left" => PatternCode::Named("Left"),
            "Right" => PatternCode::Named("Right"),
            "Home" => PatternCode::Named("Home"),
            "End" => PatternCode::Named("End"),
            "PageUp" => PatternCode::Named("PageUp"),
            "PageDown" => PatternCode::Named("PageDown"),
            "Space" => PatternCode::Char(' '),
            one if one.chars().count() == 1 => {
                // Safe: count == 1 guarantees a first char.
                PatternCode::Char(one.chars().next().unwrap_or('?'))
            }
            other => return Err(format!("unknown key {other:?}")),
        };
        Ok(KeyPattern {
            code,
            ctrl,
            shift,
            alt,
        })
    }

    fn matches(&self, ke: &crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let code_matches = match (&self.code, ke.code) {
            (PatternCode::Char(want), KeyCode::Char(got)) => *want == got,
            (PatternCode::Named("Enter"), KeyCode::Enter) => true,
            (PatternCode::Named("Esc"), KeyCode::Esc) => true,
            (PatternCode::Named("Backspace"), KeyCode::Backspace) => true,
            (PatternCode::Named("Tab"), KeyCode::Tab) => true,
            (PatternCode::Named("Up"), KeyCode::Up) => true,
            (PatternCode::Named("Down"), KeyCode::Down) => true,
            (PatternCode::Named("Left"), KeyCode::Left) => true,
            (PatternCode::Named("Right"), KeyCode::Right) => true,
            (PatternCode::Named("Home"), KeyCode::Home) => true,
            (PatternCode::Named("End"), KeyCode::End) => true,
            (PatternCode::Named("PageUp"), KeyCode::PageUp) => true,
            (PatternCode::Named("PageDown"), KeyCode::PageDown) => true,
            _ => false,
        };
        code_matches
            && ke.modifiers.contains(KeyModifiers::CONTROL) == self.ctrl
            && ke.modifiers.contains(KeyModifiers::SHIFT) == self.shift
            && ke.modifiers.contains(KeyModifiers::ALT) == self.alt
    }
}

/// Parse a Lua `region` table into a [`Region`].
///
/// Accepts `{ kind = "center" }` or `{ kind = "top", size = N }` (and the
/// same for `bottom` / `left` / `right`). `size` defaults to 1 when
/// omitted — avoids a surprise crash on `{ kind = "top" }` while not hiding
/// real bugs (zero-height rows are common by accident; size=1 is "what you
/// probably wanted").
fn parse_region(val: &Value) -> mlua::Result<Region> {
    let t = match val {
        Value::Table(t) => t,
        other => {
            return Err(mlua::Error::runtime(format!(
                "nefor.ui.register_widget: region must be a table (got {})",
                other.type_name(),
            )));
        }
    };
    let kind: String = t.get("kind").map_err(|e| {
        mlua::Error::runtime(format!(
            "nefor.ui.register_widget: region.kind must be a string: {e}"
        ))
    })?;
    let size: Option<u16> = t.get::<Option<u16>>("size").unwrap_or(None);
    let size_or_one = size.unwrap_or(1);
    // `bottom` without a size is auto-height: the widget reports its height
    // each frame via Widget::measure. Other kinds keep the "no size → 1 row"
    // default since they rarely grow dynamically (and we don't need a TopAuto
    // yet; starter registrations don't want one).
    let region = match kind.as_str() {
        "top" => Region::Top(size_or_one),
        "bottom" => match size {
            Some(h) => Region::Bottom(h),
            None => Region::BottomAuto,
        },
        "left" => Region::Left(size_or_one),
        "right" => Region::Right(size_or_one),
        "center" => Region::Center,
        other => {
            return Err(mlua::Error::runtime(format!(
                "nefor.ui.register_widget: region.kind must be one of \
                 top/bottom/left/right/center (got {other:?})"
            )));
        }
    };
    Ok(region)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::ui::WidgetRegistry;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use std::sync::Mutex;

    fn setup() -> (Lua, Arc<EventBus>, SharedRegistry) {
        let lua = Lua::new();
        let bus = Arc::new(EventBus::new());
        let registry: SharedRegistry = Arc::new(Mutex::new(WidgetRegistry::new()));
        // install_events needed so ui can coexist; but for these tests we
        // only use ui bindings — they don't depend on events table existing.
        let nefor = lua.create_table().unwrap();
        install_ui(&lua, &nefor, Arc::clone(&bus), Arc::clone(&registry)).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        (lua, bus, registry)
    }

    #[test]
    fn register_widget_returns_integer_handle() {
        let (lua, _bus, registry) = setup();
        let h: u64 = lua
            .load(
                r#"
                return nefor.ui.register_widget(
                    { kind = "bottom", size = 1 },
                    function() return { "status" } end
                )
                "#,
            )
            .eval()
            .expect("register ok");
        assert_eq!(h, 0);
        assert_eq!(registry.lock().unwrap().len(), 1);
    }

    #[test]
    fn register_widget_rejects_bad_region() {
        let (lua, _bus, _registry) = setup();
        let err = lua
            .load(r#"nefor.ui.register_widget({ kind = "nope" }, function() return {} end)"#)
            .exec()
            .expect_err("bad kind must error");
        assert!(err.to_string().contains("top/bottom/left/right/center"));
    }

    #[test]
    fn register_widget_rejects_non_function_renderer() {
        let (lua, _bus, _registry) = setup();
        let err = lua
            .load(r#"nefor.ui.register_widget({ kind = "center" }, 42)"#)
            .exec()
            .expect_err("bad renderer must error");
        assert!(err.to_string().contains("must be a function"));
    }

    #[test]
    fn invalidate_is_noop() {
        let (lua, _bus, _registry) = setup();
        lua.load("nefor.ui.invalidate(0)").exec().expect("ok");
    }

    #[test]
    fn subscribe_resize_fires_on_resize_event() {
        let (lua, bus, _registry) = setup();
        let seen = Arc::new(std::sync::Mutex::new(None::<(u16, u16)>));
        let s = Arc::clone(&seen);
        let observe = lua
            .create_function(move |_, (cols, rows): (u16, u16)| {
                *s.lock().unwrap() = Some((cols, rows));
                Ok(())
            })
            .unwrap();
        lua.globals().set("observe", observe).unwrap();

        lua.load(
            r#"
            nefor.ui.subscribe_resize(function(ev)
                observe(ev.cols, ev.rows)
            end)
            "#,
        )
        .exec()
        .unwrap();

        bus.emit(
            &EventName::from(RESIZE),
            EventPayload::Resize {
                cols: 120,
                rows: 40,
            },
        );
        assert_eq!(*seen.lock().unwrap(), Some((120, 40)));
    }

    #[test]
    fn subscribe_key_filters_by_pattern() {
        let (lua, bus, _registry) = setup();
        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = Arc::clone(&count);
        let observe = lua
            .create_function(move |_, ()| {
                c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        lua.globals().set("observe", observe).unwrap();

        lua.load(
            r#"
            nefor.ui.subscribe_key("C-c", function() observe() end)
            "#,
        )
        .exec()
        .unwrap();

        // Ctrl-c matches.
        bus.emit(
            &EventName::from(KEY),
            EventPayload::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        );
        // Plain c doesn't.
        bus.emit(
            &EventName::from(KEY),
            EventPayload::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        );
        // Ctrl-x doesn't (different char).
        bus.emit(
            &EventName::from(KEY),
            EventPayload::Key(KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        );

        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn parse_region_center() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("kind", "center").unwrap();
        let v = Value::Table(t);
        let r = parse_region(&v).unwrap();
        assert_eq!(r, Region::Center);
    }

    #[test]
    fn parse_region_top_with_size() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("kind", "top").unwrap();
        t.set("size", 3u16).unwrap();
        let v = Value::Table(t);
        let r = parse_region(&v).unwrap();
        assert_eq!(r, Region::Top(3));
    }

    #[test]
    fn parse_region_bottom_without_size_is_auto() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("kind", "bottom").unwrap();
        let v = Value::Table(t);
        let r = parse_region(&v).unwrap();
        assert_eq!(r, Region::BottomAuto);
    }

    #[test]
    fn parse_region_top_without_size_defaults_to_one() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("kind", "top").unwrap();
        let v = Value::Table(t);
        let r = parse_region(&v).unwrap();
        assert_eq!(r, Region::Top(1));
    }

    #[test]
    fn key_pattern_single_char() {
        let p = KeyPattern::parse("q").unwrap();
        let ke = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(p.matches(&ke));
    }

    #[test]
    fn key_pattern_ctrl_char() {
        let p = KeyPattern::parse("C-c").unwrap();
        let ke = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(p.matches(&ke));
    }

    #[test]
    fn key_pattern_named() {
        let p = KeyPattern::parse("Enter").unwrap();
        let ke = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(p.matches(&ke));
    }

    #[test]
    fn key_pattern_rejects_unknown() {
        let err = KeyPattern::parse("UnknownKey").unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn key_pattern_rejects_empty() {
        assert!(KeyPattern::parse("").is_err());
    }
}
