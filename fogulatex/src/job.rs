//! Jobs em thread própria (compilar, instalar, converter) com fila de eventos.
//!
//! O callback do fogu:ffi só pode rodar na thread do interpretador, durante
//! uma chamada FFI, então a thread do job nunca chama o Fogu: ela enfileira
//! eventos (JSON) e `flx_dispatch` os entrega na thread de quem chamou.
//! O último evento é sempre `done` (com `ok`), guardado também em `result`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{self, Tool, hide_window};
use crate::texlog::{self, Report};

pub struct Event {
    pub json: String,
}

#[derive(Clone)]
pub struct Emitter {
    tx: mpsc::Sender<Event>,
    result: Arc<Mutex<Option<Value>>>,
}

impl Emitter {
    pub fn emit(&self, v: Value) {
        let _ = self.tx.send(Event { json: v.to_string() });
    }

    pub fn done(&self, mut v: Value) {
        v["type"] = "done".into();
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.clone());
        self.emit(v);
    }
}

pub struct Job {
    events: Mutex<mpsc::Receiver<Event>>,
    result: Arc<Mutex<Option<Value>>>,
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Estado que a thread do job recebe.
pub struct Ctx {
    pub emit: Emitter,
    pub cancel: Arc<AtomicBool>,
    pub child: Arc<Mutex<Option<Child>>>,
}

impl Job {
    /// Cria o job e roda `f` numa thread; `f` deve terminar com `emit.done(..)`.
    pub fn spawn(name: &str, f: impl FnOnce(&Ctx) + Send + 'static) -> Job {
        let (tx, rx) = mpsc::channel();
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let ctx = Ctx { emit: Emitter { tx, result: result.clone() }, cancel: cancel.clone(), child: child.clone() };
        let handle = thread::Builder::new()
            .name(format!("fogulatex-{name}"))
            .spawn(move || {
                let emit = ctx.emit.clone();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx))).is_err() {
                    emit.done(json!({ "ok": false, "error": "internal panic in fogulatex job" }));
                }
            })
            .expect("spawn job thread");
        Job { events: Mutex::new(rx), result, cancel, child, thread: Mutex::new(Some(handle)) }
    }

    /// Espera até `timeout` pelo primeiro evento e entrega a `deliver` até `max`
    /// eventos (os seguintes sem esperar). Retorna quantos foram entregues.
    pub fn drain(&self, timeout: Duration, max: usize, mut deliver: impl FnMut(&Event) -> bool) -> usize {
        let rx = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        let mut wait = timeout;
        while n < max.max(1) {
            let ev = if wait.is_zero() { rx.try_recv().ok() } else { rx.recv_timeout(wait).ok() };
            let Some(ev) = ev else { break };
            n += 1;
            wait = Duration::ZERO;
            if !deliver(&ev) {
                break;
            }
        }
        n
    }

    pub fn result(&self) -> Option<Value> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = child.kill();
        }
    }

    /// Cancela (se ainda roda) e espera a thread terminar.
    pub fn close(&self) {
        self.cancel();
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

// ---- processos ---------------------------------------------------------

pub struct Run {
    pub status: i32,
    pub output: String,
    pub ms: u64,
    pub timed_out: bool,
}

/// Roda um comando, entregando cada linha da saída como evento `output` e
/// guardando tudo em `Run.output`. Respeita `cancel` e `timeout`.
pub fn run(ctx: &Ctx, tool: &str, cmd: &mut Command, timeout: Duration) -> Result<Run, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_window(cmd);
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|e| format!("não foi possível executar {program}: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    *ctx.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    let output = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    for (stream, name) in [(stdout.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), "stdout"), (stderr.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), "stderr")] {
        let Some(stream) = stream else { continue };
        let emit = ctx.emit.clone();
        let output = output.clone();
        let tool = tool.to_string();
        readers.push(thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.split(b'\n') {
                let Ok(line) = line else { break };
                let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
                output.lock().unwrap_or_else(|e| e.into_inner()).push_str(&text);
                output.lock().unwrap_or_else(|e| e.into_inner()).push('\n');
                emit.emit(json!({ "type": "output", "tool": tool, "stream": name, "line": text }));
            }
        }));
    }

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        let mut slot = ctx.child.lock().unwrap_or_else(|e| e.into_inner());
        let Some(child) = slot.as_mut() else { break -1 };
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => {
                if ctx.cancel.load(Ordering::Relaxed) {
                    let _ = child.kill();
                } else if start.elapsed() > timeout {
                    timed_out = true;
                    let _ = child.kill();
                }
            }
            Err(e) => return Err(format!("{program}: {e}")),
        }
        drop(slot);
        thread::sleep(Duration::from_millis(15));
    };
    *ctx.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
    for r in readers {
        let _ = r.join();
    }
    let output = output.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Run { status, output, ms: start.elapsed().as_millis() as u64, timed_out })
}

// ---- compilar ----------------------------------------------------------

pub struct CompileOpts {
    pub source: Option<String>,
    pub file: Option<PathBuf>,
    pub jobname: Option<String>,
    pub engine: String,
    pub outdir: Option<PathBuf>,
    pub format: String,
    pub max_passes: usize,
    pub bib: String,
    pub inputs: Vec<PathBuf>,
    pub files: Vec<(String, String)>,
    pub shell_escape: bool,
    pub synctex: bool,
    pub halt_on_error: bool,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub cache_dir: Option<String>,
    pub only_cached: bool,
    pub untrusted: bool,
    pub clean: bool,
}

impl CompileOpts {
    pub fn from_json(v: &Value) -> Result<CompileOpts, String> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let b = |k: &str, d: bool| v.get(k).and_then(Value::as_bool).unwrap_or(d);
        let list = |k: &str| -> Vec<String> {
            v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
        };
        let source = s("source");
        let file = s("file").map(PathBuf::from);
        if source.is_none() && file.is_none() {
            return Err("compile: informe 'source' (texto LaTeX) ou 'file' (caminho .tex)".into());
        }
        let mut files = Vec::new();
        if let Some(m) = v.get("files").and_then(Value::as_object) {
            for (name, content) in m {
                let content = content.as_str().ok_or_else(|| format!("files.{name}: o conteúdo deve ser texto"))?;
                files.push((name.clone(), content.to_string()));
            }
        }
        let mut env = Vec::new();
        if let Some(m) = v.get("env").and_then(Value::as_object) {
            for (k, val) in m {
                env.push((k.clone(), val.as_str().map_or_else(|| val.to_string(), str::to_string)));
            }
        }
        let format = s("format").unwrap_or_else(|| "pdf".into()).to_lowercase();
        if !["pdf", "dvi", "xdv", "html"].contains(&format.as_str()) {
            return Err(format!("format '{format}' desconhecido (pdf, dvi, xdv, html)"));
        }
        Ok(CompileOpts {
            source,
            file,
            jobname: s("jobname"),
            engine: s("engine").unwrap_or_default(),
            outdir: s("outdir").map(PathBuf::from),
            format,
            max_passes: v.get("passes").and_then(Value::as_u64).unwrap_or(4).clamp(1, 10) as usize,
            bib: s("bib").unwrap_or_else(|| "auto".into()),
            inputs: list("inputs").into_iter().map(PathBuf::from).collect(),
            files,
            shell_escape: b("shell_escape", false),
            synctex: b("synctex", false),
            halt_on_error: b("halt_on_error", true),
            timeout: Duration::from_millis(v.get("timeout_ms").and_then(Value::as_u64).unwrap_or(300_000)),
            env,
            args: list("args"),
            cache_dir: s("cache_dir"),
            only_cached: b("only_cached", false),
            untrusted: b("untrusted", false),
            clean: b("clean", false),
        })
    }
}

fn abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
}

fn display(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn path_list(dirs: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut s: String = dirs.iter().map(|d| display(&abs(d))).collect::<Vec<_>>().join(sep);
    // O separador duplo no fim mantém os caminhos padrão da distribuição.
    s.push_str(sep);
    s.push_str(sep);
    s
}

fn read_lossy(p: &Path) -> String {
    std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

struct Plan {
    tool: Tool,
    dir: PathBuf,
    outdir: PathBuf,
    main: PathBuf,
    jobname: String,
    env: Vec<(String, String)>,
}

fn prepare(opts: &CompileOpts) -> Result<Plan, String> {
    let tool = engine::resolve(&opts.engine)?;
    let (dir, main, jobname) = match (&opts.file, &opts.source) {
        (Some(file), _) => {
            let file = abs(file);
            if !file.is_file() {
                return Err(format!("arquivo não encontrado: {}", display(&file)));
            }
            let dir = file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
            let job = opts.jobname.clone().unwrap_or_else(|| file.file_stem().unwrap_or_default().to_string_lossy().into_owned());
            (dir, file, job)
        }
        (None, Some(source)) => {
            let job = opts.jobname.clone().unwrap_or_else(|| "document".into());
            let dir = abs(opts.outdir.as_deref().unwrap_or(Path::new("out")));
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", display(&dir)))?;
            let main = dir.join(format!("{job}.tex"));
            std::fs::write(&main, source).map_err(|e| format!("{}: {e}", display(&main)))?;
            (dir, main, job)
        }
        _ => unreachable!(),
    };
    let outdir = abs(opts.outdir.as_deref().unwrap_or(&dir));
    std::fs::create_dir_all(&outdir).map_err(|e| format!("{}: {e}", display(&outdir)))?;
    for (name, content) in &opts.files {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", display(parent)))?;
        }
        std::fs::write(&p, content).map_err(|e| format!("{}: {e}", display(&p)))?;
    }
    for d in &opts.inputs {
        if !d.is_dir() {
            return Err(format!("inputs: pasta não encontrada: {}", display(d)));
        }
    }
    let mut search = vec![dir.clone()];
    search.extend(opts.inputs.iter().cloned());
    let lists = path_list(&search);
    let mut env = vec![
        ("TEXINPUTS".to_string(), lists.clone()),
        ("BIBINPUTS".to_string(), lists.clone()),
        ("BSTINPUTS".to_string(), lists.clone()),
    ];
    if let Some(c) = &opts.cache_dir {
        env.push(("TECTONIC_CACHE_DIR".into(), c.clone()));
    }
    env.extend(opts.env.iter().cloned());
    Ok(Plan { tool, dir, outdir, main, jobname, env })
}

fn output_name(plan: &Plan, format: &str) -> PathBuf {
    plan.outdir.join(format!("{}.{}", plan.jobname, format))
}

fn tectonic_cmd(opts: &CompileOpts, plan: &Plan) -> Command {
    let mut cmd = Command::new(&plan.tool.path);
    cmd.arg(&plan.main).arg("--outdir").arg(&plan.outdir).arg("--keep-logs").arg("--keep-intermediates");
    if opts.format != "pdf" {
        cmd.arg("--outfmt").arg(&opts.format);
    }
    if opts.synctex {
        cmd.arg("--synctex");
    }
    if opts.only_cached {
        cmd.arg("--only-cached");
    }
    if opts.untrusted {
        cmd.arg("--untrusted");
    }
    for d in &opts.inputs {
        cmd.arg("-Z").arg(format!("search-path={}", display(&abs(d))));
    }
    if opts.shell_escape {
        cmd.arg("-Z").arg("shell-escape");
    }
    if !opts.halt_on_error {
        cmd.arg("-Z").arg("continue-on-errors");
    }
    cmd.args(&opts.args);
    cmd
}

fn latexmk_cmd(opts: &CompileOpts, plan: &Plan) -> Command {
    let mut cmd = Command::new(&plan.tool.path);
    cmd.arg("-interaction=nonstopmode").arg("-file-line-error");
    match opts.format.as_str() {
        "dvi" | "xdv" => cmd.arg("-dvi"),
        _ => cmd.arg("-pdf"),
    };
    cmd.arg(format!("-output-directory={}", display(&plan.outdir)));
    if opts.halt_on_error {
        cmd.arg("-halt-on-error");
    }
    if opts.shell_escape {
        cmd.arg("-shell-escape");
    }
    if opts.synctex {
        cmd.arg("-synctex=1");
    }
    if let Some(j) = &opts.jobname {
        cmd.arg(format!("-jobname={j}"));
    }
    cmd.args(&opts.args).arg(&plan.main);
    cmd
}

fn tex_cmd(opts: &CompileOpts, plan: &Plan) -> Command {
    let mut cmd = Command::new(&plan.tool.path);
    cmd.arg("-interaction=nonstopmode").arg("-file-line-error").arg(format!("-output-directory={}", display(&plan.outdir)));
    if opts.halt_on_error {
        cmd.arg("-halt-on-error");
    }
    if opts.shell_escape {
        cmd.arg("-shell-escape");
    }
    if opts.synctex {
        cmd.arg("-synctex=1");
    }
    if let Some(j) = &opts.jobname {
        cmd.arg(format!("-jobname={j}"));
    }
    if opts.format == "dvi" && plan.tool.name == "pdflatex" {
        cmd.arg("-output-format=dvi");
    }
    cmd.args(&opts.args).arg(&plan.main);
    cmd
}

/// Hash dos arquivos auxiliares (.aux, .toc, .lof, .lot, .idx, .ind, .bbl...):
/// se mudaram num passe, o próximo pode mudar o documento (como o latexmk).
fn aux_snapshot(plan: &Plan) -> Vec<u64> {
    ["aux", "toc", "lof", "lot", "idx", "ind", "bbl", "out", "nav", "glo"]
        .iter()
        .map(|ext| {
            let bytes = std::fs::read(plan.outdir.join(format!("{}.{ext}", plan.jobname))).unwrap_or_default();
            // FNV-1a
            bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
        })
        .collect()
}

/// Gera o `.ind` a partir do `.idx` (makeindex instalado, ou o interno).
/// Devolve true se o `.ind` mudou (o documento precisa de outro passe).
fn build_index(ctx: &Ctx, plan: &Plan, opts: &CompileOpts, report: &mut Report, passes: &mut usize, tools_run: &mut Vec<String>) -> Result<bool, String> {
    let idx_path = plan.outdir.join(format!("{}.idx", plan.jobname));
    let ind_path = plan.outdir.join(format!("{}.ind", plan.jobname));
    let idx = read_lossy(&idx_path);
    if idx.trim().is_empty() {
        return Ok(false);
    }
    let before = read_lossy(&ind_path);
    match engine::find("makeindex") {
        Some((path, _)) => {
            let mut cmd = Command::new(path);
            cmd.arg(format!("{}.idx", plan.jobname)).current_dir(&plan.outdir).envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            *passes += 1;
            tools_run.push("makeindex".into());
            ctx.emit.emit(json!({ "type": "pass", "n": *passes, "tool": "makeindex" }));
            let r = run(ctx, "makeindex", &mut cmd, opts.timeout)?;
            ctx.emit.emit(json!({ "type": "pass_end", "n": *passes, "tool": "makeindex", "status": r.status, "ms": r.ms, "timed_out": r.timed_out }));
            if r.status != 0 {
                report.warnings.push(texlog::Diag { message: format!("makeindex terminou com código {}", r.status), file: None, line: None, kind: "warning" });
            }
        }
        None => {
            tools_run.push("mkindex".into());
            ctx.emit.emit(json!({ "type": "pass", "n": *passes + 1, "tool": "mkindex", "note": "makeindex não encontrado; usando o gerador interno" }));
            *passes += 1;
            std::fs::write(&ind_path, crate::mkindex::make(&idx)).map_err(|e| format!("{}: {e}", display(&ind_path)))?;
        }
    }
    Ok(read_lossy(&ind_path) != before)
}

/// O .aux pede bibliografia? ("bibtex" se há \bibdata, "biber" se há .bcf).
fn bib_tool_needed(plan: &Plan) -> Option<&'static str> {
    if plan.outdir.join(format!("{}.bcf", plan.jobname)).is_file() {
        return Some("biber");
    }
    let aux = read_lossy(&plan.outdir.join(format!("{}.aux", plan.jobname)));
    if aux.contains("\\bibdata{") && aux.contains("\\citation{") {
        return Some("bibtex");
    }
    None
}

pub fn compile(ctx: &Ctx, opts: &CompileOpts) {
    let start = Instant::now();
    let plan = match prepare(opts) {
        Ok(p) => p,
        Err(e) => {
            ctx.emit.done(json!({ "ok": false, "error": e, "errors": [], "warnings": [], "passes": 0 }));
            return;
        }
    };
    ctx.emit.emit(json!({
        "type": "start", "engine": plan.tool.name, "path": display(&plan.tool.path),
        "main": display(&plan.main), "outdir": display(&plan.outdir), "jobname": plan.jobname,
    }));

    let mut report = Report::default();
    let mut passes = 0usize;
    let mut tools_run: Vec<String> = Vec::new();
    let mut failure: Option<String> = None;
    let log_path = plan.outdir.join(format!("{}.log", plan.jobname));

    let run_tool = |ctx: &Ctx, name: &str, cmd: &mut Command, passes: &mut usize, tools_run: &mut Vec<String>| -> Result<Run, String> {
        cmd.current_dir(&plan.dir).envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        *passes += 1;
        tools_run.push(name.to_string());
        ctx.emit.emit(json!({ "type": "pass", "n": *passes, "tool": name }));
        let r = run(ctx, name, cmd, opts.timeout)?;
        ctx.emit.emit(json!({ "type": "pass_end", "n": *passes, "tool": name, "status": r.status, "ms": r.ms, "timed_out": r.timed_out }));
        if r.timed_out {
            return Err(format!("{name}: tempo esgotado ({} ms)", opts.timeout.as_millis()));
        }
        if ctx.cancel.load(Ordering::Relaxed) {
            return Err("cancelado".into());
        }
        Ok(r)
    };

    let one_shot = plan.tool.name == "tectonic" || plan.tool.name == "latexmk";
    // Um passe do motor; devolve (saída, relatório do log) ou o erro.
    let engine_pass = |ctx: &Ctx, passes: &mut usize, tools_run: &mut Vec<String>| -> Result<(Run, Report), String> {
        let mut cmd = match plan.tool.name.as_str() {
            "tectonic" => tectonic_cmd(opts, &plan),
            "latexmk" => latexmk_cmd(opts, &plan),
            _ => tex_cmd(opts, &plan),
        };
        let r = run_tool(ctx, &plan.tool.name, &mut cmd, passes, tools_run)?;
        let log_text = read_lossy(&log_path);
        let mut rep = texlog::parse(if log_text.is_empty() { &r.output } else { &log_text });
        if one_shot {
            // A saída do tectonic repete o log; só os "error:" dele são novos.
            for e in texlog::parse(&r.output).errors {
                if !rep.errors.iter().any(|x| x.message == e.message) {
                    rep.errors.push(e);
                }
            }
        }
        Ok((r, rep))
    };

    if one_shot {
        match engine_pass(ctx, &mut passes, &mut tools_run) {
            Ok((r, rep)) => {
                report = rep;
                if r.status != 0 && report.errors.is_empty() {
                    failure = Some(format!("{} terminou com código {}", plan.tool.name, r.status));
                }
            }
            Err(e) => failure = Some(e),
        }
    } else {
        // pdflatex/xelatex/lualatex/latex: passes até as referências e os
        // arquivos auxiliares (sumário, listas, índice) assentarem.
        let mut bib_done = false;
        loop {
            let before = aux_snapshot(&plan);
            let (r, rep) = match engine_pass(ctx, &mut passes, &mut tools_run) {
                Ok(x) => x,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            let has_errors = !rep.errors.is_empty();
            report = rep;
            if has_errors && opts.halt_on_error {
                break;
            }
            if r.status != 0 && !has_errors {
                failure = Some(format!("{} terminou com código {}", plan.tool.name, r.status));
                break;
            }
            let aux_changed = aux_snapshot(&plan) != before;
                if !bib_done && opts.bib != "none" {
                    let wanted = if opts.bib == "auto" { bib_tool_needed(&plan) } else { Some(opts.bib.as_str()) };
                    if let Some(bib) = wanted {
                        bib_done = true;
                        match engine::resolve(bib) {
                            Ok(t) => {
                                let mut cmd = Command::new(&t.path);
                                cmd.arg(&plan.jobname).current_dir(&plan.outdir);
                                match run_tool(ctx, bib, &mut cmd, &mut passes, &mut tools_run) {
                                    Ok(br) => {
                                        if br.status != 0 {
                                            let msg = br.output.lines().find(|l| l.contains("error") || l.contains("Error") || l.contains("I couldn't")).unwrap_or("falhou").to_string();
                                            report.warnings.push(texlog::Diag { message: format!("{bib}: {msg}"), file: None, line: None, kind: "warning" });
                                        }
                                        report.rerun = true;
                                        continue;
                                    }
                                    Err(e) => {
                                        failure = Some(e);
                                        break;
                                    }
                                }
                            }
                            Err(e) => report.warnings.push(texlog::Diag { message: e, file: None, line: None, kind: "warning" }),
                        }
                    }
                }
                if (report.rerun || aux_changed) && passes < opts.max_passes {
                    continue;
                }
                break;
        }
    }

    // Índice remissivo (\makeindex + \printindex): gera o .ind e roda mais um passe.
    if failure.is_none() && (report.errors.is_empty() || !opts.halt_on_error) {
        match build_index(ctx, &plan, opts, &mut report, &mut passes, &mut tools_run) {
            Ok(true) => match engine_pass(ctx, &mut passes, &mut tools_run) {
                Ok((_, rep)) => {
                    let (overfull, underfull) = (rep.overfull, rep.underfull);
                    report = rep;
                    report.overfull = overfull;
                    report.underfull = underfull;
                }
                Err(e) => failure = Some(e),
            },
            Ok(false) => {}
            Err(e) => failure = Some(e),
        }
    }

    let output = output_name(&plan, &opts.format);
    let exists = output.is_file();
    let pages = if exists && opts.format == "pdf" { crate::pdf::page_count(&output) } else { 0 };
    let ok = failure.is_none() && exists && (report.errors.is_empty() || !opts.halt_on_error);
    if opts.clean {
        for ext in ["aux", "log", "out", "toc", "lof", "lot", "bbl", "blg", "bcf", "run.xml", "fls", "fdb_latexmk", "synctex.gz", "nav", "snm", "idx", "ilg", "ind"] {
            let _ = std::fs::remove_file(plan.outdir.join(format!("{}.{ext}", plan.jobname)));
        }
    }
    let error = failure.clone().or_else(|| {
        if !ok {
            report.errors.first().map(|e| match (&e.file, e.line) {
                (Some(f), Some(l)) => format!("{f}:{l}: {}", e.message),
                (_, Some(l)) => format!("linha {l}: {}", e.message),
                _ => e.message.clone(),
            })
        } else {
            None
        }
    });
    let mut v = json!({
        "ok": ok,
        "error": error,
        "output": if exists { Value::from(display(&output)) } else { Value::Null },
        "format": opts.format,
        "log": if log_path.is_file() { Value::from(display(&log_path)) } else { Value::Null },
        "engine": plan.tool.name,
        "jobname": plan.jobname,
        "outdir": display(&plan.outdir),
        "passes": passes,
        "tools": tools_run,
        "pages": pages,
        "ms": start.elapsed().as_millis() as u64,
    });
    for (k, val) in report.to_json().as_object().unwrap() {
        v[k] = val.clone();
    }
    ctx.emit.done(v);
}

// ---- converter --------------------------------------------------------

pub struct ConvertOpts {
    pub input: PathBuf,
    pub to: String,
    pub outdir: Option<PathBuf>,
    pub prefix: Option<String>,
    pub dpi: u32,
    pub pages: Vec<u32>,
    pub tool: String,
    pub timeout: Duration,
}

impl ConvertOpts {
    pub fn from_json(v: &Value) -> Result<ConvertOpts, String> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let input = s("input").map(PathBuf::from).ok_or("convert: informe 'input'")?;
        let to = s("to").unwrap_or_else(|| "png".into()).to_lowercase();
        if !["png", "jpg", "svg", "txt", "ps"].contains(&to.as_str()) {
            return Err(format!("convert: formato '{to}' desconhecido (png, jpg, svg, txt, ps)"));
        }
        Ok(ConvertOpts {
            input,
            to,
            outdir: s("outdir").map(PathBuf::from),
            prefix: s("prefix"),
            dpi: v.get("dpi").and_then(Value::as_u64).unwrap_or(150) as u32,
            pages: v.get("pages").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|p| p as u32).collect()).unwrap_or_default(),
            tool: s("tool").unwrap_or_default(),
            timeout: Duration::from_millis(v.get("timeout_ms").and_then(Value::as_u64).unwrap_or(120_000)),
        })
    }
}

/// Ferramentas que sabem gerar cada formato, em ordem de preferência.
fn converters_for(to: &str) -> &'static [&'static str] {
    match to {
        "png" | "jpg" => &["pdftoppm", "mutool", "magick", "gs", "gswin64c"],
        "svg" => &["pdftocairo", "dvisvgm", "mutool"],
        "txt" => &["pdftotext", "mutool"],
        "ps" => &["pdftops", "gs", "gswin64c"],
        _ => &[],
    }
}

pub fn convert(ctx: &Ctx, opts: &ConvertOpts) {
    let start = Instant::now();
    let input = abs(&opts.input);
    if !input.is_file() {
        ctx.emit.done(json!({ "ok": false, "error": format!("arquivo não encontrado: {}", display(&input)), "files": [] }));
        return;
    }
    let outdir = abs(opts.outdir.as_deref().unwrap_or_else(|| input.parent().unwrap_or(Path::new("."))));
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        ctx.emit.done(json!({ "ok": false, "error": format!("{}: {e}", display(&outdir)), "files": [] }));
        return;
    }
    let stem = input.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let prefix = opts.prefix.clone().unwrap_or(stem);
    let candidates: Vec<&str> = if opts.tool.is_empty() { converters_for(&opts.to).to_vec() } else { vec![opts.tool.as_str()] };
    let mut found: Option<(String, PathBuf)> = None;
    for c in &candidates {
        if let Some((p, _)) = engine::find(c) {
            found = Some((c.to_string(), p));
            break;
        }
    }
    let Some((tool, path)) = found else {
        ctx.emit.done(json!({
            "ok": false,
            "error": format!("nenhum conversor para {} encontrado (procurei: {}); instale poppler (pdftoppm/pdftotext), mupdf (mutool), ghostscript ou ImageMagick", opts.to, candidates.join(", ")),
            "files": [],
        }));
        return;
    };
    let before: Vec<PathBuf> = std::fs::read_dir(&outdir).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
    let out_base = outdir.join(&prefix);
    let (first, last) = (opts.pages.iter().min().copied(), opts.pages.iter().max().copied());
    let mut cmd = Command::new(&path);
    match (tool.as_str(), opts.to.as_str()) {
        ("pdftoppm", fmt) => {
            cmd.arg(if fmt == "jpg" { "-jpeg" } else { "-png" }).arg("-r").arg(opts.dpi.to_string());
            if let (Some(f), Some(l)) = (first, last) {
                cmd.arg("-f").arg(f.to_string()).arg("-l").arg(l.to_string());
            }
            cmd.arg(&input).arg(&out_base);
        }
        ("pdftocairo", _) => {
            cmd.arg("-svg");
            if let (Some(f), Some(l)) = (first, last) {
                cmd.arg("-f").arg(f.to_string()).arg("-l").arg(l.to_string());
            }
            cmd.arg(&input).arg(format!("{}.svg", display(&out_base)));
        }
        ("pdftotext", _) => {
            cmd.arg("-layout");
            if let (Some(f), Some(l)) = (first, last) {
                cmd.arg("-f").arg(f.to_string()).arg("-l").arg(l.to_string());
            }
            cmd.arg(&input).arg(format!("{}.txt", display(&out_base)));
        }
        ("pdftops", _) => {
            cmd.arg(&input).arg(format!("{}.ps", display(&out_base)));
        }
        ("mutool", fmt) => {
            cmd.arg("draw").arg("-r").arg(opts.dpi.to_string()).arg("-o");
            match fmt {
                "txt" => cmd.arg(format!("{}.txt", display(&out_base))).arg("-F").arg("txt"),
                "svg" => cmd.arg(format!("{}-%d.svg", display(&out_base))),
                "jpg" => cmd.arg(format!("{}-%d.jpg", display(&out_base))),
                _ => cmd.arg(format!("{}-%d.png", display(&out_base))),
            };
            cmd.arg(&input);
            if !opts.pages.is_empty() {
                cmd.arg(opts.pages.iter().map(u32::to_string).collect::<Vec<_>>().join(","));
            }
        }
        ("magick", fmt) => {
            cmd.arg("-density").arg(opts.dpi.to_string()).arg(&input).arg(format!("{}-%d.{}", display(&out_base), fmt));
        }
        ("gs" | "gswin64c", fmt) => {
            let device = match fmt {
                "jpg" => "jpeg",
                "ps" => "ps2write",
                _ => "png16m",
            };
            cmd.arg("-dBATCH").arg("-dNOPAUSE").arg("-dQUIET").arg(format!("-sDEVICE={device}")).arg(format!("-r{}", opts.dpi));
            if let (Some(f), Some(l)) = (first, last) {
                cmd.arg(format!("-dFirstPage={f}")).arg(format!("-dLastPage={l}"));
            }
            if fmt == "ps" {
                cmd.arg(format!("-sOutputFile={}.ps", display(&out_base)));
            } else {
                cmd.arg(format!("-sOutputFile={}-%d.{}", display(&out_base), fmt));
            }
            cmd.arg(&input);
        }
        ("dvisvgm", _) => {
            cmd.arg("--pdf").arg("-o").arg(format!("{}-%p.svg", display(&out_base)));
            if !opts.pages.is_empty() {
                cmd.arg("-p").arg(opts.pages.iter().map(u32::to_string).collect::<Vec<_>>().join(","));
            }
            cmd.arg(&input);
        }
        _ => {
            ctx.emit.done(json!({ "ok": false, "error": format!("{tool} não gera {}", opts.to), "files": [] }));
            return;
        }
    }
    ctx.emit.emit(json!({ "type": "start", "tool": tool, "path": display(&path), "to": opts.to }));
    let r = match run(ctx, &tool, &mut cmd, opts.timeout) {
        Ok(r) => r,
        Err(e) => {
            ctx.emit.done(json!({ "ok": false, "error": e, "files": [] }));
            return;
        }
    };
    let mut files: Vec<String> = std::fs::read_dir(&outdir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| !before.contains(p) || p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix.as_str()) && p.extension().is_some_and(|x| x.to_string_lossy() == opts.to)))
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix.as_str())))
                .map(|p| display(&p))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let ok = r.status == 0 && !r.timed_out && !files.is_empty();
    ctx.emit.done(json!({
        "ok": ok,
        "error": if ok { Value::Null } else { Value::from(if r.timed_out { "tempo esgotado".to_string() } else { format!("{tool} terminou com código {} sem gerar arquivos: {}", r.status, r.output.lines().last().unwrap_or("")) }) },
        "tool": tool,
        "files": files,
        "ms": start.elapsed().as_millis() as u64,
    }));
}

/// Ferramentas instaladas: `{ platform, tools_dir, engines, helpers, converters,
/// formats: { png: [conversores encontrados que geram png], svg, txt, ps, jpg } }`.
pub fn tools_json(with_version: bool) -> Value {
    let converters = engine::converters(with_version);
    let mut formats = serde_json::Map::new();
    for fmt in ["png", "jpg", "svg", "txt", "ps"] {
        let found: Vec<Value> = converters_for(fmt).iter().filter(|c| converters.iter().any(|t| t.name == **c)).map(|c| Value::from(*c)).collect();
        formats.insert(fmt.to_string(), Value::Array(found));
    }
    json!({
        "platform": engine::platform(),
        "tools_dir": display(&abs(&engine::tools_dir())),
        "engines": engine::engines(with_version).iter().map(Tool::to_json).collect::<Vec<_>>(),
        "helpers": engine::helpers(with_version).iter().map(Tool::to_json).collect::<Vec<_>>(),
        "converters": converters.iter().map(Tool::to_json).collect::<Vec<_>>(),
        "formats": formats,
    })
}

#[allow(dead_code)]
fn _unused(_: HashMap<String, String>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_opts() {
        let v: Value = serde_json::from_str(r#"{"source": "x", "inputs": ["a"], "files": {"b.tex": "y"}, "format": "PDF", "passes": 20}"#).unwrap();
        let o = CompileOpts::from_json(&v).unwrap();
        assert_eq!(o.format, "pdf");
        assert_eq!(o.max_passes, 10);
        assert_eq!(o.files, vec![("b.tex".to_string(), "y".to_string())]);
        assert!(CompileOpts::from_json(&json!({})).is_err());
        assert!(CompileOpts::from_json(&json!({"source": "x", "format": "docx"})).is_err());
        let p = path_list(&[PathBuf::from("a"), PathBuf::from("b")]);
        assert!(p.ends_with(if cfg!(windows) { ";;" } else { "::" }));
    }

    #[test]
    fn missing_engine_or_file_reports_done() {
        let job = Job::spawn("test", |ctx| {
            let v = json!({ "file": "does-not-exist.tex", "engine": "pdflatex" });
            compile(ctx, &CompileOpts::from_json(&v).unwrap());
        });
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while job.result().is_none() && Instant::now() < deadline {
            job.drain(Duration::from_millis(100), 64, |e| {
                events.push(e.json.clone());
                true
            });
        }
        let done = job.result().expect("done");
        assert_eq!(done["ok"], false);
        assert!(done["error"].as_str().unwrap().contains("não encontrado") || done["error"].as_str().unwrap().contains("nenhum motor"), "{done}");
        job.close();
    }

    #[test]
    fn run_captures_output() {
        let job = Job::spawn("echo", |ctx| {
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", "echo hello && echo err 1>&2"]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", "echo hello; echo err 1>&2"]);
                c
            };
            let r = run(ctx, "echo", &mut cmd, Duration::from_secs(10)).unwrap();
            ctx.emit.done(json!({ "ok": r.status == 0, "output": r.output }));
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut lines = 0;
        while job.result().is_none() && Instant::now() < deadline {
            job.drain(Duration::from_millis(50), 64, |e| {
                if e.json.contains("\"output\"") && e.json.contains("\"line\"") {
                    lines += 1;
                }
                true
            });
        }
        let done = job.result().unwrap();
        assert_eq!(done["ok"], true);
        assert!(done["output"].as_str().unwrap().contains("hello"));
        assert!(lines >= 2, "{lines}");
        job.close();
    }
}
