//! How small and how fast real record bytes get under each scheme: the table of
//! docs/v2/plan/design/columns.md, "Cold and moving bytes, measured".
//!
//!     examplerun.sh blaze-squeeze squeeze --recs DIR --library DIR --ladder DIR --leaves F,F \
//!         --code FILE --profile-before DIR --profile-after DIR
//!
//! Record sets: `--recs` holds `<kind>.recs` files (each record a little-endian `u32` length and
//! its bytes) that blaze-scope's pipeline example writes with `--dump`: `module_scope`,
//! `module_decls`, `impl_bucket` of one crate; `--library` is a directory whose every `.rs` file
//! is outlined here to its `FileItems` record; `--ladder` the ladder's rungs, every file under
//! a `.blaze/v2/records/` (drive envelopes holding code and surface records); `--leaves` leaf
//! tables (`.blaze/v2/last`), each one record; `--code` linked AArch64 machine code (a binary's
//! `__text`), cut in 4 KiB pieces as a stand-in for code records, with and without the branch
//! filter; `--profile-before` and `--profile-after` the profile's Parquet files, each one record,
//! and each rewritten by blaze-parquet under every codec (Part 1's before and after).
//!
//! Every speed is MB/s of the raw bytes, the best of several rounds (the machine is shared).
//! "one read" is the time to have ONE record from the middle of the set in hand: a copy for raw,
//! a decompression for per-record schemes, a block's decompression for solid ones; for blaze's
//! column encodings it is one ROW (every column's `get`).

#![feature(allocator_api)]
#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

use std::alloc::Global;
use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_columns::enc::{IdView, IntPolicy, IntView, StrPolicy, StrScheme, StrView, encode_ids, encode_ints, encode_strs};
use blaze_columns::{Batch, Field};
use blaze_own::AVec;
use blaze_parquet::{Compression, Encoding, File, Options};
use blaze_surface::{ContentId, Edition, FileItemsView, ScopeBuilder, ScopeView, Visibility};

/// Rounds a fast measurement is taken in; the best is kept.
const ROUNDS: usize = 5;

fn best(rounds: usize, mut f: impl FnMut()) -> f64 {
    (0..rounds)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

fn total(recs: &[Vec<u8>]) -> usize {
    recs.iter().map(Vec::len).sum()
}

fn odd(recs: &[Vec<u8>]) -> usize {
    recs.iter().skip(1).step_by(2).map(Vec::len).sum()
}

fn mbs(bytes: usize, s: f64) -> f64 {
    bytes as f64 / 1e6 / s
}

/// One row of the table.
struct Row {
    scheme: String,
    bytes: usize,
    /// Bytes beside the records: a dictionary, a block index.
    extra: usize,
    enc: Option<f64>,
    dec: Option<f64>,
    read_ns: Option<f64>,
}

struct Table {
    set: String,
    raw: usize,
    /// The raw bytes of the odd-numbered records, which the dictionary rows are measured on.
    odd: usize,
    records: usize,
    rows: Vec<Row>,
}

impl Table {
    fn print(&self) {
        println!("\n### {} ({} records, {} bytes raw, {:.0} bytes a record)", self.set, self.records, self.raw, self.raw as f64 / self.records.max(1) as f64);
        println!("| scheme | bytes | x smaller | beside | enc MB/s | dec MB/s | one read |");
        println!("|---|---|---|---|---|---|---|");
        for r in &self.rows {
            let opt = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.0}"));
            let read = r.read_ns.map_or("-".to_owned(), |ns| if ns >= 10_000.0 { format!("{:.1} us", ns / 1000.0) } else { format!("{ns:.0} ns") });
            // A row of the odd half is against the odd half's raw bytes.
            let raw = if r.scheme.contains("odd half") { self.odd } else { self.raw };
            let ratio = raw as f64 / (r.bytes + r.extra).max(1) as f64;
            println!("| {} | {} | {ratio:.2} | {} | {} | {} | {read} |", r.scheme, r.bytes, r.extra, opt(r.enc), opt(r.dec));
        }
    }
}

/// Times reading the record at `m` `reps` times; ns a read.
fn per_read(reps: usize, mut f: impl FnMut()) -> f64 {
    best(ROUNDS, || {
        for _ in 0..reps {
            f();
        }
    }) * 1e9
        / reps as f64
}

fn reps_for(len: usize) -> usize {
    (4_000_000 / len.max(1)).clamp(3, 20_000)
}

fn raw_row(recs: &[Vec<u8>]) -> Row {
    let m = recs.len() / 2;
    let mut buf = vec![0u8; recs[m].len()];
    let read = per_read(reps_for(recs[m].len()), || {
        buf.copy_from_slice(black_box(&recs[m]));
        black_box(&buf);
    });
    Row { scheme: "raw".into(), bytes: total(recs), extra: 0, enc: None, dec: None, read_ns: Some(read) }
}

/// zstd on each record at `level`, with `dict` if given.
fn zstd_row(name: &str, recs: &[Vec<u8>], level: i32, dict: Option<&[u8]>, rounds: usize) -> Row {
    let mut comp: Vec<Vec<u8>> = Vec::new();
    let enc = best(rounds, || {
        let mut c = match dict {
            Some(d) => zstd::bulk::Compressor::with_dictionary(level, d).unwrap(),
            None => zstd::bulk::Compressor::new(level).unwrap(),
        };
        comp = recs.iter().map(|r| c.compress(r).unwrap()).collect();
    });
    let most = recs.iter().map(Vec::len).max().unwrap_or(0);
    let mut buf = vec![0u8; most];
    let mut d = match dict {
        Some(dict) => zstd::bulk::Decompressor::with_dictionary(dict).unwrap(),
        None => zstd::bulk::Decompressor::new().unwrap(),
    };
    let dec = best(rounds.min(ROUNDS), || {
        for (c, r) in comp.iter().zip(recs) {
            let n = d.decompress_to_buffer(c.as_slice(), &mut buf[..]).unwrap();
            assert_eq!(n, r.len());
        }
    });
    let m = recs.len() / 2;
    let read = per_read(reps_for(recs[m].len()), || {
        black_box(d.decompress_to_buffer(black_box(comp[m].as_slice()), &mut buf[..]).unwrap());
    });
    let raw = total(recs);
    Row {
        scheme: name.into(),
        bytes: total(&comp),
        extra: dict.map_or(0, <[u8]>::len),
        enc: Some(mbs(raw, enc)),
        dec: Some(mbs(raw, dec)),
        read_ns: Some(read),
    }
}

fn lz4_row(recs: &[Vec<u8>]) -> Row {
    let mut comp: Vec<Vec<u8>> = Vec::new();
    let enc = best(ROUNDS, || {
        comp = recs
            .iter()
            .map(|r| {
                let mut out = blaze_own::Vec::new_in(Global);
                blaze_parquet::lz4::compress(r, &mut out);
                out.as_slice().to_vec()
            })
            .collect();
    });
    let most = recs.iter().map(Vec::len).max().unwrap_or(0);
    let mut buf = vec![0u8; most];
    let dec = best(ROUNDS, || {
        for (c, r) in comp.iter().zip(recs) {
            blaze_parquet::lz4::decompress(c, &mut buf[..r.len()]).unwrap();
        }
    });
    let m = recs.len() / 2;
    let read = per_read(reps_for(recs[m].len()), || {
        blaze_parquet::lz4::decompress(black_box(&comp[m]), &mut buf[..recs[m].len()]).unwrap();
    });
    let raw = total(recs);
    Row { scheme: "lz4 per record".into(), bytes: total(&comp), extra: 0, enc: Some(mbs(raw, enc)), dec: Some(mbs(raw, dec)), read_ns: Some(read) }
}

/// Whole records in blocks of at least `target` raw bytes, each block one zstd frame at `level`;
/// an index of 8 bytes a record (its block and offset) and 8 a block (its compressed start).
fn solid_row(recs: &[Vec<u8>], target: usize, level: i32) -> Row {
    let mut blocks: Vec<(usize, usize)> = Vec::new(); // record range
    let mut start = 0;
    let mut size = 0;
    for (i, r) in recs.iter().enumerate() {
        size += r.len();
        if size >= target {
            blocks.push((start, i + 1));
            (start, size) = (i + 1, 0);
        }
    }
    if start < recs.len() {
        blocks.push((start, recs.len()));
    }
    let raw_blocks: Vec<Vec<u8>> = blocks.iter().map(|&(a, b)| recs[a..b].concat()).collect();
    let mut comp: Vec<Vec<u8>> = Vec::new();
    let enc = best(ROUNDS.min(3), || {
        let mut c = zstd::bulk::Compressor::new(level).unwrap();
        comp = raw_blocks.iter().map(|r| c.compress(r).unwrap()).collect();
    });
    let most = raw_blocks.iter().map(Vec::len).max().unwrap_or(0);
    let mut buf = vec![0u8; most];
    let mut d = zstd::bulk::Decompressor::new().unwrap();
    let dec = best(ROUNDS.min(3), || {
        for c in &comp {
            black_box(d.decompress_to_buffer(c.as_slice(), &mut buf[..]).unwrap());
        }
    });
    let m = recs.len() / 2;
    let b = blocks.iter().position(|&(a, e)| (a..e).contains(&m)).unwrap();
    let offset: usize = recs[blocks[b].0..m].iter().map(Vec::len).sum();
    let mut out = vec![0u8; recs[m].len()];
    let read = per_read(reps_for(raw_blocks[b].len()), || {
        d.decompress_to_buffer(black_box(comp[b].as_slice()), &mut buf[..]).unwrap();
        out.copy_from_slice(&buf[offset..offset + recs[m].len()]);
        black_box(&out);
    });
    let raw = total(recs);
    let label = if target >= 1 << 20 { format!("solid {} MiB, zstd {level}", target >> 20) } else { format!("solid {} KiB, zstd {level}", target >> 10) };
    Row {
        scheme: label,
        bytes: total(&comp),
        extra: 8 * recs.len() + 8 * blocks.len(),
        enc: Some(mbs(raw, enc)),
        dec: Some(mbs(raw, dec)),
        read_ns: Some(read),
    }
}

/// A dictionary of at most `size` bytes trained on the even records; zstd 3 with it on the odd
/// ones, beside plain zstd 3 on the same odd ones.
fn dict_rows(recs: &[Vec<u8>], sizes: &[usize]) -> Vec<Row> {
    let train: Vec<&[u8]> = recs.iter().step_by(2).map(Vec::as_slice).collect();
    let test: Vec<Vec<u8>> = recs.iter().skip(1).step_by(2).cloned().collect();
    let mut rows = vec![];
    rows.push(zstd_row("zstd 3 per record (the odd half)", &test, 3, None, ROUNDS));
    for &size in sizes {
        let start = Instant::now();
        match zstd::dict::from_samples(&train, size) {
            Ok(dict) => {
                let trained = start.elapsed().as_secs_f64();
                let mut row = zstd_row(&format!("zstd 3 + {} KiB dict (odd half; trained {:.2} s)", size >> 10, trained), &test, 3, Some(&dict), ROUNDS);
                row.scheme = format!("{} [dict {} bytes]", row.scheme, dict.len());
                rows.push(row);
            }
            Err(e) => rows.push(Row { scheme: format!("{} KiB dict: training refused: {e}", size >> 10), bytes: 0, extra: 0, enc: None, dec: None, read_ns: None }),
        }
    }
    rows
}

/// The general rows every set gets.
fn general(set: &str, recs: &[Vec<u8>], heavy: bool) -> Table {
    let mut rows = vec![raw_row(recs), zstd_row("zstd 1 per record", recs, 1, None, ROUNDS), zstd_row("zstd 3 per record", recs, 3, None, ROUNDS)];
    if heavy {
        rows.push(zstd_row("zstd 19 per record", recs, 19, None, 1));
    }
    rows.push(lz4_row(recs));
    if recs.len() >= 8 {
        rows.extend(dict_rows(recs, &[16 << 10, 112 << 10]));
    }
    rows.push(solid_row(recs, 64 << 10, 3));
    rows.push(solid_row(recs, 1 << 20, 3));
    Table { set: set.into(), raw: total(recs), odd: odd(recs), records: recs.len(), rows }
}

fn read_recs(path: &Path) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = vec![];
    let mut at = 0;
    while at < bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        out.push(bytes[at + 4..at + 4 + len].to_vec());
        at += 4 + len;
    }
    out
}

fn walk(dir: &Path, ext: Option<&str>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, ext, out);
        } else if ext.is_none_or(|ext| path.extension().is_some_and(|e| e == ext)) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// blaze's column encodings on real rows.

/// A record's rows as columns: integers, ids and strings.
#[derive(Default)]
struct Cols {
    ints: Vec<(&'static str, Vec<u64>)>,
    ids: Vec<(&'static str, Vec<[u8; 32]>)>,
    strs: Vec<(&'static str, Vec<Vec<u8>>)>,
}

impl Cols {
    fn int(&mut self, name: &'static str, v: u64) {
        match self.ints.iter_mut().find(|c| c.0 == name) {
            Some(c) => c.1.push(v),
            None => self.ints.push((name, vec![v])),
        }
    }
    fn id(&mut self, name: &'static str, v: [u8; 32]) {
        match self.ids.iter_mut().find(|c| c.0 == name) {
            Some(c) => c.1.push(v),
            None => self.ids.push((name, vec![v])),
        }
    }
    fn str(&mut self, name: &'static str, v: &[u8]) {
        match self.strs.iter_mut().find(|c| c.0 == name) {
            Some(c) => c.1.push(v.to_vec()),
            None => self.strs.push((name, vec![v.to_vec()])),
        }
    }
    fn rows(&self) -> usize {
        self.ints.first().map_or(0, |c| c.1.len())
    }

    /// Each column encoded at the record policy, end to end; the columns' starts.
    fn encode(&self, heap: &'static blaze_own::Heap) -> (Vec<u8>, Vec<usize>) {
        let alloc = heap.alloc();
        let mut out = AVec::new_in(alloc);
        let mut starts = vec![];
        for (_, v) in &self.ints {
            starts.push(out.len());
            encode_ints(v, IntPolicy::RECORD, alloc, &mut out).unwrap();
        }
        for (_, v) in &self.ids {
            starts.push(out.len());
            encode_ids::<32>(v, alloc, &mut out).unwrap();
        }
        for (_, v) in &self.strs {
            starts.push(out.len());
            let s: Vec<&[u8]> = v.iter().map(Vec::as_slice).collect();
            encode_strs(&s, StrPolicy::RECORD, alloc, &mut out).unwrap();
        }
        starts.push(out.len());
        (out.as_slice().to_vec(), starts)
    }
}

/// Small enum values by their `Debug` text, numbered in first-use order (same cardinality as
/// the record's byte).
#[derive(Default)]
struct Tags(HashMap<String, u64>);

impl Tags {
    fn of(&mut self, v: &dyn std::fmt::Debug) -> u64 {
        let key = format!("{v:?}");
        let n = self.0.len() as u64;
        *self.0.entry(key).or_insert(n)
    }
}

/// One ROW out of the middle of an encoded record: every column's `get`, ns a row.
fn ours_read(bytes: &[u8], starts: &[usize], cols: &Cols) -> f64 {
    let n = cols.rows();
    let m = n / 2;
    let ni = cols.ints.len();
    let nd = cols.ids.len();
    let mut buf = vec![0u8; 1 << 16];
    per_read(20_000, || {
        let mut acc = 0u64;
        for c in 0..ni {
            let v = IntView::open(&bytes[starts[c]..starts[c + 1]]).unwrap();
            acc = acc.wrapping_add(v.get(black_box(m)));
        }
        for c in 0..nd {
            let v = IdView::<32>::open(&bytes[starts[ni + c]..starts[ni + c + 1]]).unwrap();
            acc = acc.wrapping_add(u64::from(v.get(black_box(m))[0]));
        }
        for c in 0..cols.strs.len() {
            let v = StrView::open(&bytes[starts[ni + nd + c]..starts[ni + nd + c + 1]]).unwrap();
            acc = acc.wrapping_add(v.get(black_box(m), &mut buf).unwrap().len() as u64);
        }
        black_box(acc);
    })
}

/// The scope records' binding rows as columns; the rest of each record (globs, errors, paths,
/// textual, the strings no binding names) written again as a scope with no bindings, raw.
fn scope_ours(recs: &[Vec<u8>], heap: &'static blaze_own::Heap) -> (Vec<Vec<u8>>, Vec<Cols>) {
    let alloc = heap.alloc();
    let mut tags = Tags::default();
    let mut out = vec![];
    let mut all = vec![];
    for r in recs {
        let view = ScopeView::read(r).unwrap();
        let mut cols = Cols::default();
        for e in view.bindings() {
            cols.int("ns", tags.of(&e.ns));
            cols.int("def_kind", tags.of(&e.binding.def.kind));
            cols.int("origin", tags.of(&e.binding.origin));
            cols.int("expansion", u64::from(e.binding.expansion));
            cols.int("state", tags.of(&e.binding.state));
            let (vis, key) = match e.binding.vis {
                Visibility::Public => (0, [0; 32]),
                Visibility::Crate(k) => (1, k.0.0),
                Visibility::Restricted(k) => (2, k.0.0),
            };
            cols.int("vis", vis);
            cols.id("def", e.binding.def.key.0.0);
            cols.id("vis_key", key);
            cols.str("name", e.name.as_bytes());
        }
        let mut rest = ScopeBuilder::from(&view, alloc);
        rest.bindings.clear();
        let rest = rest.write(alloc).unwrap();
        let mut rec = if cols.rows() > 0 { cols.encode(heap).0 } else { vec![] };
        rec.extend_from_slice(&rest);
        out.push(rec);
        all.push(cols);
    }
    (out, all)
}

/// The bytes one `FileItems` row takes, from two records that differ by eight items of
/// eight-byte names.
fn item_row_bytes(heap: &'static blaze_own::Heap) -> usize {
    let alloc = heap.alloc();
    let make = |n: usize| {
        let text: String = (0..n).map(|i| format!("fn fn{i:05}(){{}}\n")).collect();
        let o = blaze_file_items::outline(&text, Edition::E2024, alloc);
        o.to_file_items(&text, ContentId::of("blaze squeeze row size", text.as_bytes()), alloc).unwrap().len()
    };
    (make(16) - make(8)) / 8 - 7
}

/// Every `.rs` under `dir` outlined to its `FileItems` record.
fn library_items(dir: &Path, heap: &'static blaze_own::Heap) -> Vec<Vec<u8>> {
    let alloc = heap.alloc();
    let mut files = vec![];
    walk(dir, Some("rs"), &mut files);
    let mut out = vec![];
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let o = blaze_file_items::outline(&text, Edition::E2024, alloc);
        if let Ok(rec) = o.to_file_items(&text, ContentId::of("blaze squeeze file", text.as_bytes()), alloc) {
            out.push(rec.to_vec());
        }
    }
    out
}

fn items_ours(recs: &[Vec<u8>], row: usize, heap: &'static blaze_own::Heap) -> (usize, usize, Vec<Cols>, Vec<Vec<u8>>) {
    let mut tags = Tags::default();
    let (mut cols_bytes, mut rest) = (0, 0);
    let mut all = vec![];
    let mut encoded = vec![];
    for r in recs {
        let view = FileItemsView::read(r).unwrap();
        let mut cols = Cols::default();
        let mut names = 0;
        for it in view.items() {
            let item = it.item;
            cols.int("kind", tags.of(&item.kind()));
            cols.int("parent", item.parent.map_or(0, |p| u64::from(p.0) + 1));
            cols.int("end", u64::from(item.end.0));
            cols.int("range.start", u64::from(item.range.start));
            cols.int("range.end", u64::from(item.range.end));
            let (ns, ne) = item.name.map_or((0, 0), |r| (r.start, r.end));
            cols.int("name.start", u64::from(ns));
            cols.int("name.end", u64::from(ne));
            cols.int("signature.start", u64::from(item.signature.start));
            cols.int("signature.end", u64::from(item.signature.end));
            let (bs, be) = item.body.map_or((0, 0), |r| (r.start, r.end));
            cols.int("body.start", u64::from(bs));
            cols.int("body.end", u64::from(be));
            cols.int("vis", tags.of(&item.visibility));
            cols.int("attrs.start", u64::from(item.attrs.start));
            cols.int("attrs.end", u64::from(item.attrs.end));
            cols.int("disambiguator", u64::from(item.disambiguator));
            cols.int("flags", tags.of(&item.flags));
            cols.int("detail", tags.of(&item.detail));
            cols.id("hash", item.hash.0);
            cols.id("signature_hash", item.signature_hash.0);
            cols.str("name", it.name.as_bytes());
            names += it.name.len();
        }
        let n = view.items().len();
        let raw_rest = r.len().saturating_sub(n * row + names);
        let enc = if n > 0 { cols.encode(heap).0 } else { vec![] };
        cols_bytes += enc.len();
        rest += raw_rest;
        encoded.push(enc);
        all.push(cols);
    }
    (cols_bytes, rest, all, encoded)
}

/// The record with the most rows: where one row's read is timed.
fn biggest(cols: &[Cols]) -> usize {
    (0..cols.len()).max_by_key(|&i| cols[i].rows()).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Cascades inside blaze's encodings.

fn ints_bytes(values: &[u64], policy: IntPolicy, heap: &'static blaze_own::Heap) -> Vec<u8> {
    let alloc = heap.alloc();
    let mut out = AVec::new_in(alloc);
    encode_ints(values, policy, alloc, &mut out).unwrap();
    out.as_slice().to_vec()
}

/// ns a `get` at random rows of `get`.
fn get_ns(n: usize, mut get: impl FnMut(usize) -> u64) -> f64 {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let idx: Vec<usize> = (0..1 << 16)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        })
        .collect();
    best(ROUNDS, || {
        let mut acc = 0u64;
        for &i in &idx {
            acc = acc.wrapping_add(get(i));
        }
        black_box(acc);
    }) * 1e9
        / idx.len() as f64
}

/// Column-level run-length (runs across blocks), its run values and run starts each a column of
/// the chooser (which may dictionary-code the values): dictionary, then runs, then bit-packing.
fn cascade_rle(values: &[u64], heap: &'static blaze_own::Heap) -> (usize, f64) {
    let (mut run_values, mut starts) = (vec![], vec![]);
    for (i, &v) in values.iter().enumerate() {
        if run_values.last() != Some(&v) || i == 0 {
            run_values.push(v);
            starts.push(i as u64);
        }
    }
    let vb = ints_bytes(&run_values, IntPolicy::RECORD, heap);
    let sb = ints_bytes(&starts, IntPolicy::CONSTANT, heap);
    let vv = IntView::open(&vb).unwrap();
    let sv = IntView::open(&sb).unwrap();
    let runs = starts.len();
    let ns = get_ns(values.len(), |i| {
        let (mut lo, mut hi) = (0usize, runs);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if sv.get(mid) as usize <= i { lo = mid } else { hi = mid }
        }
        vv.get(lo)
    });
    (vb.len() + sb.len(), ns)
}

/// A sorted dictionary: distinct values in order as a delta column, codes as ranks.
fn cascade_sorted_dict(values: &[u64], heap: &'static blaze_own::Heap) -> (usize, f64) {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let rank: HashMap<u64, u64> = distinct.iter().enumerate().map(|(i, &v)| (v, i as u64)).collect();
    let codes: Vec<u64> = values.iter().map(|v| rank[v]).collect();
    let db = ints_bytes(&distinct, IntPolicy { max_get: blaze_columns::enc::GetCost::Block, ..IntPolicy::RECORD }, heap);
    let cb = ints_bytes(&codes, IntPolicy::RECORD, heap);
    let dv = IntView::open(&db).unwrap();
    let cv = IntView::open(&cb).unwrap();
    let ns = get_ns(values.len(), |i| dv.get(cv.get(i) as usize));
    (db.len() + cb.len(), ns)
}

/// Deltas as a column of the chooser (runs, a dictionary of deltas), a checkpoint a block.
fn cascade_delta(values: &[u64], heap: &'static blaze_own::Heap) -> (usize, f64) {
    let deltas: Vec<u64> = values.iter().enumerate().map(|(i, &v)| if i % 1024 == 0 { 0 } else { v.wrapping_sub(values[i - 1]) }).collect();
    let checks: Vec<u64> = values.iter().step_by(1024).copied().collect();
    let db = ints_bytes(&deltas, IntPolicy::RECORD, heap);
    let cb = ints_bytes(&checks, IntPolicy::CONSTANT, heap);
    let dv = IntView::open(&db).unwrap();
    let cv = IntView::open(&cb).unwrap();
    let ns = get_ns(values.len(), |i| {
        let b = i / 1024;
        let mut v = cv.get(b);
        for j in b * 1024 + 1..=i {
            v = v.wrapping_add(dv.get(j));
        }
        v
    });
    (db.len() + cb.len(), ns)
}

/// The single-level chooser against each cascade, on every integer column of `cols` joined
/// across records (as a column of a set would be); per column the cheapest in bytes whose `get`
/// is within 1.3 times the chooser's, then the totals.
fn cascades(set: &str, cols: &[Cols], heap: &'static blaze_own::Heap) {
    let mut joined: Vec<(&'static str, Vec<u64>)> = vec![];
    for c in cols {
        for (name, v) in &c.ints {
            match joined.iter_mut().find(|j| j.0 == *name) {
                Some(j) => j.1.extend_from_slice(v),
                None => joined.push((name, v.clone())),
            }
        }
    }
    println!("\n### cascades on {set}'s integer columns (all records' rows end to end)");
    println!("| column | rows | chooser bytes | get ns | runs+chooser | get ns | sorted dict+delta | get ns | delta+chooser | get ns |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    let (mut base, mut best_within, mut best_any) = (0usize, 0usize, 0usize);
    let (mut base_ns, mut within_ns) = (0f64, 0f64);
    for (name, v) in &joined {
        let b = ints_bytes(v, IntPolicy::RECORD, heap);
        let view = IntView::open(&b).unwrap();
        let ns = get_ns(v.len(), |i| view.get(i));
        let (r, rns) = cascade_rle(v, heap);
        let (s, sns) = cascade_sorted_dict(v, heap);
        let (d, dns) = cascade_delta(v, heap);
        println!("| {name} | {} | {} | {ns:.1} | {r} | {rns:.1} | {s} | {sns:.1} | {d} | {dns:.1} |", v.len(), b.len());
        base += b.len();
        base_ns += ns;
        let mut pick = (b.len(), ns);
        for (bytes, cns) in [(r, rns), (s, sns), (d, dns)] {
            if bytes < pick.0 && cns <= 1.3 * ns {
                pick = (bytes, cns);
            }
        }
        best_within += pick.0;
        within_ns += pick.1;
        best_any += [b.len(), r, s, d].into_iter().min().unwrap();
    }
    println!(
        "cascades, {set}: chooser {base} bytes (mean get {:.1} ns); best cascade within 1.3x get {best_within} bytes ({:.1}% smaller, mean get {:.1} ns); best at any get cost {best_any} bytes ({:.1}% smaller)",
        base_ns / joined.len() as f64,
        100.0 * (1.0 - best_within as f64 / base as f64),
        within_ns / joined.len() as f64,
        100.0 * (1.0 - best_any as f64 / base as f64)
    );
    // Strings: the chooser already cascades a dictionary into FSST; each scheme forced.
    let mut names: Vec<Vec<u8>> = vec![];
    for c in cols {
        for (n, v) in &c.strs {
            if *n == "name" {
                names.extend(v.iter().cloned());
            }
        }
    }
    let s: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let alloc = heap.alloc();
    let mut line = format!("strings, {set} `name` ({} rows, {} bytes):", s.len(), s.iter().map(|x| x.len()).sum::<usize>());
    for scheme in StrScheme::ALL.into_iter().map(Some).chain([None]) {
        let mut out = AVec::new_in(alloc);
        let choice = encode_strs(&s, StrPolicy { only: scheme, ..StrPolicy::RECORD }, alloc, &mut out).unwrap();
        let bytes = out.as_slice().to_vec();
        let view = StrView::open(&bytes).unwrap();
        let mut buf = vec![0u8; 1 << 16];
        let ns = get_ns(s.len(), |i| view.get(i, &mut buf).unwrap().len() as u64);
        line += &format!(" {} {} B {ns:.1} ns;", scheme.map_or(format!("chosen ({})", choice.scheme.name()), |x| x.name().to_owned()), bytes.len());
    }
    println!("{line}");
}

// ---------------------------------------------------------------------------------------------
// Machine code: the AArch64 branch filter.

/// Each `bl`/`b` (imm26) displacement made absolute (the instruction's word index plus it), so
/// calls to one function are equal bytes; `undo` reverses it. xz's ARM64 BCJ filter, simplest.
fn branch_filter(code: &[u8], base: usize, undo: bool) -> Vec<u8> {
    let mut out = code.to_vec();
    for (i, w) in out.chunks_exact_mut(4).enumerate() {
        let insn = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        if insn & 0x7c00_0000 == 0x1400_0000 {
            let pc = ((base / 4 + i) as u32) & 0x03ff_ffff;
            let imm = insn & 0x03ff_ffff;
            let new = if undo { imm.wrapping_sub(pc) } else { imm.wrapping_add(pc) } & 0x03ff_ffff;
            w.copy_from_slice(&((insn & 0xfc00_0000) | new).to_le_bytes());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The profile: Parquet files under each codec.

const CONFIGS: [(&str, Encoding, Compression); 5] = [
    ("plain, none", Encoding::Plain, Compression::None),
    ("encoded, none", Encoding::Auto, Compression::None),
    ("encoded, lz4_raw", Encoding::Auto, Compression::Lz4),
    ("encoded, zstd 1", Encoding::Auto, Compression::Zstd { level: 1 }),
    ("encoded, zstd 3", Encoding::Auto, Compression::Zstd { level: 3 }),
];

fn profile_sizes(dir: &Path) -> ([usize; 5], usize) {
    let mut files = vec![];
    walk(dir, Some("parquet"), &mut files);
    let mut sums = [0usize; 5];
    let mut spans = 0;
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let file = File::open(&bytes, Global).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let batches = file.read_all().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let fields: Vec<Field<Global>> = file.fields().to_vec();
        let batches: &[Batch<Global>] = &batches;
        for (at, (_, encoding, compression)) in CONFIGS.iter().enumerate() {
            let options = Options { encoding: *encoding, compression: *compression, ..Options::DEFAULT };
            let out = blaze_parquet::write(&fields, batches, &[], options, Global).unwrap();
            sums[at] += out.as_slice().len();
            if at == 1 && path.to_string_lossy().contains(".spans.") {
                spans += out.as_slice().len();
            }
        }
    }
    (sums, spans)
}

fn main() {
    let heap = blaze_own::Heap::process();
    let mut args: HashMap<String, String> = HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        args.insert(k.trim_start_matches("--").to_owned(), it.next().unwrap_or_default());
    }
    let arg = |k: &str| args.get(k).map(PathBuf::from);

    if let (Some(before), Some(after)) = (arg("profile-before"), arg("profile-after")) {
        println!("\n## Part 1: the profile's files, before and after `spans.function` names macro files by their calls");
        let (b, bs) = profile_sizes(&before);
        let (a, as_) = profile_sizes(&after);
        println!("| codec | before (bytes) | after (bytes) | x smaller |");
        println!("|---|---|---|---|");
        for (at, (name, ..)) in CONFIGS.iter().enumerate() {
            println!("| {name} | {} | {} | {:.2} |", b[at], a[at], b[at] as f64 / a[at] as f64);
        }
        println!("spans files alone, encoded, none: before {bs}, after {as_}");
    }

    if let Some(recs) = arg("recs") {
        for kind in ["module_scope", "module_decls", "impl_bucket"] {
            let path = recs.join(format!("{kind}.recs"));
            if !path.exists() {
                continue;
            }
            let set = read_recs(&path);
            let mut table = general(kind, &set, true);
            if kind == "module_scope" {
                let (ours, cols) = scope_ours(&set, heap);
                let enc = best(ROUNDS, || {
                    black_box(scope_ours(black_box(&set), heap));
                });
                let m = biggest(&cols);
                let (bytes, starts) = cols[m].encode(heap);
                let read = ours_read(&bytes, &starts, &cols[m]);
                table.rows.insert(1, Row { scheme: "ours: bindings as columns, rest raw".into(), bytes: total(&ours), extra: 0, enc: Some(mbs(total(&set), enc)), dec: None, read_ns: Some(read) });
                let mut z = zstd_row("ours, then zstd 3 per record", &ours, 3, None, ROUNDS);
                z.enc = None;
                table.rows.insert(2, z);
                let half: Vec<Vec<u8>> = ours.iter().skip(1).step_by(2).cloned().collect();
                let train: Vec<&[u8]> = ours.iter().step_by(2).map(Vec::as_slice).collect();
                if let Ok(dict) = zstd::dict::from_samples(&train, 16 << 10) {
                    let mut row = zstd_row("ours, then zstd 3 + 16 KiB dict (odd half)", &half, 3, Some(&dict), ROUNDS);
                    row.enc = None;
                    table.rows.insert(3, row);
                }
                table.rows.insert(4, solid_row(&ours, 64 << 10, 3));
                table.rows[4].scheme = format!("ours, then {}", table.rows[4].scheme);
                table.print();
                cascades("module_scope", &cols, heap);
            } else {
                table.print();
            }
        }
    }

    if let Some(lib) = arg("library") {
        let set = library_items(&lib, heap);
        let row = item_row_bytes(heap);
        let mut table = general("file_items (library/)", &set, true);
        let (cols_bytes, rest, cols, encoded) = items_ours(&set, row, heap);
        let m = biggest(&cols);
        let (bytes, starts) = cols[m].encode(heap);
        let read = ours_read(&bytes, &starts, &cols[m]);
        table.rows.insert(1, Row { scheme: format!("ours: item rows as columns ({cols_bytes}), rest raw ({rest}, estimated: a row is {row} bytes)"), bytes: cols_bytes + rest, extra: 0, enc: None, dec: None, read_ns: Some(read) });
        let z: usize = encoded.iter().map(|e| zstd::bulk::compress(e, 3).unwrap().len()).sum();
        table.rows.insert(2, Row { scheme: "ours (the columns alone), then zstd 3 per record".into(), bytes: z, extra: 0, enc: None, dec: None, read_ns: None });
        table.print();
        cascades("file_items", &cols, heap);
    }

    if let Some(ladder) = arg("ladder") {
        let mut files = vec![];
        walk(&ladder, None, &mut files);
        let set: Vec<Vec<u8>> = files.iter().filter(|p| p.to_string_lossy().contains("/.blaze/v2/records/")).map(|p| std::fs::read(p).unwrap()).collect();
        general("ladder records (drive envelopes: code, surface)", &set, true).print();
    }

    if let Some(leaves) = args.get("leaves") {
        let set: Vec<Vec<u8>> = leaves.split(',').map(|p| std::fs::read(p).unwrap()).collect();
        let mut table = general("leaf tables (each one record)", &set, true);
        table.rows.retain(|r| !r.scheme.contains("dict") && !r.scheme.contains("odd half"));
        table.print();
    }

    if let Some(code) = arg("code") {
        let text = std::fs::read(&code).unwrap();
        let set: Vec<Vec<u8>> = text.chunks(4096).map(<[u8]>::to_vec).collect();
        general("linked AArch64 code, 4 KiB pieces (a stand-in for code records)", &set, true).print();
        let filtered: Vec<Vec<u8>> = set.iter().enumerate().map(|(i, c)| branch_filter(c, i * 4096, false)).collect();
        for (i, c) in filtered.iter().enumerate() {
            assert_eq!(&branch_filter(c, i * 4096, true), &set[i], "the branch filter undoes");
        }
        let filter_s = best(ROUNDS, || {
            for (i, c) in set.iter().enumerate() {
                black_box(branch_filter(c, i * 4096, false));
            }
        });
        println!("branch filter alone: {:.0} MB/s", mbs(total(&set), filter_s));
        let mut t = Table { set: "the same after the branch filter".into(), raw: total(&set), odd: odd(&set), records: set.len(), rows: vec![] };
        t.rows.push(zstd_row("filter, zstd 3 per piece", &filtered, 3, None, ROUNDS));
        t.rows.push(zstd_row("filter, zstd 19 per piece", &filtered, 19, None, 1));
        t.rows.extend(dict_rows(&filtered, &[16 << 10, 112 << 10]));
        t.rows.push(solid_row(&filtered, 64 << 10, 3));
        t.rows.push(solid_row(&filtered, 1 << 20, 3));
        t.print();
    }

    if let Some(dir) = arg("profile-after") {
        let mut files = vec![];
        walk(&dir, Some("parquet"), &mut files);
        let set: Vec<Vec<u8>> = files
            .iter()
            .map(|p| {
                let bytes = std::fs::read(p).unwrap();
                let file = File::open(&bytes, Global).unwrap();
                let batches = file.read_all().unwrap();
                let fields: Vec<Field<Global>> = file.fields().to_vec();
                let opts = Options { encoding: Encoding::Auto, compression: Compression::None, ..Options::DEFAULT };
                blaze_parquet::write(&fields, &batches, &[], opts, Global).unwrap().as_slice().to_vec()
            })
            .collect();
        general("profile files (after Part 1), encoded, no codec, each file one record", &set, false).print();
    }
}
