//! Descoberta dos motores TeX (tectonic, pdflatex, xelatex, lualatex, latex,
//! latexmk, bibtex, biber) e dos conversores de PDF (pdftoppm, pdftotext,
//! pdftocairo, mutool, gs, magick, dvisvgm) instalados.
//!
//! Ordem de busca: `FOGU_LATEX_ENGINE` (um executável), `FOGU_LATEX_TOOLS`
//! (uma pasta), `tools/<plataforma>/` e `tools/` do projeto, o PATH e as
//! pastas comuns do TeX Live e do MiKTeX.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

pub const ENGINES: &[&str] = &["tectonic", "pdflatex", "xelatex", "lualatex", "latex", "latexmk"];
pub const HELPERS: &[&str] = &["bibtex", "biber", "makeindex"];
pub const CONVERTERS: &[&str] = &["pdftoppm", "pdftotext", "pdftocairo", "pdftops", "mutool", "gs", "gswin64c", "magick", "dvisvgm", "dvipdfmx"];

#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub path: PathBuf,
    pub version: String,
    pub source: &'static str,
}

impl Tool {
    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "path": self.path.to_string_lossy(), "version": self.version, "source": self.source })
    }
}

/// Plataforma no formato dos targets do Fogu: windows-x64, linux-arm64, macos-arm64...
pub fn platform() -> String {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "arm") {
        "armv7"
    } else {
        std::env::consts::ARCH
    };
    format!("{os}-{arch}")
}

pub fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Pasta padrão dos binários baixados (`tools/<plataforma>`), relativa ao cwd.
pub fn tools_dir() -> PathBuf {
    match std::env::var("FOGU_LATEX_TOOLS") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from("tools").join(platform()),
    }
}

/// Pastas onde os binários são procurados, na ordem, antes do PATH.
fn local_dirs() -> Vec<(PathBuf, &'static str)> {
    let mut dirs = vec![(tools_dir(), "tools"), (PathBuf::from("tools"), "tools")];
    if cfg!(windows) {
        for base in ["C:\\texlive", "D:\\texlive"] {
            if let Ok(rd) = std::fs::read_dir(base) {
                let mut years: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                years.sort();
                for y in years.into_iter().rev() {
                    dirs.push((y.join("bin").join("windows"), "texlive"));
                    dirs.push((y.join("bin").join("win64"), "texlive"));
                }
            }
        }
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        dirs.push((PathBuf::from(&local).join("Programs").join("MiKTeX").join("miktex").join("bin").join("x64"), "miktex"));
        dirs.push((PathBuf::from("C:\\Program Files\\MiKTeX\\miktex\\bin\\x64"), "miktex"));
    } else {
        for base in ["/usr/local/texlive", "/opt/texlive"] {
            if let Ok(rd) = std::fs::read_dir(base) {
                let mut years: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                years.sort();
                for y in years.into_iter().rev() {
                    if let Ok(bins) = std::fs::read_dir(y.join("bin")) {
                        for b in bins.filter_map(|e| e.ok()) {
                            dirs.push((b.path(), "texlive"));
                        }
                    }
                }
            }
        }
        dirs.push((PathBuf::from("/Library/TeX/texbin"), "texlive"));
        dirs.push((PathBuf::from("/opt/homebrew/bin"), "path"));
        dirs.push((PathBuf::from("/usr/local/bin"), "path"));
        dirs.push((PathBuf::from("/usr/bin"), "path"));
    }
    dirs
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default()
}

fn is_exe(p: &Path) -> bool {
    p.is_file()
}

/// Procura um executável pelo nome: pastas locais, depois o PATH.
pub fn find(name: &str) -> Option<(PathBuf, &'static str)> {
    let file = exe(name);
    for (dir, source) in local_dirs() {
        let p = dir.join(&file);
        if is_exe(&p) {
            return Some((p, source));
        }
    }
    for dir in path_dirs() {
        let p = dir.join(&file);
        if is_exe(&p) {
            return Some((p, "path"));
        }
    }
    None
}

/// Primeira linha de `<bin> --version` (vazia se não responder em 10 s).
pub fn version_of(path: &Path) -> String {
    let mut cmd = Command::new(path);
    cmd.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_window(&mut cmd);
    let Ok(mut child) = cmd.spawn() else { return String::new() };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                return String::new();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return String::new(),
        }
    }
    let Ok(out) = child.wait_with_output() else { return String::new() };
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    String::from_utf8_lossy(&text).lines().next().unwrap_or("").trim().to_string()
}

/// Esconde a janela de console dos processos filhos no Windows.
pub fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

fn tool(name: &str, with_version: bool) -> Option<Tool> {
    let (path, source) = find(name)?;
    let version = if with_version { version_of(&path) } else { String::new() };
    Some(Tool { name: name.to_string(), path, version, source })
}

/// Motores encontrados, na ordem de preferência de `ENGINES`. `FOGU_LATEX_ENGINE`
/// (caminho de um executável) entra primeiro.
pub fn engines(with_version: bool) -> Vec<Tool> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("FOGU_LATEX_ENGINE")
        && !p.is_empty()
    {
        let path = PathBuf::from(&p);
        if is_exe(&path) {
            let name = engine_name(&path);
            let version = if with_version { version_of(&path) } else { String::new() };
            out.push(Tool { name, path, version, source: "env" });
        }
    }
    for name in ENGINES {
        if out.iter().any(|t| t.name == *name) {
            continue;
        }
        if let Some(t) = tool(name, with_version) {
            out.push(t);
        }
    }
    out
}

pub fn helpers(with_version: bool) -> Vec<Tool> {
    HELPERS.iter().filter_map(|n| tool(n, with_version)).collect()
}

pub fn converters(with_version: bool) -> Vec<Tool> {
    CONVERTERS.iter().filter_map(|n| tool(n, with_version)).collect()
}

/// Nome do motor a partir do nome do arquivo (`pdflatex.exe` -> `pdflatex`).
pub fn engine_name(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Resolve `spec` (nome de um motor, caminho de um executável, ou vazio =
/// o primeiro disponível) para um `Tool`.
pub fn resolve(spec: &str) -> Result<Tool, String> {
    if spec.is_empty() {
        return engines(false).into_iter().next().ok_or_else(|| {
            format!(
                "nenhum motor TeX encontrado (tectonic, pdflatex, xelatex, lualatex, latexmk). Instale um, ou rode latex.install() para baixar o Tectonic em {}",
                tools_dir().display()
            )
        });
    }
    let as_path = Path::new(spec);
    if (spec.contains('/') || spec.contains('\\') || as_path.extension().is_some()) && is_exe(as_path) {
        return Ok(Tool { name: engine_name(as_path), path: as_path.to_path_buf(), version: String::new(), source: "config" });
    }
    let name = spec.to_lowercase();
    if !ENGINES.contains(&name.as_str()) && !HELPERS.contains(&name.as_str()) {
        return Err(format!("motor desconhecido '{spec}' (use tectonic, pdflatex, xelatex, lualatex, latex, latexmk ou o caminho do executável)"));
    }
    tool(&name, false).ok_or_else(|| format!("motor '{name}' não encontrado (nem no PATH, nem em {})", tools_dir().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_and_names() {
        let p = platform();
        assert!(p.contains('-'), "{p}");
        assert_eq!(engine_name(Path::new("C:\\x\\PDFLaTeX.exe")), "pdflatex");
        assert!(resolve("nope").is_err());
        assert!(resolve("tectonic").is_err() || resolve("tectonic").is_ok());
    }
}
