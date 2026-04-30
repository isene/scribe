//! Editor modes. Phase 0 ships Normal, Insert, Command. Visual / VisualLine /
//! VisualBlock / Replace land in phase 1+.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// `:` ex command line. The buffered command lives on `App`.
    Command,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal  => " NORMAL ",
            Mode::Insert  => " INSERT ",
            Mode::Command => " COMMAND ",
        }
    }
    pub fn color(&self) -> u8 {
        match self {
            Mode::Normal  => 33,   // blue
            Mode::Insert  => 46,   // green
            Mode::Command => 208,  // orange
        }
    }
}
