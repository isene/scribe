# HyperList in Scribe

HyperList is a way to describe anything — any state, item(s), pattern, action,
process, transition, program, instruction set, etc. So you can use it as an
outliner, a ToDo list handler, a process design tool, a data modeler, or any
other way you want to describe something.

Scribe ships with full support for editing HyperList (`.hl`) files: indent
folding, autonumbering, encryption (incl. dotfile auto-encryption), checkbox
toggles, references, exports to HTML / LaTeX / Markdown, and more — modeled
1:1 on the original `hyperlist.vim` plugin.

For the official HyperList definition with examples, see
<http://isene.org/hyperlist/>.

HyperList was formerly known as WOIM. Files with `.woim` extension are also
treated as HyperList for backward compatibility.

---

## Editing HyperList in scribe

Use **Tab** (or `*`) for indentation. Items at greater indent are children of
the item above.

All HyperList commands live behind the `\` leader. Press `\?` inside any
HL buffer to see the cheatsheet popup.

### Folding

| Key / command | Action |
|---|---|
| `<SPACE>` | Toggle fold under cursor |
| `<C-SPACE>` | Toggle fold recursively |
| `\0` … `\9` | Set fold level globally (0–9) |
| `\a` | Open every fold |
| `:fold-all` / `:fold-open` | Same via ex-command |

### Numbering

| Key / command | Action |
|---|---|
| `\n` | Toggle autonumbering |
| `Ctrl-T` (insert) | Indent + add nested number (e.g. `1.2` → `1.2.1`) |
| `Ctrl-D` (insert) | De-indent and renumber (e.g. `1.2.1` → `1.3`) |
| `\R` (visual) | Renumber the visual selection from the first item's index |

The previous item must be numbered for the next item to be auto-numbered.

### Underlining States / Transitions

`\u` cycles through three modes:
1. State items (prefixed with `S:` or `|`) underlined
2. Transition items (prefixed with `T:` or `/`) underlined
3. None underlined

### Checkboxes

| Key | Action |
|---|---|
| `\v` | Toggle a checkbox at the start of the item: `[_]` ↔ `[x]` |
| `\V` | Toggle and add a date stamp on completion |
| `\o` | Mark as in-progress: `[O]` |

### References

A reference is enclosed in angle brackets. Examples:

- `<The first Item>` — soft reference / lookup
- `<file:/path/to/file>` — open a file
- `<file:~/notes/list.hl>` — open with `~` expansion
- `<https://example.com>` — open URL in browser
- `<+5>` — jump 5 lines down
- `<-3>` — jump 3 lines up
- `<Parent/Child/Grandchild>` — path with `/` separator
- `<<Subroutine>>` — hard redirect; jump back after executing

Press **`\r`** with the cursor on (or near) a reference to jump. `\r`
auto-detects: in-buffer references are searched in-file, `<file:…>` and URLs
are opened externally via xdg-open. Bare URLs and `~/`-style paths on a line
also work. After jumping, press `''` to come back.

### Templates

`\<SPACE>` jumps to the next "open" template element on a line — defined as
an item ending with `=` (sign meaning "fill me in").

### Presentation mode

`\p` toggles presentation mode. While on, the bare `<UP>` / `<DOWN>` arrow
keys behave like `g<UP>` / `g<DOWN>` — moving item-by-item with everything
else folded. Status bar shows `presentation ON`. Toggle off with `\p` again.

`g<UP>` / `g<DOWN>` always work, regardless of the toggle.

### Highlight current branch

`\h` toggles paragraph dimming / Limelight-style focus on the current item
and its children.

### Show / hide by word

Filter the visible portion of the list by word match:

| Key | Action |
|---|---|
| `\S` (or `zs`) | Show only items containing the word under the cursor |
| `\H` (or `zh`) | Hide items containing the word under the cursor |
| `\N` (or `z0`) | Reset (back to normal indent folding) |
| `:show <pattern>` | Show items matching the regex |
| `:hide <pattern>` | Hide items matching the regex |

`zs` / `zh` / `z0` are kept as muscle-memory aliases. Originally inspired by
VIM script #1594 (Amit Sethi).

### Sort

Visually select a block of items and press `\s`. They're sorted alphabetically
at the indent of the first selected line, and their children come along
correctly. Useful when out-of-sequence numbered items need re-sorting.

(Caveat: the last selected line cannot be the very last line of the buffer.)

### Encryption

Uses AES-256-CBC + PBKDF2-HMAC-SHA256 (10 000 iterations, 32-byte key).
Byte-for-byte compatible with the Ruby `hyperlist` app's `ENC:` format.

| Key | Action |
|---|---|
| `\ee` | Encrypt — visual selection if active, else whole file |
| `\ed` | Decrypt — visual selection if active, else whole file |
| `\ek` | Rekey — re-encrypt with a new password |

#### Auto-encrypted dotfiles

A file whose name starts with `.` (e.g. `.passwords.hl`), or any file whose
content starts with the `ENC:` header, is **automatically decrypted on open**
(scribe asks for the password — three attempts, then quits) and
**automatically re-encrypted on save**. While editing, scribe disables backup
files for that buffer to keep the cleartext from hitting disk.

### Calendar export

`\g` extracts every item with a future date and posts it to your default
calendar via `gcalcli`. If `gcalcli` isn't on PATH or the variable
`g:calendar` isn't set, `.ics` files are written to the working directory
instead.

In scribe's rcfile (`~/.config/scribe/scriberc`):

```
calendar = your.email@gmail.com
alldates = false       # set true to include past events
```

### Exports

Convert the buffer to another format and replace the buffer content (use `u`
to undo, then `:set syntax=hyperlist` to restore HL highlighting).

| Key | Format |
|---|---|
| `\xh` | HTML (responsive, color-coded) |
| `\xl` | LaTeX (modern packages, color-coded) |
| `\xm` | Markdown (GitHub-flavored, with checkboxes and nested lists) |

Or use `:export html`, `:export latex`, `:export markdown` ex-commands.

### Complexity stat

`\c` shows the total of items + references in the current HyperList.

---

## HyperList — the format

This section is the canonical definition. It is unchanged from the original
HyperList specification.

### A HyperList Item

A HyperList Item is a line. It can have **children** — items indented to the
right below it.

An Item has, in sequence:

1. **Starter** (optional) — Identifier or Multi-line Indicator
2. **Type** (optional) — State or Transition
3. **Content** — Element and/or Additive
4. **Separator**

#### 1. Starter

An **Identifier** is a unique reference handle: `1`, `1.1`, `1.2.3` (numeric
path) or `1A1A` (alphanumeric, equivalent to `1.1.1.1`).

A **Multi-line Indicator** is a `+` at the start of an item. Use it when a
single item spans more than one display line — the second line is indented
to the same level with a leading space:

```
+ If one Item on a certain level/indent is multi-line, all Items
 on the same level/indent must start with a plus sign ("+") or <Identifier>
```

#### 2. Type

If unclear, prefix with:
- `S:` or `|` — **State** item (something descriptive)
- `T:` or `/` — **Transition** item (something to do)

Children inherit Type unless overridden.

#### 3. Content

##### 3.1 Element (Operator | Qualifier | Substitution | Property | Description)

**Operator** — operates on an item or set of items. ALL CAPS, ends with `: `:

```
AND: 
OR: 
AND/OR: 
NOT: 
IMPLIES: 
EXAMPLE: 
EXAMPLES: 
CHOOSE: 
ONE OF THESE: 
CONTINUOUS:    (item runs concurrently with remaining items)
ENCRYPTION:    (sub-items are encrypted)
```

A literal block (HyperList markup is not interpreted) is bracketed by `\` on
its own line:

```
\
This is a block of literal text...
Where nothing counts as HyperList markup
Thus - neither this: [?] nor THIS: <Test> - are seen as markup
\
```

**Qualifier** — square brackets, qualifies the item:

| Form | Meaning |
|---|---|
| `[The mail has arrived]` | If condition is true |
| `[3]` | Do 3 times |
| `[1+]` | Do 1 or more times |
| `[2..4]` | 2 to 4 times |
| `[2, foo=true]` | 2 times while `foo=true` |
| `[?]` | Optional |
| `[? Raining]` | Same as `[Raining]` (the `?` makes it explicit) |
| `[YYYY-MM-DD]` | Timestamp |
| `[+YYYY-MM-DD]` | Wait this long before doing |
| `[<-YYYY-MM-04]` | Less than 4 days before next item |
| `[YYYY-MM-03]` | Every third of every month |
| `[Tue,Fri 12.00]` | Noon every Tue & Fri |
| `[2011-05-01+7 13.00]` | Repeat every 7 days at 1pm |
| `[_]` `[O]` `[x]` | Unchecked / in-progress / checked |

**Substitution** — curly braces, value substituted from elsewhere:

```
[fruit = apples, oranges, bananas] Eat {fruit}
```

reads as "Eat apples, then oranges, then bananas".

**Property** — attribute of the content, ends with `: `:

```
Location = Stockholm: 
Color = Green: 
```

**Description** — the main body. Most items are just a description.

##### 3.2 Additive (Reference | Tag | Comment | Quote | Change Markup)

**Reference** — angle brackets. See above.

**Tag** — hash sign + alnum: `#TODO #RememberThis #Geir`. No spaces.

**Comment** — anything in parentheses. Not executed.

**Quote** — anything in double quotes. Not executed.

**Change Markup** — used to mark edits on paper or in collaborative review:

| Markup | Meaning |
|---|---|
| `Item ##<` | Slated for deletion |
| `Item ##><Ref>` | Move item below the referenced item |
| `Item ##<-` | Indent left (move out one level) |
| `Item ##->` | Indent right (move in as child) |
| `##><Ref>##->` | Move below `<Ref>` and make it a child |
| `##John 2012-03-21##` (prefix) | Item changed (by who, when) |

#### 4. Separator

Items are separated by **newline** by default. Multiple items on one line are
joined with a **semicolon** (`;`).

The relationship between an item and its children depends on whether the
parent has a Description:

- **Parent has a Description**: child reads as "with" or "consists of"
- **Parent has no Description (just an Operator like `OR:`)**: child reads
  as "applies to", and siblings read as "and"

Examples:

```
A kitchen
    Stove
    Table
        Plates
        Knives
        Forks
```

reads as "A kitchen with stove and table with plates, knives and forks".

```
Walk the dog
    Check the weather
        [?rain] AND/OR:
            Get rain coat
            Get umbrella
        Dress for the temperature
    Get chain
```

reads as "Walk the dog: check the weather (consisting of: if rain, get
rain coat and/or get umbrella; then dress for the temperature); then get
chain".

### Self-defining

The full HyperList syntax defined in HyperList itself:

```
HyperList
    [1+] HyperList Item
        [?] Starter; OR: 
            Identifier (Numbers: Format = "1.1.1.1", Mixed: Format = "1A1A")
                [? Multi-line Item] The Identifier acts like the plus sign ("+")
            Multi-line Indicator = "+"
        [?] Type; OR: 
            State = "S:" or "|"
            Transition = "T:" or "/"
        Content; AND/OR: 
            Element; AND/OR: 
                Operator
                Qualifier
                Substitution
                Property
                Description
            Additive; AND/OR: 
                Reference
                Tag
                Comment
                Quote
                Change Markup
        Separator; OR: 
            Semicolon
            Newline
                Indent
                    Tab or asterisk
```

---

## Highlighting

scribe colors HyperList syntax 1:1 with the original `hyperlist.vim`:

| Element | Color |
|---|---|
| Identifier (numbering) | Magenta |
| Multi-line indicator (`+`) | Red |
| Property (`Name: `) | Red |
| Operator (`AND:`, `OR:`, …) | Blue |
| Qualifier (`[…]`) | Green (LimeGreen) |
| Substitution (`{…}`) | Light green |
| Hashtag (`#tag`) | Yellow |
| Reference (`<…>`) | Magenta |
| Keywords `END` `SKIP` | Magenta |
| Comment (`(…)`) | Cyan |
| Quote (`"…"`) | Cyan |
| Literal block (between `\` lines) | Italic, no other coloring |
| `*bold*` `/italic/` `_underline_` | As named |
| `TODO` `FIXME` | Black on yellow |
| State (`S:` `|`) / Transition (`T:` `/`) | Underlined when toggled by `\u` |

---

## Credits

Thanks to Jean-Dominique Warnier and Kenneth Orr for the original idea
(Warnier-Orr diagrams).

Special thanks to:
- Egil Möller — early WOIM cultivation
- Axel Liljencrantz — early WOIM methodology input
- Christian Bryn — plugin testing
- Christopher Truett — Checkbox VIM plugin
- Noah Spurrier — OpenSSL VIM plugin
- Amit Sethi — Show/Hide functionality (VIM script #1594)
- Jerry Antosh — autonumbering suggestion + testing
- Don Kelley — testing, improvements, the test suite

The `hyperlist.vim` plugin and the HyperList definition are by
**Geir Isene <g@isene.com>** — see <https://isene.org/hyperlist/>.

## License

Public domain. Use, copy, modify, distribute, and sell freely. No warranty.
