# Scribe — Modal Text Editor for Writers

<img src="img/scribe.svg" align="left" width="150" height="150">

![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Vim-flavoured modal editor with the parts a writer actually needs and a few features vim never had. Single static binary, sub-10 ms startup, soft-wrap by default, **Claude Code in the editor**, syntax highlighting, hunspell spellcheck, persistent command history, six themes.

Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3). Built on [crust](https://github.com/isene/crust) and [highlight](https://github.com/isene/highlight).

<br clear="left"/>

## Why scribe (and not vim)

Vim has a thousand features. A writer needs about thirty of them. Scribe is "vim minus 90 % minus the programming subsystem, plus a handful of writer-first niceties":

- **Modal core** — `hjkl`, motions, operators, text-objects, registers, marks, dot-repeat, undo tree.
- **Soft-wrap by default** — long prose lines wrap at the pane edge with a continuation indicator. Live resize via `SIGWINCH`.
- **Claude integration in the prompt** — `:claude {prompt}` runs `claude -p` over your selection / paragraph / buffer and splices the response back. One `u` reverses an entire turn.
- **Email-mode rendering** — `.eml` files and kastrup compose tempfiles get header / quote-level / signature colors that match kastrup's right pane 1-for-1. Inline email addresses + URLs highlighted everywhere.
- **Syntax highlighting** for ~18 source languages plus dedicated HyperList / Markdown / LaTeX renderers via the shared `highlight` crate.
- **Spellcheck** via hunspell — auto-on for email mode, opt-in elsewhere via `:set spell`. Curly red underline, `]s` / `[s` navigation, `z=` suggestions, `zg` to add to personal dict.
- **No LSP / debugger / quickfix / `:make`** — writers don't compile.
- **Block paste that actually pastes a block** — `Ctrl-v` selection lays each row at the same column on consecutive lines. One `u` reverses the whole thing.

## Claude Code integration

`:claude` runs `claude -p` with your text on stdin and splices the response back. The scoping is deliberately conservative — whole-buffer replacement requires an explicit selection so a stray prompt can't silently destroy your file:

```
:claude rewrite this paragraph in plainer English
   → with a Visual selection: replace the selection with the response
   → without selection:        replace the CURRENT PARAGRAPH (text-object `ap`)

:claude what's a tighter version of this?
   → same scoping rules: selection > current paragraph

:claude grammar
   → shorthand: "Fix grammar, spelling, punctuation. Preserve meaning + tone."

:claude tighten
   → shorthand: "Rewrite to be more concise."

:claude plain
   → shorthand: "Rewrite in plainer English."

:claude continue
   → input = buffer up to cursor; INSERT response at cursor (no replace)
```

To rewrite the **whole buffer**, select it first: `ggVG:claude …`.

Verbs (`grammar`, `tighten`, `plain`, `continue`) are baked-in shortcuts. Anything else is sent verbatim as the prompt. The whole turn is one compound undo node, so `u` reverses the change in one step. The status line shows ` claude: NNN chars  (u to undo)` after a successful turn as a reminder.

If Claude has rewritten code into prose (or vice versa) and the highlighter looks wrong, swap it with `:set syntax=plain` / `:set syntax=markdown` / `:set syntax=rust` — see the [status table](#status) below for the full list of recognised syntaxes.

### Full Claude Code session — `:chat`

For multi-turn discussion, `:chat` suspends scribe and opens a regular interactive Claude Code session in the same terminal. The current buffer (including unsaved edits) is snapshotted to `/tmp/scribe-chat-<pid>.txt` and its path is mentioned in the initial message, so Claude can read it on demand:

```
:chat
   → scribe yields the terminal; you're in `claude` interactively.
   → ask anything, paste excerpts, iterate. The buffer's tempfile path
     is in claude's first message — read it via /file or just ask claude
     to read it.
   → /exit (or claude's normal quit) returns you to scribe, buffer
     untouched.
```

Use `:claude {prompt}` for surgical one-shot edits where you want the response spliced back; use `:chat` when you want a real conversation.

Requires `claude` on `PATH` (both commands).

## Status

**v0.1.26** — daily-driveable for prose. Implemented:

| Area | Keys / commands |
|---|---|
| Motion | `h j k l` (+ arrows, line-wrap), `0 ^ $`, `gg G`, `w b e W B`, `Ctrl-D Ctrl-U`, `PgUp/PgDn`, `f F t T`, `n N`, `* #`, counts (`5j`, `12G`) |
| Insert | `i a o I A O s S`, arrows + `HOME / END` work in Insert too, **bracketed paste** batched into one undo node |
| Operators + motion | `d c y > < gq` over any motion or text-object; `5dw`, `d3w`, `cgg`, `yG`, `c$`, `>ap`, `gqap` |
| Linewise ops | `dd cc yy >> << gqq`, `D C Y`, counts (`5dd`) |
| Text objects | `iw aw i" a" i' a' i\` a\` i( a( i[ a[ i{ a{ i< a< ip ap` (+ `ib`/`iB` aliases) |
| Edit primitives | `x X r{c}`, `J`, `~`, `p P` |
| Visual modes | `v` charwise, `V` linewise, `Ctrl-v` block — operate with any operator. Statusline shows live `sel: Nl Nw Nc` while selecting. |
| Registers | `"a` … `"z`, `"+`/`"*` (system clipboard via OSC 52), `"0` last yank, unnamed `""`. Named registers persist to `~/.config/scribe/registers.json` on every yank — survives restarts AND shares live across concurrent scribe sessions (yank in scribe A, `"ap` in scribe B). |
| Search | `/ ?` (regex), `n N`, `* #` (word under cursor) |
| Substitute | `:s/pat/rep/[gi]` current line, `:%s/pat/rep/[gi]` whole buffer, atomic undo |
| Undo | `u` undo, `Ctrl-R` redo, undo **tree** in memory; cursor follows the edit site |
| Dot-repeat | `.` replays the last change (operator + motion + inserted text, replace, paste) |
| Macros | `M{reg}` start recording, `M` again to stop; `@{reg}` replay, `@@` last. Stored in the same registers as yanks — `"ap` pastes the captured key sequence (`<Esc>`, `<C-Up>`, `<CR>`, …) as editable text; yank an edited version back into a register and replay runs the new sequence. |
| Marks | `m{a-z}` set, `'a` jump to first non-blank of mark line, `` `a `` jump to exact column. Session-local. |
| Yank/cut feedback | Statusline confirms every register write: `5 lines yanked`, `23 chars yanked into "a`, `3 lines deleted`, etc. |
| Register inspector | `:reg` (or `:registers`) opens a popup listing all set registers with kind + first 60 chars. ESC closes. |
| Move lines | `Ctrl-Up` / `Ctrl-Down` swap the current line with the one above / below. Counts work (`5 Ctrl-Down`). |
| Increment | `Ctrl-A` / `Ctrl-X` increment / decrement the number at-or-after the cursor. Recognises ISO 8601 dates `YYYY-MM-DD` with month-end / leap-year rollover (e.g. `2024-02-28` + 1 = `2024-02-29`; `2025-02-28` + 1 = `2025-03-01`). Counts work (`30 Ctrl-A` adds 30 days). Zero-padding preserved on integers. |
| Insert helpers | `Ctrl-Y` / `Ctrl-E` in Insert mode insert the character from the same column on the line above / below. Useful for stretching tables and ASCII diagrams. |
| Auto-wrap | `:set textwidth=N` (or `:set tw=N`) — typing a space past column N breaks the line at the last preceding whitespace. `:set tw=0` disables. |
| Reading mode | `:read` toggles distraction-free rendering — line numbers off, header / footer dimmed to a divider line. `:noread` exits. `:set readingwidth=80` centers text in an 80-col column (Goyo-style). `:set paragraphdim` dims every paragraph except the cursor's (Limelight-style). |
| Quick spell + read | `zr` toggles reading mode, `zq` saves + quits, `zn` / `zp` jump next / prev misspelling. (`z=` suggest, `zg` add to dict are unchanged.) |
| Spellcheck | `:set spell`, `:set spelllang=NAME` (e.g. `nb_NO`), `]s` / `[s` next/prev miss, `z=` suggestions, `zg` add to dict |
| Config popup | `:config` — modal preferences pane (theme, numbers, spell on/off, lang, underline color). `W` saves to scriberc, `ESC` closes. |
| Themes | `:set theme=NAME` (monokai / solarized / nord / dracula / gruvbox / plain), `--theme=NAME` CLI |
| Syntax override | `:set syntax=NAME` (plain / email / rust / md / py / sh / …) — change the buffer's filetype on the fly |
| Line numbers | `:set number` / `:set rnu` (relative) / `:set nonumber` |
| Claude | `:claude {prompt}` one-shot, `:chat` interactive session (see [Claude Code integration](#claude-code-integration)) |
| Command history | Up / Down at the `:` prompt, persisted in `~/.config/scribe/cmdhistory` |
| Ex commands | `:w :q :q! :wq :x :e <path> :set …` |
| Quit | `q` quits when clean (refuses + warns if dirty), `Q` quits + discards changes, `:wq` saves + quits. Every save first writes `<path>.scribe-bak` so an accidental `:wq` after a destructive `:claude` is recoverable. |

## Roadmap
- **Reading mode** (`:read` toggle) — distraction-free, prose-styled, dim chrome, Markdown rendered.
- **HyperList editing intelligence** — Tab fold/unfold, smart auto-indent, Operator preservation.
- **Cross-session shared registers** via Unix socket — yank in one scribe, paste in another.

## Install

```bash
git clone https://github.com/isene/scribe
cd scribe
PATH="/usr/bin:$PATH" cargo build --release
ln -sf "$PWD/target/release/scribe" ~/bin/scribe
```

Or grab the binary from the [latest release](https://github.com/isene/scribe/releases/latest).

## Use as `$EDITOR`

```bash
export EDITOR=scribe
```

(Or set in your shell rc.) Scribe accepts `+N` for line-jump (vim convention), so kastrup's compose flow drops the cursor straight on the body.

## Configuration

`~/.config/scribe/scriberc` — simple `key = value` per line, `#` comments:

```
theme = dracula
number = true
relativenumber = false
spell = false
lang = en_US           # hunspell dict tag (en_US, nb_NO, nn_NO, de_DE, …)
spellcolor = 196       # xterm-256 palette index for the spellcheck underline
read = false           # enter reading mode at startup
readingwidth = 80      # centered column width when reading (0 = full pane)
paragraphdim = true    # Limelight-style dim of non-current paragraphs
```

`--theme NAME` overrides the rcfile for one session. Runtime `:set` commands stay in-session (the rcfile is the hand-edited source of truth).

## Files / locations

| Path | Contents |
|---|---|
| `~/.config/scribe/scriberc` | persistent settings |
| `~/.config/scribe/cmdhistory` | `:` command history (capped at 100) |
| `~/.config/scribe/spell.add` | personal dictionary for `zg` |

## Philosophy

A writer's editor. Not a programmer's editor. Not an "everything" editor. Built specifically because every other editor is bloated with features for a job the user doesn't have — and is missing the one feature a writer in 2026 actually wants: AI in the loop without leaving the buffer.

## License

Public domain ([Unlicense](https://unlicense.org/)).
