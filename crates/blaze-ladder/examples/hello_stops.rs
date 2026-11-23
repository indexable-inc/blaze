//! THE STOP LIST of one program, without running it: everything `blaze run` would stop at on
//! the new front end, in one release run (the command itself shows one stop a run, after the
//! toolchain's names: about 30 s each).
//!
//! `mixcheck.sh blaze-ladder run hello_stops <program folder> <package> <edition> <shelf> <out file>`
//!
//! - `<program folder>`: holds `src/main.rs` (every `src/*.rs` is read);
//! - `<package>`, `<edition>`: as its `Cargo.toml` writes them (`hello-world`, `blaze-2026.11`);
//! - `<shelf>`: the toolchain's shelf (`<HOME>/.blaze/v2/toolchain`), or `-` for none;
//! - `<out file>`: the lines, also printed.
//!
//! Lines: `ENTRY: ..` (the survey front's entry row: its path, export and whether the root's
//! text holds the appended entry module), `LEFT OUT: n` and each (`left out: <fn>: <class>`),
//! then either `REFUSED BEFORE THE RUN: <error>` (what [`build_stubbed`] stops at: the entry,
//! a function of the program's own crate, or a whole-build error) or `NOT BUILT: n of m` and
//! each stub's refusal (`stub <n>: <refusal>`): the functions a run stops at IF it calls them.
//!
//! A tool: it reads the folder with `std::fs` and prints; nothing here is blaze's runtime.

#![feature(allocator_api)]

use std::alloc::Global;
use std::fmt::Write as _;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_stubbed};
use blaze_ladder::seam::FrontEnd;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, folder, package, edition, shelf, out_file] = args.as_slice() else {
        eprintln!("usage: hello_stops <program folder> <package> <edition> <shelf or -> <out file>");
        std::process::exit(2);
    };
    let mut sources: Vec<(String, String)> = Vec::new();
    let src = std::path::Path::new(folder).join("src");
    for entry in std::fs::read_dir(&src).unwrap_or_else(|e| panic!("{}: {e}", src.display())) {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_some_and(|x| x == "rs") {
            let name = path.file_name().expect("a file name").to_string_lossy().into_owned();
            sources.push((format!("src/{name}"), std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))));
        }
    }
    sources.sort();
    let files: Vec<SourceFile<'_>> = sources.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let shelf = (shelf != "-").then_some(shelf.as_str());
    // `blaze run`'s setting: the dev profile's overflow checks (D101).
    let new = NewFront { package, edition, shelf, overflow: blaze_build::OverflowChecks::On };
    let mut lines = String::new();
    // The program's tables are `'static` (front2's store, the build's): until a tool's main is
    // handed its heap, the global allocator is the only `Alloc<'static>` it can name.
    let alloc = blaze_own::Alloc::global();
    let tc = match shelf {
        Some(shelf) => Toolchain::open(&mut DirShelf::new(shelf.into())).unwrap_or_else(|e| panic!("the toolchain: {e}")),
        None => Toolchain::none(alloc),
    };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let appended = user.files.iter().any(|(_, text)| text.contains("mod __blaze_entry"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the frame");
    let erased = blaze_own::Erased::new(&frame);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, erased.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    {
        let mut left_out = Vec::new_in(Global);
        match Front2Program::survey(&names, &frame, &files, &mut left_out, alloc) {
            Err(e) => writeln!(lines, "ENTRY: no survey: {e}").expect("a line"),
            Ok(program) => {
                match program.entry() {
                    None => writeln!(lines, "ENTRY: none (the root did not check or was left out); entry module appended: {appended}"),
                    Some(i) => {
                        let f = program.function(i);
                        writeln!(lines, "ENTRY: row {i} `{}` export {:?} kind {:?} of {} fns; entry module appended: {appended}", f.path, f.export, f.kind, program.fn_count())
                    }
                }
                .expect("a line");
                if let Some((item, why)) = program.own_left_out() {
                    writeln!(lines, "OWN LEFT OUT: `{item}`: {why}").expect("a line");
                }
            }
        }
        writeln!(lines, "LEFT OUT: {}", left_out.len()).expect("a line");
        for (function, class) in left_out.iter() {
            writeln!(lines, "left out: {function}: {class}").expect("a line");
        }
    }
    match build_stubbed(&names, &files, &texts, alloc) {
        Err(e) => writeln!(lines, "REFUSED BEFORE THE RUN: {e}").expect("a line"),
        Ok(built) => {
            writeln!(lines, "NOT BUILT: {} of {}", built.refused().len(), built.reached()).expect("a line");
            for (n, refusal) in built.refused().iter().enumerate() {
                writeln!(lines, "stub {n}: {refusal}").expect("a line");
            }
        }
    }
    print!("{lines}");
    std::fs::write(out_file, &lines).unwrap_or_else(|e| panic!("{out_file}: {e}"));
}
