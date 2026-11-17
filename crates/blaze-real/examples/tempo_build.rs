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
//!
//! Under each of the [`MOST_WHY`] largest `SPLIT` rows, a `WHY` block: the evidence for the
//! row's first fn, read from its typed refusal and the names' tables (blaze-ladder
//! front2/census_why.rs: an unresolved header path's modules, macro calls, imports and cfgs; a
//! constant's chain innermost first; a refused head's range and token mark; else the innermost
//! refusal and its place), at most 20 lines.
//!
//! `--only <crate>[,<crate>..]`: the walk starts from every free fn (no member, no foreign fn)
//! DEFINED in the crates of those names, in place of the positional root's `main`; the LEFT,
//! SPLIT, WHY, REFUSALS and CRATE rows count only fns of those crates (what the walk reached
//! in their dependencies is counted on one line, not listed). The names are still resolved
//! for every unit, but the walk is limited to those crates (`Front2Program::survey_within`):
//! a fn of another crate is left out from its head only (reach.rs `OUTSIDE`), never lowered,
//! checked or built, and a call of one is one counted line, not a refusal row; a root that does
//! not lower is a left-out row. `PHASES` gives each phase's wall time. `REACHED BY CRATE` gives
//! the 15 crates with the most fns the walk tabled.

use std::{env, panic, process, time::Instant};

use blaze_ladder::front2::census_why;
use blaze_ladder::front2::program::{Front2Program, LeftOut};
use blaze_ladder::front2::reach;
use blaze_ladder::run::{OverflowChecks, build_survey};
use blaze_ladder::seam::FrontEnd;
use blaze_own::{AString, AVec, Alloc, ASortedMap};
use blaze_scope::names::{self as scope_names, Written};
use blaze_scope::resolve::Names;
use blaze_surface::{ItemDetail, Namespace, PathKind};

#[expect(dead_code, reason = "stages' loader, shared: this example reads only the names and the members")]
mod names_load;

/// The largest `SPLIT` rows that get a `WHY` block.
const MOST_WHY: usize = 12;

/// One class's row: how many refusals, and the first example (in the survey's order, which is
/// the program's key order: deterministic) with its index in the list it came from.
struct Row<'a> {
    count: usize,
    function: AString<'a>,
    line: AString<'a>,
    first: usize,
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

/// Files `(function, said)`, the `at`th of its list, under its class in `table`.
fn file<'a>(table: &mut ASortedMap<'a, AString<'a>, Row<'a>>, key: &str, function: &str, line: &str, at: usize, alloc: Alloc<'a>) {
    let k = AString::from_str_in(key, alloc);
    match table.get_mut(&k) {
        Some(row) => row.count += 1,
        None => {
            table.insert(k, Row { count: 1, function: AString::from_str_in(function, alloc), line: AString::from_str_in(line, alloc), first: at });
        }
    }
}

/// Whether `function` is counted: every fn without `--only`, else a fn of a named crate.
fn counted(only: &[&str], function: &str) -> bool {
    only.is_empty() || only.contains(&crate_of(function))
}

/// The `--only` roots: every free fn defined in each crate of a name in `only` (a fn of a
/// module or of a fn's body; no member of an impl or trait, no foreign fn), sorted by key.
fn only_roots<'a>(names: &Names<'_>, only: &[&str], alloc: Alloc<'a>) -> AVec<'a, blaze_id::ItemKey> {
    let mut out: AVec<'a, blaze_id::ItemKey> = AVec::with_capacity_in(256, alloc);
    for want in only {
        let mut crates = 0usize;
        for (k, c) in names.crates().iter().enumerate() {
            if c.name != *want {
                continue;
            }
            crates += 1;
            let before = out.len();
            for (key, _, place) in names.items(k) {
                let Some((_, outline)) = names.outline_at(k, place.at) else { continue };
                let item = outline.item(place.item);
                if !matches!(item.detail, ItemDetail::Fn { .. }) {
                    continue;
                }
                let free = item.parent.is_none_or(|p| !matches!(outline.item(p).detail, ItemDetail::Impl | ItemDetail::Trait | ItemDetail::ExternBlock { .. }));
                if free {
                    out.push(key);
                }
            }
            println!("only: crate {want} ({k}): {} free fns as roots", out.len() - before);
        }
        if crates == 0 {
            println!("only: no crate is named {want:?}");
        }
    }
    out.as_mut_slice().sort_unstable();
    out.dedup();
    out
}

/// The indices of the crates of the names in `only`, sorted (a name two units share is both).
fn only_crates<'a>(names: &Names<'_>, only: &[&str], alloc: Alloc<'a>) -> AVec<'a, usize> {
    let mut out: AVec<'a, usize> = AVec::with_capacity_in(only.len(), alloc);
    for (k, c) in names.crates().iter().enumerate() {
        if only.contains(&c.name) {
            out.push(k);
        }
    }
    out
}

/// How many crates `REACHED BY CRATE` lists.
const MOST_REACHED_CRATES: usize = 15;

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
/// why. Printed before the survey, so a survey that stops still leaves them. The
/// [`MOST_WHY`] largest split rows each get a `WHY` block (census_why.rs) for their first fn.
/// With `only` non-empty, only the fns of those crates are counted.
fn print_left<'n>(names: &'n Names<'n>, left: &[(Box<str>, LeftOut)], only: &[&str], alloc: Alloc<'_>) {
    let mut lefts: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    // Keyed `<class>\t<split>`: a class never holds a tab (its words are the refusal's).
    let mut splits: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(256, alloc);
    let mut outside = 0usize;
    for (at, (function, row)) in left.iter().enumerate() {
        let function: &str = function;
        if !counted(only, function) {
            outside += 1;
            continue;
        }
        let mut line = AString::from_str_in(function, alloc);
        line.push_str(": ");
        for c in row.whole.chars() {
            line.push(if c == '\n' { ' ' } else { c });
        }
        let class = class_of(&row.class, alloc);
        file(&mut lefts, class.as_str(), function, line.as_str(), at, alloc);
        if !row.split.is_empty() {
            let mut key = AString::from_str_in(class.as_str(), alloc);
            key.push('\t');
            key.push_str(&row.split);
            file(&mut splits, key.as_str(), function, line.as_str(), at, alloc);
        }
    }
    println!("LEFT OUT BY THE FRONT END ({} fns, {} classes, {} split rows)", left.len() - outside, lefts.len(), splits.len());
    if !only.is_empty() {
        println!("only: {outside} more fns left out are of other crates (reached in dependencies), not listed");
    }
    let split_rows = sorted(&splits, alloc);
    // The split rows that get a WHY block: the largest, by the order `sorted` gives.
    let why_rows = &split_rows[..split_rows.len().min(MOST_WHY)];
    let mut block = AString::new_in(alloc);
    for (k, r) in sorted(&lefts, alloc).iter() {
        println!("LEFT {} {k}", r.count);
        println!("    e.g. {}", r.line.as_str());
        for (sk, sr) in split_rows.iter() {
            let Some((class, split)) = sk.split_once('\t') else { continue };
            if class != *k {
                continue;
            }
            println!("SPLIT {} {class} | {split}", sr.count);
            println!("    e.g. {}", sr.line.as_str());
            if why_rows.iter().any(|(wk, _)| wk == sk) {
                block.truncate(0);
                let why = census_why::why_of(names, &left[sr.first].1, alloc);
                match why.write_to(&mut block, "    ") {
                    Ok(()) => print!("{}", block.as_str()),
                    Err(e) => println!("    WHY: the block did not write: {e}"),
                }
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
    const USAGE: &str = "usage: tempo_build <checkout> <fetch-store> <module-store> <modules-out> <script-records> <root crate[,crate..]> [--only <crate>[,<crate>..]] [--crates]";
    if args.len() < 7 {
        println!("{USAGE}");
        process::exit(2);
    }
    // The flags after the positional ones; an unknown one stops the run by name.
    let mut only: AVec<'_, &str> = AVec::with_capacity_in(4, alloc);
    let mut i = 7;
    while i < args.len() {
        match args[i].as_str() {
            "--only" => {
                let Some(list) = args.get(i + 1) else {
                    println!("stopped: `--only` names no crate\n{USAGE}");
                    process::exit(2);
                };
                only.extend(list.as_str().split(',').filter(|c| !c.is_empty()));
                i += 2;
            }
            // The per-crate table (`CRATE` rows) is always printed.
            "--crates" => i += 1,
            other => {
                println!("stopped: unknown flag {other:?}\n{USAGE}");
                process::exit(2);
            }
        }
    }
    only.as_mut_slice().sort_unstable();
    only.dedup();
    let only: &[&str] = only.as_slice();
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
    // What a body's macro calls expand with: the texts, environment and proc-macro runner the
    // names' items were expanded with (as `blaze build`'s program has them).
    let body_sources = loaded.body_sources();
    let names_s = t0.elapsed().as_secs_f64();
    println!("names: {} crates in {names_s:.1}s", names.crates().len());
    let roots = if only.is_empty() {
        roots(names, &loaded.member, path(6), alloc)
    } else {
        println!("only: the roots are the free fns of {only:?}; the positional root {} is not walked", path(6));
        only_roots(names, only, alloc)
    };
    let from: &str = if only.is_empty() { path(6) } else { "the --only crates" };
    let only_at = only_crates(names, only, alloc);
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
        let walked = if only.is_empty() {
            Front2Program::survey_from(names, &frame, &[], roots.as_slice(), &mut left_out, Some(body_sources), alloc)
        } else {
            let within = reach::Within { crates: Some(only_at.as_slice()), most_fns: None };
            Front2Program::survey_within(names, &frame, &[], roots.as_slice(), within, &mut left_out, Some(body_sources), alloc)
        };
        let mut program = match walked {
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
        println!("REACH from {from}: {fns} fns tabled ({bodied} with a body); {left_n} left out by the front end; {reach_s:.1}s");
        let by_crate = program.reached_by_crate();
        println!("REACHED BY CRATE ({} crates; the {} with the most fns the walk tabled, left-out ones too)", by_crate.len(), by_crate.len().min(MOST_REACHED_CRATES));
        for &(k, n) in by_crate.iter().take(MOST_REACHED_CRATES) {
            let name = names.crates().get(k).map_or("(no crate)", |c| c.name);
            println!("  {n} {name} ({k})");
        }
        print_left(names, &left_out, only, alloc);
        let t2 = Instant::now();
        let counts = match build_survey(&mut program, &mut refused, OverflowChecks::On, alloc) {
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
    let _ = bodied;
    // By stage and class.
    let mut classes: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    let mut crates: ASortedMap<'_, AString<'_>, Row<'_>> = ASortedMap::with_capacity_in(64, alloc);
    let (mut builder, mut callee, mut tier) = (0usize, 0usize, 0usize);
    let mut refused_outside = 0usize;
    let mut calls_outside = 0usize;
    for (at, (function, said)) in refused.iter().enumerate() {
        let (function, said): (&str, &str) = (function, said);
        if !counted(only, function) {
            refused_outside += 1;
            continue;
        }
        // A call of a fn the walk was limited away from (reach.rs `OUTSIDE`): not a refusal
        // of this crate's code, one counted line.
        if said.contains(reach::OUTSIDE) {
            calls_outside += 1;
            continue;
        }
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
        file(&mut classes, key.as_str(), function, line.as_str(), at, alloc);
        file(&mut crates, crate_of(function), function, line.as_str(), at, alloc);
    }
    let pct = |n: usize| if tried == 0 { 0.0 } else { 100.0 * n as f64 / tried as f64 };
    println!(
        "INSTANCES tried {tried}: built {built} ({:.1}%); refused {}: builder {builder}, callee {callee}, tier {tier}; generic fns never instanced {no_instance}; build {build_s:.1}s",
        pct(built),
        refused.len()
    );
    if !only.is_empty() {
        println!("only: {refused_outside} of the {} refusals are of fns of other crates, not listed (tried and built count every crate)", refused.len());
        println!("only: {calls_outside} refusals are a call of a fn of a crate outside the walk (tabled from its head only), not listed");
    }
    println!("PHASES names {names_s:.1}s, reach {reach_s:.1}s, instances {build_s:.1}s");
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
    println!("TOTAL {from}: reached {fns} fns, left out {}; instances tried {tried}, built {built} ({:.1}%), refused at the builder {builder}, at a callee {callee}, at the tier {tier}; {:.1}s in all", left.len(), pct(built), t0.elapsed().as_secs_f64());
}
