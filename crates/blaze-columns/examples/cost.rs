//! What `blaze_columns::enc` costs on real Rust: every `.rs` file under a directory, lexed by
//! the pinned nightly's lexer, made into four columns: each identifier's text in order of
//! appearance (strings that repeat), each identifier's byte offset in the files end to end (a
//! sorted column), each line's length (small integers) and each token's kind (a small enum).
//! For each column and scheme: the bytes before (a plain array of the narrowest of `u32` and
//! `u64`; strings as `u32` offsets and their bytes) and after, nanoseconds a `get(i)` at random
//! `i`, nanoseconds a value for a whole-block decode (vector and scalar unpack), and blaze's own
//! DEFLATE on the plain bytes as a reference ratio only: it is not read in place.
//!
//!     cargo run --release -p blaze-columns --example cost -- <dir>
//!
//! The floor a `get` aims at: the plain array's get (one load) plus the block's directory
//! entry, a load whose address is known before the value's, so about a nanosecond. Last, the
//! id dictionary on a synthetic set of `ModuleScope` id columns ([`scope_ids`]).

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_columns::enc::bits::BLOCK;
use blaze_columns::enc::block::pfor_estimate;
use blaze_columns::enc::{
    DictUse, GetCost, IdView, IntPolicy, IntView, Scheme, StrPolicy, StrScheme, StrView, encode_ids, encode_ints,
    encode_strs,
};
use blaze_own::{Alloc, Vec};
use rustc_lexer::{FrontmatterAllowed, TokenKind, tokenize};

/// Random gets a row times.
const GETS: usize = 1 << 20;

fn walk(dir: &Path, out: &mut std::vec::Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// xorshift64*, for indices only.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let r = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        usize::try_from(r % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

fn indices(n: usize, count: usize) -> std::vec::Vec<usize> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..count).map(|_| rng.below(n)).collect()
}

fn deflated(bytes: &[u8], heap: &'static blaze_own::Heap) -> usize {
    let mut out = Vec::new_in(heap.alloc());
    match blaze_deflate::deflate(bytes, blaze_deflate::Level::Default, heap.alloc(), &mut out) {
        Ok(()) => out.len(),
        Err(_) => 0,
    }
}

fn ns(start: Instant, ops: usize) -> f64 {
    let ops = u32::try_from(ops.max(1)).map_or(f64::from(u32::MAX), f64::from);
    start.elapsed().as_secs_f64() * 1e9 / ops
}

fn ints_column(name: &str, values: &[u64], heap: &'static blaze_own::Heap) {
    let n = values.len();
    let max = values.iter().copied().max().unwrap_or(0);
    let wide = max > u64::from(u32::MAX);
    let plain_bytes: std::vec::Vec<u8> = if wide {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    } else {
        values.iter().flat_map(|&v| u32::try_from(v).unwrap_or(0).to_le_bytes()).collect()
    };
    let at = indices(n, GETS);
    // The plain array's own get.
    let narrow: std::vec::Vec<u32> = values.iter().map(|&v| u32::try_from(v).unwrap_or(0)).collect();
    let start = Instant::now();
    let mut sum = 0u64;
    for &i in &at {
        sum = sum.wrapping_add(if wide { values[i] } else { u64::from(narrow[i]) });
    }
    let plain_get = ns(start, at.len());
    std::hint::black_box(sum);
    println!(
        "\n{name}: {n} values, max {max}; plain {} bytes ({} a value), get {plain_get:.2} ns; deflate {} bytes (ratio {:.2}, reference only)",
        plain_bytes.len(),
        if wide { 8 } else { 4 },
        deflated(&plain_bytes, heap),
        plain_bytes.len() as f64 / deflated(&plain_bytes, heap).max(1) as f64,
    );
    let (pfor, plain_for): (usize, usize) =
        values.chunks(BLOCK).map(pfor_estimate).fold((0, 0), |(a, b), (p, f)| (a + p, b + f));
    println!(
        "  patched FOR estimate: {pfor} bytes against FOR's {plain_for} ({:.1}% saved): exceptions {}",
        100.0 * (1.0 - pfor as f64 / plain_for.max(1) as f64),
        if pfor * 100 < plain_for * 95 { "pay" } else { "do not pay" }
    );
    println!(
        "  {:<14} {:>10} {:>7} {:>9} {:>10} {:>10} {:>9}",
        "scheme", "bytes", "ratio", "get ns", "decode ns", "scalar ns", "encode ms"
    );
    let mut rows: std::vec::Vec<(String, IntPolicy)> = Scheme::ALL
        .iter()
        .map(|&s| (s.name().to_owned(), IntPolicy { max_get: GetCost::Block, dict: DictUse::Never, only: Some(s) }))
        .collect();
    rows.push(("dict".to_owned(), IntPolicy { dict: DictUse::Always, ..IntPolicy::RECORD }));
    rows.push(("chosen".to_owned(), IntPolicy::RECORD));
    rows.push(("chosen const".to_owned(), IntPolicy::CONSTANT));
    for (label, policy) in rows {
        let mut bytes = Vec::new_in(heap.alloc());
        let start = Instant::now();
        let choice = match encode_ints(values, policy, heap.alloc(), &mut bytes) {
            Ok(choice) => choice,
            Err(e) => {
                println!("  {label:<14} refused: {e}");
                continue;
            }
        };
        let encode_ms = start.elapsed().as_secs_f64() * 1e3;
        let view = match IntView::open(&bytes) {
            Ok(view) => view,
            Err(e) => {
                println!("  {label:<14} open refused: {e}");
                continue;
            }
        };
        let slow = matches!(policy.only, Some(Scheme::Delta | Scheme::Delta2));
        let gets = if slow { &at[..at.len() / 64] } else { &at[..] };
        let start = Instant::now();
        let mut sum = 0u64;
        for &i in gets {
            sum = sum.wrapping_add(view.get(i));
        }
        let get = ns(start, gets.len());
        let mut out = [0u64; BLOCK];
        let start = Instant::now();
        for b in 0..view.blocks() {
            view.decode_block(b, &mut out);
            sum = sum.wrapping_add(out[0]);
        }
        let decode = ns(start, n);
        let start = Instant::now();
        for b in 0..view.blocks() {
            view.decode_block_scalar(b, &mut out);
            sum = sum.wrapping_add(out[0]);
        }
        let scalar = ns(start, n);
        std::hint::black_box(sum);
        let mix: std::vec::Vec<String> = Scheme::ALL
            .iter()
            .zip(choice.blocks)
            .filter(|(_, count)| *count > 0)
            .map(|(s, count)| format!("{}:{count}", s.name()))
            .collect();
        println!(
            "  {label:<14} {:>10} {:>7.2} {get:>9.2} {decode:>10.3} {scalar:>10.3} {encode_ms:>9.1}  {}{}",
            choice.bytes,
            plain_bytes.len() as f64 / choice.bytes.max(1) as f64,
            if choice.dict { "dict " } else { "" },
            mix.join(" ")
        );
    }
}

fn strs_column(name: &str, strs: &[&[u8]], heap: &'static blaze_own::Heap) {
    let n = strs.len();
    let total: usize = strs.iter().map(|s| s.len()).sum();
    let distinct = strs.iter().collect::<std::collections::HashSet<_>>().len();
    let mut plain = std::vec::Vec::with_capacity(total + 4 * (n + 1));
    let mut end = 0u32;
    plain.extend_from_slice(&0u32.to_le_bytes());
    for s in strs {
        end += u32::try_from(s.len()).unwrap_or(0);
        plain.extend_from_slice(&end.to_le_bytes());
    }
    for s in strs {
        plain.extend_from_slice(s);
    }
    let zipped = deflated(&plain, heap);
    println!(
        "\n{name}: {n} strings, {distinct} distinct, {total} bytes; plain {} bytes (u32 offsets and bytes); deflate {zipped} bytes (ratio {:.2}, reference only)",
        plain.len(),
        plain.len() as f64 / zipped.max(1) as f64
    );
    println!(
        "  {:<14} {:>10} {:>7} {:>9} {:>12} {:>9} {:>8}",
        "scheme", "bytes", "ratio", "get ns", "scan ns/str", "encode ms", "symbols"
    );
    let at = indices(n, GETS);
    let mut rows: std::vec::Vec<(String, StrPolicy)> = StrScheme::ALL
        .iter()
        .map(|&s| (s.name().to_owned(), StrPolicy { only: Some(s), ..StrPolicy::RECORD }))
        .collect();
    rows.push(("chosen".to_owned(), StrPolicy::RECORD));
    let mut buf = std::vec![0u8; 1 << 16];
    for (label, policy) in rows {
        let mut bytes = Vec::new_in(heap.alloc());
        let start = Instant::now();
        let choice = match encode_strs(strs, policy, heap.alloc(), &mut bytes) {
            Ok(choice) => choice,
            Err(e) => {
                println!("  {label:<14} refused: {e}");
                continue;
            }
        };
        let encode_ms = start.elapsed().as_secs_f64() * 1e3;
        let view = match StrView::open(&bytes) {
            Ok(view) => view,
            Err(e) => {
                println!("  {label:<14} open refused: {e}");
                continue;
            }
        };
        let mut sum = 0usize;
        let start = Instant::now();
        for &i in &at {
            sum += view.get(i, &mut buf).map_or(0, <[u8]>::len);
        }
        let get = ns(start, at.len());
        let start = Instant::now();
        for i in 0..n {
            sum += view.get(i, &mut buf).map_or(0, <[u8]>::len);
        }
        let scan = ns(start, n);
        std::hint::black_box(sum);
        let wrong = (0..n).filter(|&i| view.get(i, &mut buf).ok() != Some(strs[i])).count();
        println!(
            "  {label:<14} {:>10} {:>7.2} {get:>9.2} {scan:>12.2} {encode_ms:>9.1} {:>8}  {}{}",
            choice.bytes,
            plain.len() as f64 / choice.bytes.max(1) as f64,
            choice.symbols,
            choice.scheme.name(),
            if wrong > 0 { format!("  {wrong} WRONG") } else { String::new() }
        );
    }
}

/// 32 bytes from the generator: an id as uniform as a `ContentId`.
fn random_id(rng: &mut Rng) -> [u8; 32] {
    let mut id = [0u8; 32];
    for chunk in id.chunks_mut(8) {
        rng.below(2);
        chunk.copy_from_slice(&rng.0.to_le_bytes());
    }
    id
}

/// The `ContentId` columns of `ModuleScope` binding rows (`def` and `vis_key`, 64 of a row's
/// bytes), synthetic: core's real shape from the scope pipeline's run (406 modules, 102,681
/// bindings, 253 a module) with a model of repetition (stated in the output), written plain
/// and as id dictionaries, a record at a time.
fn scope_ids(heap: &'static blaze_own::Heap) {
    const MODULES: usize = 406;
    const BINDINGS: usize = 102_681;
    let mut rng = Rng(0x5eed_0000_0000_0001);
    let crate_root = random_id(&mut rng);
    let mut records: std::vec::Vec<(std::vec::Vec<[u8; 32]>, std::vec::Vec<[u8; 32]>)> = std::vec::Vec::new();
    for m in 0..MODULES {
        let rows = BINDINGS / MODULES + usize::from(m < BINDINGS % MODULES);
        let (module, parent) = (random_id(&mut rng), random_id(&mut rng));
        let mut defs: std::vec::Vec<[u8; 32]> = std::vec::Vec::with_capacity(rows);
        let mut vis: std::vec::Vec<[u8; 32]> = std::vec::Vec::with_capacity(rows);
        for _ in 0..rows {
            // 15% a def bound before in another namespace (a unit or tuple struct's type and
            // constructor, a fn and its macro), else a new def.
            let def = if !defs.is_empty() && rng.below(100) < 15 { defs[rng.below(defs.len())] } else { random_id(&mut rng) };
            defs.push(def);
            // `pub` (the zero key) half the time, `pub(crate)` a fifth, private a quarter,
            // `pub(super)` the rest.
            vis.push(match rng.below(100) {
                0..50 => [0; 32],
                50..70 => crate_root,
                70..95 => module,
                _ => parent,
            });
        }
        records.push((defs, vis));
    }
    println!(
        "\nscope record ids (synthetic, described above `scope_ids`): {MODULES} records, {BINDINGS} binding rows; def: 15% repeats in a record; vis_key: 4 keys a record"
    );
    println!("  {:<14} {:>10} {:>10} {:>7} {:>9}", "column", "plain", "dict", "ratio", "get ns");
    for (label, pick) in [("def", 0usize), ("vis_key", 1)] {
        let mut encoded: std::vec::Vec<Vec<u8, Alloc<'static>>> = std::vec::Vec::new();
        let (mut plain, mut dict) = (0usize, 0usize);
        for (defs, vis) in &records {
            let rows = if pick == 0 { defs } else { vis };
            let mut bytes = Vec::new_in(heap.alloc());
            if let Err(e) = encode_ids(rows, heap.alloc(), &mut bytes) {
                println!("  {label:<14} refused: {e}");
                return;
            }
            plain += 32 * rows.len();
            dict += bytes.len();
            encoded.push(bytes);
        }
        let views: std::vec::Vec<IdView<'_, 32>> = match encoded.iter().map(|b| IdView::<32>::open(b)).collect() {
            Ok(views) => views,
            Err(e) => {
                println!("  {label:<14} open refused: {e}");
                return;
            }
        };
        let picks: std::vec::Vec<(usize, usize)> = indices(BINDINGS, GETS)
            .into_iter()
            .map(|i| {
                let r = i % MODULES;
                (r, i % views[r].len().max(1))
            })
            .collect();
        let start = Instant::now();
        let mut sum = 0u8;
        for &(r, i) in &picks {
            sum ^= views[r].get(i)[0];
        }
        let get = ns(start, picks.len());
        std::hint::black_box(sum);
        let wrong = records
            .iter()
            .zip(&views)
            .map(|((defs, vis), view)| {
                let rows = if pick == 0 { defs } else { vis };
                rows.iter().enumerate().filter(|&(i, row)| view.get(i) != row).count()
            })
            .sum::<usize>();
        println!(
            "  {label:<14} {plain:>10} {dict:>10} {:>7.2} {get:>9.2}{}",
            plain as f64 / dict.max(1) as f64,
            if wrong > 0 { format!("  {wrong} WRONG") } else { String::new() }
        );
    }
}

/// A token kind's variant name, interned to a small id in order of first sight.
fn kind_id(kind: &TokenKind, ids: &mut HashMap<String, u64>) -> u64 {
    let text = format!("{kind:?}");
    let name = text.split(['{', '(', ' ']).next().unwrap_or("").to_owned();
    let next = u64::try_from(ids.len()).unwrap_or(0);
    *ids.entry(name).or_insert(next)
}

fn main() {
    let heap = blaze_own::Heap::process();
    let Some(dir) = std::env::args().nth(1) else {
        println!("usage: cost <directory of .rs files>");
        return;
    };
    let mut files = std::vec::Vec::new();
    walk(Path::new(&dir), &mut files);
    files.sort();
    let sources: std::vec::Vec<String> = files.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect();
    let mut names: std::vec::Vec<&[u8]> = std::vec::Vec::new();
    let (mut offsets, mut lines, mut kinds) = (std::vec::Vec::new(), std::vec::Vec::new(), std::vec::Vec::new());
    let mut ids = HashMap::new();
    let mut base = 0u64;
    for src in &sources {
        let mut at = 0usize;
        for token in tokenize(src, FrontmatterAllowed::No) {
            let len = usize::try_from(token.len).unwrap_or(0);
            kinds.push(kind_id(&token.kind, &mut ids));
            if matches!(token.kind, TokenKind::Ident | TokenKind::RawIdent) {
                names.push(&src.as_bytes()[at..at + len]);
                offsets.push(base + u64::try_from(at).unwrap_or(0));
            }
            at += len;
        }
        lines.extend(src.split('\n').map(|l| u64::try_from(l.len()).unwrap_or(0)));
        base += u64::try_from(src.len()).unwrap_or(0);
    }
    println!(
        "{} files, {} bytes of source, {} tokens of {} kinds, {} identifiers, {} lines",
        sources.len(),
        base,
        kinds.len(),
        ids.len(),
        names.len(),
        lines.len()
    );
    strs_column("identifier names", &names, heap);
    ints_column("identifier offsets", &offsets, heap);
    ints_column("line lengths", &lines, heap);
    ints_column("token kinds", &kinds, heap);
    scope_ids(heap);
}
