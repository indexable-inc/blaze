//! A removal experiment (AGENTS.md rule 23): how long the outline of every `.rs` file under the
//! directories given takes as it is, with every `fn` body cut unread, and with every `fn` body
//! cut and lexed once for its flags. The last two outlines are WRONG (no body says it holds an
//! item): only their times mean anything, as the bound on what leaving bodies out of the parse
//! can gain. Rounds are interleaved so the machine's load falls on all three alike.
//!
//! A measurement tool, not a test.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, sync::atomic::Ordering, time::Instant};

use blaze_file_items::{REMOVAL, outline};
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
        // A file that is not UTF-8 (a test fixture) is no source file: left out, and counted.
        if let Ok(text) = std::fs::read_to_string(file) {
            texts.push(text);
        }
    }
    let bytes: usize = texts.iter().map(|it| it.len()).sum();
    // What the bodies are, from the real outline.
    let (mut fns, mut bodies, mut body_bytes, mut holding, mut holding_bytes) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for text in &texts {
        let made = outline(text, Edition::E2024, Alloc::global());
        for item in made.items() {
            let ItemDetail::Fn { body_holds_items } = item.detail else { continue };
            fns += 1;
            let Some(body) = item.body else { continue };
            bodies += 1;
            body_bytes += body.to_usize().len();
            if body_holds_items {
                holding += 1;
                holding_bytes += body.to_usize().len();
            }
        }
    }
    println!(
        "{} files of {} read, {:.1} MB; {fns} fns, {bodies} with a body of {:.1} MB in all; {holding} bodies hold an item or a macro call, {:.1} MB",
        texts.len(),
        files.len(),
        bytes as f64 / 1e6,
        body_bytes as f64 / 1e6,
        holding_bytes as f64 / 1e6,
    );
    let mut best = [f64::MAX; 3];
    for _ in 0..5 {
        for (mode, slot) in best.iter_mut().enumerate() {
            REMOVAL.store(u8::try_from(mode).expect("three modes"), Ordering::Relaxed);
            let start = Instant::now();
            let mut items = 0usize;
            for text in &texts {
                items += outline(text, Edition::E2024, Alloc::global()).items().len();
            }
            *slot = slot.min(start.elapsed().as_secs_f64());
            std::hint::black_box(items);
        }
    }
    REMOVAL.store(0, Ordering::Relaxed);
    let rate = |seconds: f64| bytes as f64 / 1e6 / seconds;
    println!(
        "as it is: {:.1} ms ({:.0} MB/s); every fn body cut unread: {:.1} ms ({:.0} MB/s); every fn body cut and lexed for its flags: {:.1} ms ({:.0} MB/s) (best of 5 each, one thread)",
        best[0] * 1e3,
        rate(best[0]),
        best[1] * 1e3,
        rate(best[1]),
        best[2] * 1e3,
        rate(best[2]),
    );
}
