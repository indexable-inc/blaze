//! What presence conditions cost on real crate roots, against the floor in the crate doc (one
//! cache probe and one unique-table probe per `ite` step: 10 to 20 ns past L2, ~3 ns in L1).
//!
//! `bash examplerun.sh blaze-cond cost <file.rs>...`
//!
//! (a) For each file: a plain text scan for `#[cfg(..)]`, `#![cfg(..)]` and `cfg_attr(..)`; each
//! is one entry's condition. The space is a dozen real targets (their atoms), every feature the
//! file names (one package, no implications) and every other atom it names as a switch. Prints
//! the variables, entries, distinct conditions, nodes, the portable form's bytes, the ns per
//! operation, and the scope record's read list (the union of the supports): what the fork
//! design would have multiplied by.
//!
//! (b) Operations a second on random conditions over 32 variables (each an `or` of four
//! random three-literal cubes).

use std::{env, fs, hint::black_box, time::Instant};

use blaze_cond::{
    Atom, Bdd, Cond, Limits, Space, SpaceBuilder, Var,
    cfg::{self, Expr, Op},
    isop::Cover,
    portable::{export, import},
};
use blaze_own::{AVec, Alloc};

/// A target and its atoms: (name, `unix`/`windows`/"", family, os, arch, pointer width, env,
/// vendor, 64-bit atomics, panic).
type Row = (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str, &'static str, &'static str, bool, &'static str);

const TARGETS: &[Row] = &[
    ("x86_64-unknown-linux-gnu", "unix", "unix", "linux", "x86_64", "64", "gnu", "unknown", true, "unwind"),
    ("aarch64-unknown-linux-gnu", "unix", "unix", "linux", "aarch64", "64", "gnu", "unknown", true, "unwind"),
    ("x86_64-unknown-linux-musl", "unix", "unix", "linux", "x86_64", "64", "musl", "unknown", true, "unwind"),
    ("riscv64gc-unknown-linux-gnu", "unix", "unix", "linux", "riscv64", "64", "gnu", "unknown", true, "unwind"),
    ("aarch64-apple-darwin", "unix", "unix", "macos", "aarch64", "64", "", "apple", true, "unwind"),
    ("x86_64-apple-darwin", "unix", "unix", "macos", "x86_64", "64", "", "apple", true, "unwind"),
    ("aarch64-apple-ios", "unix", "unix", "ios", "aarch64", "64", "", "apple", true, "unwind"),
    ("aarch64-linux-android", "unix", "unix", "android", "aarch64", "64", "", "unknown", true, "unwind"),
    ("x86_64-unknown-freebsd", "unix", "unix", "freebsd", "x86_64", "64", "", "unknown", true, "unwind"),
    ("x86_64-pc-windows-msvc", "windows", "windows", "windows", "x86_64", "64", "msvc", "pc", true, "unwind"),
    ("i686-pc-windows-msvc", "windows", "windows", "windows", "x86", "32", "msvc", "pc", true, "unwind"),
    ("wasm32-unknown-unknown", "", "wasm", "unknown", "wasm32", "32", "", "unknown", true, "abort"),
    ("wasm32-wasip1", "", "wasm", "wasi", "wasm32", "32", "p1", "unknown", true, "abort"),
];

const TARGET_ATOMS: &[&str] = &[
    "unix",
    "windows",
    "target_family",
    "target_os",
    "target_arch",
    "target_pointer_width",
    "target_env",
    "target_vendor",
    "target_endian",
    "target_has_atomic",
    "panic",
    "target_feature",
    "target_abi",
];

fn atoms_of(row: &Row) -> Vec<Atom<'static>> {
    let (_, flag, family, os, arch, width, env, vendor, atomic64, panic) = *row;
    let mut atoms = vec![
        Atom::pair("target_family", family),
        Atom::pair("target_os", os),
        Atom::pair("target_arch", arch),
        Atom::pair("target_pointer_width", width),
        Atom::pair("target_env", env),
        Atom::pair("target_vendor", vendor),
        Atom::pair("target_endian", "little"),
        Atom::pair("panic", panic),
        Atom::pair("target_has_atomic", "8"),
        Atom::pair("target_has_atomic", "16"),
        Atom::pair("target_has_atomic", "32"),
        Atom::pair("target_has_atomic", "ptr"),
    ];
    if !flag.is_empty() {
        atoms.push(Atom::flag(flag));
    }
    if atomic64 {
        atoms.push(Atom::pair("target_has_atomic", "64"));
    }
    atoms
}

/// The byte after the `)` matching the `(` at `open`, skipping string literals.
fn matching(text: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < text.len() {
        match text[i] {
            b'"' => {
                i += 1;
                while i < text.len() && text[i] != b'"' {
                    i += if text[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Every cfg predicate's text in the file: `#[cfg(..)]`, `#![cfg(..)]`, and `cfg_attr`'s first
/// argument.
fn predicates(text: &str) -> Vec<(&str, bool)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for (needle, attr) in [("#[cfg(", false), ("#![cfg(", false), ("cfg_attr(", true)] {
        let mut from = 0;
        while let Some(at) = text[from..].find(needle) {
            let open = from + at + needle.len() - 1;
            from = open + 1;
            if let Some(close) = matching(bytes, open) {
                out.push((&text[open + 1..close], attr));
            }
        }
    }
    out
}

fn ns(start: Instant, n: usize) -> f64 {
    start.elapsed().as_nanos() as f64 / n.max(1) as f64
}

fn measure(path: &str, alloc: Alloc<'static>) {
    let Ok(text) = fs::read_to_string(path) else {
        println!("{path}: cannot read");
        return;
    };
    let short = path.rsplit("/src/").nth(1).map_or(path, |p| p.rsplit('/').next().unwrap_or(p));
    let mut exprs: Vec<Expr<'_, 'static>> = Vec::new();
    let mut unparsed = 0;
    for (pred, attr) in predicates(&text) {
        let parsed = if attr { cfg::parse_prefix(pred, alloc).map(|(e, _)| e) } else { cfg::parse(pred, alloc) };
        match parsed {
            Ok(e) => exprs.push(e),
            Err(_) => unparsed += 1,
        }
    }
    // The space: the targets, the file's features and its other atoms as switches.
    let mut b = SpaceBuilder::new_in(alloc);
    for row in TARGETS {
        b.target(row.0, &atoms_of(row)).unwrap();
    }
    let (mut features, mut switches, mut target_named) = (Vec::new(), Vec::new(), Vec::new());
    for e in &exprs {
        for op in e.ops.iter() {
            if let Op::Atom(a) = *op {
                if a.name == "feature" {
                    features.push(a.value.unwrap_or(""));
                } else if TARGET_ATOMS.contains(&a.name) {
                    target_named.push((a.name, a.value));
                } else {
                    switches.push((a.name, a.value));
                }
            }
        }
    }
    for list in [&mut features] {
        list.sort_unstable();
        list.dedup();
    }
    switches.sort_unstable();
    switches.dedup();
    target_named.sort_unstable();
    target_named.dedup();
    for f in &features {
        b.feature("krate", f).unwrap();
    }
    for &(name, value) in &switches {
        b.switch(Atom { name, value }).unwrap();
    }
    let space: Space<'static> = b.build().unwrap();
    let mut bdd = Bdd::new_in(space.len(), Limits { nodes: 1 << 22, cache_bits: 16 }, alloc).unwrap();
    let valid = space.valid_in(&mut bdd).unwrap();
    let valid_nodes = bdd.nodes();
    let before = bdd.stats();

    // Lowering: every entry's condition.
    let mut absent = AVec::new_in(alloc);
    let start = Instant::now();
    let mut conds: Vec<Cond> = Vec::with_capacity(exprs.len());
    for e in &exprs {
        conds.push(cfg::lower(e, &space, "krate", &mut bdd, &mut absent).unwrap());
    }
    let lower_ns = ns(start, exprs.len());
    let after = bdd.stats();
    let steps = (after.steps - before.steps) as usize;
    let hits = (after.hits - before.hits) as usize;
    let step_ns = start.elapsed().as_nanos() as f64 / (steps + hits).max(1) as f64;

    let mut distinct = conds.clone();
    distinct.sort_unstable();
    distinct.dedup();
    // The scope record's read list: the union of its entries' supports.
    let mut union = Cond::FALSE;
    let mut support: AVec<'_, Var> = AVec::new_in(alloc);
    let mut read: Vec<u16> = Vec::new();
    for &c in &distinct {
        bdd.support(c, &mut support).unwrap();
        read.extend(support.iter().map(|v| v.index()));
        union = bdd.or(union, c).unwrap();
    }
    read.sort_unstable();
    read.dedup();

    // Portable form.
    let mut bytes = AVec::new_in(alloc);
    let mut total = 0usize;
    let mut most = 0usize;
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let start = Instant::now();
    for &c in &distinct {
        export(&mut bdd, &space, c, &mut bytes).unwrap();
        total += bytes.len();
        most = most.max(bytes.len());
        blobs.push(bytes.as_slice().to_vec());
    }
    let export_ns = ns(start, distinct.len());
    let mut fresh = Bdd::new_in(space.len(), Limits { nodes: 1 << 22, cache_bits: 16 }, alloc).unwrap();
    let start = Instant::now();
    for blob in &blobs {
        black_box(import(&mut fresh, &space, blob).unwrap());
    }
    let import_ns = ns(start, blobs.len());

    // `implies` over every pair of distinct conditions, and within valid.
    let start = Instant::now();
    let mut implied = 0usize;
    for &a in &distinct {
        for &b in &distinct {
            implied += bdd.implies(a, b).unwrap() as usize;
        }
    }
    let pairs = distinct.len() * distinct.len();
    let implies_ns = ns(start, pairs);
    let start = Instant::now();
    let mut possible = 0usize;
    for &c in &distinct {
        possible += !bdd.disjoint(c, valid).unwrap() as usize;
    }
    let feasible_ns = ns(start, distinct.len());

    // The longest condition, printed within valid.
    let mut cover = Cover::new_in(alloc);
    let mut widest = (0usize, String::new());
    let mut print_ns = 0f64;
    let start = Instant::now();
    for &c in &distinct {
        let lo = bdd.and(c, valid).unwrap();
        let hi = bdd.or(c, valid.not()).unwrap();
        cover.compute(&mut bdd, lo, hi, 1 << 20).unwrap();
        let mut s = String::new();
        cover.write_cfg(&space, "krate", &mut s).unwrap();
        if s.len() > widest.0 {
            widest = (s.len(), s);
        }
    }
    print_ns += ns(start, distinct.len());

    println!("{short} ({path})");
    println!(
        "  variables: {} in the space ({} target atoms of {} targets, {} features, {} switches); the file names {} features, {} target atoms, {} other switches",
        space.len(),
        space.len() - features.len() - switches.len(),
        space.targets(),
        features.len(),
        switches.len(),
        features.len(),
        target_named.len(),
        switches.len()
    );
    println!(
        "  entries: {} ({} predicates not parsed), distinct conditions: {}, impossible within valid: {}, atoms absent from the space: {}",
        exprs.len(),
        unparsed,
        distinct.len(),
        distinct.len() - possible,
        absent.len()
    );
    println!(
        "  scope record: read list {} variables (the fork design: up to 2^{} records); one record of {} entries here",
        read.len(),
        read.len(),
        exprs.len()
    );
    println!(
        "  arena: {} nodes ({} for valid), {} KB; portable form {:.1} bytes a distinct condition on average, {} at most",
        bdd.nodes(),
        valid_nodes,
        bdd.bytes() / 1024,
        total as f64 / distinct.len().max(1) as f64,
        most
    );
    println!(
        "  ns: lower {lower_ns:.0}/entry (atom lookups, {steps} ite steps, {hits} cache hits: {step_ns:.0} ns per step with the lookups), export {export_ns:.0}, import {import_ns:.0}, implies {implies_ns:.1} ({implied} of {pairs} pairs hold), feasible-within-valid {feasible_ns:.0}, print within valid {print_ns:.0}"
    );
    println!("  widest printed within valid: {}", widest.1);
}

/// SplitMix64: the random conditions of (b), seeded, so a rerun draws the same.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn random(alloc: Alloc<'static>) {
    const VARS: usize = 32;
    let mut bdd = Bdd::new_in(VARS, Limits { nodes: 1 << 23, cache_bits: 18 }, alloc).unwrap();
    let mut rng = Rng(0x5eed);
    let mut pool = Vec::new();
    for _ in 0..1000 {
        let mut f = Cond::FALSE;
        for _ in 0..4 {
            let mut cube = Cond::TRUE;
            for _ in 0..3 {
                let v = bdd.var(Var::new((rng.next() % VARS as u64) as u16)).unwrap();
                let lit = if rng.next() & 1 == 1 { v } else { v.not() };
                cube = bdd.and(cube, lit).unwrap();
            }
            f = bdd.or(f, cube).unwrap();
        }
        pool.push(f);
    }
    let nodes = bdd.nodes();
    let before = bdd.stats();
    // The arena keeps every node it made (a task's arena dies with the task): 50,000 binary
    // operations on these grow it by a few million nodes.
    let ops = 50_000;
    let start = Instant::now();
    for i in 0..ops {
        let a = pool[(rng.next() % 1000) as usize];
        let b = pool[(rng.next() % 1000) as usize];
        black_box(if i % 2 == 0 { bdd.and(a, b) } else { bdd.or(a, b) }.unwrap());
    }
    let el = start.elapsed();
    let after = bdd.stats();
    let steps = after.steps - before.steps;
    let hits = after.hits - before.hits;
    let start = Instant::now();
    let mut held = 0;
    for _ in 0..ops {
        let a = pool[(rng.next() % 1000) as usize];
        let b = pool[(rng.next() % 1000) as usize];
        held += bdd.implies(a, b).unwrap() as usize;
    }
    let imp = start.elapsed();
    let start = Instant::now();
    for i in 0..ops {
        black_box(bdd.restrict(pool[i % 1000], Var::new((i % VARS) as u16), i & 1 == 1).unwrap());
    }
    let res = start.elapsed();
    println!("random over {VARS} variables: 1000 conditions in {nodes} nodes");
    println!(
        "  and/or: {:.2} M ops/s, {:.0} ns/op, {:.1} ite steps + {:.1} hits an op, {:.1} ns per step (a step that missed the cache, the hits' time in it); arena now {} nodes, {} MB",
        ops as f64 / el.as_secs_f64() / 1e6,
        el.as_nanos() as f64 / ops as f64,
        steps as f64 / ops as f64,
        hits as f64 / ops as f64,
        el.as_nanos() as f64 / steps.max(1) as f64,
        bdd.nodes(),
        bdd.bytes() >> 20
    );
    println!(
        "  implies: {:.2} M ops/s ({held} of {ops} held); restrict: {:.2} M ops/s",
        ops as f64 / imp.as_secs_f64() / 1e6,
        ops as f64 / res.as_secs_f64() / 1e6
    );
}

fn main() {
    let heap = blaze_own::Heap::process();
    let alloc = heap.alloc();
    for path in env::args().skip(1) {
        measure(&path, alloc);
    }
    random(alloc);
}
