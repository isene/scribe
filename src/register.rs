//! Vim-style registers. Phase 1 ships:
//! - Unnamed `""` — last yank/delete (default for p/P).
//! - Named `"a` … `"z` — explicitly addressed.
//! - System `"+` and `"*` — clipboard via OSC 52 on yank.
//! - Last yank `"0` — yank populates "" AND "0; delete only "".
//!
//! Each register stores text + a kind (charwise vs linewise) so paste places
//! correctly: linewise paste opens a new line above/below; charwise paste
//! inserts at cursor column.

use std::collections::HashMap;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum YankKind { Charwise, Linewise }

#[derive(Clone)]
pub struct Yank {
    pub text: String,
    pub kind: YankKind,
}

pub struct Registers {
    /// Slots keyed by register name char ('"', '0', 'a'..'z', '+', '*').
    slots: HashMap<char, Yank>,
}

impl Registers {
    pub fn new() -> Self { Self { slots: HashMap::new() } }

    pub fn get(&self, name: char) -> Option<&Yank> { self.slots.get(&name) }

    /// Generic store that DOES NOT touch "0 or "" — used internally for
    /// named-register writes by yank/delete dispatchers.
    pub fn put(&mut self, name: char, y: Yank) { self.slots.insert(name, y); }

    /// Yank semantics: write "", "0, optional named, and broadcast to system
    /// clipboard via OSC 52.
    pub fn yank(&mut self, name: Option<char>, text: String, kind: YankKind) {
        let y = Yank { text: text.clone(), kind };
        self.slots.insert('"', y.clone());
        self.slots.insert('0', y.clone());
        if let Some(n) = name { self.slots.insert(n, y.clone()); }
        // OSC 52 to system clipboard.
        crust::clipboard_copy(&text, "c");
        crust::clipboard_copy(&text, "p");
    }

    /// Delete semantics: write "" and optional named. Does not touch "0.
    pub fn cut(&mut self, name: Option<char>, text: String, kind: YankKind) {
        let y = Yank { text: text.clone(), kind };
        self.slots.insert('"', y.clone());
        if let Some(n) = name { self.slots.insert(n, y.clone()); }
        crust::clipboard_copy(&text, "c");
        crust::clipboard_copy(&text, "p");
    }
}
