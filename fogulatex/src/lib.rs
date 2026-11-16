//! fogulatex — LaTeX para o Fogu via `fogu:ffi`: compila documentos para PDF
//! (e DVI/XDV/HTML) com o motor TeX instalado (tectonic, pdflatex, xelatex,
//! lualatex, latexmk), baixa o Tectonic quando não há nenhum, lê o log com
//! erros e avisos, converte PDF para PNG/SVG/TXT/PS com as ferramentas que
//! existirem e lê PDFs (páginas, metadados, texto).
//!
//! Convenções (Fogu 0.7+: o fogu:ffi aceita qualquer número de argumentos e
//! callbacks; ver `cb.rs` e `flx_set_sink`):
//! - Trabalhos demorados (compilar, instalar, converter) são jobs `i64` (> 0)
//!   em thread própria; `flx_dispatch` entrega os eventos (JSON) pelo callback
//!   e o último é `done` (também em `flx_result`). `flx_close` libera o job.
//! - Funções de status retornam `0` em sucesso e `-1` em erro; a mensagem
//!   fica em `flx_last_error`.
//! - Saídas (JSON ou texto) vão para o buffer do chamador (`ffi.alloc`),
//!   terminadas em NUL; retorno = tamanho escrito, `-(tamanho necessário)` se
//!   o buffer for pequeno, ou -1 em erro. Com um sink registrado
//!   (`flx_set_sink`), `buf = NULL, cap = 0` entrega a saída inteira pelo callback.

mod cb;
mod engine;
mod install;
mod job;
mod mkindex;
mod pdf;
mod texlog;

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use job::Job;

static JOBS: LazyLock<Mutex<HashMap<i64, Arc<Job>>>> = LazyLock::new(Default::default);
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn set_error(msg: impl Into<String>) {
    *LAST_ERROR.lock().unwrap() = msg.into();
}

/// Executa `f` sem deixar um panic atravessar a fronteira C.
fn guard<T>(on_error: T, f: impl FnOnce() -> Result<T, String>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            set_error(e);
            on_error
        }
        Err(_) => {
            set_error("internal panic in fogulatex");
            on_error
        }
    }
}

fn status(f: impl FnOnce() -> Result<(), String>) -> i32 {
    guard(-1, || f().map(|_| 0))
}

fn get(handle: i64) -> Result<Arc<Job>, String> {
    JOBS.lock().unwrap().get(&handle).cloned().ok_or_else(|| format!("invalid job handle {handle}"))
}

fn register(job: Job) -> i64 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    JOBS.lock().unwrap().insert(handle, Arc::new(job));
    handle
}

unsafe fn text<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Ok("");
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().map_err(|_| "string is not valid UTF-8".to_string())
}

unsafe fn json_arg(ptr: *const c_char) -> Result<Value, String> {
    let s = unsafe { text(ptr) }?;
    if s.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(s).map_err(|e| format!("invalid JSON argument: {e}"))
}

unsafe fn write_out(buf: *mut u8, cap: i64, s: &str) -> i64 {
    if let Some(n) = cb::try_sink(buf, cap, s.as_bytes()) {
        return n;
    }
    let need = s.len() + 1;
    if buf.is_null() || cap < need as i64 {
        return -(need as i64);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len());
        *buf.add(s.len()) = 0;
    }
    s.len() as i64
}

unsafe fn write_json(buf: *mut u8, cap: i64, value: Value) -> i64 {
    unsafe { write_out(buf, cap, &value.to_string()) }
}

/// Versão da biblioteca. Retorna o tamanho escrito (ver convenções).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_version(buf: *mut u8, cap: i64) -> i64 {
    unsafe { write_out(buf, cap, env!("CARGO_PKG_VERSION")) }
}

/// Última mensagem de erro. Retorna o tamanho escrito (0 se não há erro).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_last_error(buf: *mut u8, cap: i64) -> i64 {
    let msg = LAST_ERROR.lock().unwrap().clone();
    unsafe { write_out(buf, cap, &msg) }
}

/// Registra (ou remove, com NULL) o callback (Fogu 0.7+, `ffi.callback`) que
/// recebe as saídas das funções chamadas com `buf = NULL, cap = 0` (ver `cb.rs`).
#[unsafe(no_mangle)]
pub extern "C" fn flx_set_sink(sink: Option<cb::Sink>) -> i32 {
    cb::set_sink(sink);
    0
}

// ---- ferramentas --------------------------------------------------------

/// `{"platform", "tools_dir", "engines": [{name, path, version, source}],
/// "helpers": [...], "converters": [...]}`. `with_version` != 0 roda
/// `--version` em cada uma (mais lento).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_tools(with_version: i32, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || Ok(unsafe { write_json(buf, cap, job::tools_json(with_version != 0)) }))
}

/// Gera o `.ind` de um `.idx` (texto) com o makeindex interno (ver `mkindex.rs`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_make_index(idx: *const c_char, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || {
        let idx = unsafe { text(idx) }?;
        Ok(unsafe { write_out(buf, cap, &mkindex::make(idx)) })
    })
}

/// Lê um log do TeX (texto): `{"errors", "warnings", "rerun", "overfull", "underfull"}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_log_parse(log: *const c_char, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || {
        let log = unsafe { text(log) }?;
        Ok(unsafe { write_json(buf, cap, texlog::parse(log).to_json()) })
    })
}

// ---- jobs -------------------------------------------------------------

/// Compila. `opts_json`: `{"source" | "file", "jobname", "engine", "outdir",
/// "format": "pdf"|"dvi"|"xdv"|"html", "passes", "bib": "auto"|"bibtex"|"biber"|"none",
/// "inputs": [pastas], "files": {nome: conteúdo}, "shell_escape", "synctex",
/// "halt_on_error", "timeout_ms", "env": {}, "args": [], "cache_dir",
/// "only_cached", "untrusted", "clean"}`. Retorna o handle do job ou -1.
/// Eventos: `start`, `pass`, `output` (cada linha), `pass_end`, `done`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_compile(opts_json: *const c_char) -> i64 {
    guard(-1, || {
        let opts = job::CompileOpts::from_json(&unsafe { json_arg(opts_json) }?)?;
        Ok(register(Job::spawn("compile", move |ctx| job::compile(ctx, &opts))))
    })
}

/// Baixa o Tectonic. `opts_json`: `{"version", "dir", "url", "force", "timeout_ms"}`.
/// Eventos: `start`, `progress` {bytes, total}, `extract`, `done` {ok, path}.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_install(opts_json: *const c_char) -> i64 {
    guard(-1, || {
        let opts = install::InstallOpts::from_json(&unsafe { json_arg(opts_json) }?)?;
        Ok(register(Job::spawn("install", move |ctx| {
            let emit = ctx.emit.clone();
            match install::install(&opts, &|v| emit.emit(v), &ctx.cancel) {
                Ok(path) => ctx.emit.done(json!({ "ok": true, "path": path.to_string_lossy(), "version": opts.version })),
                Err(e) => ctx.emit.done(json!({ "ok": false, "error": e })),
            }
        })))
    })
}

/// Converte um PDF. `opts_json`: `{"input", "to": "png"|"jpg"|"svg"|"txt"|"ps",
/// "outdir", "prefix", "dpi", "pages": [..], "tool", "timeout_ms"}`.
/// Eventos: `start`, `output`, `done` {ok, tool, files}.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_convert(opts_json: *const c_char) -> i64 {
    guard(-1, || {
        let opts = job::ConvertOpts::from_json(&unsafe { json_arg(opts_json) }?)?;
        Ok(register(Job::spawn("convert", move |ctx| job::convert(ctx, &opts))))
    })
}

/// Espera até `timeout_ms` pelo primeiro evento e entrega até `max` eventos
/// (os já enfileirados, sem esperar de novo) chamando `on_event(json, len, bin, len)`
/// na thread de quem chamou. Retorna quantos foram entregues ou -1.
#[unsafe(no_mangle)]
pub extern "C" fn flx_dispatch(handle: i64, timeout_ms: i64, max: i64, on_event: Option<cb::EventFn>) -> i64 {
    guard(-1, || {
        let f = on_event.ok_or("dispatch: on_event is NULL")?;
        let job = get(handle)?;
        let n = job.drain(Duration::from_millis(timeout_ms.max(0) as u64), max.max(1) as usize, |ev| cb::event(f, &ev.json, &[]));
        Ok(n as i64)
    })
}

/// O evento `done` do job (JSON), ou 0 se ainda não terminou.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_result(handle: i64, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || match get(handle)?.result() {
        Some(v) => Ok(unsafe { write_json(buf, cap, v) }),
        None => Ok(0),
    })
}

/// Cancela o job (mata o processo em execução).
#[unsafe(no_mangle)]
pub extern "C" fn flx_cancel(handle: i64) -> i32 {
    status(|| {
        get(handle)?.cancel();
        Ok(())
    })
}

/// Cancela se preciso, espera a thread e libera o handle.
#[unsafe(no_mangle)]
pub extern "C" fn flx_close(handle: i64) -> i32 {
    status(|| {
        let job = JOBS.lock().unwrap().remove(&handle).ok_or_else(|| format!("invalid job handle {handle}"))?;
        job.close();
        Ok(())
    })
}

// ---- PDF ----------------------------------------------------------------

/// `{"pages", "version", "size", "title", "author", "subject", "keywords",
/// "creator", "producer", "page_sizes", "encrypted"}`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_pdf_info(path: *const c_char, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || {
        let path = unsafe { text(path) }?;
        Ok(unsafe { write_json(buf, cap, pdf::info(Path::new(path))?) })
    })
}

/// Texto das páginas (`pages_json`: lista 1-based, vazia = todas), páginas
/// separadas por form feed (`\f`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flx_pdf_text(path: *const c_char, pages_json: *const c_char, buf: *mut u8, cap: i64) -> i64 {
    guard(-1, || {
        let path = unsafe { text(path) }?;
        let pages_text = unsafe { text(pages_json) }?;
        let pages: Vec<u32> = if pages_text.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str::<Vec<u32>>(pages_text).map_err(|e| format!("pages: {e}"))?
        };
        Ok(unsafe { write_out(buf, cap, &pdf::text(Path::new(path), &pages)?) })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn call(f: impl FnOnce(*mut u8, i64) -> i64) -> (i64, String) {
        let mut buf = vec![0u8; 1 << 16];
        let n = f(buf.as_mut_ptr(), buf.len() as i64);
        let s = if n >= 0 { String::from_utf8(buf[..n as usize].to_vec()).unwrap() } else { String::new() };
        (n, s)
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn last_error() -> String {
        call(|b, n| unsafe { flx_last_error(b, n) }).1
    }

    #[test]
    fn version_tools_and_log() {
        let (n, v) = call(|b, c| unsafe { flx_version(b, c) });
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
        assert_eq!(n as usize, v.len());
        let (_, j) = call(|b, n| unsafe { flx_tools(0, b, n) });
        let t: Value = serde_json::from_str(&j).unwrap();
        assert!(t["engines"].is_array());
        assert!(t["platform"].as_str().unwrap().contains('-'));
        let log = c("./a.tex:3: Undefined control sequence.\nLaTeX Warning: Rerun to get cross-references right.\n");
        let (_, j) = call(|b, n| unsafe { flx_log_parse(log.as_ptr(), b, n) });
        let r: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(r["errors"][0]["line"], 3);
        assert_eq!(r["rerun"], true);
        assert_eq!(flx_close(999), -1);
        assert!(last_error().contains("invalid job"));
    }

    #[test]
    fn pdf_info_and_text() {
        // Um PDF mínimo gerado com lopdf, para testar sem motor TeX.
        use lopdf::{Dictionary, Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let resources_id = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
        let content = lopdf::content::Content {
            operations: vec![
                lopdf::content::Operation::new("BT", vec![]),
                lopdf::content::Operation::new("Tf", vec!["F1".into(), 24.into()]),
                lopdf::content::Operation::new("Td", vec![72.into(), 700.into()]),
                lopdf::content::Operation::new("Tj", vec![Object::string_literal("Ola Fogu")]),
                lopdf::content::Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        let pages = dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1, "Resources" => resources_id };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut info = Dictionary::new();
        info.set("Title", Object::string_literal("Teste"));
        let info_id = doc.add_object(info);
        doc.trailer.set("Info", info_id);
        let path = std::env::temp_dir().join(format!("fogulatex-test-{}.pdf", std::process::id()));
        doc.save(&path).unwrap();

        let p = c(path.to_str().unwrap());
        let (n, j) = call(|b, n| unsafe { flx_pdf_info(p.as_ptr(), b, n) });
        assert!(n > 0, "{}", last_error());
        let v: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["pages"], 1);
        assert_eq!(v["title"], "Teste");
        assert_eq!(v["page_sizes"][0][0], 595.0);
        let empty = c("");
        let (_, t) = call(|b, n| unsafe { flx_pdf_text(p.as_ptr(), empty.as_ptr(), b, n) });
        assert!(t.contains("Ola Fogu"), "{t}");
        let missing = c("nope.pdf");
        assert_eq!(call(|b, n| unsafe { flx_pdf_info(missing.as_ptr(), b, n) }).0, -1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compile_without_file_fails_cleanly() {
        let opts = c(r#"{"file": "nao-existe.tex"}"#);
        let h = unsafe { flx_compile(opts.as_ptr()) };
        assert!(h > 0, "{}", last_error());
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut done = String::new();
        while std::time::Instant::now() < deadline {
            let (n, s) = call(|b, n| unsafe { flx_result(h, b, n) });
            if n > 0 {
                done = s;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let v: Value = serde_json::from_str(&done).expect("done");
        assert_eq!(v["ok"], false);
        assert!(v["error"].is_string());
        assert_eq!(flx_close(h), 0);
        let bad = c(r#"{"format": "pdf"}"#);
        assert_eq!(unsafe { flx_compile(bad.as_ptr()) }, -1);
        assert!(last_error().contains("source"));
    }
}
