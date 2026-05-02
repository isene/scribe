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

`:claude` runs `claude -p` with your text on stdin and splices the response back. The behaviour adapts to your mode:

```
:claude rewrite this paragraph in plainer English
   → with a Visual selection: replace the selection with the response
   → without selection: rewrite the whole buffer

:claude what's a tighter version of this?
   → same scoping rules (selection > buffer)

:claude grammar
   → shorthand: "Fix grammar, spelling, punctuation. Preserve meaning + tone."

:claude tighten
   → shorthand: "Rewrite to be more concise."

:claude plain
   → shorthand: "Rewrite in plainer English."

:claude continue
   → input = buffer up to cursor; INSERT response at cursor (no replace)
```

Verbs (`grammar`, `tighten`, `plain`, `continue`) are baked-in shortcuts. Anything else is sent verbatim as the prompt. The whole turn is one compound undo node, so `u` reverses the change in one step.

Requires `claude` on `PATH`.

## Status

**v0.1.17** — daily-driveable for prose. Implemented:

| Area | Keys / commands |
|---|---|
| Motion | `h j k l` (+ arrows, line-wrap), `0 ^ $`, `gg G`, `w b e W B`, `Ctrl-D Ctrl-U`, `PgUp/PgDn`, `f F t T`, `n N`, `* #`, counts (`5j`, `12G`) |
| Insert | `i a o I A O s S`, arrows + `HOME / END` work in Insert too, **bracketed paste** batched into one undo node |
| Operators + motion | `d c y > < gq` over any motion or text-object; `5dw`, `d3w`, `cgg`, `yG`, `c$`, `>ap`, `gqap` |
| Linewise ops | `dd cc yy >> << gqq`, `D C Y`, counts (`5dd`) |
| Text objects | `iw aw i" a" i' a' i\` a\` i( a( i[ a[ i{ a{ i< a< ip ap` (+ `ib`/`iB` aliases) |
| Edit primitives | `x X r{c}`, `J`, `~`, `p P` |
| Visual modes | `v` charwise, `V` linewise, `Ctrl-v` block — operate with any operator |
| Registers | `"a` … `"z`, `"+`/`"*` (system clipboard via OSC 52), `"0` last yank, unnamed `""` |
| Search | `/ ?` (regex), `n N`, `* #` (word under cursor) |
| Substitute | `:s/pat/rep/[gi]` current line, `:%s/pat/rep/[gi]` whole buffer, atomic undo |
| Undo | `u` undo, `Ctrl-R` redo, undo **tree** in memory; cursor follows the edit site |
| Dot-repeat | `.` replays the last change (operator + motion + inserted text, replace, paste) |
| Spellcheck | `:set spell`, `]s` / `[s` next/prev miss, `z=` suggestions, `zg` add to dict |
| Themes | `:set theme=NAME` (monokai / solarized / nord / dracula / gruvbox / plain), `--theme=NAME` CLI |
| Line numbers | `:set number` / `:set rnu` (relative) / `:set nonumber` |
| Claude | `:claude {prompt}` (see [section above](#claude-code-integration)) |
| Command history | Up / Down at the `:` prompt, persisted in `~/.config/scribe/cmdhistory` |
| Ex commands | `:w :q :q! :wq :x :e <path> :set …` |
| Quit | `q` save+quit, `Q` quit no save (Fe₂O₃ harmonised; vim ex-style still works) |

## Roadmap

- **Macros** — `q{reg}` to record, `@{reg}` / `@@` to replay.
- **Multi-line state in source highlighting** — block comments / multi-line strings keep their color across line breaks. Currently line-stateless (visible at the top of any file with a `/* */` block).
- **Persistent registers** — yank survives across scribe restarts (`~/.config/scribe/registers.json`).
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
