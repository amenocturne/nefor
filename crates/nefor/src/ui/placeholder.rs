//! Placeholder center-pane widget used before Lua registers its own widgets.
//!
//! Per spec §Startup sequence step 2: "If missing, enter the TUI with a
//! single 'no config found' pane." Once the user writes an `init.lua` that
//! registers widgets via `nefor.ui.register_widget`, the startup path skips
//! this widget and defers to whatever Lua set up.

use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::paths::ConfigDir;
use crate::ui::widget::Widget;

/// Rendered when `<config_dir>/init.lua` is missing (or failed to load before
/// registering any widget). Stays on screen until the user presses `q` /
/// Ctrl-C; no other input is handled by this widget.
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
