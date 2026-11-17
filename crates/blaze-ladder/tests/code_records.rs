//! The run path's code records (docs/v2/plan/design/run.md, "Pieces" 3 and 7;
//! crates/blaze-ladder/src/code_records.rs, src/symbols.rs): every instance a program's entry
//! reaches is built by `run::build_records_new` (the new front end) to one entry in a store
//! under its full key, computed before the work.
//!
//! What is promised, and the line each test prints:
//!
//! - every record of the hello-world rung, and of every running and support rung, names its
//!   targets by symbol or C name only: no load of a literal word, every `adrp` a site row
//!   (blaze-emit `no_address_word`, the emitter test's checker), every outside target with its
//!   name kept; every function target is an instance its caller declares, or the program's
//!   thread-local getter (its own record, met once per rung with thread-locals and counted
//!   among the instances), and either way has an entry; each declared instance's name decodes
//!   and encodes back to equal bytes; no two different instances share a symbol. A failure
//!   names its rung, its caller and its callee (fn, symbol and canonical instance name):
//!   `hello: 1 instance, 1 record; targets: 0 by symbol, 1 C name (write); 0 literal words`
//!   `corpus: <R> rungs, <N> instances, <S> distinct symbols, 0 literal words, 0 refused`
//! - building twice gives equal keys and byte-equal entries, and a second build into the same
//!   store makes nothing:
//!   `second build: built 0 of 6: all found; keys equal, entries byte-equal`
//! - an edit of one body's text moves exactly that instance's key (its callers' checked results
//!   read only its signature); a comment edit moves none:
//!   `body edit: 1 of 6 keys moved (alpha); comment edit: 0 of 6 moved`
//! - the tier moves every key; the key with the tier left out (the planted bug) is caught:
//!   `tier: 6 of 6 keys moved; the planted key without the tier: caught (6 of 6 equal across tiers)`
//!
//! The rungs are edition 2024 with no `core` (no toolchain needed) except where a rung's
//! edition is a blaze edition, which reads the toolchain's shelf under `$HOME/.blaze/v2`.

#![feature(allocator_api)]

mod rungs;

use std::collections::{BTreeMap, BTreeSet};

use blaze_code::record::{Space, Symbol};
use blaze_emit::{Isa, Os};
use blaze_ladder::code_records::{
    Builder, CodeKey, CodeStore, EntryView, KeyPlant, MemoryStore, Met, RecordRun, Settings, code_key, code_key_planted, getter_parts,
};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_records_new};
use blaze_ladder::symbols::{decode_instance, encode_instance, fn_symbol_of_name};
use blaze_own::{AVec, Alloc, Vec as OwnVec};
use blaze_run::Tier;
use blaze_toolchain::ToolchainId;
use blaze_typeck::PortableTys;
use rungs::{RUNNING, SUPPORT, ladder, sources};

/// Six instances: `main`, `hello`, `beta`, `alpha`, and `pick` at `i32` and at `u8`.
const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

unsafe extern \"C\" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

fn hello() -> isize {
    unsafe { write(1, b\"hello\\n\" as *const u8, 6) }
}

fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    alpha()
}

fn pick<T>(c: bool, a: T, b: T) -> T {
    if c { a } else { b }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    hello();
    let x: i32 = pick(true, beta(), 2);
    let y: u8 = pick(false, 3, 4);
    x + y as i32
}
";

/// What every test builds for: a fixed target and toolchain, nothing of this machine.
fn builder(tier: Tier) -> Builder {
    Builder { isa: Isa { lse: true, os: Os::MacOs }, tier, settings: Settings::baseline(OVERFLOW), toolchain: ToolchainId([7; 32]) }
}

/// The overflow setting every walk here builds with and its records are keyed by: `blaze
/// run`'s (the dev profile's, D101).
const OVERFLOW: blaze_build::OverflowChecks = blaze_build::OverflowChecks::On;

/// The instances `files` (rooted at `src/main.rs`, of `edition`) reach, built into `store`.
fn walk(files: &[(String, String)], edition: &str, store: &mut dyn CodeStore, tier: Tier) -> Result<(Vec<Met>, String), String> {
    let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let shelf = edition.starts_with("blaze-").then(|| format!("{}/.blaze/v2/toolchain", std::env::var("HOME").expect("HOME")));
    let new = NewFront { package: "ladder", edition, shelf: shelf.as_deref(), overflow: OVERFLOW };
    let mut run = RecordRun::new(store, builder(tier));
    build_records_new(&views, "src/main.rs", new, &mut run).map_err(|e| e.to_string())?;
    let summary = run.summary();
    Ok((run.met, summary))
}

fn program(text: &str) -> Vec<(String, String)> {
    vec![("src/main.rs".to_string(), text.to_string())]
}

fn walk_program(text: &str, store: &mut dyn CodeStore, tier: Tier) -> (Vec<Met>, String) {
    walk(&program(text), "2024", store, tier).unwrap_or_else(|e| panic!("the program's records were not built: {e}"))
}

fn edition_of(dir: &std::path::Path) -> String {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
    line.trim().trim_start_matches('=').trim().trim_matches('"').to_string()
}

/// What one record's targets are.
#[derive(Default)]
struct Targets {
    by_symbol: usize,
    c_names: Vec<String>,
}

/// An instance's canonical name (symbols.rs `encode_instance`) as hex, its first 48 bytes.
fn canonical(name: &[u8]) -> String {
    let shown: String = name.iter().take(48).map(|b| format!("{b:02x}")).collect();
    match name.len() > 48 {
        true => format!("{shown}.. ({} bytes)", name.len()),
        false => shown,
    }
}

/// Who `symbol` is in a walk that met `all`: the fn it is tabled as (or the thread-local getter)
/// and, when `declared` has it, its canonical instance name.
fn who(symbol: Symbol, all: &[Met], declared: &BTreeMap<Symbol, Vec<u8>>) -> String {
    let fname = all.iter().find(|m| m.symbol == symbol).map_or("an instance the walk never met", |m| &*m.function);
    match declared.get(&symbol) {
        Some(name) => format!("`{fname}` (symbol {symbol}, instance {})", canonical(name)),
        None => format!("`{fname}` (symbol {symbol}, declared by no caller)"),
    }
}

/// The entry under `met.key` in `store` holds the promises (module doc, first bullet), or an
/// error naming the rung, the caller and the callee; its targets. `all` is every instance the
/// walk met: a function target is a declared instance or the program's thread-local getter, and
/// either way the walk met it, so its entry is in `store`.
fn check_entry(rung: &str, store: &dyn CodeStore, met: &Met, all: &[Met]) -> Result<Targets, String> {
    let caller = &met.function;
    let bytes = store.get(met.key).map_err(|e| format!("{rung}: `{caller}`: the store: {e:?}"))?;
    let bytes = bytes.ok_or_else(|| format!("{rung}: no entry for `{caller}` under {}", met.key))?;
    let entry = EntryView::read(bytes).map_err(|e| format!("{rung}: `{caller}`'s entry: {e}"))?;
    entry.no_address_word().map_err(|e| format!("{rung}: `{caller}`: {e}"))?;
    let record = entry.record;
    let mut out = Targets::default();
    let mut declared: BTreeMap<Symbol, Vec<u8>> = BTreeMap::new();
    let mut ground = PortableTys::new_in(Alloc::global());
    entry
        .each_declared(
            |d| {
                let (item, args) = decode_instance(d.name, &mut ground, Alloc::global()).map_err(|e| format!("instance {}: {e}", canonical(d.name)))?;
                let mut again: AVec<'_, u8> = OwnVec::new_in(Alloc::global());
                encode_instance(item, args, ground.view(), Alloc::global(), &mut again).map_err(|e| format!("instance {}: {e}", canonical(d.name)))?;
                if &again[..] != d.name {
                    return Err(format!("instance {} encodes back as {}: not equal bytes", canonical(d.name), canonical(&again)));
                }
                declared.insert(fn_symbol_of_name(d.name), d.name.to_vec());
                Ok(())
            },
            |e| e.to_string(),
        )
        .map_err(|e: String| format!("{rung}: `{caller}`'s declared instances: {e}"))?;
    let getter = getter_parts().symbol;
    for i in 0..record.targets() {
        let target = record.target(i);
        match target.space {
            Space::Outside => {
                let name = record.name(target.symbol).ok_or_else(|| format!("{rung}: `{caller}` names an outside symbol {} with no name kept", target.symbol))?;
                out.c_names.push(name.to_string());
            }
            Space::Func => {
                let callee = who(target.symbol, all, &declared);
                if target.symbol != getter && !declared.contains_key(&target.symbol) {
                    return Err(format!("{rung}: `{caller}` calls {callee}: an undeclared callee, which its parent could not make"));
                }
                if !all.iter().any(|m| m.symbol == target.symbol) {
                    return Err(format!("{rung}: `{caller}` calls {callee}, whose record the walk never made or found"));
                }
                out.by_symbol += 1;
            }
            Space::Static | Space::Vtable | Space::Tls => out.by_symbol += 1,
        }
    }
    Ok(out)
}

/// The run path's piece 3 on the hello-world rung: prints
/// `hello: 1 instance, 1 record; targets: 0 by symbol, 1 C name (write); 0 literal words`.
#[test]
fn hello_records_name_targets_by_symbol() {
    let dir = ladder().join("3-hello");
    let mut store = MemoryStore::new_in(Alloc::global());
    let (met, _) = walk(&sources(&dir), &edition_of(&dir), &mut store, Tier::Fast).unwrap_or_else(|e| panic!("3-hello: {e}"));
    let mut by_symbol = 0;
    let mut c_names = Vec::new();
    for m in &met {
        let t = check_entry("3-hello", &store, m, &met).unwrap_or_else(|e| panic!("{e}"));
        by_symbol += t.by_symbol;
        c_names.extend(t.c_names);
    }
    let line = format!(
        "hello: {} instance, {} record; targets: {by_symbol} by symbol, {} C name ({}); 0 literal words",
        met.len(),
        store.len(),
        c_names.len(),
        c_names.join(", ")
    );
    println!("{line}");
    assert_eq!(line, "hello: 1 instance, 1 record; targets: 0 by symbol, 1 C name (write); 0 literal words");
}

/// Every running and support rung: prints
/// `corpus: <R> rungs, <N> instances, <S> distinct symbols, 0 literal words, 0 refused`, with
/// S equal to N (no two instances of one program share a symbol).
#[test]
fn every_rung_records_name_targets_by_symbol() {
    let (mut rungs, mut instances, mut symbols) = (0, 0, 0);
    let mut refused = Vec::new();
    // Every broken promise, each naming its rung, caller and callee: all rungs are checked
    // before the test stops, so one run names every failure.
    let mut broken = Vec::new();
    for rung in RUNNING.iter().chain(SUPPORT) {
        let dir = ladder().join(rung);
        let mut store = MemoryStore::new_in(Alloc::global());
        let met = match walk(&sources(&dir), &edition_of(&dir), &mut store, Tier::Fast) {
            Ok((met, _)) => met,
            Err(e) => {
                refused.push(format!("{rung}: {e}"));
                continue;
            }
        };
        for m in &met {
            if let Err(e) = check_entry(rung, &store, m, &met) {
                broken.push(e);
            }
        }
        let mut by_symbol: BTreeMap<Symbol, Vec<&str>> = BTreeMap::new();
        for m in &met {
            by_symbol.entry(m.symbol).or_default().push(&m.function);
        }
        for (symbol, fns) in by_symbol.iter().filter(|(_, fns)| fns.len() > 1) {
            broken.push(format!("{rung}: {} instances share symbol {symbol}: {}", fns.len(), fns.join(", ")));
        }
        let distinct = by_symbol.len();
        rungs += 1;
        instances += met.len();
        symbols += distinct;
    }
    let line = format!("corpus: {rungs} rungs, {instances} instances, {symbols} distinct symbols, 0 literal words, {} refused", refused.len());
    println!("{line}");
    assert!(broken.is_empty(), "{} broken promises in the rungs' records:\n{}", broken.len(), broken.join("\n"));
    assert!(refused.is_empty(), "rungs whose records were not built:\n{}", refused.join("\n"));
}

/// Each instance's fn and key, sorted: a walk that finds its entries meets the instances in its
/// entries' declared order, not the first walk's.
fn keys(met: &[Met]) -> Vec<(String, CodeKey)> {
    let mut out: Vec<(String, CodeKey)> = met.iter().map(|m| (m.function.to_string(), m.key)).collect();
    out.sort();
    out
}

/// Piece 7: prints `second build: built 0 of 6: all found; keys equal, entries byte-equal`.
#[test]
fn a_second_build_makes_nothing() {
    let mut store = MemoryStore::new_in(Alloc::global());
    let (first, made) = walk_program(PROGRAM, &mut store, Tier::Fast);
    assert_eq!(made, format!("built {n} of {n}: 0 found", n = first.len()), "the first build makes every instance");
    let puts = store.puts;
    let (second, summary) = walk_program(PROGRAM, &mut store, Tier::Fast);
    assert_eq!(keys(&first), keys(&second), "a second build meets the same instances under the same keys");
    assert_eq!(store.puts, puts, "a second build writes nothing");
    // A build into an empty store elsewhere writes equal bytes under equal keys (rule 25).
    let mut other = MemoryStore::new_in(Alloc::global());
    let (third, _) = walk_program(PROGRAM, &mut other, Tier::Fast);
    assert_eq!(keys(&first), keys(&third));
    for m in &first {
        assert_eq!(store.get(m.key).expect("reads"), other.get(m.key).expect("reads"), "`{}`: two builders of one key write equal bytes", m.function);
    }
    let line = format!("second build: {summary}; keys equal, entries byte-equal");
    println!("{line}");
    assert_eq!(line, "second build: built 0 of 6: all found; keys equal, entries byte-equal");
}

/// The functions whose keys differ between two walks of one program's shape.
fn moved(before: &[Met], after: &[Met]) -> Vec<String> {
    assert_eq!(before.len(), after.len(), "an edit of a body's text reaches the same instances");
    let mut out = Vec::new();
    for (b, a) in before.iter().zip(after) {
        assert_eq!(b.symbol, a.symbol, "an edit of a body's text keeps every symbol (`{}`)", b.function);
        if b.key != a.key {
            out.push(b.function.to_string());
        }
    }
    out
}

/// Prints `body edit: 1 of 6 keys moved (alpha); comment edit: 0 of 6 moved`.
#[test]
fn an_edit_moves_exactly_its_keys() {
    let (base, _) = walk_program(PROGRAM, &mut MemoryStore::new_in(Alloc::global()), Tier::Fast);
    let edited = PROGRAM.replace("fn alpha() -> i32 {\n    1\n}", "fn alpha() -> i32 {\n    5\n}");
    assert_ne!(edited, PROGRAM);
    let (body, _) = walk_program(&edited, &mut MemoryStore::new_in(Alloc::global()), Tier::Fast);
    let body_moved = moved(&base, &body);
    // Comments inside a body and between items: no token, no read changes.
    let commented = PROGRAM
        .replace("fn beta() -> i32 {\n    alpha()\n}", "fn beta() -> i32 {\n    // the one call\n    alpha() /* inline */\n}")
        .replace("fn hello()", "// prints a line\nfn hello()");
    assert_ne!(commented, PROGRAM);
    let (comment, _) = walk_program(&commented, &mut MemoryStore::new_in(Alloc::global()), Tier::Fast);
    let comment_moved = moved(&base, &comment);
    let names: Vec<&str> = body_moved.iter().map(|f| if f.contains("alpha") { "alpha" } else { f.as_str() }).collect();
    let line = format!(
        "body edit: {} of {} keys moved ({}); comment edit: {} of {} moved",
        body_moved.len(),
        base.len(),
        names.join(", "),
        comment_moved.len(),
        base.len()
    );
    println!("{line}");
    assert_eq!(line, "body edit: 1 of 6 keys moved (alpha); comment edit: 0 of 6 moved", "moved by the body edit: {body_moved:?}; by the comment edit: {comment_moved:?}");
}

/// Prints `tier: 6 of 6 keys moved; the planted key without the tier: caught (6 of 6 equal across tiers)`.
#[test]
fn the_tier_moves_every_key() {
    let (fast, _) = walk_program(PROGRAM, &mut MemoryStore::new_in(Alloc::global()), Tier::Fast);
    let (optimized, _) = walk_program(PROGRAM, &mut MemoryStore::new_in(Alloc::global()), Tier::Optimized);
    let moved = moved(&fast, &optimized).len();
    for m in &fast {
        assert_eq!(code_key(&m.parts, &builder(Tier::Fast)), m.key, "`{}`: the walk's key is `code_key` of its parts", m.function);
    }
    // The planted bug: the same oracle over keys made without the tier finds them equal.
    let equal = fast
        .iter()
        .filter(|m| code_key_planted(&m.parts, &builder(Tier::Fast), KeyPlant::WithoutTier) == code_key_planted(&m.parts, &builder(Tier::Optimized), KeyPlant::WithoutTier))
        .count();
    let caught = if equal > 0 { "caught" } else { "MISSED" };
    let line = format!("tier: {moved} of {} keys moved; the planted key without the tier: {caught} ({equal} of {} equal across tiers)", fast.len(), fast.len());
    println!("{line}");
    assert_eq!(line, "tier: 6 of 6 keys moved; the planted key without the tier: caught (6 of 6 equal across tiers)");
}

/// D101 and rule 21: the profile's overflow setting is part of every code key, so a dev record
/// (checked `+`) and a release record (wrapping `+`) of one instance never share a key.
/// Prints `overflow: 6 of 6 keys moved; the planted key without it: caught (6 of 6 equal)`.
#[test]
fn the_overflow_setting_moves_every_key() {
    let (met, _) = walk_program(PROGRAM, &mut MemoryStore::new_in(Alloc::global()), Tier::Fast);
    let with = |overflow| Builder { settings: Settings::baseline(overflow), ..builder(Tier::Fast) };
    let (on, off) = (with(blaze_build::OverflowChecks::On), with(blaze_build::OverflowChecks::Off));
    let moved = met.iter().filter(|m| code_key(&m.parts, &on) != code_key(&m.parts, &off)).count();
    // The planted bug: the same oracle over keys made without the setting finds them equal.
    let equal = met.iter().filter(|m| code_key_planted(&m.parts, &on, KeyPlant::WithoutOverflow) == code_key_planted(&m.parts, &off, KeyPlant::WithoutOverflow)).count();
    let caught = if equal > 0 { "caught" } else { "MISSED" };
    let line = format!("overflow: {moved} of {} keys moved; the planted key without it: {caught} ({equal} of {} equal)", met.len(), met.len());
    println!("{line}");
    assert_eq!(line, "overflow: 6 of 6 keys moved; the planted key without it: caught (6 of 6 equal)");
}
