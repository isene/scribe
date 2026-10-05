//! Pictures inside a Markdown note.
//!
//! A line that is one picture link and nothing else, `![](img/cat.jpg)`,
//! gets empty rows under it, and the picture is shown there through glow.
//! That takes a terminal which keeps a picture while the text around it
//! is redrawn (the kitty protocol: glass, kitty, WezTerm). In any other
//! terminal the link stays a line of text and nothing here runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::SystemTime;

/// One picture on screen: its file, its top left cell, the box it was
/// fitted to, and how many of its rows are in view.
#[derive(Clone, PartialEq, Debug)]
pub struct Shown {
    pub path: String,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    pub visible: u16,
}

#[derive(Default)]
struct State {
    /// The terminal's picture channel, opened at the first picture link.
    display: Option<glow::Display>,
    /// The cells each picture takes in the box `fitted_to`, with the
    /// file's time, so a file is measured once until it changes.
    sizes: HashMap<String, (SystemTime, (u16, u16))>,
    fitted_to: (u16, u16),
    shown: Vec<Shown>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The file a line names, when the line is a picture link and nothing else.
pub fn link(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("![")?;
    let path = rest[rest.find("](")? + 2..].strip_suffix(')')?;
    (!path.is_empty() && !path.contains(|c: char| c == ')' || c.is_whitespace())).then_some(path)
}

/// The cells (columns, rows) a picture takes in a box of `max_cols` ×
/// `max_rows`. `None` when the terminal shows no pictures, or the file is
/// missing or is not a picture. One `stat` per call; the file is opened
/// only when it is new or has changed.
pub fn size(path: &str, max_cols: u16, max_rows: u16) -> Option<(u16, u16)> {
    STATE.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        if !s.display.get_or_insert_with(glow::Display::new).keeps_pictures() { return None; }
        if s.fitted_to != (max_cols, max_rows) {
            s.sizes.clear();
            s.fitted_to = (max_cols, max_rows);
        }
        let changed = std::fs::metadata(path).ok()?.modified().ok()?;
        if let Some(&(at, cells)) = s.sizes.get(path) {
            if at == changed { return Some(cells); }
        }
        let cells = glow::fit_cells(path, max_cols, max_rows)?;
        s.sizes.insert(path.to_string(), (changed, cells));
        Some(cells)
    })
}

/// Put these pictures on screen and take away the ones not named. Sends
/// nothing when the list is the one already showing.
pub fn place(now: Vec<Shown>) {
    STATE.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else { return };
        if s.shown == now { return; }
        let State { display, shown, .. } = &mut *s;
        let Some(display) = display.as_mut() else { return };
        for old in shown.iter() {
            if !now.iter().any(|p| p.path == old.path) { display.forget_path(&old.path); }
        }
        for p in &now {
            if !shown.contains(p) {
                display.show_clipped(&p.path, p.x, p.y, p.cols, p.rows, 0, p.visible);
            }
        }
        *shown = now;
    })
}

/// Take every picture off the screen: before a popup (glass draws a
/// picture over any text), before another program gets the screen, and
/// before the screen is cleared. The next frame puts them back.
pub fn hide() {
    STATE.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else { return };
        if s.shown.is_empty() { return; }
        if let Some(display) = s.display.as_mut() { display.clear_all(); }
        s.shown.clear();
    })
}

#[cfg(test)]
mod tests {
    use super::link;

    #[test]
    fn a_picture_link_alone_on_a_line_names_its_file() {
        assert_eq!(link("![](img/cat.jpg)"), Some("img/cat.jpg"));
        assert_eq!(link("  ![a cat](../img/cat.jpg)  "), Some("../img/cat.jpg"));
        assert_eq!(link("see ![](img/cat.jpg)"), None);
        assert_eq!(link("![](img/cat.jpg) and more"), None);
        assert_eq!(link("![](img/a cat.jpg)"), None);
        assert_eq!(link("![]()"), None);
        assert_eq!(link("[a link](img/cat.jpg)"), None);
        assert_eq!(link("![](img/a.jpg) ![](img/b.jpg)"), None);
    }
}
