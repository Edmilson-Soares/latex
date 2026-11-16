//! Leitura do log do TeX (e da saída do tectonic): erros com arquivo/linha,
//! avisos (LaTeX, pacotes, fontes, caixas cheias) e se é preciso rodar de novo.

use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub kind: &'static str,
}

impl Diag {
    pub fn to_json(&self) -> Value {
        json!({ "message": self.message, "file": self.file, "line": self.line, "kind": self.kind })
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub errors: Vec<Diag>,
    pub warnings: Vec<Diag>,
    pub rerun: bool,
    pub overfull: usize,
    pub underfull: usize,
}

impl Report {
    pub fn to_json(&self) -> Value {
        json!({
            "errors": self.errors.iter().map(Diag::to_json).collect::<Vec<_>>(),
            "warnings": self.warnings.iter().map(Diag::to_json).collect::<Vec<_>>(),
            "rerun": self.rerun,
            "overfull": self.overfull,
            "underfull": self.underfull,
        })
    }

}

/// `caminho:linha: mensagem` (modo `-file-line-error`).
fn file_line_error(line: &str) -> Option<Diag> {
    // Evita confundir "C:\x\y.tex:12: msg" (Windows) com um "C:" de drive.
    let mut search_from = 0;
    loop {
        let rel = line[search_from..].find(':')?;
        let i = search_from + rel;
        let rest = &line[i + 1..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with(": ") {
            let file = line[..i].trim();
            let looks_like_file = file.ends_with(".tex")
                || file.ends_with(".sty")
                || file.ends_with(".cls")
                || file.ends_with(".def")
                || file.ends_with(".ltx")
                || file.ends_with(".bib")
                || file.ends_with(".bbl")
                || file.ends_with(".aux")
                || file.ends_with(".clo");
            if looks_like_file {
                let message = rest[digits.len() + 2..].trim().to_string();
                if message.is_empty() {
                    return None;
                }
                return Some(Diag { message, file: Some(file.to_string()), line: digits.parse().ok(), kind: "error" });
            }
        }
        search_from = i + 1;
    }
}

/// `... on input line 42.` -> 42
fn input_line(s: &str) -> Option<u32> {
    let i = s.rfind("on input line ")?;
    let digits: String = s[i + 14..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Lê o log (ou a saída do motor). `log` pode ter linhas quebradas em 79
/// colunas; os avisos que continuam em linhas seguintes são juntados.
pub fn parse(log: &str) -> Report {
    let lines: Vec<&str> = log.lines().collect();
    let mut r = Report::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();

        if let Some(d) = file_line_error(trimmed) {
            r.errors.push(d);
            i += 1;
            continue;
        }

        if let Some(msg) = trimmed.strip_prefix("! ") {
            // Erro clássico: "! Undefined control sequence." seguido de "l.12 \foo"
            let mut diag = Diag { message: msg.trim().to_string(), file: None, line: None, kind: "error" };
            let mut j = i + 1;
            while j < lines.len() && j <= i + 12 {
                let l = lines[j].trim_start();
                if let Some(rest) = l.strip_prefix("l.") {
                    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    diag.line = digits.parse().ok();
                    let ctx = rest[digits.len()..].trim();
                    if !ctx.is_empty() {
                        diag.message = format!("{} (l.{} {})", diag.message, digits, ctx);
                    }
                    break;
                }
                if l.starts_with("! ") {
                    break;
                }
                j += 1;
            }
            // Não duplica um erro já capturado por file:line:.
            if !r.errors.iter().any(|e| e.message == diag.message || diag.message.starts_with(&e.message)) {
                r.errors.push(diag);
            }
            i += 1;
            continue;
        }

        if let Some(msg) = trimmed.strip_prefix("error: ") {
            // tectonic: "error: halted on potentially-recoverable error as specified"
            let m = msg.trim();
            if !m.is_empty() && !m.starts_with("halted on") {
                r.errors.push(Diag { message: m.to_string(), file: None, line: None, kind: "error" });
            }
            i += 1;
            continue;
        }

        let is_warning = trimmed.starts_with("LaTeX Warning:")
            || trimmed.starts_with("LaTeX Font Warning:")
            || trimmed.starts_with("Class ") && trimmed.contains(" Warning:")
            || trimmed.starts_with("Package ") && trimmed.contains(" Warning:")
            || trimmed.starts_with("pdfTeX warning")
            || trimmed.starts_with("warning: ");
        if is_warning {
            // Junta as linhas de continuação (até a linha em branco).
            let mut text = trimmed.to_string();
            let mut j = i + 1;
            while j < lines.len() && j <= i + 4 {
                let l = lines[j];
                let starts_new = ["! ", "LaTeX", "Package", "Class ", "error: ", "warning: ", "note: ", "Overfull", "Underfull", "l."];
                if l.trim().is_empty() || l.starts_with(')') || l.starts_with('[') || starts_new.iter().any(|p| l.starts_with(p)) {
                    break;
                }
                // "(hyperref)   removing ..." continua o aviso; "(./cap1.tex" abre um arquivo.
                let cont = match l.strip_prefix('(') {
                    Some(rest) => match rest.find(')') {
                        Some(k) if !rest[..k].contains(['/', '\\', '.']) => rest[k + 1..].trim(),
                        _ => break,
                    },
                    None => l.trim(),
                };
                if cont.is_empty() {
                    break;
                }
                text.push(' ');
                text.push_str(cont);
                j += 1;
            }
            let lower = text.to_lowercase();
            if lower.contains("rerun") || lower.contains("undefined references") || lower.contains("label(s) may have changed") {
                r.rerun = true;
            }
            let message = text.trim_end_matches('.').to_string();
            r.warnings.push(Diag { line: input_line(&text), message, file: None, kind: "warning" });
            i = j.max(i + 1);
            continue;
        }

        if trimmed.starts_with("Overfull \\") {
            r.overfull += 1;
            if r.overfull <= 20 {
                r.warnings.push(Diag { message: trimmed.to_string(), file: None, line: None, kind: "overfull" });
            }
        } else if trimmed.starts_with("Underfull \\") {
            r.underfull += 1;
            if r.underfull <= 20 {
                r.warnings.push(Diag { message: trimmed.to_string(), file: None, line: None, kind: "underfull" });
            }
        } else if trimmed.contains("Rerun to get") || trimmed.contains("rerun LaTeX") {
            r.rerun = true;
        }
        i += 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_with_file_and_line() {
        let log = "This is pdfTeX\n./main.tex:12: Undefined control sequence.\nl.12 \\foo\n! Emergency stop.\n";
        let r = parse(log);
        assert_eq!(r.errors.len(), 2);
        assert_eq!(r.errors[0].file.as_deref(), Some("./main.tex"));
        assert_eq!(r.errors[0].line, Some(12));
        assert_eq!(r.errors[0].message, "Undefined control sequence.");
        assert_eq!(r.errors[1].message, "Emergency stop.");

        let r = parse("! Undefined control sequence.\n<recently read> \\foo\nl.3 \\foo\n");
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].line, Some(3));
        assert!(r.errors[0].message.contains("(l.3 \\foo)"));

        let r = parse("C:\\docs\\a.tex:4: Missing $ inserted.\n");
        assert_eq!(r.errors[0].file.as_deref(), Some("C:\\docs\\a.tex"));
        assert_eq!(r.errors[0].line, Some(4));
    }

    #[test]
    fn warnings_and_rerun() {
        let log = "LaTeX Warning: Reference `fig:a' on page 1 undefined on input line 20.\n\nPackage hyperref Warning: Token not allowed in a PDF string\n(hyperref)                removing `\\label' on input line 7.\n\nOverfull \\hbox (12.0pt too wide) in paragraph at lines 5--6\nLaTeX Warning: Label(s) may have changed. Rerun to get cross-references right.\n";
        let r = parse(log);
        assert!(r.rerun);
        assert_eq!(r.overfull, 1);
        assert_eq!(r.warnings.len(), 4);
        assert_eq!(r.warnings[0].line, Some(20));
        assert!(r.warnings[1].message.contains("removing"), "{}", r.warnings[1].message);
        assert_eq!(r.warnings[1].line, Some(7));
        assert_eq!(r.errors.len(), 0);
        let t = parse("warning: bundle fetch slow\nerror: halted on potentially-recoverable error as specified\nerror: the TeX engine failed\n");
        assert_eq!(t.errors.len(), 1);
        assert_eq!(t.warnings.len(), 1);
    }
}
