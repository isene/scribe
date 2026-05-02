//! HyperList → HTML / LaTeX / Markdown exporters.
//!
//! Direct port of `hyper/src/export.rs` (also written for the same
//! HyperList format), adapted to operate on a raw `&str` buffer
//! instead of a parsed Document. Indent is read from leading TAB / `*`
//! using `fold::fold_level`.
//!
//! Mirrors the `\H` / `\L` / `\M` actions in hyperlist.vim.

use crate::fold;

const C_PROP:   &str = "#CC0000";
const C_QUAL:   &str = "#00AA00";
const C_OP:     &str = "#0000CC";
const C_REF:    &str = "#AA00AA";
const C_PAREN:  &str = "#00AAAA";
const C_SUBST:  &str = "#AA8800";
const C_TAG:    &str = "#CC5500";

#[derive(Clone, Copy, Debug)]
enum Role {
    Plain,
    Property,
    Operator,
    Qualifier,
    Reference,
    Paren,
    String,
    Subst,
    Tag,
    Bold,
    Italic,
    Underline,
}

fn tokenize(line: &str) -> Vec<(String, Role)> {
    let mut out: Vec<(String, Role)> = Vec::new();
    if line.is_empty() { return out; }
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    if let Some(op_end) = detect_operator(&chars) {
        out.push((chars[..op_end].iter().collect(), Role::Operator));
        i = op_end;
    } else if let Some(prop_end) = detect_property_chain(&chars) {
        out.push((chars[..prop_end].iter().collect(), Role::Property));
        i = prop_end;
    }

    while i < chars.len() {
        let c = chars[i];
        if c == '[' {
            if let Some(end) = find_matching(&chars, i, '[', ']') {
                out.push((chars[i..=end].iter().collect(), Role::Qualifier));
                i = end + 1; continue;
            }
        }
        if c == '<' {
            if let Some(end) = find_matching(&chars, i, '<', '>') {
                out.push((chars[i..=end].iter().collect(), Role::Reference));
                i = end + 1; continue;
            }
        }
        if c == '{' {
            if let Some(end) = find_matching(&chars, i, '{', '}') {
                out.push((chars[i..=end].iter().collect(), Role::Subst));
                i = end + 1; continue;
            }
        }
        if c == '(' {
            if let Some(end) = find_matching(&chars, i, '(', ')') {
                out.push((chars[i..=end].iter().collect(), Role::Paren));
                i = end + 1; continue;
            }
        }
        if c == '"' {
            if let Some(rel) = chars[i+1..].iter().position(|&x| x == '"') {
                let span: String = chars[i..=i + 1 + rel].iter().collect();
                out.push((span, Role::String));
                i = i + 2 + rel; continue;
            }
        }
        if c == '#' && i + 1 < chars.len() && chars[i+1].is_alphanumeric() {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-') { j += 1; }
            out.push((chars[i..j].iter().collect(), Role::Tag));
            i = j; continue;
        }
        if c == ';' {
            out.push((";".into(), Role::Qualifier));
            i += 1; continue;
        }
        if c == '*' && i + 1 < chars.len() && chars[i+1] != ' ' {
            if let Some(rel) = chars[i+1..].iter().position(|&x| x == '*') {
                let inner: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push((inner, Role::Bold));
                i = i + 2 + rel; continue;
            }
        }
        if c == '_' && i + 1 < chars.len() && chars[i+1] != ' ' {
            if let Some(rel) = chars[i+1..].iter().position(|&x| x == '_') {
                let inner: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push((inner, Role::Underline));
                i = i + 2 + rel; continue;
            }
        }
        if c == '/' && i + 1 < chars.len() && chars[i+1] != ' ' && chars[i+1] != '/' {
            if let Some(rel) = chars[i+1..].iter().position(|&x| x == '/') {
                let inner: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push((inner, Role::Italic));
                i = i + 2 + rel; continue;
            }
        }
        let start = i;
        while i < chars.len() {
            let c = chars[i];
            if c == '[' || c == '<' || c == '{' || c == '(' || c == '"' || c == '#' || c == ';' || c == '*' || c == '_' || c == '/' { break; }
            i += 1;
        }
        if i > start {
            out.push((chars[start..i].iter().collect(), Role::Plain));
        } else {
            out.push((c.to_string(), Role::Plain));
            i += 1;
        }
    }
    out
}

fn detect_operator(chars: &[char]) -> Option<usize> {
    let mut i = 0;
    let mut seen_upper = false;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_uppercase() || c == '-' || c == '_' || c.is_ascii_digit() || c == '/' {
            if c.is_ascii_uppercase() { seen_upper = true; }
            i += 1;
        } else { break; }
    }
    if !seen_upper || i == 0 { return None; }
    if i + 1 < chars.len() && chars[i] == ':' && chars[i + 1] == ' ' { Some(i + 2) } else { None }
}

fn detect_property_chain(chars: &[char]) -> Option<usize> {
    let mut i = 0;
    let mut last_match = 0;
    loop {
        let segment_start = i;
        while i < chars.len() {
            let c = chars[i];
            if c == '\n' || c == '[' || c == '<' || c == '(' || c == '{' || c == '"' { return None; }
            if c == ':' { break; }
            i += 1;
        }
        if i >= chars.len() || chars[i] != ':' { break; }
        if i + 1 < chars.len() && chars[i + 1] != ' ' { return None; }
        if i == segment_start { return None; }
        i += if i + 1 < chars.len() { 2 } else { 1 };
        last_match = i;
        let mut look = i;
        let mut found_another = false;
        while look < chars.len() {
            let c = chars[look];
            if c == ':' && look + 1 < chars.len() && chars[look + 1] == ' ' { found_another = true; break; }
            if c == ' ' || c == '\n' || c == '[' || c == '<' || c == '(' || c == '{' { break; }
            look += 1;
        }
        if !found_another { break; }
    }
    if last_match > 0 { Some(last_match) } else { None }
}

fn find_matching(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0;
    for i in start..chars.len() {
        if chars[i] == open { depth += 1; }
        else if chars[i] == close {
            depth -= 1;
            if depth == 0 { return Some(i); }
        }
    }
    None
}

/// Parse buffer text into (depth, body) tuples. Skips empty lines as
/// emit-as-blank (depth 0, empty body).
fn parse_lines(text: &str) -> Vec<(usize, String)> {
    text.lines().map(|l| {
        let depth = fold::fold_level(l);
        let body: String = l.chars().skip(depth).collect();
        (depth, body)
    }).collect()
}

// ── HTML emitter ──────────────────────────────────────────────────────
fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

fn line_to_html(line: &str) -> String {
    let toks = tokenize(line);
    let mut out = String::new();
    for (text, role) in toks {
        let esc = esc_html(&text);
        match role {
            Role::Plain      => out.push_str(&esc),
            Role::Property   => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_PROP, esc)),
            Role::Operator   => out.push_str(&format!("<span style=\"color:{};font-weight:bold\">{}</span>", C_OP, esc)),
            Role::Qualifier  => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_QUAL, esc)),
            Role::Reference  => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_REF, esc)),
            Role::Paren      => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_PAREN, esc)),
            Role::String     => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_PAREN, esc)),
            Role::Subst      => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_SUBST, esc)),
            Role::Tag        => out.push_str(&format!("<span style=\"color:{}\">{}</span>", C_TAG, esc)),
            Role::Bold       => out.push_str(&format!("<strong>{}</strong>", esc)),
            Role::Italic     => out.push_str(&format!("<em>{}</em>", esc)),
            Role::Underline  => out.push_str(&format!("<u>{}</u>", esc)),
        }
    }
    out
}

pub fn to_html(text: &str, title: &str) -> String {
    let mut s = String::new();
    s.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head>\n");
    s.push_str("<meta charset=\"utf-8\">\n");
    s.push_str(&format!("<title>{}</title>\n", esc_html(title)));
    s.push_str("<style>\n");
    s.push_str("body{font-family:Menlo,Consolas,monospace;font-size:11pt;background:#fff;color:#222;padding:2em;line-height:1.4}\n");
    s.push_str("h1{font-size:14pt;border-bottom:1px solid #aaa;padding-bottom:.3em}\n");
    s.push_str(".hl{white-space:pre-wrap}\n");
    s.push_str(".hl-line{display:block}\n");
    s.push_str("</style></head><body>\n");
    s.push_str(&format!("<h1>{}</h1>\n", esc_html(title)));
    s.push_str("<div class=\"hl\">\n");
    for (depth, body) in parse_lines(text) {
        if body.is_empty() { s.push_str("<span class=\"hl-line\">&nbsp;</span>\n"); continue; }
        let indent = "&nbsp;&nbsp;".repeat(depth * 2);
        let line = line_to_html(&body);
        s.push_str(&format!("<span class=\"hl-line\">{}{}</span>\n", indent, line));
    }
    s.push_str("</div>\n</body></html>\n");
    s
}

// ── LaTeX emitter ─────────────────────────────────────────────────────
fn esc_latex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '{'  => out.push_str("\\{"),
            '}'  => out.push_str("\\}"),
            '$'  => out.push_str("\\$"),
            '&'  => out.push_str("\\&"),
            '%'  => out.push_str("\\%"),
            '#'  => out.push_str("\\#"),
            '_'  => out.push_str("\\_"),
            '^'  => out.push_str("\\^{}"),
            '~'  => out.push_str("\\~{}"),
            '<'  => out.push_str("\\textless{}"),
            '>'  => out.push_str("\\textgreater{}"),
            _    => out.push(c),
        }
    }
    out
}

fn role_color_latex(role: Role) -> Option<&'static str> {
    match role {
        Role::Property  => Some("hlprop"),
        Role::Operator  => Some("hlop"),
        Role::Qualifier => Some("hlqual"),
        Role::Reference => Some("hlref"),
        Role::Paren | Role::String => Some("hlparen"),
        Role::Subst     => Some("hlsubst"),
        Role::Tag       => Some("hltag"),
        _               => None,
    }
}

fn line_to_latex(line: &str) -> String {
    let toks = tokenize(line);
    let mut out = String::new();
    for (text, role) in toks {
        let esc = esc_latex(&text);
        match role {
            Role::Plain      => out.push_str(&esc),
            Role::Bold       => out.push_str(&format!("\\textbf{{{}}}", esc)),
            Role::Italic     => out.push_str(&format!("\\textit{{{}}}", esc)),
            Role::Underline  => out.push_str(&format!("\\underline{{{}}}", esc)),
            Role::Operator   => out.push_str(&format!("{{\\color{{hlop}}\\textbf{{{}}}}}", esc)),
            r => {
                if let Some(c) = role_color_latex(r) {
                    out.push_str(&format!("{{\\color{{{}}}{}}}", c, esc));
                } else {
                    out.push_str(&esc);
                }
            }
        }
    }
    out
}

pub fn to_latex(text: &str, title: &str) -> String {
    let mut s = String::new();
    s.push_str("\\documentclass[10pt,a4paper]{article}\n");
    s.push_str("\\usepackage[margin=1.5cm]{geometry}\n");
    s.push_str("\\usepackage[T1]{fontenc}\n");
    s.push_str("\\usepackage[utf8]{inputenc}\n");
    s.push_str("\\usepackage{xcolor}\n");
    s.push_str("\\usepackage{enumitem}\n");
    s.push_str("\\definecolor{hlprop}{HTML}{CC0000}\n");
    s.push_str("\\definecolor{hlqual}{HTML}{00AA00}\n");
    s.push_str("\\definecolor{hlop}{HTML}{0000CC}\n");
    s.push_str("\\definecolor{hlref}{HTML}{AA00AA}\n");
    s.push_str("\\definecolor{hlparen}{HTML}{00AAAA}\n");
    s.push_str("\\definecolor{hlsubst}{HTML}{AA8800}\n");
    s.push_str("\\definecolor{hltag}{HTML}{CC5500}\n");
    s.push_str("\\setlength{\\parindent}{0pt}\n");
    s.push_str("\\begin{document}\n");
    s.push_str(&format!("\\section*{{{}}}\n", esc_latex(title)));
    s.push_str("\\begin{description}[leftmargin=0pt,labelwidth=0pt,labelindent=0pt,style=multiline]\n");
    s.push_str("\\ttfamily\\small\n");
    for (depth, body) in parse_lines(text) {
        if body.is_empty() { s.push_str("\\\\[3pt]\n"); continue; }
        let indent: String = "\\hspace*{1.5em}".repeat(depth);
        let line = line_to_latex(&body);
        s.push_str(&format!("{}{}\\\\\n", indent, line));
    }
    s.push_str("\\end{description}\n");
    s.push_str("\\end{document}\n");
    s
}

// ── Markdown emitter ──────────────────────────────────────────────────
fn line_to_markdown(line: &str) -> String {
    let toks = tokenize(line);
    let mut out = String::new();
    for (text, role) in toks {
        match role {
            Role::Bold      => out.push_str(&format!("**{}**", text)),
            Role::Italic    => out.push_str(&format!("*{}*", text)),
            Role::Underline => out.push_str(&format!("<u>{}</u>", text)),
            _ => out.push_str(&text),
        }
    }
    out
}

pub fn to_markdown(text: &str, title: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {}\n\n", title));
    for (depth, body) in parse_lines(text) {
        if body.is_empty() { s.push('\n'); continue; }
        let indent = "  ".repeat(depth);
        s.push_str(&format!("{}- {}\n", indent, line_to_markdown(&body)));
    }
    s.push_str("\n---\n\n## Source\n\n```hyperlist\n");
    for (depth, body) in parse_lines(text) {
        if body.is_empty() { s.push('\n'); continue; }
        for _ in 0..depth { s.push('\t'); }
        s.push_str(&body);
        s.push('\n');
    }
    s.push_str("```\n");
    s
}
