//! Vim-style registers. Phase 1 ships:
//! - Unnamed `""` — last yank/delete (default for p/P).
//! - Named `"a` … `"z` — explicitly addressed.
//! - System `"+` and `"*` — clipboard via OSC 52 on yank.
//!
//! The system clipboard gets a yank or delete only when it was made on a
//! visual selection, or into `"+` / `"*`. A plain `x`, `dw` or `yy` in
//! Normal mode stays in scribe's own registers. Before, every deleted
//! letter replaced what the desktop had on its clipboard, and a
//! clipboard history filled up with them.
//! - Last yank `"0` — yank populates "" AND "0; delete only "".
//!
//! Each register stores text + a kind (charwise vs linewise) so paste places
//! correctly: linewise paste opens a new line above/below; charwise paste
//! inserts at cursor column.
//!
//! ## Persistence
//!
//! Named registers (`"a` .. `"z`, plus `"0` and `""`) are persisted to
//! `~/.config/scribe/registers.json` on every yank / delete / put.
//! That gives two things:
//!
//! 1. Yanks (and recorded macros, which live in the same registers)
//!    survive scribe restarts.
//! 2. Two scribe instances running concurrently see each other's
//!    yanks at the next register access — yank in scribe A, paste in
//!    scribe B without the system clipboard.
//!
//! Save-on-write costs one small JSON write per yank — measured at
//! a few hundred microseconds for typical prose-sized yanks. The
//! system-clipboard registers (`"+`, `"*`) are NOT persisted — those
//! are owned by the OS clipboard.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum YankKind {
    Charwise,
    Linewise,
    /// Visual-block yank. `text` is `\n`-joined lines, each representing the
    /// row's column range. Paste lays each row at the same column on
    /// consecutive buffer lines, NOT inline.
    Block,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Yank {
    pub text: String,
    pub kind: YankKind,
}

pub struct Registers {
    /// Slots keyed by register name char ('"', '0', 'a'..'z', '+', '*').
    slots: HashMap<char, Yank>,
}

fn registers_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/registers.json")
}

/// Slots that are worth persisting. We omit the system-clipboard slots
/// (`+` / `*`) which are externally owned, and we omit any junk slot
/// somebody might wedge in here in the future. Also includes `0` and
/// `"` because they're the default last-yank slots.
fn is_persistent(name: char) -> bool {
    name == '"' || name == '0' || name.is_ascii_alphanumeric()
}

impl Registers {
    pub fn new() -> Self { Self { slots: HashMap::new() } }

    /// Construct from disk. Missing or malformed file → empty registers
    /// (silent — the editor still works without persisted state).
    pub fn load() -> Self {
        let mut s = Self::new();
        let path = registers_path();
        let Ok(content) = std::fs::read_to_string(&path) else { return s };
        if let Ok(map) = serde_json::from_str::<HashMap<String, Yank>>(&content) {
            for (k, v) in map {
                if let Some(c) = k.chars().next() {
                    if is_persistent(c) { s.slots.insert(c, v); }
                }
            }
        }
        s
    }

    /// Write the persistent slots back to `~/.config/scribe/registers.json`.
    /// Atomic-ish: writes to `<path>.tmp` then renames so a crash mid-write
    /// can't truncate the file.
    pub fn save(&self) {
        let path = registers_path();
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        let map: HashMap<String, &Yank> = self.slots.iter()
            .filter(|(c, _)| is_persistent(**c))
            .map(|(c, y)| (c.to_string(), y))
            .collect();
        let Ok(json) = serde_json::to_string_pretty(&map) else { return };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn get(&self, name: char) -> Option<&Yank> { self.slots.get(&name) }

    /// Generic store that DOES NOT touch "0 or "" — used internally for
    /// named-register writes by yank/delete dispatchers and for macro
    /// recordings.
    pub fn put(&mut self, name: char, y: Yank) {
        self.slots.insert(name, y);
        self.save();
    }

    /// Yank semantics: write "", "0, optional named. With `clip`, or into
    /// "+ or "*, the text goes to the system clipboard as well.
    pub fn yank(&mut self, name: Option<char>, text: String, kind: YankKind, clip: bool) {
        let y = Yank { text: text.clone(), kind };
        self.slots.insert('"', y.clone());
        self.slots.insert('0', y.clone());
        if let Some(n) = name { self.slots.insert(n, y.clone()); }
        broadcast(name, &text, clip);
        self.save();
    }

    /// Delete semantics: write "" and optional named. Does not touch "0.
    /// The system clipboard gets it by the same rule as a yank.
    pub fn cut(&mut self, name: Option<char>, text: String, kind: YankKind, clip: bool) {
        let y = Yank { text: text.clone(), kind };
        self.slots.insert('"', y.clone());
        if let Some(n) = name { self.slots.insert(n, y.clone()); }
        broadcast(name, &text, clip);
        self.save();
    }
}

/// Whether a yank or delete goes to the system clipboard too: when the
/// caller asks for it (a visual selection), or when it is made into the
/// "+ or "* register.
pub fn to_clipboard(name: Option<char>, asked: bool) -> bool {
    asked || matches!(name, Some('+') | Some('*'))
}

/// OSC 52 to the system clipboard, for the yanks and deletes that go there.
/// One call: the two there were ("c" and "p") both named the clipboard to
/// crust, which knows "clipboard" and "primary", so every copy was made
/// twice.
fn broadcast(name: Option<char>, text: &str, clip: bool) {
    if to_clipboard(name, clip) {
        crust::clipboard_copy(text, "clipboard");
    }
}

#[cfg(test)]
mod tests {
    use super::to_clipboard;

    #[test]
    fn only_a_visual_selection_or_the_plus_register_reaches_the_system_clipboard() {
        assert!(!to_clipboard(None, false), "x, dw, dd and yy in Normal mode stay in scribe");
        assert!(!to_clipboard(Some('a'), false), "and so does a named register");
        assert!(to_clipboard(None, true), "a yank or delete of a visual selection");
        assert!(to_clipboard(Some('+'), false), "\"+yy asks for the clipboard by name");
        assert!(to_clipboard(Some('*'), false));
    }
}
