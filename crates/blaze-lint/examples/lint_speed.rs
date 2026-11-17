//! Lints every item of one file, as a driver does (`Units::plan`, then `Units::lint_all`), N
//! times with every rule on, and prints the bytes per second beside the lexer alone over the same
//! bytes (the floor) and their ratio.
//!
//! `cargo run --release -p blaze-lint --example lint_speed -- crates/blaze-lint/src/engine.rs 200`
//!
//! Prints, for example:
//! `engine.rs: 98765 bytes, 214 units, 15 rules, 200 times`
//! `lex (floor)  412.3 MB/s`
//! `lint         131.0 MB/s  ratio 3.15  findings 57`

#![feature(allocator_api)]

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use blaze_file_items::outline;
use blaze_lint::{Findings, Rule, RuleSet, Scope, TestBits, Units, lex_floor};
use blaze_own::AVec;
use blaze_surface::Edition;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("lint_speed: give a .rs file, and how many times to lint it (default 100)");
        return ExitCode::from(2);
    };
    let times: u32 = match args.next().map(|n| n.parse::<u32>()) {
        None => 100,
        Some(Ok(n)) if n > 0 => n,
        Some(_) => {
            eprintln!("lint_speed: the count must be a whole number above 0");
            return ExitCode::from(2);
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("lint_speed: reading {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    // The entry makes the heap and hands it down: everything below takes `alloc`.
    let heap = blaze_own::Heap::process();
    let alloc = heap.alloc();
    let lines = outline(&text, Edition::E2024, alloc);
    let units = match Units::plan(lines.items(), text.len(), alloc) {
        Ok(units) => units,
        Err(error) => {
            eprintln!("lint_speed: {path}: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let file = Scope::file(TestBits::NONE, false, false);
    let mut out = Findings::new_in(alloc);
    let mut scopes: AVec<'_, Scope> = AVec::new_in(alloc);

    // Bounded: `times` turns.
    let start = Instant::now();
    let mut tokens = 0usize;
    for _ in 0..times {
        tokens += lex_floor(black_box(&text));
    }
    let lex = start.elapsed().as_secs_f64();

    let mut findings = 0usize;
    let start = Instant::now();
    // Bounded: `times` turns, each one pass per unit.
    for _ in 0..times {
        out.clear();
        if let Err(error) = units.lint_all(&RuleSet::ALL, black_box(&text), file, alloc, &mut out, &mut scopes, |_| {}) {
            eprintln!("lint_speed: {path}: {error:?}");
            return ExitCode::FAILURE;
        }
        findings = out.list.len();
    }
    let lint = start.elapsed().as_secs_f64();

    let bytes = text.len() as f64 * f64::from(times);
    let name = path.rsplit('/').next().unwrap_or(&path);
    println!("{name}: {} bytes, {} units, {} rules, {times} times", text.len(), units.list.len(), Rule::ALL.len());
    println!("lex (floor)  {:.1} MB/s  tokens {}", bytes / lex / 1e6, tokens / times as usize);
    println!("lint         {:.1} MB/s  ratio {:.2}  findings {findings}", bytes / lint / 1e6, lint / lex);
    ExitCode::SUCCESS
}
