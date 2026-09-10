//! Colors by role, nebula's default palette: cyan accent, yellow for running, green for a
//! finished job, red for a failure, violet for what is new and unread.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    pub dim: Color,
    pub ok: Color,
    pub new: Color,
    pub warn: Color,
    pub err: Color,
    pub special: Color,
    pub sel_bg: Color,
    pub edge: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            text: Color::White,
            muted: Color::Gray,
            dim: Color::DarkGray,
            ok: Color::Green,
            new: Color::Indexed(141),
            warn: Color::Yellow,
            err: Color::Red,
            special: Color::Magenta,
            sel_bg: Color::Indexed(237),
            edge: Color::Indexed(238),
        }
    }
}
