//! The outline lane's oracle: every `.rs` file under the directories given is outlined by the
//! path that is the definition ([`reference`]) and by the path being built to replace it
//! ([`candidate`]), and the two outlines are compared row for row (`Outline`'s `==`: entries
//! with their keys and digests, attributes, findings and their messages, flags). A candidate may
//! refuse a text by type; a refusal is counted by its kind and is never a difference. The run
//! ends with one line of counts and exits 1 when any file differs.
//!
//! To put a new path under the oracle, change the body of [`candidate`] and [`Refused`]: the
//! rest stays. As it stands the candidate is the cold path of `outline` (split into top-level
//! parts, each walked into the file's one outline) against the whole-file walk.
//!
//! Arguments: directories, and `@file` for a file of directories, one a line. A file that is
//! not UTF-8 is no source file: left out and counted.

#![feature(allocator_api)]

use std::{alloc::Global, collections::BTreeMap, path::PathBuf, time::Instant};

use blaze_file_items::{Bodies, Outline, outline, outline_with};
use blaze_own::Alloc;
use blaze_surface::Edition;

/// Why the candidate did not outline a text: one variant a kind the run counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Refused {
    /// A text longer than `u32::MAX` bytes: an outline's ranges are `u32` pairs.
    TooLong,
}

/// The definition: the whole file walked at once from the parser's events.
fn reference(text: &str) -> Outline<'static> {
    outline_with(text, Edition::E2024, Bodies::Skipped, Alloc::global())
}

/// The path under test.
fn candidate(text: &str) -> Result<Outline<'static>, Refused> {
    if u32::try_from(text.len()).is_err() {
        return Err(Refused::TooLong);
    }
    Ok(outline(text, Edition::E2024, Alloc::global()))
}

fn main() {
    let mut files: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut stack: Vec<PathBuf, Global> = Vec::new_in(Global);
    for arg in std::env::args().skip(1) {
        match arg.strip_prefix('@') {
            Some(list) => {
                let Ok(lines) = std::fs::read_to_string(list) else {
                    eprintln!("not a file that reads: {list}");
                    std::process::exit(2);
                };
                stack.extend(lines.lines().filter(|it| !it.is_empty()).map(PathBuf::from));
            }
            None => stack.push(PathBuf::from(arg)),
        }
    }
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
    let (mut read, mut not_utf8, mut bytes, mut entries, mut findings) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut equal, mut different, mut refused_bytes) = (0usize, 0usize, 0usize);
    let mut refused: BTreeMap<Refused, usize> = BTreeMap::new();
    let (mut reference_time, mut candidate_time) = (0f64, 0f64);
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            not_utf8 += 1;
            continue;
        };
        read += 1;
        bytes += text.len();
        let start = Instant::now();
        let old = reference(&text);
        reference_time += start.elapsed().as_secs_f64();
        entries += old.items().len();
        findings += old.errors().len();
        let start = Instant::now();
        let new = candidate(&text);
        candidate_time += start.elapsed().as_secs_f64();
        match new {
            Err(why) => {
                *refused.entry(why).or_default() += 1;
                refused_bytes += text.len();
            }
            Ok(new) if new == old => equal += 1,
            Ok(new) => {
                different += 1;
                if different <= 5 {
                    // The first row that differs, both ways, so a difference names its item.
                    let at = old.items().iter().zip(new.items()).position(|(a, b)| a != b);
                    println!(
                        "DIFFERENT {}: {} entries against {}, {} findings against {}; first differing entry {at:?}",
                        file.display(),
                        old.items().len(),
                        new.items().len(),
                        old.errors().len(),
                        new.errors().len(),
                    );
                    if let Some(at) = at {
                        println!("  reference {:?}", old.items()[at]);
                        println!("  candidate {:?}", new.items()[at]);
                    }
                }
            }
        }
    }
    for (why, count) in &refused {
        println!("refused {why:?}: {count} files");
    }
    println!(
        "oracle: {read} files, {:.1} MB, {entries} entries, {findings} findings ({not_utf8} files not UTF-8 left out); {equal} equal, {different} different, {} refused ({:.1} MB); reference {:.1} ms, candidate {:.1} ms on one thread",
        bytes as f64 / 1e6,
        refused.values().sum::<usize>(),
        refused_bytes as f64 / 1e6,
        reference_time * 1e3,
        candidate_time * 1e3,
    );
    if different != 0 {
        std::process::exit(1);
    }
}
