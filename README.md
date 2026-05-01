# scribe — modal text editor for writers

A vim-like editor with only the features a writer needs and a few features
vim never had. Single binary, sub-10ms startup, Claude Code session
integration, cross-instance shared registers.

Part of the [Fe2O3 Rust terminal suite](https://github.com/isene/fe2o3).

## Status

**Phase 0 — proof of concept**. `hjkl`, `i/a/o/I/A/O`, `Esc`, `:w`/`:q`/`:wq`,
basic motions, undo/redo, byte-correct UTF-8 cursor.

## Building

```bash
PATH="/usr/bin:$PATH" cargo build --release
```

`~/bin/scribe` is a symlink to `target/release/scribe`.

## Roadmap

- **Phase 1**: full motion set + operators (`d/c/y` over motions and
  text-objects), registers (`"a` … `"z`), search `/?`, dot-repeat.
- **Phase 2**: visual modes (incl. `Ctrl-v` block edit), macros (`q@`),
  marks (`m'\``), exhaustive ex commands.
- **Phase 3**: syntax highlighting (syntect + custom HyperList engine).
- **Phase 4**: word completion, plugin system (external-process protocol),
  per-filetype config (`~/.scribe/ftplugin/<ext>.json`).
- **Phase 5**: Claude Code integration via persistent `claude -p
  --session-id=…` (spell, grammar, gen, chat — all sharing context).
- **Phase 6**: spellchecking via `hunspell` (offline, fast — primary)
  and `aspell` fallback. Highlight misspelled words with squiggly
  underline, `z=` to choose suggestion, `zg`/`zw` to add/remove from
  personal dict (`~/.scribe/spell/<lang>.dic`). AI-grade "explain why"
  defers to phase 5's claude path. Per-buffer language toggle.
- **Phase 7**: reading mode (Goyo-style centered narrow column), sticky
  section header, word counter, swap-file recovery.
- **Phase 8**: cross-session shared registers via Unix socket — yank in
  one scribe, paste in another. Better than vim's `+`/`*`.

## Philosophy

vim minus 90% (no LSP, no debugger, no quickfix list, no Q-recording-
windows, no built-in compiler integration), plus 10% writer-focused
features vim doesn't have.

## License

Public domain ([Unlicense](https://unlicense.org/)).
