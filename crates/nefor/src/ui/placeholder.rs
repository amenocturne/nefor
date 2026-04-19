//! Placeholder center-pane widgets used before the Lua VM is wired.
//!
//! Per spec §Startup sequence step 2: "If missing, enter the TUI with a
//! single 'no config found' pane." During the multi-commit buildout the
//! sibling [`InitLuaFoundWidget`] gives us a visible "Lua not yet wired"
//! state when an `init.lua` *is* present — so we can see the startup path
//! reach this stage while the loader is still absent.

use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::paths::ConfigDir;
use crate::ui::widget::Widget;

/// Rendered when `<config_dir>/init.lua` is missing. Stays on screen until
/// the user presses `q` / Ctrl-C; no other input is handled yet.
pub struct NoConfigWidget {
    config_dir: ConfigDir,
}

impl NoConfigWidget {
    /// Build a `NoConfigWidget` pointing at `config_dir`.
    pub fn new(config_dir: ConfigDir) -> Self {
        Self { config_dir }
    }
}

impl Widget for NoConfigWidget {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = format!(
            "nefor\n\
             \n\
             no init.lua found at {}\n\
             \n\
             quick start: https://github.com/<placeholder>/nefor#quick-start\n\
             \n\
             press q or Ctrl-C to quit",
            self.config_dir,
        );
        let paragraph = Paragraph::new(text).alignment(Alignment::Center);
        frame.render_widget(paragraph, area);
    }
}

/// Rendered when `<config_dir>/init.lua` exists but the Lua loader isn't
/// wired yet. Purely a diagnostic placeholder: makes the "Lua-not-wired"
/// state visible so the next commit's startup path is easy to eyeball.
pub struct InitLuaFoundWidget {
    init_lua_path: std::path::PathBuf,
}

impl InitLuaFoundWidget {
    /// Build an `InitLuaFoundWidget` pointing at the concrete `init.lua` path.
    pub fn new(init_lua_path: std::path::PathBuf) -> Self {
        Self { init_lua_path }
    }
}

impl Widget for InitLuaFoundWidget {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = format!(
            "nefor\n\
             \n\
             found init.lua at {} — Lua loader not yet implemented\n\
             \n\
             press q or Ctrl-C to quit",
            self.init_lua_path.display(),
        );
        let paragraph = Paragraph::new(text).alignment(Alignment::Center);
        frame.render_widget(paragraph, area);
    }
}
