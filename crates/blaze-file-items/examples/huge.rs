//! Times the outline of one huge file made of parts (crate docs, "A file is outlined item by
//! item"): the `.rs` files under the directories given, each wrapped as `mod mN { .. }` and
//! concatenated until the file holds 10 MB. Cold on 1 thread and on 18 (the parts outlined on
//! all of them, the split and the assembly on one), the whole-file walk beside them, and an
//! edit to one part when every other part's outline is in hand, found by its digest.
//!
//! A measurement tool, not a test.

#![feature(allocator_api)]

use std::{
    alloc::Global,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use blaze_file_items::{Bodies, ContentId, Outline, assemble, outline, outline_item, outline_with, split_top_level};
use blaze_hash::ByHash;
use blaze_own::Alloc;
use blaze_surface::{Edition, Range};

const SIZE: usize = 10_000_000;

/// An outline made on a worker thread, sent to the one that assembles.
struct Made(usize, Outline<'static>);

// SAFETY: every outline here is made in `Alloc::global()`, the global heap, which is `Sync`:
// its columns may be freed and read on any thread. An `Outline` holds nothing else bound to a
// thread.
unsafe impl Send for Made {}

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
    let mut text = String::with_capacity(SIZE + 1_000_000);
    let mut modules = 0usize;
    for file in &files {
        if text.len() >= SIZE {
            break;
        }
        let Ok(body) = std::fs::read_to_string(file) else { continue };
        // A file that starts with a shebang or frontmatter is no module body.
        if body.starts_with("#!") && !body.starts_with("#![") || body.starts_with("---") {
            continue;
        }
        text.push_str(&format!("mod m{modules} {{\n{body}\n}}\n"));
        modules += 1;
    }
    let lines = text.lines().count();
    println!("file: {modules} modules, {lines} lines, {:.1} MB", text.len() as f64 / 1e6);
    let ms = |since: Instant| since.elapsed().as_secs_f64() * 1e3;

    // The whole-file walk, and the composition on one thread.
    let mut best = (f64::MAX, f64::MAX);
    let mut items = 0;
    for _ in 0..3 {
        let start = Instant::now();
        let whole = outline_with(&text, Edition::E2024, Bodies::Skipped, Alloc::global());
        best.0 = best.0.min(ms(start));
        let start = Instant::now();
        let parts = outline(&text, Edition::E2024, Alloc::global());
        best.1 = best.1.min(ms(start));
        assert!(whole == parts, "the parts' outline is the whole file's");
        items = parts.items().len();
    }
    println!("whole-file walk, 1 thread: {:.1} ms best of 3, {items} entries", best.0);
    println!("split + parts + assemble, 1 thread: {:.1} ms best of 3", best.1);

    // Cold on N threads: the split on one, the parts on N, the assembly on one.
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut reference = None;
    for threads in [1usize, 18] {
        if threads > most {
            println!("threads {threads}: this machine has {most}");
            continue;
        }
        let mut best = [f64::MAX; 4];
        let mut count = 0;
        for _ in 0..3 {
            let start = Instant::now();
            let split_ranges = split_top_level(&text, Alloc::global());
            let split = ms(start);
            // The workers read the ranges from a column over the global heap, which is `Sync`.
            let mut ranges = Vec::<Range, Global>::with_capacity_in(split_ranges.len(), Global);
            ranges.extend_from_slice(&split_ranges);
            let next = AtomicUsize::new(0);
            let made = std::sync::Mutex::new(Vec::<Made, Global>::with_capacity_in(ranges.len(), Global));
            let start_parts = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        let mut mine = Vec::<Made, Global>::new_in(Global);
                        loop {
                            let at = next.fetch_add(1, Ordering::Relaxed);
                            let Some(range) = ranges.get(at) else { break };
                            mine.push(Made(at, outline_item(&text[range.to_usize()], Edition::E2024, Alloc::global())));
                        }
                        made.lock().expect("no worker panics").extend(mine);
                    });
                }
            });
            let parts_ms = ms(start_parts);
            let start_assemble = Instant::now();
            let mut made = made.into_inner().expect("no worker panics");
            made.sort_by_key(|it| it.0);
            let assembled = assemble(made.iter().map(|it| (ranges[it.0].start, &it.1)), Alloc::global());
            let assemble_ms = ms(start_assemble);
            let total = ms(start);
            for (slot, value) in best.iter_mut().zip([total, split, parts_ms, assemble_ms]) {
                *slot = slot.min(value);
            }
            count = ranges.len();
            if reference.is_none() {
                reference = Some(assembled.items().len());
            }
            assert_eq!(Some(assembled.items().len()), reference);
        }
        println!(
            "cold, {threads} threads: {:.1} ms best of 3 ({count} parts; split {:.1} ms, parts {:.1} ms, assemble {:.1} ms, each best of 3)",
            best[0], best[1], best[2], best[3]
        );
    }

    // An edit to one part, every other part's outline in hand under its digest.
    let ranges = split_top_level(&text, Alloc::global());
    let digest = |part: &str| ContentId::of("blaze-file-items huge: a part's bytes and edition", part.as_bytes());
    let mut index: ByHash<ContentId, usize, Alloc<'static>> = ByHash::new_in(Alloc::global());
    let mut cache: Vec<Outline<'static>, Global> = Vec::new_in(Global);
    for range in ranges.iter() {
        let part = &text[range.to_usize()];
        index.entry(digest(part)).or_insert(cache.len());
        cache.push(outline_item(part, Edition::E2024, Alloc::global()));
    }
    // The edit: a line added at the top of the middle part's last `fn` body.
    let middle = ranges[ranges.len() / 2];
    let part = &text[middle.to_usize()];
    let at = part.rfind("fn ").and_then(|fn_at| part[fn_at..].find('{').map(|brace| fn_at + brace + 1));
    let Some(at) = at else {
        eprintln!("the middle part holds no `fn` body");
        std::process::exit(2);
    };
    let at = usize::try_from(middle.start).expect("a small file") + at;
    let edited = format!("{}\n        let edited = 1;{}", &text[..at], &text[at..]);
    let mut best = [f64::MAX; 5];
    let mut missed = 0;
    for _ in 0..3 {
        let start = Instant::now();
        let ranges = split_top_level(&edited, Alloc::global());
        let split = ms(start);
        let start_look = Instant::now();
        let mut found: Vec<Result<usize, Range>, Global> = Vec::with_capacity_in(ranges.len(), Global);
        for range in ranges.iter() {
            let key = digest(&edited[range.to_usize()]);
            found.push(index.get(&key).copied().ok_or(*range));
        }
        let look = ms(start_look);
        let start_make = Instant::now();
        let mut fresh: Vec<Outline<'static>, Global> = Vec::new_in(Global);
        for missing in found.iter().filter_map(|it| it.err()) {
            fresh.push(outline_item(&edited[missing.to_usize()], Edition::E2024, Alloc::global()));
        }
        let make = ms(start_make);
        missed = fresh.len();
        let start_assemble = Instant::now();
        let mut fresh_at = 0;
        let parts = found.iter().zip(ranges.iter()).map(|(found, range)| match found {
            Ok(at) => (range.start, &cache[*at]),
            Err(_) => {
                fresh_at += 1;
                (range.start, &fresh[fresh_at - 1])
            }
        });
        let parts: Vec<(u32, &Outline<'static>), Global> = {
            let mut all = Vec::with_capacity_in(ranges.len(), Global);
            all.extend(parts);
            all
        };
        let assembled = assemble(parts.iter().copied(), Alloc::global());
        let assemble_ms = ms(start_assemble);
        let total = ms(start);
        for (slot, value) in best.iter_mut().zip([total, split, look, make, assemble_ms]) {
            *slot = slot.min(value);
        }
        assert!(
            assembled == outline_with(&edited, Edition::E2024, Bodies::Skipped, Alloc::global()),
            "the edited file's outline from parts in hand is the whole walk's"
        );
    }
    println!(
        "edit to one part ({:.1} KB of {:.1} MB), {missed} part outlined again: {:.2} ms best of 3 (split {:.2} ms, digest and look up every part {:.2} ms, outline the missing {:.2} ms, assemble {:.2} ms)",
        middle.to_usize().len() as f64 / 1e3,
        edited.len() as f64 / 1e6,
        best[0],
        best[1],
        best[2],
        best[3],
        best[4],
    );
}
