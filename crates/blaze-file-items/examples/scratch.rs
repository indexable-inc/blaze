//! The bytes an outline passes through the task's frame, per item and per input byte, and its
//! time: on 1,000 one-line `fn`s, on library/core, and on one large file; and one digest of
//! every `.rs` file's `FileItems` record under a directory, so a change that must not move a
//! record by a byte is checked by that line alone.
//!
//! `scratch <library dir> <core dir> <large file>`. A measurement tool, not a test.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, time::Instant};

use blaze_file_items::{ContentId, outline};
use blaze_mem::frame::Frame;
use blaze_mem::tag::Tag;
use blaze_own::{Erased};
use blaze_surface::{Edition, ItemDetail, Range, Rename, WrittenVis};

/// Every `.rs` file under `dir`, sorted, with its text.
fn files(dir: &PathBuf) -> Vec<(PathBuf, String), Global> {
    let mut out = Vec::new_in(Global);
    let mut stack = Vec::new_in(Global);
    stack.push(dir.clone());
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push((path, text));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The frame's peak bytes for outlining `texts` one after another, each in a fresh frame
/// scope, summed; the items; and the best time of five rounds.
fn measure(texts: &[&str]) -> (usize, usize, f64) {
    let mut frame = Frame::with_reserve(1 << 34, Tag::Parse).expect("a frame");
    let (mut bytes, mut items) = (0usize, 0usize);
    for text in texts {
        frame.reset();
        let erased = Erased::new(&frame);
        items += outline(text, Edition::E2021, erased.handle()).items().len();
        drop(erased);
        bytes += frame.peak();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for text in texts {
            frame.reset();
            let erased = Erased::new(&frame);
            std::hint::black_box(outline(text, Edition::E2021, erased.handle()).items().len());
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    (bytes, items, best)
}

fn report(name: &str, texts: &[&str]) {
    let input: usize = texts.iter().map(|t| t.len()).sum();
    let (bytes, items, secs) = measure(texts);
    println!(
        "{name}: {} files, {input} bytes, {items} items; frame {bytes} bytes = {:.0} per item, {:.2} per input byte; {:.2} ms",
        texts.len(),
        bytes as f64 / items.max(1) as f64,
        bytes as f64 / input.max(1) as f64,
        secs * 1e3
    );
}

/// The two ranges an entry's detail holds, if any.
fn detail_ranges(detail: &ItemDetail) -> [Option<Range>; 2] {
    match *detail {
        ItemDetail::Mod { path, .. } => [path, None],
        ItemDetail::Use { tree } => [tree, None],
        ItemDetail::ExternCrate { rename: Rename::To(name) } => [Some(name), None],
        ItemDetail::ExternBlock { abi } => [abi, None],
        ItemDetail::MacroCall { path, input } => [path, input],
        ItemDetail::Variant { discriminant } => [discriminant, None],
        ItemDetail::Field { ty, default } => [ty, default],
        _ => [None, None],
    }
}

/// What each field of [`Item`] would cost as a column: per file, a frame of reference packed
/// at the width of the file's widest value (columns.md, `For`, one block a file), in bits an
/// entry over all files, and the share of entries at the frame's zero. Positions are as the
/// record has them: the entry's start a delta from the previous entry's, the others relative
/// to the entry; `parent` and `end` as distances back and forward.
fn field_costs(texts: &[&str], heap: &'static blaze_own::Heap) {
    const NAMES: [&str; 22] = [
        "detail tag", "detail range 1 start", "detail range 1 len", "detail range 2 start", "detail range 2 len",
        "parent (distance back)", "end (descendants)", "range start (delta)", "range len", "name start",
        "name len", "visibility tag", "visibility path", "signature start", "signature len", "body start",
        "body len", "attrs start (delta)", "attrs count", "disambiguator", "flags", "hash, signature_hash",
    ];
    let mut bits = [0u64; 22];
    let mut zeros = [0u64; 22];
    let mut same_hash = 0u64;
    let mut total = 0u64;
    for text in texts {
        let made = outline(text, Edition::E2021, heap.alloc());
        let items = made.items();
        let mut columns: [Vec<u64>; 22] = std::array::from_fn(|_| Vec::new());
        let (mut previous_start, mut previous_attrs) = (0u64, 0u64);
        for (at, item) in items.iter().enumerate() {
            let at = at as u64;
            let start = u64::from(item.range.start);
            let pair = |range: Option<Range>| range.map_or((0, 0), |r| (u64::from(r.start) + 1, u64::from(r.len())));
            let [first, second] = detail_ranges(&item.detail).map(pair);
            let (vis_tag, vis_path) = match item.visibility {
                WrittenVis::Private => (0, 0),
                WrittenVis::Pub => (1, 0),
                WrittenVis::Crate => (2, 0),
                WrittenVis::Super => (3, 0),
                WrittenVis::SelfOnly => (4, 0),
                WrittenVis::In(path) => (5, u64::from(path.start) + 1),
                WrittenVis::WithoutIn(path) => (6, u64::from(path.start) + 1),
            };
            let name = pair(item.name);
            let body = pair(item.body);
            let values = [
                u64::from(item.kind() as u8),
                first.0,
                first.1,
                second.0,
                second.1,
                item.parent.map_or(0, |parent| at - u64::from(parent.0)),
                u64::from(item.end.0) - at - 1,
                start - previous_start,
                u64::from(item.range.len()),
                name.0,
                name.1,
                vis_tag,
                vis_path,
                u64::from(item.signature.start),
                u64::from(item.signature.len()),
                body.0,
                body.1,
                u64::from(item.attrs.start) - previous_attrs,
                u64::from(item.attrs.end - item.attrs.start),
                u64::from(item.disambiguator),
                u64::from(item.flags.bits()),
                0,
            ];
            previous_start = start;
            previous_attrs = u64::from(item.attrs.end);
            same_hash += u64::from(item.hash == item.signature_hash);
            for (column, value) in columns.iter_mut().zip(values) {
                column.push(value);
            }
        }
        for (field, column) in columns.iter().enumerate() {
            let min = column.iter().copied().min().unwrap_or(0);
            let width = column.iter().map(|v| 64 - (v - min).leading_zeros()).max().unwrap_or(0);
            bits[field] += u64::from(width) * column.len() as u64;
            zeros[field] += column.iter().filter(|&&v| v == min).count() as u64;
        }
        bits[21] += 512 * items.len() as u64;
        total += items.len() as u64;
    }
    println!("field costs over {total} entries: bits an entry packed per file, share at the frame's zero");
    let mut sum = 0u64;
    for field in 0..22 {
        sum += bits[field];
        println!(
            "  {:<24} {:>6.2} bits  {:>5.1}%",
            NAMES[field],
            bits[field] as f64 / total as f64,
            100.0 * zeros[field] as f64 / total as f64
        );
    }
    println!(
        "  packed: {:.1} bytes an entry (hashes {:.0}); signature_hash equal to hash in {:.1}% of entries",
        sum as f64 / 8.0 / total as f64,
        bits[21] as f64 / 8.0 / total as f64,
        100.0 * same_hash as f64 / total as f64
    );
}

fn main() {
    let heap = blaze_own::Heap::process();
    let args: Vec<String> = std::env::args().collect();
    // `scratch profile <core dir> <seconds>`: outline library/core over and over on one thread,
    // for a sampling profiler to attach to.
    if let [_, mode, core, seconds] = args.as_slice()
        && mode == "profile"
    {
        let core = files(&PathBuf::from(core));
        let seconds: f64 = seconds.parse().expect("seconds");
        let mut frame = Frame::with_reserve(1 << 34, Tag::Parse).expect("a frame");
        let (start, mut rounds) = (Instant::now(), 0u32);
        println!("profile: pid {}", std::process::id());
        while start.elapsed().as_secs_f64() < seconds {
            for (_, text) in &core {
                frame.reset();
                let erased = Erased::new(&frame);
                std::hint::black_box(outline(text, Edition::E2021, erased.handle()).items().len());
            }
            rounds += 1;
        }
        println!("profile: {rounds} rounds of library/core, {:.2} ms each", start.elapsed().as_secs_f64() * 1e3 / f64::from(rounds));
        return;
    }
    let [_, library, core, large] = args.as_slice() else {
        eprintln!("usage: scratch <library dir> <core dir> <large file> | scratch profile <core dir> <seconds>");
        std::process::exit(2);
    };
    println!(
        "an Item is {} bytes, an Attr {}, a SyntaxError {}",
        size_of::<blaze_surface::Item>(),
        size_of::<blaze_surface::Attr>(),
        size_of::<blaze_surface::SyntaxError>()
    );
    println!(
        "Item fields: detail {}, parent {}, end {}, range {}, name {}, visibility {}, signature {}, body {}, attrs {}, disambiguator {}, hash {}, signature_hash {}, flags {}",
        size_of::<ItemDetail>(),
        size_of::<Option<blaze_surface::ItemIndex>>(),
        size_of::<blaze_surface::ItemIndex>(),
        size_of::<Range>(),
        size_of::<Option<Range>>(),
        size_of::<WrittenVis<Range>>(),
        size_of::<Range>(),
        size_of::<Option<Range>>(),
        size_of::<blaze_surface::AttrSpan>(),
        size_of::<u32>(),
        size_of::<ContentId>(),
        size_of::<ContentId>(),
        size_of::<blaze_surface::ItemFlags>()
    );
    let fns: String = (0..1000).map(|i| format!("fn f{i}() {{}}\n")).collect();
    report("1,000 one-line fns", &[fns.as_str()]);
    let core = files(&PathBuf::from(core));
    report("library/core", &core.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>());
    field_costs(&core.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), heap);
    let large = std::fs::read_to_string(large).expect("the large file reads");
    report("large file", &[large.as_str()]);

    // One digest of every record under `library`: the outline's values, all of them.
    let library = files(&PathBuf::from(library));
    let mut all = Vec::new();
    for (path, text) in &library {
        let made = outline(text, Edition::E2021, heap.alloc());
        let record = made
            .to_file_items(text, ContentId(blaze_blake3::hash(text.as_bytes())), heap.alloc())
            .unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
        all.extend_from_slice(&blaze_blake3::hash(&record));
    }
    let digest = blaze_blake3::hash(&all);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    println!("records of {} files under library/: digest {hex}", library.len());
}
