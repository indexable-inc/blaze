//! Times the outline of one huge file made of parts (crate docs, "A file is outlined item by
//! item"): the `.rs` files under the directories given, each wrapped as `mod mN { .. }` and
//! concatenated until the file holds 10 MB. Cold on 1 thread and on 18 (the split and the parts
//! on all of them, the assembly on one), the whole-file walk beside them, and an
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

use blaze_file_items::{
    Assembled, Bodies, ContentId, Outline, Part, Speculated, chunk_starts, common_prefix, common_suffix, join,
    OneByOne, SideBySide, outline, outline_item, outline_with, part_digest, part_digests, resplit, speculate,
    split_top_level,
};
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
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
    let heap = blaze_own::Heap::process();
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
        let whole = outline_with(&text, Edition::E2024, Bodies::Skipped, heap.alloc());
        best.0 = best.0.min(ms(start));
        let start = Instant::now();
        let parts = outline(&text, Edition::E2024, heap.alloc());
        best.1 = best.1.min(ms(start));
        assert!(whole == parts, "the parts' outline is the whole file's");
        items = parts.items().len();
    }
    println!("whole-file walk, 1 thread: {:.1} ms best of 3, {items} entries", best.0);
    println!("split + parts + assemble, 1 thread: {:.1} ms best of 3", best.1);

    // Cold on N threads: the split on N (speculate per chunk, then join), the parts on N, the
    // assembly on one.
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let whole = outline_with(&text, Edition::E2024, Bodies::Skipped, heap.alloc());
    for threads in [1usize, 18] {
        if threads > most {
            println!("threads {threads}: this machine has {most}");
            continue;
        }
        let mut best = [f64::MAX; 4];
        let mut count = 0;
        for _ in 0..3 {
            let start = Instant::now();
            let (ranges, _) = split_on(&text, threads, heap);
            let split = ms(start);
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
                            mine.push(Made(at, outline_item(&text[range.to_usize()], Edition::E2024, heap.alloc())));
                        }
                        made.lock().expect("no worker panics").extend(mine);
                    });
                }
            });
            let parts_ms = ms(start_parts);
            let start_assemble = Instant::now();
            let mut made = made.into_inner().expect("no worker panics");
            made.sort_by_key(|it| it.0);
            let assembled = Assembled::new(made.iter().map(|it| (ranges[it.0].start, &it.1)), heap.alloc());
            let assemble_ms = ms(start_assemble);
            let total = ms(start);
            for (slot, value) in best.iter_mut().zip([total, split, parts_ms, assemble_ms]) {
                *slot = slot.min(value);
            }
            count = ranges.len();
            assert!(assembled.to_outline(heap.alloc()) == whole, "the cold outline from parts is the whole walk's");
        }
        println!(
            "cold, {threads} threads: {:.1} ms best of 3 ({count} parts; split {:.1} ms, parts {:.1} ms, assemble {:.1} ms, each best of 3)",
            best[0], best[1], best[2], best[3]
        );
    }

    // An edit to one part, every other part's outline in hand under its digest.
    let ranges = split_top_level(&text, heap.alloc());
    let digest = |part: &str| part_digest(part.as_bytes());
    let mut index: Map<ContentId, usize, BuildKeyHasher, Alloc<'static>> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: heap.alloc(), capacity: None });
    let mut cache: Vec<Outline<'static>, Global> = Vec::new_in(Global);
    for range in ranges.iter() {
        let part = &text[range.to_usize()];
        index.entry(digest(part)).or_insert(cache.len());
        cache.push(outline_item(part, Edition::E2024, heap.alloc()));
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
    // The previous parts table, as the driver keeps it with the previous text.
    let prev: Vec<Part, Global> = {
        let mut prev = Vec::with_capacity_in(ranges.len(), Global);
        prev.extend(ranges.iter().map(|range| Part { range: *range, digest: digest(&text[range.to_usize()]) }));
        prev
    };
    // The compare alone: the common prefix and suffix of the old and new bytes.
    let mut compare = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let prefix = common_prefix(text.as_bytes(), edited.as_bytes());
        let suffix = common_suffix(&text.as_bytes()[prefix..], &edited.as_bytes()[prefix..]);
        compare = compare.min(ms(start));
        std::hint::black_box((prefix, suffix));
    }
    println!(
        "compare of old and new bytes: {compare:.3} ms best of 5 ({:.0} MB/s over the shorter text, read twice)",
        text.len() as f64 / 1e6 / (compare / 1e3)
    );
    let mut best = [f64::MAX; 5];
    let mut missed = 0;
    let mut rescanned = 0;
    for _ in 0..3 {
        let start = Instant::now();
        let made = resplit(&text, &prev, &edited, heap.alloc());
        let split = ms(start);
        rescanned = made.rescanned.to_usize().len();
        assert!(made.synced, "a one-line edit inside a body meets an old cut");
        let start_look = Instant::now();
        let mut found: Vec<Result<usize, Range>, Global> = Vec::with_capacity_in(made.parts.len(), Global);
        for part in made.parts.iter() {
            found.push(index.get(&part.digest).copied().ok_or(part.range));
        }
        let ranges: Vec<Range, Global> = {
            let mut all = Vec::with_capacity_in(made.parts.len(), Global);
            all.extend(made.parts.iter().map(|part| part.range));
            all
        };
        let look = ms(start_look);
        let start_make = Instant::now();
        let mut fresh: Vec<Outline<'static>, Global> = Vec::new_in(Global);
        for missing in found.iter().filter_map(|it| it.err()) {
            fresh.push(outline_item(&edited[missing.to_usize()], Edition::E2024, heap.alloc()));
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
        let assembled = Assembled::new(parts.iter().copied(), heap.alloc());
        let assemble_ms = ms(start_assemble);
        let total = ms(start);
        for (slot, value) in best.iter_mut().zip([total, split, look, make, assemble_ms]) {
            *slot = slot.min(value);
        }
        assert!(
            assembled.to_outline(heap.alloc()) == outline_with(&edited, Edition::E2024, Bodies::Skipped, heap.alloc()),
            "the edited file's outline from parts in hand is the whole walk's"
        );
    }
    println!(
        "edit to one part ({:.1} KB of {:.1} MB), {missed} part outlined again: {:.2} ms best of 3 (resplit with the compare {:.2} ms, {rescanned} bytes scanned and digested again; look up every part {:.2} ms, outline the missing {:.2} ms, assemble {:.2} ms)",
        middle.to_usize().len() as f64 / 1e3,
        edited.len() as f64 / 1e6,
        best[0],
        best[1],
        best[2],
        best[3],
        best[4],
    );

    // The file-sized work of a cold outline, split and digest, on 1 and 18 threads.
    for (threads, side_by_side) in [(1usize, false), (1, true), (18, false), (18, true)] {
        if threads > most {
            continue;
        }
        let mut best = [f64::MAX; 4];
        let mut again = 0;
        for _ in 0..3 {
            let start = Instant::now();
            let (ranges, times) = split_on(&text, threads, heap);
            again = times.again;
            let (speculated, joined) = (times.speculate, times.join);
            let start_digest = Instant::now();
            let mut digests = Vec::<ContentId, Global>::with_capacity_in(ranges.len(), Global);
            digests.resize(ranges.len(), ContentId([0; 32]));
            let per = ranges.len().div_ceil(threads);
            std::thread::scope(|scope| {
                for (ranges, digests) in ranges.chunks(per).zip(digests.chunks_mut(per)) {
                    let text = &text;
                    scope.spawn(move || match side_by_side {
                        true => part_digests::<SideBySide>(text.as_bytes(), ranges, digests),
                        false => part_digests::<OneByOne>(text.as_bytes(), ranges, digests),
                    });
                }
            });
            let digested = ms(start_digest);
            for (slot, value) in best.iter_mut().zip([ms(start), speculated, joined, digested]) {
                *slot = slot.min(value);
            }
            assert!(ranges[..] == split_top_level(&text, heap.alloc())[..], "the joined split is the split");
        }
        println!(
            "split and digest, {threads} threads, digests {}: {:.2} ms best of 3 (speculative scans {:.2} ms, join {:.2} ms with {again} bytes scanned again, digests {:.2} ms, each best of 3)",
            if side_by_side { "side by side" } else { "one by one" },
            best[0], best[1], best[2], best[3]
        );
    }
}

/// How long the parts of a split on threads took.
struct SplitTimes {
    speculate: f64,
    join: f64,
    /// The bytes [`join`] scanned again at the seams.
    again: usize,
}

/// The top-level split of `text` on `threads`: each chunk scanned from its guessed start on a
/// worker ([`speculate`]), then verified and mended at the seams on this thread ([`join`]). The
/// ranges are copied into a column over the global heap, which is `Sync`, for the workers.
fn split_on(text: &str, threads: usize, heap: &'static blaze_own::Heap) -> (Vec<Range, Global>, SplitTimes) {
    let ms = |since: Instant| since.elapsed().as_secs_f64() * 1e3;
    let start = Instant::now();
    let starts = chunk_starts(text, threads, heap.alloc());
    let mut bounds = Vec::<(u32, u32), Global>::with_capacity_in(starts.len(), Global);
    for (at, &chunk) in starts.iter().enumerate() {
        let until = starts.get(at + 1).copied().unwrap_or(u32::try_from(text.len()).expect("a file under 4 GiB"));
        bounds.push((chunk, until));
    }
    let next = AtomicUsize::new(0);
    let made = std::sync::Mutex::new(Vec::<Spec, Global>::with_capacity_in(bounds.len(), Global));
    std::thread::scope(|scope| {
        for _ in 0..threads.min(bounds.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(chunk, until)) = bounds.get(at) else { break };
                    let spec = Spec(at, speculate(text, chunk, until, heap.alloc()));
                    made.lock().expect("no worker panics").push(spec);
                }
            });
        }
    });
    let speculated = ms(start);
    let start_join = Instant::now();
    let mut made = made.into_inner().expect("no worker panics");
    made.sort_by_key(|it| it.0);
    let mut chunks = Vec::with_capacity_in(made.len(), Global);
    chunks.extend(made.into_iter().map(|it| it.1));
    let (parts, again) = join(text, &chunks, heap.alloc());
    let mut ranges = Vec::<Range, Global>::with_capacity_in(parts.len(), Global);
    ranges.extend_from_slice(&parts);
    (ranges, SplitTimes { speculate: speculated, join: ms(start_join), again })
}

/// A chunk's speculative scan made on a worker thread, sent to the one that joins.
struct Spec(usize, Speculated<'static>);

// SAFETY: every scan here is made in `Alloc::global()`, the global heap, which is `Sync`; a
// `Speculated` holds nothing else bound to a thread.
unsafe impl Send for Spec {}
