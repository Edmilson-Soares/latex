# latex — LaTeX para Fogu (PDF e mais)

Uma lib Rust (`fogulatex`) com API C para o `fogu:ffi`, mais um módulo Fogu (`latex.fg`) que
compila LaTeX para **PDF** (e DVI/XDV/HTML), importa arquivos e bibliotecas, lê o log com
erros e avisos, converte PDF para PNG/SVG/TXT/PS e lê PDFs (páginas, metadados, texto).

A compilação usa o motor TeX instalado na máquina (**tectonic**, **pdflatex**, **xelatex**,
**lualatex**, **latexmk** — TeX Live ou MiKTeX). Se não houver nenhum, `latex.install()` baixa o
[Tectonic](https://tectonic-typesetting.github.io) (um binário só, ~20 MB) para
`tools/<plataforma>/`; ele busca os pacotes LaTeX que cada documento usa sob demanda, então não
é preciso instalar uma distribuição TeX.

```
fogulatex/      lib Rust (cdylib) com a API C
native/         fogulatex.dll / libfogulatex.so / libfogulatex.dylib (gerado pelo build)
tools/          Tectonic baixado por latex.install() (gerado)
latex.fg        wrapper Fogu: compile(), Document, install(), convert(), pdfInfo()...
main.fg         exemplo: relatório com \input, pacote próprio, PDF e PNG
```

<!-- fogu:plataformas -->
## Plataformas, binários e pré-requisitos

Ao importar `latex.fg`, a lib reconhece o sistema e a arquitetura
(`system.info.os`/`arch`) e já carrega o binário certo — `.dll` no Windows,
`.so` no Linux, `.dylib` no macOS. Se ele faltar, o programa não para no `import`:
o erro (com a lista de caminhos tentados) volta na primeira chamada que precisar da lib.
`nativeInfo()` mostra o que foi detectado: `{ platform, os, file, path, loaded, error }`.

| Plataforma | Arquivo | Neste repositório |
|---|---|---|
| `windows-x64` | `native/windows-x64/fogulatex.dll` | incluído |
| `windows-arm64` | `native/windows-arm64/fogulatex.dll` | — |
| `linux-x64` | `native/linux-x64/libfogulatex.so` | incluído |
| `linux-arm64` | `native/linux-arm64/libfogulatex.so` | incluído |
| `macos-x64` | `native/macos-x64/libfogulatex.dylib` | incluído |
| `macos-arm64` | `native/macos-arm64/libfogulatex.dylib` | incluído |

**Instalação** (Fogu 0.8.0+): `fogu add Edmilson-Soares/latex@^0.2.0`. Os binários de `native/` vão
no repositório git, então a lib já funciona depois de instalada — sem Rust/cargo na máquina.

Ordem de busca: o caminho passado a `load(path)`, a variável `FOGU_LATEX_LIB`,
`native/<plataforma>/`, `native/` e o build do cargo no projeto em execução, uma
cópia em `fogu_modules/` e os pacotes instalados com `fogu add` (`$FOGU_HOME/fogu_modules`).

**Pré-requisitos**

- Para a lib: nenhum (Rust puro: ureq + rustls para o download, lopdf para ler PDF).
- Para compilar: um motor TeX. Ou um já instalado (TeX Live, MiKTeX), ou o Tectonic baixado
  por `latex.install()` (precisa de internet na instalação e na primeira compilação de cada
  pacote novo; depois tudo fica no cache). Motores são procurados em `FOGU_LATEX_ENGINE`,
  `FOGU_LATEX_TOOLS`, `tools/<plataforma>/`, no PATH e nas pastas padrão do TeX Live/MiKTeX.
- Para converter PDF em imagem/texto: poppler (`pdftoppm`, `pdftotext`, `pdftocairo`),
  mupdf (`mutool`), Ghostscript (`gs`) ou ImageMagick (`magick`), o que existir.
- Rust ≥ 1.85 (edition 2024) para gerar os binários.

**Gerar os binários**

```sh
fogu run build:native        # Windows: native/windows-<arch>/ (build.ps1)
sh build.sh                  # Linux/macOS/WSL: o sistema atual
sh build.sh all              # Linux x64/arm64 e macOS x64/arm64 (cargo-zigbuild + zig)
```

## Callbacks do fogu:ffi (requer Fogu 0.8.0+)

A camada nativa usa `ffi.callback` (veja `_native.fg`, igual em todas as libs deste repositório):

- **Sink de saída**: a lib recebe um callback uma vez (`flx_set_sink`) e, chamada com
  `buf = nil, cap = 0`, entrega a resposta inteira por ele (JSON ou texto de um PDF).
- **Regras do fogu:ffi respeitadas**: um callback só roda na thread do interpretador, durante
  uma chamada FFI — as threads dos jobs (motor TeX, download) nunca chamam o Fogu; elas
  enfileiram eventos e `flx_dispatch` os entrega na thread de quem chamou. Um erro dentro de um
  callback derruba o programa, então o callback só guarda os dados e os handlers do usuário
  (`onOutput`, `onProgress`) rodam depois, fora do frame nativo.
- **Esperas cooperativas**: `compile()`/`install()`/`convert()` esperam em fatias de 10 ms com
  `time.delay(1ms)` entre elas, então as outras tarefas `fg` continuam rodando enquanto o
  motor TeX trabalha.
- **Fogu 0.7.4+** (e 0.8.0): o código compilado chama direto as funções FFI de assinatura numérica (sem
  alocação por chamada), então sondar status/eventos num laço é barato; `fogu .` roda o projeto
  (o script `start` do `fogu.json`); os tipos `int32`, `uint8`, `float64`... valem nas assinaturas.
<!-- /fogu:plataformas -->

## Build e execução

```sh
fogu run build:native       # ou ./build.ps1 (Windows) / ./build.sh (Linux/macOS)
fogu run main.fg            # gera out/relatorio.pdf (baixa o Tectonic se não houver motor)
fogu test                   # os testes de compilação são pulados sem motor
```

## Como funciona

```fogu
import latex from "./latex.fg"

fn main() {
    latex.ensureEngine()?                      // usa o motor instalado ou baixa o Tectonic
    const doc = latex.document({ packages: ["graphicx", ["geometry", "margin=2cm"]], title: "Relatório" })
    doc.section("Dados").text("Vendas: 100% & R$ 5").table([["Item", "Qtd"], ["Café", "2"]], { caption: "Compras" })
       .input("cap1")                          // importa cap1.tex
    const r = doc.compile({
        outdir: "out", jobname: "relatorio",
        files: { "cap1.tex": "Capítulo importado.\n" },   // arquivos gravados junto do documento
        inputs: ["libs"],                                  // pastas com .sty/.cls/.bib/imagens
    })?
    print(r.output, r.pages)                   // out/relatorio.pdf 1
    print(latex.pdfText(r.output)?)
    latex.convert(r.output, { to: "png", dpi: 150 })?     // se houver pdftoppm/mutool/gs/magick
    return true, nil
}
main()
```

A lib roda o motor num processo filho, em thread própria, e enfileira cada linha da saída como
evento (`onOutput`). Com pdflatex/xelatex/lualatex ela repete os passes até as referências
assentarem (até `passes`, padrão 4) e roda `bibtex`/`biber` quando o `.aux` pede. O Tectonic e o
latexmk fazem isso sozinhos (um passe). No fim lê o `.log`: erros com arquivo e linha
(`-file-line-error`), avisos de LaTeX/pacotes/fontes, caixas cheias e se faltou um rerun.

### `latex.compile(source, options)` / `compileFile(file, options)`

| opção | padrão | |
|---|---|---|
| `engine` | o primeiro instalado | `"tectonic"`, `"pdflatex"`, `"xelatex"`, `"lualatex"`, `"latexmk"`, `"latex"` ou o caminho do executável |
| `outdir`, `jobname` | `"out"`, `"document"` | pasta e nome do PDF (`compileFile`: a pasta do `.tex`) |
| `format` | `"pdf"` | `"dvi"`, `"xdv"`, `"html"` (tectonic) |
| `files` | — | `{ "cap1.tex": "...", "refs.bib": "..." }`, gravados na pasta do documento |
| `inputs` | — | pastas com `.sty`/`.cls`/`.bib`/imagens (`TEXINPUTS`/`BIBINPUTS`, `-Z search-path` no tectonic) |
| `bib` | `"auto"` | `"bibtex"`, `"biber"`, `"none"` |
| `passes` | `4` | máximo de passes do motor |
| `haltOnError` | `true` | `false` continua e gera o PDF mesmo com erros |
| `shellEscape`, `synctex`, `clean` | `false` | `clean` apaga `.aux`, `.log`, `.toc`... no fim |
| `timeout` | `5min` | por processo |
| `env`, `args` | — | variáveis e argumentos extras para o motor |
| `cacheDir`, `onlyCached`, `untrusted` | — | só tectonic (`TECTONIC_CACHE_DIR`, `--only-cached`, `--untrusted`) |
| `onOutput(line, ev)`, `onEvent(ev)` | — | linhas do motor; todos os eventos (`start`, `pass`, `output`, `pass_end`, `done`) |

Resultado: `{ ok, output, log, engine, jobname, outdir, pages, passes, tools, ms, errors: [{ message,
file, line }], warnings, rerun, overfull, underfull }`. Em erro de compilação a função devolve
um `error` com `arquivo:linha: mensagem` e o resultado completo fica em `latex.lastResult()`.
`latex.start(options)` devolve o `Job` sem esperar (`job.events()`, `job.wait()`, `job.cancel()`).

### `Document`

`latex.document({ class, classOptions, lang, packages, title, author, date, hyperref, index,
preamble })` (ou `new Document(...)` com `import { Document } from "./latex.fg"`) e os métodos
encadeáveis:

| | |
|---|---|
| estrutura | `toc()` (sumário), `listOfFigures()`, `listOfTables()`, `part`, `chapter` (report/book), `section(title, level, starred)`, `abstract`, `appendix()`, `label(name)`, `newpage()`, `printIndex()` |
| conteúdo | `text` (escapado), `raw`, `itemize`, `enumerate`, `table(rows, { header, align, caption, label })`, `image(path, { width, caption, label })`, `math(expr, display)`, `code` (verbatim), `quote`, `center`, `env`, `vspace`, `rule` |
| arquivos | `input(file)`, `include(file)`, `bibliography(file, style)`, `usePackage`, `preamble` |
| saída | `build()`, `save(path)`, `compile(options)` |

Trechos para usar dentro de `raw()`: `latex.ref(label)`, `pageref`, `cite(key)`, `footnote(text)`,
`bold`, `italic`, `mono`, `url`, `href(url, text)`, `indexEntry(term)` (`"termo!sub"` cria subitem).

- `hyperref: true` (ou um mapa de opções) carrega o hyperref por último, com links no sumário e
  `pdftitle`/`pdfauthor` nos metadados do PDF.
- `index: true` carrega `makeidx` + `\makeindex`; com `indexEntry()` no texto e `printIndex()` no
  fim a lib gera o índice remissivo: usa o `makeindex` se existir, senão um gerador interno em
  Rust (o Tectonic não traz o makeindex), e roda o passe extra.
- Sumário, listas e referências cruzadas: com pdflatex/xelatex/lualatex a lib repete os passes
  enquanto os arquivos auxiliares (`.aux`, `.toc`, `.lof`, `.lot`, `.idx`...) mudarem, como o
  latexmk; o Tectonic faz isso sozinho.

`latex.escape(text)` escapa `\ { } $ & # ^ _ % ~`. `title`, `author` e `date` aceitam LaTeX cru quando começam com
`\` (`date: "\\today"`). `lang` carrega o babel: `"pt-BR"` (brazil), `"pt-PT"` (portuguese),
`"en"`, `"es"`, `"fr"`, `"de"`… ou uma lista (`["pt-PT", "pt-BR"]`, a última é a principal), para
hifenização, `\today` e os nomes ("Tabela", "Capítulo") no idioma. `latex.babelName(code)` dá o
nome babel. Imagens: `image("logo.png", { width, caption })` com a pasta em `inputs`.

### Outras funções

| | |
|---|---|
| `tools(withVersion)` | `{ platform, tools_dir, engines, helpers, converters, formats }` |
| `converters()`, `canConvert(to)` | conversores encontrados / há ferramenta para esse formato? |
| `engines()`, `engine()`, `hasEngine()`, `ensureEngine()` | motores encontrados / o que será usado |
| `install({ version, dir, url, force, timeout, onProgress })` | baixa o Tectonic; devolve o caminho |
| `convert(pdf, { to, outdir, prefix, dpi, pages, tool })` | PNG/JPG/SVG/TXT/PS com a ferramenta disponível; lista de arquivos |
| `pdfInfo(path)` | `{ pages, version, size, title, author, subject, producer, page_sizes, encrypted }` |
| `pdfText(path, pages)` | texto (páginas separadas por `\f`) |
| `parseLog(text)` | `{ errors, warnings, rerun, overfull, underfull }` |

## API C (`fogulatex`)

Prefixo `flx_`. Jobs `i64` (> 0) em thread própria; status `0`/`-1` (mensagem em
`flx_last_error`); saídas no buffer do chamador ou pelo sink.

| função | |
|---|---|
| `flx_version`, `flx_last_error`, `flx_set_sink` | |
| `flx_tools(with_version, buf, cap)`, `flx_log_parse(text, buf, cap)` | JSON |
| `flx_compile(opts_json)`, `flx_install(opts_json)`, `flx_convert(opts_json)` | handle do job |
| `flx_dispatch(job, timeout_ms, max, cb)`, `flx_result(job, buf, cap)`, `flx_cancel`, `flx_close` | eventos / `done` |
| `flx_pdf_info(path, buf, cap)`, `flx_pdf_text(path, pages_json, buf, cap)` | |

Testes da lib Rust: `cargo test --manifest-path fogulatex/Cargo.toml`.
