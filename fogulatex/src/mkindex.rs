//! Um makeindex mínimo: lê o `.idx` (`\indexentry{termo}{página}`) e grava o
//! `.ind` (`\begin{theindex} ... \end{theindex}`) que `\printindex` espera.
//! Usado quando o `makeindex` não está instalado (o Tectonic não o traz).
//! Suporta `termo!subtermo` (um nível), `chave@texto` (ordenação) e
//! `termo|textbf` (formatação da página).

use std::collections::BTreeMap;

#[derive(Default)]
struct Entry {
    text: String,
    pages: Vec<String>,
    subs: BTreeMap<String, (String, Vec<String>)>,
}

/// `\indexentry{...}{...}` -> (termo, página), respeitando chaves aninhadas.
fn parse_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("\\indexentry")?;
    let (term, rest) = braced(rest)?;
    let (page, _) = braced(rest)?;
    Some((term, page))
}

fn braced(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if first != '{' {
        return None;
    }
    let mut depth = 1;
    for (i, c) in chars {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((s[1..i].to_string(), &s[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// (chave de ordenação, texto, formatação da página)
fn split_term(term: &str) -> (String, String, Option<String>) {
    let (term, fmt) = match term.rsplit_once('|') {
        Some((t, f)) => (t, Some(f.to_string())),
        None => (term, None),
    };
    let (key, text) = match term.split_once('@') {
        Some((k, t)) => (k.to_string(), t.to_string()),
        None => (term.to_string(), term.to_string()),
    };
    (key.to_lowercase(), text, fmt)
}

fn page_text(page: &str, fmt: &Option<String>) -> String {
    match fmt {
        Some(f) if !f.is_empty() => format!("\\{f}{{{page}}}"),
        _ => page.to_string(),
    }
}

fn push_page(pages: &mut Vec<String>, p: String) {
    if !pages.contains(&p) {
        pages.push(p);
    }
}

/// Gera o conteúdo do `.ind` a partir do texto do `.idx`.
pub fn make(idx: &str) -> String {
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    for line in idx.lines() {
        let Some((term, page)) = parse_line(line) else { continue };
        let (main, sub) = match term.split_once('!') {
            Some((m, s)) => (m, Some(s)),
            None => (term.as_str(), None),
        };
        let (mkey, mtext, mfmt) = split_term(main);
        let entry = entries.entry(mkey).or_default();
        if entry.text.is_empty() {
            entry.text = mtext;
        }
        match sub {
            None => push_page(&mut entry.pages, page_text(&page, &mfmt)),
            Some(s) => {
                let (skey, stext, sfmt) = split_term(s);
                let sub = entry.subs.entry(skey).or_insert_with(|| (stext, Vec::new()));
                push_page(&mut sub.1, page_text(&page, &sfmt));
            }
        }
    }
    let mut out = String::from("\\begin{theindex}\n\n");
    let mut last_initial: Option<char> = None;
    for entry in entries.values() {
        let initial = entry.text.chars().next().map(|c| c.to_lowercase().next().unwrap_or(c));
        if last_initial.is_some() && initial != last_initial {
            out.push_str("\n  \\indexspace\n\n");
        }
        last_initial = initial;
        out.push_str("  \\item ");
        out.push_str(&entry.text);
        if !entry.pages.is_empty() {
            out.push_str(", ");
            out.push_str(&entry.pages.join(", "));
        }
        out.push('\n');
        for (text, pages) in entry.subs.values() {
            out.push_str("    \\subitem ");
            out.push_str(text);
            if !pages.is_empty() {
                out.push_str(", ");
                out.push_str(&pages.join(", "));
            }
            out.push('\n');
        }
    }
    out.push_str("\n\\end{theindex}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_an_index() {
        let idx = "\\indexentry{Fogu}{1}\n\\indexentry{Fogu}{3}\n\\indexentry{Fogu!FFI}{2}\n\\indexentry{fogu}{1}\n\\indexentry{LaTeX|textbf}{2}\n\\indexentry{aaa@\\'{E}}{4}\n";
        let ind = make(idx);
        assert!(ind.starts_with("\\begin{theindex}\n"));
        assert!(ind.contains("\\item \\'{E}, 4\n"), "{ind}");
        assert!(ind.contains("\\item Fogu, 1, 3\n    \\subitem FFI, 2\n"), "{ind}");
        assert!(ind.contains("\\item LaTeX, \\textbf{2}\n"), "{ind}");
        assert!(ind.contains("\\indexspace"));
        assert!(ind.ends_with("\\end{theindex}\n"));
        assert_eq!(parse_line("\\indexentry{a{b}c}{12}"), Some(("a{b}c".into(), "12".into())));
    }
}
