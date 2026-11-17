//! The BUILD census of a real workspace (tempo): how much of the code the front end checks the
//! back half builds. `tempo_build <checkout> <fetch-store> <module-store> <modules-out>
//! <script-records> <root crate[,crate..]> [--crates]`.
//!
//! The names are loaded as `tempo_front` loads them (examples/names_load: the unit graph, the
//! fetch store, the proc-macro modules, the build scripts' records). Then ONE WALK FROM THE
//! ROOTS: each named crate's `fn main` (a workspace binary's), through front2's reach
//! (`Front2Program::survey_from`: every fn reached is lowered and checked; a fn that does not
//! check is LEFT OUT, by name and cause, and the walk goes on from what it calls). Over that
//! program, `blaze_ladder::run::build_survey`, the run path's own build: every non-generic fn
//! that checked is built to LIR (blaze-build) as an instance of its own, each generic fn at
//! every ground instance a built body calls it at, and each built body is handed to the run
//! path's emitter (`blaze_emit::emit`); a refusal is filed by the fn and its typed name and the
//! survey goes on. Nothing of tempo is run, placed or executed.
//!
//! What is printed: the reach (fns met, left out by the front end), the instances tried, built
//! and refused, the refusals by STAGE (`builder`: blaze-build and the builder's lookups;
//! `callee`: a callee the walk could not table (left out, or never reached); `tier`: the
//! emitter) and by CLASS (the refusal's own words with every number, key and `a::b` path taken
//! out), largest first, each with one example: the fn's path and the whole refusal line. Then
//! the refusals by the refused fn's crate, and the front end's left-out classes the same way.
//! Every table is sorted (count, then text): no map order reaches the output. One thread.

use std::{env, panic, process, time::Instant};

use blaze_ladder::front2::program::{Front2Program, LeftOut};
use blaze_ladder::run::{OverflowChecks, build_survey};
use blaze_ladder::seam::FrontEnd;
use blaze_own::{AString, AVec, Alloc, ASortedMap};
use blaze_scope::names::{self as scope_names, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{Namespace, PathKind};

#[expect(dead_code, reason = "stages' loader, shared: this example reads only the names and the members")]
mod names_load;

/// One class's row: how many refusals, and the first example (in the survey's order, which is
/// the program's key order: deterministic).
struct Row<'a> {
    count: usize,
    function: AString<'a>,
    line: AString<'a>,
}

/// A refusal's stage, from the words `build_survey` files it under (run.rs `filed`).
fn stage(class: &str) -> &'static str {
    if class.starts_with("emit:") {
        "tier"
    } else if class.starts_with("callee") || class.starts_with("left out:") {
        "callee"
    } else {
        "builder"
    }
}

/// The refusal's own name with the numbers, keys and paths taken out: a hex number is `0x_`, a
/// back-quoted span holding `::` is `` `_` ``, a run of digits not inside a word is `N`.
fn class_of<'a>(said: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(said.len(), alloc);
    let b = said.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'`' {
            if let Some(len) = said[i + 1..].find('`') {
                let inner = &said[i + 1..i + 1 + len];
                if inner.contains("::") {
                    out.push_str("`_`");
                } else {
                    out.push('`');
                    out.push_str(inner);
                    out.push('`');
                }
                i += len + 2;
                continue;
            }
        }
        let after_word = i > 0 && (b[i - 1].is_ascii_alphabetic() || b[i - 1] == b'_');
        if c == b'0' && b.get(i + 1) == Some(&b'x') && !after_word {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            out.push_str("0x_");
            i = j;
            continue;
        }
        if c.is_ascii_digit() && !after_word {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            out.push('N');
            i = j;
            continue;
        }
        let ch = said[i..].chars().next().unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A fn path's crate: the text before its first `::`, a leading `<` dropped.
fn crate_of(path: &str) -> &str {
    let p = path.trim_start_matches('<');
    p.split("::").next().unwrap_or(p)
}

/// Files `(function, said)` under its class in `table`.
fn file<'a>(table: &mut ASortedMap<'a, AString<'a>, Row<'a>>, key: &str, function: &str, line: &str, alloc: Alloc<'a>) {
    let k = AString::from_str_in(key, alloc);
    match table.get_mut(&k) {
        Some(row) => row.count += 1,
        None => {
            table.insert(k, Row { count: 1, function: AString::from_str_in(function, alloc), line: AString::from_str_in(line, alloc) });
        }
    }
}

/// `table`'s rows largest first, then by class.
fn sorted<'t, 'a>(table: &'t ASortedMap<'a, AString<'a>, Row<'a>>, alloc: Alloc<'a>) -> AVec<'a, (&'t str, &'t Row<'a>)> {
    let mut rows: AVec<'a, (&'t str, &'t Row<'a>)> = AVec::with_capacity_in(table.len(), alloc);
    for (k, r) in table.iter() {
        rows.push((k.as_str(), r));
    }
    rows.as_mut_slice().sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    rows
}

/// Each named crate's `fn main`, by its library key: every member crate of that name that has
/// one (a package's binary and its library share the name; only the binary has `main`).
fn roots<'a>(names: &Names<'_>, member: &[bool], wanted: &str, alloc: Alloc<'a>) -> AVec<'a, blaze_id::ItemKey> {
    let mut out: AVec<'a, blaze_id::ItemKey> = AVec::with_capacity_in(4, alloc);
    for want in wanted.split(',') {
        let mut found = 0;
        for (k, c) in names.crates().iter().enumerate() {
            if c.name != want || !member.get(k).copied().unwrap_or(false) {
                continue;
            }
            let written = Written { kind: PathKind::Plain, segments: &["main"] };
            if let Ok(r) = scope_names::path(names, names.root(k), Namespace::Values, written, alloc)
                && let Some(def) = r.whole(1)
            {
                println!("root: crate {want} ({k}): `main` at key {:#x}", def.key.0);
                out.push(def.key);
                found += 1;
            }
        }
        if found == 0 {
            println!("root: no member crate named {want:?} has a `fn main`");
        }
    }
    out
}

/// The front end's left-out fns by class (the cause's words with numbers, keys and paths taken
/// out), largest first, each with its first fn and the whole cause; under each class, the
/// causes behind it by the row's split (front2 program/left_out.rs `LeftOut::split`: a gap's
/// own item by its path and why), kept whole, so "check gap: RefusedImpl" splits by impl and
/// why. Printed before the survey, so a survey that stops still leaves them.
fn print_left(left: &[(Box<str>, LeftOut)], alloc: Alloc<'_>) {
    let mut lefts: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    // Keyed `<class>\t<split>`: a class never holds a tab (its words are the refusal's).
    let mut splits: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(256, alloc);
    for (function, row) in left.iter() {
        let function: &str = function;
        let mut line = AString::from_str_in(function, alloc);
        line.push_str(": ");
        for c in row.whole.chars() {
            line.push(if c == '\n' { ' ' } else { c });
        }
        let class = class_of(&row.class, alloc);
        file(&mut lefts, class.as_str(), function, line.as_str(), alloc);
        if !row.split.is_empty() {
            let mut key = AString::from_str_in(class.as_str(), alloc);
            key.push('\t');
            key.push_str(&row.split);
            file(&mut splits, key.as_str(), function, line.as_str(), alloc);
        }
    }
    println!("LEFT OUT BY THE FRONT END ({} fns, {} classes, {} split rows)", left.len(), lefts.len(), splits.len());
    let split_rows = sorted(&splits, alloc);
    for (k, r) in sorted(&lefts, alloc).iter() {
        println!("LEFT {} {k}", r.count);
        println!("    e.g. {}", r.line.as_str());
        for (sk, sr) in split_rows.iter() {
            let Some((class, split)) = sk.split_once('\t') else { continue };
            if class == *k {
                println!("SPLIT {} {class} | {split}", sr.count);
                println!("    e.g. {}", sr.line.as_str());
            }
        }
    }
}

fn main() {
    let heap = blaze_own::Heap::process();
    let alloc = heap.alloc();
    let args: AVec<'_, AString<'_>> = {
        let mut v = AVec::with_capacity_in(8, alloc);
        for a in env::args() {
            v.push(AString::from_str_in(&a, alloc));
        }
        v
    };
    if args.len() < 7 {
        println!("usage: tempo_build <checkout> <fetch-store> <module-store> <modules-out> <script-records> <root crate[,crate..]>");
        process::exit(2);
    }
    let mut shown = AString::new_in(alloc);
    for a in args.iter() {
        shown.push_str(a.as_str());
        shown.push(' ');
    }
    println!("tempo_build: {}", shown.as_str().trim_end());
    let t0 = Instant::now();
    let path = |i: usize| -> &str { args[i].as_str() };
    let loaded = match names_load::load(path(1).as_ref(), path(2).as_ref(), Some((path(3).as_ref(), path(4).as_ref())), Some(path(5).as_ref()), false, heap) {
        Ok(l) => l,
        Err(e) => {
            println!("stopped: the names: {e}");
            process::exit(1);
        }
    };
    let names = loaded.names;
    println!("names: {} crates in {:.1}s", names.crates().len(), t0.elapsed().as_secs_f64());
    let roots = roots(names, &loaded.member, path(6), alloc);
    if roots.is_empty() {
        println!("stopped: no root");
        process::exit(1);
    }
    let frame = match blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer) {
        Ok(f) => f,
        Err(e) => {
            println!("stopped: the command's frame: {e}");
            process::exit(1);
        }
    };
    // The run's two lists, of the types `survey_from` and `build_survey` take, made by
    // `Default` (empty) so this file names no collection of its own beside blaze_own's.
    let mut left_out = Default::default();
    let mut refused = Default::default();
    let t1 = Instant::now();
    let got = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut program = match Front2Program::survey_from(names, &frame, &[], roots.as_slice(), &mut left_out) {
            Ok(p) => p,
            Err(e) => {
                println!("stopped: the walk from the roots: {e}");
                return None;
            }
        };
        let reach_s = t1.elapsed().as_secs_f64();
        let fns = program.fn_count();
        let mut bodied = 0usize;
        for c in 0..fns {
            if program.function(c).body.is_some() {
                bodied += 1;
            }
        }
        // Printed before the survey, so a survey that stops still leaves the reach's counts.
        let left_n: usize = left_out.len();
        println!("REACH from {}: {fns} fns tabled ({bodied} with a body); {left_n} left out by the front end; {reach_s:.1}s", path(6));
        print_left(&left_out, alloc);
        let t2 = Instant::now();
        let counts = match build_survey(&mut program, &mut refused, OverflowChecks::On) {
            Ok(c) => c,
            Err(e) => {
                println!("stopped: the build survey: {e}");
                return None;
            }
        };
        Some((reach_s, fns, bodied, counts.tried, counts.built, counts.no_instance.len(), t2.elapsed().as_secs_f64()))
    }));
    let Ok(Some((reach_s, fns, bodied, tried, built, no_instance, build_s))) = got else {
        if got.is_err() {
            println!("stopped: a panic (its place is on stderr, above)");
        }
        process::exit(1);
    };
    let left: &[(_, _)] = &left_out;
    let refused: &[(_, _)] = &refused;
    let _ = (reach_s, bodied);
    // By stage and class.
    let mut classes: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    let mut crates: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    let (mut builder, mut callee, mut tier) = (0usize, 0usize, 0usize);
    for (function, said) in refused.iter() {
        let (function, said): (&str, &str) = (function, said);
        let s = stage(said);
        match s {
            "tier" => tier += 1,
            "callee" => callee += 1,
            _ => builder += 1,
        }
        let mut key = AString::from_str_in(s, alloc);
        key.push('\t');
        key.push_str(class_of(said, alloc).as_str());
        let mut line = AString::from_str_in(function, alloc);
        line.push_str(": ");
        line.push_str(said);
        file(&mut classes, key.as_str(), function, line.as_str(), alloc);
        file(&mut crates, crate_of(function), function, line.as_str(), alloc);
    }
    let pct = |n: usize| if tried == 0 { 0.0 } else { 100.0 * n as f64 / tried as f64 };
    println!(
        "INSTANCES tried {tried}: built {built} ({:.1}%); refused {}: builder {builder}, callee {callee}, tier {tier}; generic fns never instanced {no_instance}; build {build_s:.1}s",
        pct(built),
        refused.len()
    );
    println!("REFUSALS BY CLASS ({} classes; stage, count, class; then the first example)", classes.len());
    for (k, r) in sorted(&classes, alloc).iter() {
        let (s, c) = k.split_once('\t').unwrap_or(("?", k));
        println!("CLASS {s} {} {c}", r.count);
        println!("    e.g. {}", r.function.as_str());
        println!("    line: {}", r.line.as_str());
    }
    println!("REFUSALS BY THE REFUSED FN'S CRATE ({} crates)", crates.len());
    for (k, r) in sorted(&crates, alloc).iter() {
        println!("CRATE {} {k}", r.count);
    }
    println!("TOTAL {}: reached {fns} fns, left out {}; instances tried {tried}, built {built} ({:.1}%), refused at the builder {builder}, at a callee {callee}, at the tier {tier}; {:.1}s in all", path(6), left.len(), pct(built), t0.elapsed().as_secs_f64());
}
