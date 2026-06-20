//! Searchable help index. One curated row per binding / command / feature:
//! `(category, trigger, description)`. Powers scribe's `\?` / `g?` /
//! `:help <query>` fuzzy help. Keep it in sync when adding a feature — one
//! line each; the filter searches all three fields.

pub type Entry = (&'static str, &'static str, &'static str);

pub const HELP: &[Entry] = &[
    // Motion
    ("Motion", "h j k l", "left / down / up / right (arrows too)"),
    ("Motion", "0 ^ $", "line start / first non-blank / end (Home/End)"),
    ("Motion", "gg G", "first / last line; 12G jumps to line 12"),
    ("Motion", "w b e  W B E", "next/prev word, end of word; CAPS = WORD"),
    ("Motion", "f F t T {c}", "jump on/before next/prev char on the line"),
    ("Motion", "Ctrl-D Ctrl-U", "half-page scroll down/up (PgDn/PgUp full)"),
    ("Motion", "n N", "next / previous search match"),
    ("Motion", "* #", "search word under cursor forward / back"),
    ("Motion", "{count}", "prefix any motion: 5j, 12G, 3w"),
    // Insert
    ("Insert", "i a", "insert before / after cursor"),
    ("Insert", "I A", "insert at line start / end"),
    ("Insert", "o O", "open new line below / above"),
    ("Insert", "s S", "substitute char / line and enter Insert"),
    ("Insert", "Ctrl-Y Ctrl-E", "insert char from column on line above / below"),
    // Operators + text objects
    ("Operator", "d c y > < gq", "delete/change/yank/indent/dedent/format + motion"),
    ("Operator", "dd cc yy >> <<", "linewise (doubled operator)"),
    ("Operator", "D C Y", "delete / change / yank to end of line"),
    ("Text-obj", "iw aw  ip ap", "inner/around word, paragraph (after an operator)"),
    ("Text-obj", "i\" a\"  i( a(  i{ a{", "inner/around quotes, brackets, braces (ci\", dap)"),
    // Edit
    ("Edit", "x X", "delete char forward / backward"),
    ("Edit", "r{c}", "replace char under cursor"),
    ("Edit", "J", "join line below"),
    ("Edit", "~", "toggle case under cursor"),
    ("Edit", "p P", "paste after / before"),
    ("Edit", "Ctrl-A Ctrl-X", "increment / decrement number or ISO date"),
    ("Edit", "Ctrl-Up Ctrl-Down", "swap current line up / down"),
    ("Edit", ".", "dot-repeat the last change"),
    // Visual
    ("Visual", "v V Ctrl-v", "visual char / line / block; live selection stats"),
    // Registers
    ("Register", "\"a..\"z  \"0-\"9", "named registers; \"ay$ yanks into a"),
    ("Register", "\"+ \"*", "system clipboard via OSC 52"),
    ("Register", ":reg", "register inspector popup (also :registers)"),
    // Search + substitute
    ("Search", "/pat  ?pat", "regex search forward / backward"),
    ("Search", ":s/pat/rep/[gi]", "substitute on the current line"),
    ("Search", ":%s/pat/rep/[gi]", "substitute the whole buffer (one undo)"),
    // Undo
    ("Undo", "u  Ctrl-R", "undo / redo (in-memory undo tree)"),
    // Macros
    ("Macro", "M{reg}", "start recording into reg; M again stops"),
    ("Macro", "@{reg}  @@", "replay macro / replay last-played"),
    // Marks
    ("Mark", "m{a-z}", "set a mark at the cursor"),
    ("Mark", "'a  `a", "jump to mark: line / exact column"),
    // Folds
    ("Fold", "zo zc  Space", "open / close fold; Space toggles under cursor"),
    ("Fold", "zs zh", "show / hide lines matching a pattern"),
    // Spell
    ("Spell", ":set spell / nospell", "toggle spellcheck"),
    ("Spell", ":set spelllang=NAME", "switch dictionary (alias :set lang=)"),
    ("Spell", "]s [s  (zn zp)", "next / previous misspelling"),
    ("Spell", "z=", "spelling suggestions (type the number)"),
    ("Spell", "zg", "add word at cursor to personal dictionary"),
    // Reading mode
    ("Reading", ":read  zr", "toggle distraction-free reading mode"),
    ("Reading", ":set readingwidth=N", "centered column width (alias rw); 0 = full"),
    ("Reading", ":set paragraphdim", "dim all paragraphs but the cursor's (alias pdim)"),
    // Auto-wrap
    ("Wrap", ":set textwidth=N", "auto-wrap typing at column N (alias tw); 0 = off"),
    // Theme + syntax
    ("Theme", ":set theme=NAME", "monokai / solarized / nord / dracula / gruvbox / plain"),
    ("Syntax", ":set syntax=NAME", "force filetype (aliases :set ft= / :set filetype=)"),
    ("Syntax", ":set syntax=md|html|hl", "render markdown / html / hyperlist"),
    // Inline colour / font / markup
    ("Style", "\\C", "colour the Visual selection (prism picks fg/bg)"),
    ("Style", "\\F", "set the selection's font (fonts picker, then size prompt)"),
    ("Style", "\\M", "toggle colour/font markup conceal (reveals on cursor line)"),
    // Export
    ("Export", "\\xh \\xl \\xm \\xp", "export HTML / LaTeX / Markdown / PDF"),
    ("Export", "\\xd \\xo", "export docx / odt via LibreOffice"),
    // Line numbers
    ("Number", ":set number", "line numbers (nu; rnu relative; nonu off)"),
    // Config
    ("Config", ":config", "config popup: theme / numbers / spell / save"),
    // HyperList leader bindings
    ("HyperList", "\\0 .. \\9", "fold to level 0..9"),
    ("HyperList", "\\a", "open all folds"),
    ("HyperList", "\\v \\V \\o", "checkbox / + timestamp / in-progress"),
    ("HyperList", "\\n", "autonumber toggle"),
    ("HyperList", "\\R", "renumber the Visual selection"),
    ("HyperList", "\\s", "sort the Visual selection by indent"),
    ("HyperList", "\\u", "state / transition underline cycle"),
    ("HyperList", "\\h", "limelight highlight (reading mode)"),
    ("HyperList", "\\r", "reference jump (in-file ref / file path / URL)"),
    ("HyperList", "\\p", "presentation mode toggle"),
    ("HyperList", "\\c", "complexity report"),
    ("HyperList", "\\g", "calendar add"),
    ("HyperList", "\\S \\H \\N", "show / hide / clear word filter"),
    // Encryption
    ("Crypt", "\\ee \\ed \\ek", "encrypt / decrypt / rekey the buffer"),
    // Word lookup
    ("Lookup", "\\w  K", "look up the word under the cursor via Claude"),
    // Commands
    ("Command", ":w :wq :q :q!", "write / write-quit / quit / force-quit"),
    ("Command", ":e <file>", "open a file in the buffer"),
    ("Command", ":claude {prompt}", "run claude -p over selection / paragraph / buffer"),
    ("Command", ":chat", "full interactive Claude session"),
    ("Command", ":help  :h", "open the bundled README in the buffer"),
    // Quit semantics
    ("Quit", "q", "quit when clean; refuses + warns if dirty"),
    ("Quit", "Q  zz", "Q discards unsaved changes; zz saves + quits"),
    // Help
    ("Help", "\\?  g?", "this searchable help index"),
];

/// Rows whose trigger / description / category contains `query`
/// (case-insensitive). Empty query returns everything. An exact trigger
/// match sorts to the top.
pub fn search(query: &str) -> Vec<&'static Entry> {
    let q = query.trim().to_lowercase();
    let mut rows: Vec<&'static Entry> = HELP
        .iter()
        .filter(|(cat, trig, desc)| {
            q.is_empty()
                || trig.to_lowercase().contains(&q)
                || desc.to_lowercase().contains(&q)
                || cat.to_lowercase().contains(&q)
        })
        .collect();
    if !q.is_empty() {
        // Stable sort keeps original order within each group.
        rows.sort_by_key(|(_, trig, _)| if trig.to_lowercase() == q { 0 } else { 1 });
    }
    rows
}
