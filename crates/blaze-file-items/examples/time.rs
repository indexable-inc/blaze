//! Times `outline` over every `.rs` file under the directories given: files, bytes, items,
//! syntax errors, unhandled forms, and the wall time of the outlines alone (reads excluded).
//!
//! A measurement tool for the design's floor (docs/v2/plan/design/bottom.md), not a test.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, time::Instant};

use blaze_file_items::outline;
use blaze_own::Alloc;
use blaze_surface::{Edition, ItemDetail};

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
            let made = outline(text, Edition::E2024, Alloc::global());
            items += made.items().len();
            errors += made.errors().len();
            unhandled += made.items().iter().filter(|it| matches!(it.detail, ItemDetail::Unhandled { .. })).count();
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
    // Half the floor (crate docs, "Why bytes, not tokens"): one BLAKE3 pass over every byte,
    // on one thread. The other half is a skip of every byte, measured by blaze-lex's `cost`.
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let mut sum = 0u8;
        for text in &texts {
            let mut hasher = blaze_ids::Hasher::new("blaze-file-items time: the floor");
            hasher.update(text.as_bytes());
            sum ^= hasher.finish().0[0];
        }
        best = best.min(start.elapsed().as_secs_f64());
        std::hint::black_box(sum);
    }
    println!(
        "blake3 over every byte: {:.1} ms best of 3 on one thread ({:.0} MB/s)",
        best * 1e3,
        bytes as f64 / 1e6 / best
    );
    // The same work on N threads, each taking the next file from one shared counter (so big
    // files do not unbalance a thread): what "divides by N" comes to on this machine. Every
    // outline allocates from the global heap here, so its contention is in the number.
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut one = 0f64;
    for threads in [1usize, 2, 4, 6, 8, 12, 16, 18, 24, 32, 64, 128] {
        if threads > most {
            break;
        }
        let mut best = f64::MAX;
        let mut items = 0usize;
        for _ in 0..3 {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let made = std::sync::atomic::AtomicUsize::new(0);
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        let mut mine = 0usize;
                        loop {
                            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(text) = texts.get(at) else { break };
                            mine += outline(text, Edition::E2024, Alloc::global()).items().len();
                        }
                        made.fetch_add(mine, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            });
            best = best.min(start.elapsed().as_secs_f64());
            items = made.load(std::sync::atomic::Ordering::Relaxed);
        }
        if threads == 1 {
            one = best;
        }
        println!(
            "threads {threads}: {:.1} ms best of 3, {items} items, {:.2}x of one thread, {:.0}% of ideal",
            best * 1e3,
            one / best,
            100.0 * one / best / threads as f64,
        );
    }
}
