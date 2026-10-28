//! `ra-jit proc-macro-wasm`: one macro of a module expanded on an input from a file, timed,
//! and, given a library of the same crate and a proc-macro server, the same expansion by the
//! server, timed too, and the outputs compared.
//!
//! ```text
//! ra-jit proc-macro-wasm <module.wasm> --macro <name> --input <file> [--attr <text>]
//!     [--package <dir>] [--native <library> --server <proc-macro-srv>] [--runs <n>]
//!     [--store <dir>] [--quick]
//! ```
//!
//! What it prints, a line each, in milliseconds: the module's compile by Cranelift (by Winch
//! with `--quick`), its first expansion (a fresh instance, as every expansion is) and the
//! median of `--runs` more; with `--store`, the first expansion, kept, and the median of
//! finding it kept; with `--native`, the server's start and load of the library, its first
//! expansion and the median of more.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base_db::Env;
use paths::AbsPathBuf;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

use crate::{Call, Wasm};

struct Args {
    module: PathBuf,
    name: String,
    input: PathBuf,
    attr: Option<String>,
    package: Option<PathBuf>,
    native: Option<PathBuf>,
    server: Option<PathBuf>,
    runs: usize,
    store: Option<PathBuf>,
    quick: bool,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let module = PathBuf::from(args.next().ok_or("the module is named first")?);
    let mut parsed = Args {
        module,
        name: String::new(),
        input: PathBuf::new(),
        attr: None,
        package: None,
        native: None,
        server: None,
        runs: 20,
        store: None,
        quick: false,
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--macro" => parsed.name = value()?,
            "--input" => parsed.input = value()?.into(),
            "--attr" => parsed.attr = Some(value()?),
            "--package" => parsed.package = Some(value()?.into()),
            "--native" => parsed.native = Some(value()?.into()),
            "--server" => parsed.server = Some(value()?.into()),
            "--runs" => parsed.runs = value()?.parse().map_err(|_| "--runs takes a number")?,
            "--store" => parsed.store = Some(value()?.into()),
            "--quick" => parsed.quick = true,
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    if parsed.name.is_empty() {
        return Err("--macro names the macro".to_owned());
    }
    Ok(parsed)
}

fn ms(took: Duration) -> String {
    format!("{:.3}", took.as_secs_f64() * 1e3)
}

fn median(mut all: Vec<Duration>) -> Duration {
    all.sort();
    all.get(all.len() / 2).copied().unwrap_or_default()
}

fn span(file: u32) -> Span {
    let file_id = EditionedFileId::new(FileId::from_raw(file), Edition::CURRENT);
    Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// The environment cargo gives a crate of the package at `dir`.
fn env_of(dir: Option<&Path>) -> Env {
    let mut env = Env::default();
    if let Some(dir) = dir {
        let name = dir.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        env.set("CARGO_MANIFEST_DIR", dir.display().to_string());
        env.set("CARGO_PKG_NAME", name.clone());
        env.set("CARGO_CRATE_NAME", name.replace('-', "_"));
        env.set("CARGO_PKG_VERSION", "0.1.0");
    }
    env
}

pub fn command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let args = parse(args)?;
    let text = std::fs::read_to_string(&args.input)
        .map_err(|e| format!("{}: {e}", args.input.display()))?;
    let call_site = span(0);
    let subtree = crate::tokens(&text, call_site)?;
    let attr = args.attr.as_deref().map(|it| crate::tokens(it, call_site)).transpose()?;
    let env = env_of(args.package.as_deref());
    let call = Call {
        subtree: &subtree,
        attrs: attr.as_ref(),
        env: &env,
        def_site: call_site,
        call_site,
        mixed_site: call_site,
    };

    // What the store keeps of the machine code is of this build, which no toolchain is
    // linked against here: the expansions are kept apart from it, of no build.
    ra_jit_store::compiles_against("ra-jit proc-macro-wasm");
    let code = args.store.as_deref().map(ra_jit_store::Store::open_at).transpose();
    let code = code.map_err(|e| e.to_string())?;
    let wasm = Wasm::at(&args.module, code.as_ref(), args.quick)?;
    println!("wasm compile: {}", ms(wasm.compiled_in));
    if let Some(code) = code {
        code.flush();
    }
    let macros = wasm.macros()?;
    let index = macros.iter().position(|it| it.name == args.name).ok_or_else(|| {
        let names: Vec<_> = macros.iter().map(|it| it.name.as_str()).collect();
        format!("the module has no macro {}: it has {names:?}", args.name)
    })?;
    let listed = &macros[index];
    let first = wasm.expand(index as u32, listed, None, &call, None);
    let output = first.output.map_err(|e| format!("{e:?}"))?;
    println!("wasm first: {}", ms(first.took));
    let runs = (0..args.runs).map(|_| wasm.expand(index as u32, listed, None, &call, None).took);
    println!("wasm warm: {}", ms(median(runs.collect())));
    let wasm_text = output.to_string();

    if let Some(dir) = &args.store {
        let store = ra_jit_store::Store::open_at(dir).map_err(|e| e.to_string())?;
        let kept = wasm.expand(index as u32, listed, None, &call, Some(&store));
        println!("wasm kept: {} (found {})", ms(kept.took), kept.found);
        store.flush();
        drop(store);
        let store = ra_jit_store::Store::open_at(dir).map_err(|e| e.to_string())?;
        let mut found = Vec::new();
        for _ in 0..args.runs.max(1) {
            let ran = wasm.expand(index as u32, listed, None, &call, Some(&store));
            let same = ran.output.as_ref().is_ok_and(|it| it.to_string() == wasm_text);
            if !ran.found || !same {
                println!("wasm found: not (found {}, same {same})", ran.found);
                break;
            }
            found.push(ran.took);
        }
        if found.len() == args.runs.max(1) {
            println!("wasm found: {}", ms(median(found)));
        }
    }

    if let (Some(native), Some(server)) = (&args.native, &args.server) {
        let start = Instant::now();
        let server =
            AbsPathBuf::assert_utf8(std::path::absolute(server).map_err(|e| e.to_string())?);
        let none: [(&str, &Option<&str>); 0] = [];
        let client = proc_macro_api::ProcMacroClient::spawn(&server, none, None, 1)
            .map_err(|e| format!("{server}: {e}"))?;
        let native =
            AbsPathBuf::assert_utf8(std::path::absolute(native).map_err(|e| e.to_string())?);
        let loaded = client
            .load_dylib(proc_macro_api::MacroDylib::new(native.clone()))
            .map_err(|e| format!("{native}: {e}"))?;
        let found = loaded.into_iter().find(|it| it.name() == args.name);
        let found = found.ok_or_else(|| format!("{native} has no macro {}", args.name))?;
        println!("native start: {}", ms(start.elapsed()));
        let env: Vec<(String, String)> =
            env.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        let expand = || {
            let start = Instant::now();
            let made = found.expand(
                subtree.view(),
                attr.as_ref().map(|it| it.view()),
                env.clone(),
                call_site,
                call_site,
                call_site,
                std::env::current_dir().map(|it| it.display().to_string()).unwrap_or_default(),
                None,
            );
            (made, start.elapsed())
        };
        let (made, took) = expand();
        let made = made.map_err(|e| e.to_string())??;
        println!("native first: {}", ms(took));
        println!("native warm: {}", ms(median((0..args.runs).map(|_| expand().1).collect())));
        let native_text = made.to_string();
        println!("same output: {}", native_text == wasm_text);
        if native_text != wasm_text {
            println!("wasm: {wasm_text}");
            println!("native: {native_text}");
        }
    } else {
        println!("output: {wasm_text}");
    }
    Ok(())
}
