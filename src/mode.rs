//! Editor modes.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Like Insert but typed chars OVERWRITE the char under cursor
    /// instead of pushing it right. Entered with `R` from Normal,
    /// or by pressing `<Insert>` while in Insert mode (which then
    /// toggles back on the next `<Insert>`).
    Replace,
    /// `:` ex command line. The buffered command lives on `App`.
    Command,
    /// Visual character-wise (`v`).
    Visual,
    /// Visual line-wise (`V`).
    VisualLine,
    /// Visual block-wise (`Ctrl-v`).
    VisualBlock,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal      => " NORMAL ",
            Mode::Insert      => " INSERT ",
            Mode::Replace     => " REPLACE ",
            Mode::Command     => " COMMAND ",
            Mode::Visual      => " VISUAL ",
            Mode::VisualLine  => " V-LINE ",
            Mode::VisualBlock => " V-BLOCK ",
        }
    }
    pub fn color(&self) -> u8 {
        match self {
            Mode::Normal      => 33,    // blue
            Mode::Insert      => 46,    // green
            Mode::Replace     => 196,   // red
            Mode::Command     => 208,   // orange
            Mode::Visual      |
            Mode::VisualLine  |
            Mode::VisualBlock => 165,   // magenta
        }
    }
    pub fn is_visual(&self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine | Mode::VisualBlock)
    }
}
