//! Leitura de PDFs com lopdf: páginas, tamanhos, metadados e texto.

use std::path::Path;

use lopdf::{Dictionary, Document, Object};
use serde_json::{Value, json};

fn pdf_string(doc: &Document, obj: &Object) -> Option<String> {
    let obj = doc.dereference(obj).ok()?.1;
    let bytes = obj.as_str().ok()?;
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..].chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return Some(String::from_utf16_lossy(&units));
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    // PDFDocEncoding ~ Latin-1 para o que importa.
    Some(bytes.iter().map(|&b| b as char).collect())
}

fn info_field(doc: &Document, info: Option<&Dictionary>, key: &[u8]) -> Value {
    info.and_then(|d| d.get(key).ok()).and_then(|o| pdf_string(doc, o)).map(Value::String).unwrap_or(Value::Null)
}

/// MediaBox da página (herdado do pai se preciso): [w, h] em pontos.
fn media_box(doc: &Document, page: lopdf::ObjectId) -> Option<[f64; 2]> {
    let mut id = page;
    for _ in 0..32 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(mb) = dict.get(b"MediaBox") {
            let arr = doc.dereference(mb).ok()?.1.as_array().ok()?;
            let n = |o: &Object| -> Option<f64> {
                match doc.dereference(o).ok()?.1 {
                    Object::Integer(i) => Some(*i as f64),
                    Object::Real(r) => Some(*r as f64),
                    _ => None,
                }
            };
            if arr.len() == 4 {
                let (x0, y0, x1, y1) = (n(&arr[0])?, n(&arr[1])?, n(&arr[2])?, n(&arr[3])?);
                return Some([(x1 - x0).abs(), (y1 - y0).abs()]);
            }
        }
        match dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok()) {
            Some(parent) => id = parent,
            None => return None,
        }
    }
    None
}

/// `{ pages, version, size, title, author, subject, keywords, creator,
/// producer, page_sizes: [[w, h] em pt], encrypted }`
pub fn info(path: &Path) -> Result<Value, String> {
    let doc = Document::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let pages = doc.get_pages();
    let info = doc.trailer.get(b"Info").ok().and_then(|o| doc.dereference(o).ok()).and_then(|(_, o)| o.as_dict().ok());
    let sizes: Vec<Value> = pages
        .values()
        .map(|&id| media_box(&doc, id).map(|[w, h]| json!([(w * 100.0).round() / 100.0, (h * 100.0).round() / 100.0])).unwrap_or(Value::Null))
        .collect();
    Ok(json!({
        "path": path.to_string_lossy(),
        "pages": pages.len(),
        "version": doc.version,
        "size": size,
        "title": info_field(&doc, info, b"Title"),
        "author": info_field(&doc, info, b"Author"),
        "subject": info_field(&doc, info, b"Subject"),
        "keywords": info_field(&doc, info, b"Keywords"),
        "creator": info_field(&doc, info, b"Creator"),
        "producer": info_field(&doc, info, b"Producer"),
        "page_sizes": sizes,
        "encrypted": doc.is_encrypted(),
    }))
}

/// Texto das páginas pedidas (1-based; vazio = todas), separadas por `\f`.
pub fn text(path: &Path, pages: &[u32]) -> Result<String, String> {
    let doc = Document::load(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let all: Vec<u32> = if pages.is_empty() { doc.get_pages().keys().copied().collect() } else { pages.to_vec() };
    let mut out = String::new();
    for (i, p) in all.iter().enumerate() {
        if i > 0 {
            out.push('\u{c}');
        }
        match doc.extract_text(&[*p]) {
            Ok(t) => out.push_str(&t),
            Err(e) => return Err(format!("página {p}: {e}")),
        }
    }
    Ok(out)
}

/// Número de páginas (0 se o arquivo não abrir).
pub fn page_count(path: &Path) -> usize {
    Document::load(path).map(|d| d.get_pages().len()).unwrap_or(0)
}
