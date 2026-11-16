//! Times `outline` over every `.rs` file under the directories given: files, bytes, items,
//! syntax errors, unhandled forms, and the wall time of the outlines alone (reads excluded).
//!
//! A measurement tool for the design's floor (docs/v2/plan/design/bottom.md), not a test.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, time::Instant};

use blaze_file_items::{Edition, ItemKind, outline};

fn main() {
    let mut files: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut stack: Vec<PathBuf, Global> = Vec::new_in(Global);
    stack.extend(std::env::args_os().skip(1).map(PathBuf::from));
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("not a directory that reads: {}", dir.display());
            std::process::exit(2);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut texts: Vec<std::string::String, Global> = Vec::with_capacity_in(files.len(), Global);
    for file in &files {
        match std::fs::read_to_string(file) {
            Ok(text) => texts.push(text),
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                std::process::exit(2);
            }
        }
    }
    let bytes: usize = texts.iter().map(|it| it.len()).sum();
    let lines: usize = texts.iter().map(|it| it.lines().count()).sum();
    for round in 0..3 {
        let (mut items, mut errors, mut unhandled) = (0usize, 0usize, 0usize);
        let start = Instant::now();
        for text in &texts {
            let made = outline(text, Edition::Edition2024, Global);
            items += made.items().len();
            errors += made.errors().len();
            unhandled += made.items().iter().filter(|it| matches!(it.kind, ItemKind::Unhandled { .. })).count();
        }
        let took = start.elapsed();
        println!(
            "round {round}: {} files, {} lines, {:.1} MB, {items} items, {errors} syntax errors, {unhandled} unhandled, {:.1} ms on one thread ({:.0} MB/s, {:.2} us a line)",
            files.len(),
            lines,
            bytes as f64 / 1e6,
            took.as_secs_f64() * 1e3,
            bytes as f64 / 1e6 / took.as_secs_f64(),
            took.as_secs_f64() * 1e6 / lines as f64,
        );
    }
}
