//! Baixa o Tectonic (um binário só, que busca os pacotes LaTeX sob demanda)
//! dos releases do GitHub para `tools/<plataforma>/`, com eventos de progresso.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::engine::{exe, platform, tools_dir};

pub const DEFAULT_VERSION: &str = "0.17.0";

pub struct InstallOpts {
    pub version: String,
    pub dir: PathBuf,
    pub url: Option<String>,
    pub force: bool,
    pub timeout: Duration,
}

impl InstallOpts {
    pub fn from_json(v: &Value) -> Result<InstallOpts, String> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(InstallOpts {
            version: s("version").unwrap_or_else(|| DEFAULT_VERSION.into()),
            dir: s("dir").map(PathBuf::from).unwrap_or_else(tools_dir),
            url: s("url"),
            force: v.get("force").and_then(Value::as_bool).unwrap_or(false),
            timeout: Duration::from_millis(v.get("timeout_ms").and_then(Value::as_u64).unwrap_or(600_000)),
        })
    }
}

/// (target triple, extensão do pacote) do release para esta plataforma.
pub fn asset_for(platform: &str) -> Result<(&'static str, &'static str), String> {
    Ok(match platform {
        "windows-x64" => ("x86_64-pc-windows-msvc", "zip"),
        "linux-x64" => ("x86_64-unknown-linux-musl", "tar.gz"),
        "linux-arm64" => ("aarch64-unknown-linux-musl", "tar.gz"),
        "linux-armv7" => ("arm-unknown-linux-musleabihf", "tar.gz"),
        "linux-x86" => ("i686-unknown-linux-gnu", "tar.gz"),
        "macos-x64" => ("x86_64-apple-darwin", "tar.gz"),
        "macos-arm64" => ("aarch64-apple-darwin", "tar.gz"),
        other => return Err(format!("não há binário do Tectonic para {other}; instale um motor TeX (pdflatex/xelatex) manualmente")),
    })
}

pub fn download_url(version: &str, platform: &str) -> Result<String, String> {
    let (triple, ext) = asset_for(platform)?;
    Ok(format!(
        "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%40{version}/tectonic-{version}-{triple}.{ext}"
    ))
}

pub fn install(opts: &InstallOpts, emit: &dyn Fn(Value), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let plat = platform();
    let dest = opts.dir.join(exe("tectonic"));
    if dest.is_file() && !opts.force {
        emit(json!({ "type": "progress", "bytes": 0, "total": 0, "message": "já instalado" }));
        return Ok(dest);
    }
    let url = match &opts.url {
        Some(u) => u.clone(),
        None => download_url(&opts.version, &plat)?,
    };
    let is_zip = url.ends_with(".zip");
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("não foi possível criar {}: {e}", opts.dir.display()))?;

    emit(json!({ "type": "start", "url": url, "dir": opts.dir.to_string_lossy(), "version": opts.version }));
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(opts.timeout))
            .user_agent("fogulatex")
            .max_redirects(10)
            .build(),
    );
    let mut resp = agent.get(&url).call().map_err(|e| format!("download de {url}: {e}"))?;
    let total = resp.body_mut().content_length().unwrap_or(0);
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();

    let tmp = opts.dir.join(format!(".tectonic-download.{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        let mut last_report = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelado".into());
            }
            let n = reader.read(&mut buf).map_err(|e| format!("download: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", tmp.display()))?;
            done += n as u64;
            if done - last_report >= 512 * 1024 {
                last_report = done;
                emit(json!({ "type": "progress", "bytes": done, "total": total }));
            }
        }
        file.flush().map_err(|e| e.to_string())?;
        emit(json!({ "type": "progress", "bytes": done, "total": total }));
        drop(file);
        emit(json!({ "type": "extract", "file": tmp.to_string_lossy() }));
        if is_zip { extract_zip(&tmp, &dest) } else { extract_tar_gz(&tmp, &dest) }
    })();
    let _ = std::fs::remove_file(&tmp);
    result?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(dest)
}

fn is_tectonic_entry(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base == "tectonic" || base == "tectonic.exe"
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("zip inválido: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map(|n| n.into_owned()).unwrap_or_default();
        if entry.is_file() && is_tectonic_entry(&name) {
            let mut out = File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("o pacote não contém o executável tectonic".into())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().map_err(|e| format!("tar inválido: {e}"))? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        if entry.header().entry_type().is_file() && is_tectonic_entry(&path) {
            let mut out = File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("o pacote não contém o executável tectonic".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = download_url("0.17.0", "windows-x64").unwrap();
        assert_eq!(u, "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-x86_64-pc-windows-msvc.zip");
        assert!(download_url("0.17.0", "linux-arm64").unwrap().ends_with("aarch64-unknown-linux-musl.tar.gz"));
        assert!(download_url("0.17.0", "windows-arm64").is_err());
        assert!(is_tectonic_entry("tectonic-0.17.0/tectonic.exe"));
        assert!(!is_tectonic_entry("README.md"));
    }
}
