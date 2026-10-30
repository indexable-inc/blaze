//! `ra-jit borrowck [--edition E]`: the diagnostics of single-file crates, one per line of
//! standard input, each a path, written as one JSON object per line on standard output. A
//! tab after the path starts the `cfg`s the file is read with, as rustc's `--cfg`: `name`
//! or `name="value"`, separated by spaces.
//!
//! The standard library is loaded once; each file then takes the place of the one before as
//! the root of a library crate of the given edition, so what is checked again is only what
//! the file changes. `harness/borrowck_ui.py` scores rustc's own `tests/ui` with it, and
//! `harness/verdict_ui.py` the check verdict: each diagnostic says whether it is in that
//! verdict (`"verdict"`, from [`crate::in_verdict`], with lints not capped).
//!
//! `ra-jit borrowck --project DIR` checks a cargo workspace instead: the errors the borrow
//! checker finds in its crates, which for code rustc accepts must be none.

use std::{
    io::{BufRead as _, Write as _},
    time::Instant,
};

use anyhow::Context as _;
use ide_db::{ChangeWithProcMacros, RootDatabase, base_db::SourceDatabase as _};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{CargoConfig, RustLibSource};
use vfs::{AbsPathBuf, VfsPath};

pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut edition = "2015".to_owned();
    let mut project = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edition" => edition = args.next().context("--edition needs a year")?,
            "--project" => project = Some(args.next().context("--project needs a directory")?),
            _ => anyhow::bail!("unexpected argument {arg}"),
        }
    }
    if let Some(project) = project {
        return check_project(&project);
    }
    let dir =
        std::env::temp_dir().join(format!("ra-jit-borrowck-{}-{edition}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"ui\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\n\n\
             [lib]\npath = \"lib.rs\"\n\n[workspace]\n"
        ),
    )?;
    let root = dir.join("lib.rs");
    std::fs::write(&root, "")?;
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: false,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::None,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let (mut db, vfs, _) =
        load_cargo::load_workspace_at(&dir, &cargo_config, &load_config, &|_| ())?;
    let file = [root.clone(), dunce::canonicalize(&root)?]
        .into_iter()
        .find_map(|it| vfs.file_id(&VfsPath::from(AbsPathBuf::assert_utf8(it))))
        .map(|(file, _)| file)
        .context("the crate root is not loaded")?;
    let config = ide_diagnostics::DiagnosticsConfig::test_sample();
    let krate = *ide_db::base_db::relevant_crates(&db, file)
        .first()
        .context("the crate root is in no crate")?;
    let base_cfg = krate.cfg_options(&db).clone();
    let mut cfgs = String::new();
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let (test, wanted) = match line.split_once('\t') {
            Some((test, wanted)) => (test.to_owned(), wanted.to_owned()),
            None => (line, String::new()),
        };
        if wanted != cfgs {
            use ide_db::base_db::salsa::Setter as _;
            let mut options = base_cfg.clone();
            for cfg in wanted.split_whitespace() {
                match cfg.split_once('=') {
                    Some((key, value)) => options.insert_key_value(
                        intern::Symbol::intern(key),
                        intern::Symbol::intern(value.trim_matches('"')),
                    ),
                    None => options.insert_atom(intern::Symbol::intern(cfg)),
                }
            }
            krate.set_cfg_options(&mut db).to(options);
            cfgs = wanted;
        }
        let started = Instant::now();
        let text = std::fs::read_to_string(&test).unwrap_or_default();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file, Some(load_cargo::source_text(text)));
        db.apply_change(change);
        if let Ok(name) = std::env::var("RA_JIT_BORROWCK_MIR") {
            dump_mir(&db, file, &name);
        }
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diagnostics(&db, &config, file)
        }));
        let mut out =
            serde_json::json!({ "path": test, "ms": started.elapsed().as_millis() as u64 });
        match found {
            Ok(found) => out["diagnostics"] = serde_json::Value::Array(found),
            Err(panic) => out["panic"] = crate::panic_message(&*panic).into(),
        }
        let mut stdout = stdout.lock();
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The codes of what the borrow checker reports: what a project rustc accepts must have none.
const BORROWCK_CODES: &[&str] = &[
    "E0373", "E0381", "E0382", "E0384", "E0499", "E0500", "E0501", "E0502", "E0503", "E0505",
    "E0506", "E0507", "E0508", "E0509", "E0510", "E0515", "E0521", "E0524", "E0596", "E0597",
    "E0621", "E0713", "E0716", "E0793", "E0309", "E0310", "E0311", "borrowck",
];

/// `ra-jit borrowck --project DIR`: the borrow checker's errors in the crates of the cargo
/// workspace at `DIR`, one JSON object per line, and a last line counting files, errors,
/// `unused_mut` warnings and files the checker panicked on, which fail it.
fn check_project(project: &str) -> anyhow::Result<()> {
    let root = AbsPathBuf::assert_utf8(dunce::canonicalize(project)?);
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: true,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: std::thread::available_parallelism().map_or(1, |it| it.get()),
        proc_macro_processes: 1,
    };
    let started = Instant::now();
    let (db, vfs, _) =
        load_cargo::load_workspace_at(root.as_ref(), &cargo_config, &load_config, &|_| ())?;
    let config = ide_diagnostics::DiagnosticsConfig::test_sample();
    let files: Vec<vfs::FileId> = hir::attach_db(&db, || {
        let mut files = Vec::new();
        for krate in hir::Crate::all(&db) {
            if !krate.origin(&db).is_local() {
                continue;
            }
            for module in krate.modules(&db) {
                let file = module.definition_source_file_id(&db).original_file(&db);
                files.push(file.file_id(&db));
            }
        }
        files.sort();
        files.dedup();
        files
    });
    let stdout = std::io::stdout();
    let mut errors = 0;
    let mut unused_mut = 0;
    let mut panics = 0;
    for &file in &files {
        if let Ok(name) = std::env::var("RA_JIT_BORROWCK_MIR") {
            dump_mir(&db, file, &name);
        }
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diagnostics(&db, &config, file)
        }));
        let path = vfs.file_path(file).to_string();
        let mut stdout = stdout.lock();
        match found {
            Ok(found) => {
                for it in found {
                    let code = it["code"].as_str().unwrap_or_default();
                    if it["severity"] == "Error"
                        && (BORROWCK_CODES.contains(&code) || code == "unused_mut")
                    {
                        errors += 1;
                        writeln!(stdout, "{}", serde_json::json!({ "path": path, "error": it }))?;
                    } else if code == "unused_mut" {
                        unused_mut += 1;
                        writeln!(stdout, "{}", serde_json::json!({ "path": path, "warning": it }))?;
                    }
                }
            }
            Err(panic) => {
                panics += 1;
                let panic = crate::panic_message(&*panic);
                writeln!(stdout, "{}", serde_json::json!({ "path": path, "panic": panic }))?;
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    writeln!(
        stdout.lock(),
        "{}",
        serde_json::json!({
            "files": files.len(),
            "errors": errors,
            "unused_mut": unused_mut,
            "panics": panics,
            "secs": secs
        })
    )?;
    // A file the checker panics on has none of its errors reported.
    anyhow::ensure!(
        panics == 0,
        "the borrow checker panicked on {panics} of {} files",
        files.len()
    );
    Ok(())
}

/// Prints the MIR of each function named `name` at the top of `file`, and of its closures, to
/// stderr: `RA_JIT_BORROWCK_MIR`, for working on the checker.
fn dump_mir(db: &RootDatabase, file: vfs::FileId, name: &str) {
    use hir_ty::db::HirDatabase as _;
    hir::attach_db(db, || {
        let sema = hir::Semantics::new(db);
        let Some(module) = sema.file_to_module_def(file) else { return };
        for def in module.declarations(db) {
            if let hir::ModuleDef::Function(function) = def
                && function.name(db).as_str() == name
            {
                let Ok(id) = hir_def::FunctionId::try_from(function) else { continue };
                let mut bodies = match db.mir_body(hir_ty::InferBodyId::DefWithBodyId(id.into())) {
                    Ok(body) => vec![body],
                    Err(err) => {
                        eprintln!("{name}: {err:?}");
                        continue;
                    }
                };
                // The closures after it, as the borrow checker checks them with it.
                while let Some(body) = bodies.pop() {
                    eprintln!("{}", hir_ty::mir::print_mir_body(db, body, name));
                    for &closure in &body.closures {
                        match db.mir_body_for_closure(closure) {
                            Ok(it) => bodies.push(it),
                            Err(err) => eprintln!("{name} closure: {err:?}"),
                        }
                    }
                }
            }
        }
    })
}

/// Every diagnostic rust-analyzer has for `file`, with the line and column (from 1) of where
/// it starts.
fn diagnostics(
    db: &RootDatabase,
    config: &ide_diagnostics::DiagnosticsConfig,
    file: vfs::FileId,
) -> Vec<serde_json::Value> {
    hir::attach_db(db, || {
        let found = ide_diagnostics::full_diagnostics(
            db,
            config,
            &ide_db::assists::AssistResolveStrategy::None,
            file,
        );
        let text = db.file_text(file).text(db);
        found
            .into_iter()
            .filter(|it| it.range.file_id == file)
            .map(|it| {
                let start = usize::from(it.range.range.start()).min(text.len());
                let before = &text[..start];
                let line = before.matches('\n').count() + 1;
                let col = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
                let expanded = it.main_node.is_some_and(|it| it.file_id.is_macro());
                let verdict = crate::in_verdict(
                    it.code.as_str(),
                    it.severity,
                    expanded,
                    true,
                    crate::borrowck_refuses(),
                );
                serde_json::json!({
                    "code": it.code.as_str(),
                    "severity": format!("{:?}", it.severity),
                    "line": line,
                    "col": col,
                    "message": it.message,
                    "expanded": expanded,
                    "verdict": verdict,
                })
            })
            .collect()
    })
}
