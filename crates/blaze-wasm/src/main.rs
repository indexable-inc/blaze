//! The compiler of the browser run from a shell, to try it without one, and to pack the
//! source of `core` for the page.
//!
//! ```text
//! blaze-wasm bundle library core.bundle
//! blaze-wasm build core.bundle out/ first.rs [edited.rs ...]
//! ```
//!
//! `build` writes `out/<n>.wasm` and `out/<n>.json` for each source in turn, as edits of
//! one program.

use std::{error::Error, fs, path::Path};

use blaze_wasm::{Compiler, MAIN, bundle, pack};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["bundle", library, out] => {
            let mut files = Vec::new();
            for (dir, only) in [
                ("core/src", None),
                ("stdarch/crates/core_arch/src", Some("wasm32")),
                ("portable-simd/crates/core_simd/src", None),
            ] {
                collect(Path::new(library), Path::new(dir), only, &mut files)?;
            }
            fs::write(out, bundle::pack(&files))?;
        }
        ["build", core, out, ref sources @ ..] => {
            let mut compiler = Compiler::default();
            compiler.set_files(bundle::unpack(&fs::read(core)?)?);
            fs::create_dir_all(out)?;
            for (at, source) in sources.iter().enumerate() {
                compiler.set_file(MAIN, fs::read_to_string(source)?);
                let build = compiler.build()?;
                fs::write(format!("{out}/{at}.wasm"), &build.patch.module)?;
                fs::write(format!("{out}/{at}.json"), build.report())?;
                eprintln!("{source}: {}", build.report());
            }
        }
        ["pack", project, main, out] => {
            let pack =
                pack::record(Path::new(project), "wasm32-unknown-unknown", main, |db, crates| {
                    for &krate in crates {
                        hir_def::nameres::crate_def_map(db, krate);
                    }
                    hir::attach_db(db, || {
                        for function in
                            blaze_wasm::functions_of(db, crates[crates.len() - 1].into())
                        {
                            hir_ty::InferenceResult::of(db, hir_def::DefWithBodyId::from(function));
                        }
                    });
                })?;
            eprintln!(
                "{} crates, {} files, {} expansions",
                pack.crates.len(),
                pack.files.len(),
                pack.expansions.len()
            );
            fs::write(out, serde_json::to_vec(&pack)?)?;
        }
        ["host", module, pack] => {
            let mut packed: pack::Pack = serde_json::from_slice(&fs::read(pack)?)?;
            let host = blaze_wasm::host::Host::read(&fs::read(module)?)?;
            eprintln!("{} functions, {} data symbols", host.functions.len(), host.data.len());
            packed.host = Some(host);
            fs::write(pack, serde_json::to_vec(&packed)?)?;
        }
        ["analyze", pack] => {
            let mut compiler = Compiler::default();
            let pack: pack::Pack = serde_json::from_slice(&fs::read(pack)?)?;
            let start = std::time::Instant::now();
            compiler.set_pack(pack, blaze_wasm::macros::expand)?;
            eprintln!("load: {:.1?}", start.elapsed());
            let (crates, resolve, infer) = compiler.analyze();
            eprintln!("{crates} crates: names {resolve:.1?}, types of the program {infer:.1?}");
            let missed = compiler.missed();
            eprintln!("{} expansions missed", missed.len());
            for call in missed.iter().take(20) {
                eprintln!("  {}", call.chars().take(200).collect::<String>());
            }
            if std::env::var_os("BLAZE_EXPANDED").is_some() {
                eprintln!("{}", compiler.expanded());
            }
            let start = std::time::Instant::now();
            let build = compiler.build()?;
            eprintln!(
                "build: {:.1?}, {} functions, {} bytes, {} of {} bodies compiled, {} skipped",
                start.elapsed(),
                build.patch.functions.len(),
                build.patch.module.len(),
                build.compiled,
                build.bodies,
                build.skipped.len()
            );
            for (name, why) in build.skipped.iter().take(20) {
                eprintln!("  {name}: {why}");
            }
        }
        _ => {
            return Err(
                "usage: blaze-wasm bundle <library> <out> | build <bundle> <out> <rs>..".into()
            );
        }
    }
    Ok(())
}

/// The Rust files under `library/dir`, named as the compiler's file system has them. With
/// `only`, of the directories right under it only that one.
fn collect(
    library: &Path,
    dir: &Path,
    only: Option<&str>,
    files: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(library.join(dir))? {
        let entry = entry?;
        let path = dir.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if only.is_none_or(|only| entry.file_name() == only) {
                collect(library, &path, None, files)?;
            }
        } else if path.extension().is_some_and(|it| it == "rs" || it == "md") {
            files.push((format!("/{}", path.display()), fs::read_to_string(entry.path())?));
        }
    }
    Ok(())
}
