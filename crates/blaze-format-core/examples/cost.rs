//! What a lookup in sorted rows costs, old against new, at realistic sizes: ns a call, printed.
//! A measurement (rule 82), not a test; the twins' equality is held by the tests in
//! src/rows.rs, crates/blaze-scope/src/items_record.rs and crates/blaze-surface/src/module_scope.rs.
//!
//! `cargo run --release -p blaze-format-core --example cost`
//!
//! Three sections, one line a size:
//!
//! - `find`: `ItemsRecord::find` (blaze-scope) over a table of 80-byte item rows keyed by one
//!   crate's item keys (one 8-byte crate head, then hash bits), 1k to 100k rows: the word-first
//!   halving it was against `find_keyed`, which interpolates on bytes 8..32 when the table's
//!   rows share the key's head, and against `find_row` on all 32 bytes (what interpolating on
//!   the crate head alone would give). Held keys, sought in a shuffled order.
//! - `seek`: `rows::seek_from`'s own probe compare, `probe.cmp(key)` at every probe as it was,
//!   against the prefix-first compare (`rows::order_by_prefix`), over 48-byte rows keyed by 32
//!   bytes (blaze-cas's store index shape).
//! - `names`: `ScopeView::get`'s probe (blaze-surface) over a scope's names in a string table,
//!   `(ns, &str)` compared whole with `str::get`'s boundary check as it was, against the
//!   namespace then `rows::prefix_in` word first; identifier-like names, many sharing a prefix
//!   (`from_`, `try_`, `into_`), at 16, 128 and 2048 bindings.

use std::cmp::Ordering;
use std::hint::black_box;
use std::time::Instant;

use blaze_format_core::rows;

/// SplitMix64's `i`th output from `start`.
fn mix(start: u64, i: u64) -> u64 {
    let mut z = start.wrapping_add((i + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `n` distinct sorted 32-byte digest-like keys.
fn keys(n: usize, seed: u64) -> Vec<[u8; 32]> {
    let mut out: Vec<[u8; 32]> = (0..n as u64)
        .map(|i| {
            let mut key = [0u8; 32];
            for (w, chunk) in key.chunks_mut(8).enumerate() {
                chunk.copy_from_slice(&mix(seed, i * 4 + w as u64).to_be_bytes());
            }
            key
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Rows of `width` bytes, each `key` first and filler after.
fn rows_of(keys: &[[u8; 32]], width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(keys.len() * width);
    for key in keys {
        out.extend_from_slice(key);
        out.resize(out.len() + width - 32, 0x5a);
    }
    out
}

/// The keys in a shuffled order, `count` of them (repeating).
fn sought(keys: &[[u8; 32]], count: usize, seed: u64) -> Vec<[u8; 32]> {
    (0..count as u64).map(|i| keys[(mix(seed ^ 0xabcd, i) % keys.len() as u64) as usize]).collect()
}

/// ns a call of `f` over `queries`, best of five rounds.
fn ns_a_call<Q>(queries: &[Q], mut f: impl FnMut(&Q) -> usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let mut sum = 0usize;
        for query in queries {
            sum = sum.wrapping_add(f(black_box(query)));
        }
        black_box(sum);
        best = best.min(start.elapsed().as_nanos() as f64 / queries.len() as f64);
    }
    best
}

/// The old `ItemsRecord::find`: word-first halving over rows of `width`.
fn find_by_halving(bytes: &[u8], width: usize, count: usize, key: &[u8; 32]) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, count);
    let want = u64::from_be_bytes(*key.first_chunk::<8>().expect("32 bytes"));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let row = mid * width;
        let word = u64::from_be_bytes(*bytes[row..].first_chunk::<8>().expect("a row holds its key"));
        match word.cmp(&want).then_with(|| bytes[row + 8..row + 32].cmp(&key[8..])) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// The old `rows::seek_from` from row 0, `probe.cmp(key)` at every probe: the copy of the code
/// before the prefix-first compare, kept here to be measured against.
fn seek_whole_compare(rows: &[u8], width: usize, key: &[u8]) -> Result<usize, usize> {
    const FENCE: usize = 16;
    let n = rows.len() / width;
    let key_of = |i: usize| &rows[i * width..i * width + key.len()];
    let want = rows::prefix(key);
    let (mut lo, mut hi) = (0, n);
    let (mut least, mut most) = (0u64, u64::MAX);
    let mut guesses = 0;
    let mut moved = None;
    while lo < hi {
        let at = if guesses + 1 < rows::INTERPOLATIONS && most > least {
            guesses += 1;
            let distance = want - least;
            let count = hi - lo;
            let offset = match (most - least).checked_add(1) {
                None => ((u128::from(distance) * count as u128) >> u64::BITS) as usize,
                Some(span) => match u32::try_from(count) {
                    Ok(n) => rows::interpolated_offset(distance, span, n) as usize,
                    Err(_) => ((u128::from(distance) * count as u128) / u128::from(span)) as usize,
                },
            };
            lo + offset.min(count - 1)
        } else if guesses + 1 == rows::INTERPOLATIONS && hi - lo > 2 * FENCE && moved.is_some() {
            guesses += 1;
            match moved {
                Some(Ordering::Less) => lo + FENCE,
                _ => hi - 1 - FENCE,
            }
        } else {
            lo + (hi - lo) / 2
        };
        let probe = key_of(at);
        match probe.cmp(key) {
            Ordering::Equal => return Ok(at),
            Ordering::Less => {
                lo = at + 1;
                least = rows::prefix(probe);
                moved = Some(Ordering::Less);
            }
            Ordering::Greater => {
                hi = at;
                most = rows::prefix(probe);
                moved = Some(Ordering::Greater);
            }
        }
    }
    Err(lo)
}

/// A scope's names: `(ns, at, len)` into one string table, sorted by `(ns, name)`, one each.
struct Names {
    strings: String,
    rows: Vec<(u8, u32, u32)>,
}

/// `n` identifier-like names over three namespaces, many sharing a common head.
fn names(n: usize, seed: u64) -> Names {
    const HEADS: [&str; 10] = ["from_", "try_", "into_", "as_", "is_", "with_", "new", "get_", "Block", "u"];
    const TAILS: [&str; 12] = ["str", "bytes", "number", "hash", "header", "u64", "slice", "Receipt", "inner", "mut", "vec", "len"];
    let mut drawn: Vec<(u8, String)> = (0..n as u64)
        .map(|i| {
            let r = mix(seed, i);
            let head = HEADS[(r % 10) as usize];
            let tail = TAILS[((r >> 8) % 12) as usize];
            let name = if (r >> 16) % 3 == 0 { head.trim_end_matches('_').to_string() } else { format!("{head}{tail}{}", (r >> 24) % 7) };
            (((r >> 40) % 3) as u8, name)
        })
        .collect();
    drawn.sort_unstable();
    drawn.dedup();
    let mut strings = String::new();
    let mut rows = Vec::with_capacity(drawn.len());
    for (ns, name) in &drawn {
        rows.push((*ns, strings.len() as u32, name.len() as u32));
        strings.push_str(name);
    }
    Names { strings, rows }
}

fn main() {
    println!("find: ItemsRecord::find, 80-byte rows of one crate's item keys (one 8-byte head), held keys");
    for n in [1_000usize, 10_000, 50_000, 100_000] {
        let mut keys = keys(n, 7);
        for key in &mut keys {
            key[..8].copy_from_slice(b"cratehd1");
        }
        keys.sort_unstable();
        let bytes = rows_of(&keys, 80);
        let queries = sought(&keys, 1_000_000, 11);
        let old = ns_a_call(&queries, |key| find_by_halving(&bytes, 80, keys.len(), key).unwrap_or(0));
        // `find_keyed`'s path for a table whose first and last rows share the key's head.
        let new = ns_a_call(&queries, |key| {
            let shared = bytes[..8] == key[..8] && bytes[bytes.len() - 80..bytes.len() - 72] == key[..8];
            let found = if shared { rows::find_row(&bytes, 80, 8, &key[8..]) } else { rows::find_row(&bytes, 80, 0, key) };
            found.unwrap_or(0)
        });
        let blind = ns_a_call(&queries, |key| rows::find_row(&bytes, 80, 0, key).unwrap_or(0));
        println!(
            "  rows {n:>7}: old halving {old:>6.1} ns, new find_keyed {new:>6.1} ns (ratio {:.2}); find_row on all 32 bytes {blind:>6.1} ns",
            old / new
        );
    }

    println!("seek: rows::seek_from's probe compare, 48-byte rows, 32-byte keys, held keys");
    for n in [10_000usize, 100_000, 1_000_000] {
        let keys = keys(n, 13);
        let bytes = rows_of(&keys, 48);
        let queries = sought(&keys, 1_000_000, 17);
        let old = ns_a_call(&queries, |key| seek_whole_compare(&bytes, 48, key).unwrap_or(0));
        let new = ns_a_call(&queries, |key| rows::find_row(&bytes, 48, 0, key).unwrap_or(0));
        println!("  rows {n:>7}: old cmp each probe {old:>6.1} ns, new prefix first {new:>6.1} ns, ratio {:.2}", old / new);
    }

    println!("names: ScopeView::get's probe over a scope's names, bound names sought");
    for n in [16usize, 128, 2048] {
        let table = names(n, 19);
        let (strings, found) = (&table.strings, &table.rows);
        let queries: Vec<(u8, String)> = (0..200_000u64)
            .map(|i| {
                let (ns, at, len) = found[(mix(23, i) % found.len() as u64) as usize];
                (ns, strings[at as usize..(at + len) as usize].to_string())
            })
            .collect();
        let old = ns_a_call(&queries, |(ns, name)| {
            found
                .binary_search_by(|&(row_ns, at, len)| {
                    let text = strings.get(at as usize..(at + len) as usize).expect("checked at read");
                    (row_ns, text).cmp(&(*ns, name.as_str()))
                })
                .unwrap_or(0)
        });
        let bytes = strings.as_bytes();
        let new = ns_a_call(&queries, |(ns, name)| {
            let (want, name) = (rows::prefix(name.as_bytes()), name.as_bytes());
            found
                .binary_search_by(|&(row_ns, at, len)| {
                    row_ns.cmp(ns).then_with(|| {
                        let (start, len) = (at as usize, len as usize);
                        rows::order_by_prefix(rows::prefix_in(bytes, start, len), &bytes[start..start + len], want, name)
                    })
                })
                .unwrap_or(0)
        });
        println!("  bindings {:>5}: old whole names {old:>6.1} ns, new prefix first {new:>6.1} ns, ratio {:.2}", found.len(), old / new);
    }
}
