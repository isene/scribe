# Scribe — Modal Text Editor for Writers

<img src="img/scribe.svg" align="left" width="150" height="150">

![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Vim-flavoured modal editor with the parts a writer actually needs and a few features vim never had. Single static binary, sub-10ms startup, soft-wrap by default, Claude Code session integration on the roadmap, and shared-clipboard registers across instances.

Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3). Built on [crust](https://github.com/isene/crust).

<br clear="left"/>

## Why scribe (and not vim)

Vim has a thousand features. A writer needs about thirty of them. Scribe is "vim minus 90 % minus the programming subsystem, plus a handful of writer-first niceties":

- **Modal core** — `hjkl`, motions, operators, text-objects, registers, marks, macros, dot-repeat, undo tree.
- **Soft-wrap by default** — long prose lines wrap at the pane edge with a continuation indicator. Live resize handled via `SIGWINCH`.
- **No LSP / debugger / quickfix / Q-recording-windows / `:make`** — writers don't compile.
- **Spellcheck, grammar, generation, discussion** via persistent Claude Code sessions (planned phase 5). Grammar correction can promote into a conversation about *why* the change was suggested without losing context.
- **Shared registers across scribe instances** via Unix socket — yank in one, paste in another. Vim's `+` and `*` clipboard at terminal-level (planned phase 8).
- **Reading mode** (Goyo-style centered narrow column), sticky section header for long docs, word counter (planned phase 7).
- **Block paste that actually pastes a block** — `Ctrl-v` block yank lays each row at the same column on consecutive lines. One `u` reverses the whole thing.

## Status

**v0.1** — daily-driveable for prose. Implemented:

| Area | Keys |
|---|---|
| Motion | `h j k l` (+ arrows, line-wrap), `0 ^ $`, `gg G`, `w b e W B`, `Ctrl-D Ctrl-U`, `PgUp/PgDn`, `f F t T`, `n N`, `* #`, counts (`5j`, `12G`) |
| Insert | `i a o I A O`, arrows + `HOME / END` work in Insert too |
| Operators + motion | `d c y` over any motion or text-object; `5dw`, `d3w`, `cgg`, `yG`, `c$` |
| Linewise ops | `dd cc yy`, `D C Y`, counts (`5dd`) |
| Text objects | `iw aw i" a" i' a' i\` a\` i( a( i[ a[ i{ a{ i< a< ip ap` (incl. `ib`/`iB` aliases) |
| Edit primitives | `x X r{c}`, `J`, `~`, `p P` |
| Visual modes | `v` charwise, `V` linewise, `Ctrl-v` block — operate with any operator |
| Registers | `"a` … `"z`, `"+`/`"*` (system clipboard via OSC 52), `"0` last yank, unnamed `""` |
| Search | `/ ?` (regex), `n N`, `* #` (word under cursor) |
| Undo | `u` undo, `Ctrl-R` redo, undo TREE in memory; cursor follows the edit site |
| Dot-repeat | `.` replays the last change (operator + motion + inserted text, replace, paste) |
| Ex commands | `:w :q :q! :wq :x :e <path>` |
| Quit | `q` save+quit, `Q` quit no save (Fe₂O₃ harmonised; vim ex-style still works) |

## Roadmap

- **Phase 2** — macros (`q{reg}` / `@{reg}` / `@@`), marks (`m'\``), buffer history (`:bn`/`:bp`).
- **Phase 3** — syntax highlighting via `syntect` (TextMate grammars) + a custom HyperList engine for `.hl`.
- **Phase 4** — buffer-words completion (`Ctrl-n` / `Ctrl-p` in Insert), per-filetype config (`~/.scribe/ftplugin/<ext>.json`), language-agnostic plugin protocol (rush-style).
- **Phase 5** — Claude Code integration: persistent `claude --session-id=…` per scribe instance. `:spell`, `:grammar`, `:gen`, `:chat`. Grammar fix can promote into discussion.
- **Phase 6** — spellchecking via `hunspell` (offline) + `aspell` fallback. Squiggly underline; `z=` suggest; `zg`/`zw` add to / remove from personal dict.
- **Phase 7** — reading mode (Goyo-style narrow centered column), sticky section header, word counter, swap-file recovery.
- **Phase 8** — cross-session shared registers via Unix socket. Yank in one scribe, paste in another. Better than vim's `+` / `*`.

## Install

```bash
git clone https://github.com/isene/scribe
cd scribe
PATH="/usr/bin:$PATH" cargo build --release
ln -sf "$PWD/target/release/scribe" ~/bin/scribe
```

## Use as `$EDITOR`

```bash
export EDITOR=scribe
```

(Or set in your shell rc.)

## Files / locations

| Path | Contents |
|---|---|
| `~/.scribe/` | future home for config, swap, plugins, ftplugin (TBD) |

(Phase 0 has no on-disk state.)

## Philosophy

A writer's editor. Not a programmer's editor. Not an "everything" editor. Built specifically because every other editor is bloated with features for a job the user doesn't have.

## License

Public domain ([Unlicense](https://unlicense.org/)).
