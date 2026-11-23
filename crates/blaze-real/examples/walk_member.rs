//! The code builder's WALK from ONE target of ONE member of a real workspace (tempo):
//! `walk_member <checkout> <fetch-store> <module-store> <modules-out> <script-records> <member>
//! <target> <out-file> <tree>`, the first five as `tempo_front` takes them.
//!
//! - `<target>`: `bin:<name>` (the roots: its `main`), `lib-tests` (the member library's test
//!   unit) or `test:<name>` (an integration test): the roots are every `#[test]` fn of that
//!   unit, from its tests record (blaze-scope tests_record.rs: by attributes alone; a `#[bench]`
//!   is no root).
//! - `<tree>`: the commit the run is measured on, printed as given (this example runs no git).
//!
//! The world is `tempo_front`'s, made by the same function (examples/names_load `load`): the
//! unit graph with every dependency, the proc-macro modules, the build scripts' exported
//! outputs. Then the walk (blaze-ladder front2/program/walk_report.rs `walk`, the hello world's
//! walk moved into the library): `REACHABLE:` and `BUILDS:` with their class lists.
//!
//! What the walk cannot take is REFUSED BY NAME, never skipped: a world that does not load (a
//! dependency whose records are missing is `LoadError`'s line, and the run stops); in the
//! target's dependency closure, each proc-macro crate with no prepared module and each crate
//! whose build script has no exported outputs (`WORLD REFUSED ..` lines, a class each, then the
//! walk goes on: their calls and cfgs are refused where met); a root that does not lower or
//! check (the walk's own `WORK LIST (lowering ..)`, `ROOT REFUSED`, `ROOT NOT CHECKED` lines).
//!
//! Output: the whole report to `<out-file>`; to standard output its first line (member, target,
//! roots, tree), the `WORLD REFUSED` summary, `REACHABLE:` and `BUILDS:`. One thread. Files are
//! read and written with `std::fs` as `tempo_front` and names_load do (the examples' form, not
//! the effect layer).

#![feature(allocator_api)]

use std::path::PathBuf;
use std::time::Instant;

use blaze_ladder::front2::program::walk_report::{Ask, BuildFrom, NoHooks, Receivers, RootRead, World, walk};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_scope::tests_record::{TestsRecord, encode};
use blaze_scope::units::Origin;
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::UnitKind;

mod names_load;

/// The target asked for, parsed from `<target>`.
#[derive(Clone, Copy, Debug)]
enum Target<'a> {
    Bin(&'a str),
    LibTests,
    Test(&'a str),
}

impl<'a> Target<'a> {
    fn parse(text: &'a str) -> Option<Target<'a>> {
        match text {
            "lib-tests" => Some(Target::LibTests),
            _ => match text.split_once(':') {
                Some(("bin", name)) if !name.is_empty() => Some(Target::Bin(name)),
                Some(("test", name)) if !name.is_empty() => Some(Target::Test(name)),
                _ => None,
            },
        }
    }

    /// Whether `kind` is this target's unit.
    fn is(&self, kind: &UnitKind<'_>) -> bool {
        match (self, kind) {
            (Target::Bin(want), UnitKind::Bin(name)) => name.as_str() == *want,
            (Target::LibTests, UnitKind::Test { bin: None }) => true,
            (Target::Test(want), UnitKind::IntegrationTest(x)) => x.name.as_str() == *want,
            _ => false,
        }
    }
}

/// A unit kind as the target argument writes it, for the list printed when no unit fits.
fn written(kind: &UnitKind<'_>) -> String {
    match kind {
        UnitKind::Bin(name) => format!("bin:{}", name.as_str()),
        UnitKind::Test { bin: None } => "lib-tests".to_string(),
        UnitKind::Test { bin: Some(name) } => format!("bin-tests:{} (not walked here)", name.as_str()),
        UnitKind::IntegrationTest(x) => format!("test:{}", x.name.as_str()),
        UnitKind::Lib => "lib (no roots: not walked here)".to_string(),
        other => format!("{other:?} (not walked here)"),
    }
}

/// Stops the run with `line` on standard output and in the report file.
fn stop(out_file: &PathBuf, line: &str) -> ! {
    println!("{line}");
    std::fs::write(out_file, format!("{line}\n")).unwrap_or_else(|e| panic!("writing {}: {e}", out_file.display()));
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 10 {
        eprintln!("usage: walk_member <checkout> <fetch-store> <module-store> <modules-out> <script-records> <member> <bin:NAME|lib-tests|test:NAME> <out-file> <tree>");
        std::process::exit(2);
    }
    let (root, store, modules, modules_out, scripts) = (PathBuf::from(&args[1]), PathBuf::from(&args[2]), PathBuf::from(&args[3]), PathBuf::from(&args[4]), PathBuf::from(&args[5]));
    let (member, target_text, out_file, tree) = (args[6].as_str(), args[7].as_str(), PathBuf::from(&args[8]), args[9].as_str());
    let Some(target) = Target::parse(target_text) else {
        stop(&out_file, &format!("REFUSED: the target `{target_text}` is none of `bin:<name>`, `lib-tests`, `test:<name>`"));
    };
    let t0 = Instant::now();
    // The test units are in the graph only when asked for (names_load `load`'s `tests`).
    let tests = !matches!(target, Target::Bin(_));
    let loaded = match names_load::load(&root, &store, Some((modules.as_path(), modules_out.as_path())), Some(scripts.as_path()), tests, blaze_own::Heap::process()) {
        Ok(l) => l,
        Err(e) => stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: the world did not load: {e} (tree {tree})")),
    };
    let names = loaded.names;
    let crates = loaded.crates;

    // The target's crate: a member's unit of the asked kind, by package name.
    let unit_of = |k: usize| match crates.crates[k].origin {
        Origin::Unit(u) if loaded.member.get(k).copied().unwrap_or(false) => Some(&loaded.graph.units[u.index()]),
        _ => None,
    };
    let mut found: Option<usize> = None;
    let mut its_targets: Vec<String> = Vec::new();
    for k in 0..crates.crates.len() {
        let Some(unit) = unit_of(k) else { continue };
        if unit.name.as_str() != member {
            continue;
        }
        if target.is(&unit.kind) {
            if let Some(first) = found {
                stop(&out_file, &format!("REFUSED: two units of `{member}` are `{target_text}`: crates {first} and {k}"));
            }
            found = Some(k);
        } else {
            its_targets.push(written(&unit.kind));
        }
    }
    let Some(k) = found else {
        its_targets.sort();
        stop(&out_file, &format!("REFUSED: `{member}` has no unit `{target_text}` in the graph; its units: {}", its_targets.join(", ")));
    };

    // The roots.
    let alloc = Alloc::global();
    let Ok(frame) = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer) else {
        stop(&out_file, "REFUSED: the command's frame");
    };
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(frame));
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let mut roots: Vec<blaze_id::ItemKey> = Vec::new();
    match target {
        Target::Bin(_) => {
            let main = names::path(names, names.root(k), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, alloc)
                .ok()
                .and_then(|f| f.whole(1))
                .map(|d| d.key);
            match main {
                Some(key) => roots.push(key),
                None => stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: no `main` at the crate root of crate {k} (tree {tree})")),
            }
        }
        Target::LibTests | Target::Test(_) => {
            let bytes = encode(names.krate(k), alloc).unwrap_or_else(|e| stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: its tests record: {e} (tree {tree})"))).bytes;
            let record = TestsRecord::open(blaze_scope::resolve::aligned(&bytes, bump))
                .unwrap_or_else(|e| stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: its tests record does not open: {e} (tree {tree})")));
            for i in 0..record.len() {
                match record.row(i) {
                    Ok(Some(row)) if !row.bench => roots.push(row.key),
                    Ok(Some(_)) => {}
                    Ok(None) => stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: its tests record has no row {i} inside its count (tree {tree})")),
                    Err(e) => stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: its tests record's row {i}: {e} (tree {tree})")),
                }
            }
            if roots.is_empty() {
                stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: the unit has no `#[test]` fn (tree {tree})"));
            }
        }
    }

    let mut report = format!("WALK {member} {target_text} (crate {k}): {} roots; tree {tree}; world loaded in {:.1}s\n", roots.len(), t0.elapsed().as_secs_f64());
    let mut summary = report.clone();

    // The world's gaps in the target's dependency closure, each a class by name: a work list
    // over the crates' dependency edges, each crate once, listed in crate order.
    let mut seen = vec![false; crates.crates.len()];
    let mut work = vec![k];
    seen[k] = true;
    while let Some(at) = work.pop() {
        for &(_, dep) in crates.crates[at].deps.iter() {
            if !seen[dep] {
                seen[dep] = true;
                work.push(dep);
            }
        }
    }
    let mut no_module: Vec<&str> = Vec::new();
    let mut no_outputs: Vec<&str> = Vec::new();
    for (i, c) in crates.crates.iter().enumerate() {
        if !seen[i] {
            continue;
        }
        let proc_macro = match c.origin {
            Origin::Unit(u) => loaded.graph.units[u.index()].kind == UnitKind::ProcMacro,
            Origin::Toolchain { .. } => false,
        };
        if proc_macro && !loaded.procs.is_some_and(|p| p.registered(i)) {
            no_module.push(c.name);
        }
        if c.build_script && c.script.is_none() {
            no_outputs.push(c.name);
        }
    }
    for (class, list) in [("a proc-macro crate with no prepared module (its macros' calls are refused where met)", &no_module), ("a build script with no exported outputs (its cfgs and OUT_DIR are not known)", &no_outputs)] {
        if !list.is_empty() {
            let line = format!("WORLD REFUSED: {} x {class}: {}\n", list.len(), list.join(", "));
            report.push_str(&line);
            summary.push_str(&line);
        }
    }

    // The walk, over the world's names with the bodies' macro sources and the receivers'
    // pointer types, as the census reads bodies.
    let crate_names: Vec<&str> = crates.crates.iter().map(|c| c.name).collect();
    let survey_frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame")));
    let world = World {
        names,
        frame,
        survey_frame,
        prepared: Prepares::new_in(bump),
        sources: &[],
        crate_names: &crate_names,
        body_sources: Some(loaded.body_sources()),
        receivers: Receivers::Found,
    };
    let label = match target {
        Target::Bin(_) => "its main".to_string(),
        _ => format!("{} tests", roots.len()),
    };
    let ask = Ask { roots: &roots, build_from: BuildFrom::Roots, read: RootRead::Whole, label: &label };
    let t1 = Instant::now();
    let mut walked = String::new();
    let counts = walk(&world, &ask, &mut NoHooks, &mut walked, alloc).unwrap_or_else(|e| stop(&out_file, &format!("WALK {member} {target_text}: REFUSED: {e} (tree {tree})")));
    report.push_str(&walked);
    for line in walked.lines().filter(|l| l.starts_with("REACHABLE:") || l.starts_with("BUILDS:")) {
        summary.push_str(line);
        summary.push('\n');
    }
    let tail = format!(
        "WALKED {member} {target_text}: {} roots ({} not lowered, {} not spelled, {} not checked); walk and survey {:.1}s; {:.1}s in all\n",
        counts.roots,
        counts.not_lowered,
        counts.not_spelled,
        counts.walked - counts.roots_checked,
        t1.elapsed().as_secs_f64(),
        t0.elapsed().as_secs_f64()
    );
    report.push_str(&tail);
    summary.push_str(&tail);
    print!("{summary}");
    std::fs::write(&out_file, &report).unwrap_or_else(|e| panic!("writing {}: {e}", out_file.display()));
}
