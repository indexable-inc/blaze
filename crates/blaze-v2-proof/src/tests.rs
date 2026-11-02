use std::collections::BTreeMap;

use blaze_ids::{ContentId, Fingerprint};
use blaze_query::{Edge, QueryKey, QueryKind};

use super::*;

fn key(n: u8) -> QueryKey {
    QueryKey { kind: QueryKind(0x2000), input: ContentId::of("blaze v2 proof test key 1", &[n]) }
}

fn edge(read: u8, saw: u128) -> Edge {
    Edge { read: key(read), saw: Fingerprint(saw) }
}

/// A file (1) read by a parse (2), read by a type check (3), read by codegen (4); and a
/// result (5) of another root, read by nothing here.
fn links(file: u128, parse: u128, check: u128, codegen: u128, ran: bool) -> TraceBuilder {
    let mut trace = TraceBuilder::default();
    trace.insert(key(1), Fingerprint(file), &[], ran);
    trace.insert(key(2), Fingerprint(parse), &[edge(1, file)], ran);
    trace.insert(key(3), Fingerprint(check), &[edge(2, parse)], ran);
    trace.insert(key(4), Fingerprint(codegen), &[edge(3, check)], ran);
    trace
}

fn chain(file: u128, parse: u128, check: u128, codegen: u128, ran: bool) -> Trace {
    let mut trace = links(file, parse, check, codegen, ran);
    trace.insert(key(5), Fingerprint(50), &[], ran);
    trace.finish()
}

#[test]
fn equal_traces_have_nothing_stale() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = chain(10, 20, 30, 40, false);
    assert_eq!(under(&incremental, &scratch, &[]), vec![]);
}

#[test]
fn a_result_that_read_the_same_and_came_out_otherwise_is_the_culprit() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = chain(10, 20, 31, 41, true);
    let stale = under(&incremental, &scratch, &[]);
    assert_eq!(
        stale,
        vec![
            Stale::UnrecordedRead { query: key(3), incremental: Fingerprint(31), scratch: Fingerprint(30) },
            Stale::Downstream { query: key(4), incremental: Fingerprint(41), scratch: Fingerprint(40) },
        ]
    );
    assert!(stale[0].is_culprit() && !stale[1].is_culprit());
}

#[test]
fn a_result_kept_though_what_it_read_changed_is_unverified() {
    let scratch = chain(11, 21, 30, 40, true);
    // The file and the parse reran; the type check was kept with its edge to the old parse.
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(1), Fingerprint(11), &[], true);
    incremental.insert(key(2), Fingerprint(21), &[edge(1, 11)], true);
    incremental.insert(key(5), Fingerprint(50), &[], false);
    let stale = under(&incremental.finish(), &scratch, &[]);
    assert_eq!(
        stale[0],
        Stale::Unverified { query: key(3), read: key(2), saw: Fingerprint(20), now: Some(Fingerprint(21)) }
    );
    assert!(stale[0].is_culprit());
}

#[test]
fn a_cut_off_edge_is_not_unverified() {
    // The file changed, the parse reran to the same fingerprint: keeping the check is right.
    let scratch = chain(11, 20, 30, 40, true);
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(1), Fingerprint(11), &[], true);
    incremental.insert(key(2), Fingerprint(20), &[edge(1, 11)], true);
    incremental.insert(key(5), Fingerprint(50), &[], false);
    assert_eq!(under(&incremental.finish(), &scratch, &[]), vec![]);
}

#[test]
fn a_result_missing_from_the_incremental_build_is_absent() {
    let scratch = chain(10, 20, 30, 40, true);
    let incremental = links(10, 20, 30, 40, false).finish();
    assert_eq!(under(&incremental, &scratch, &[]), vec![Stale::Absent { query: key(5) }]);
}

#[test]
fn only_what_the_roots_reach_is_compared() {
    let scratch = chain(10, 20, 30, 40, true);
    let mut incremental = links(10, 20, 30, 40, false);
    incremental.insert(key(5), Fingerprint(51), &[], true);
    let incremental = incremental.finish();
    assert_eq!(under(&incremental, &scratch, &[key(4)]), vec![]);
    assert_eq!(under(&incremental, &scratch, &[key(5)]).len(), 1);
    let reached = scratch.reachable(&[key(3)]);
    let names: Vec<QueryKey> =
        (0..scratch.keys.len() as u32).filter(|&i| reached.contains(i)).map(|i| scratch.keys[i as usize]).collect();
    let mut want = vec![key(1), key(2), key(3)];
    want.sort();
    assert_eq!(names, want);
}

#[test]
fn over_counts_reruns_for_nothing_cutoffs_and_new_results() {
    let before = chain(10, 20, 30, 40, true);
    let mut after = links(11, 20, 30, 40, false);
    after.insert(key(5), Fingerprint(50), &[], false);
    after.insert(key(1), Fingerprint(11), &[], true);
    // The parse reran and came out the same: cut off.
    after.insert(key(2), Fingerprint(20), &[edge(1, 11)], true);
    // The check reran though its edge is what it was: for nothing.
    after.insert(key(3), Fingerprint(30), &[edge(2, 20)], true);
    after.insert(key(6), Fingerprint(60), &[], true);
    let (work, needless) = over(&before, &after.finish());
    assert_eq!(work, Work { ran: 4, needless: 1, cut_off: 1, new: 1 });
    assert_eq!(needless, vec![key(3)]);
}

#[test]
fn apart_names_every_key_that_differs_in_order() {
    let first: BTreeMap<u32, &str> = [(1, "a"), (2, "b"), (4, "d")].into_iter().collect();
    let second: BTreeMap<u32, &str> = [(2, "B"), (3, "c"), (4, "d")].into_iter().collect();
    assert_eq!(
        apart(&first, &second),
        vec![
            Apart::OnlyFirst { key: 1, value: "a" },
            Apart::Differ { key: 2, first: "b", second: "B" },
            Apart::OnlySecond { key: 3, value: "c" },
        ]
    );
    assert_eq!(apart(&first, &first), vec![]);
}

#[test]
fn rng_is_the_same_from_one_seed_and_stays_below() {
    let a: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
    let b: Vec<u64> = (0..8).scan(Rng::new(7), |r, _| Some(r.next_u64())).collect();
    assert_eq!(a, b);
    assert_ne!(a, (0..8).scan(Rng::new(8), |r, _| Some(r.next_u64())).collect::<Vec<_>>());
    // SplitMix64's first output from 0, as its reference implementation gives it.
    assert_eq!(Rng::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    let mut rng = Rng::new(1);
    assert!((0..1000).all(|_| rng.below(3) < 3));
}

#[test]
fn sample_chooses_distinct_items_in_order() {
    let items: Vec<u32> = (0..100).collect();
    let chosen = sample(&items, 10, 42);
    assert_eq!(chosen.len(), 10);
    assert!(chosen.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(chosen, sample(&items, 10, 42));
    assert_eq!(sample(&items[..5], 10, 42), items[..5].to_vec());
}

#[test]
fn a_trace_round_trips_in_deterministic_cbor() {
    let trace = chain(10, 20, 30, 40, true);
    let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
    let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
    assert_eq!(back, trace);
}

#[test]
fn every_simd_path_finds_the_first_difference_the_scalar_one_does() {
    let mut rng = Rng::new(1606);
    let paths = bytes::available();
    assert_eq!(paths.last(), Some(&bytes::Simd::Scalar));
    for len in (0..300).chain([1 << 12, (1 << 16) + 7]) {
        let a: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let mut cases = vec![(a.clone(), None)];
        for _ in 0..4 {
            if len == 0 {
                break;
            }
            let at = rng.below(len as u64) as usize;
            let mut b = a.clone();
            b[at] ^= 1 << rng.below(8);
            // A second difference after the first must not hide it.
            if at + 1 < len {
                b[len - 1] ^= 0x80;
            }
            cases.push((b, Some(at)));
        }
        let mut shorter = a.clone();
        shorter.truncate(len / 2);
        cases.push((shorter, if len / 2 < len { Some(len / 2) } else { None }));
        for (b, want) in &cases {
            for &path in &paths {
                assert_eq!(bytes::first_difference_with(path, &a, b), *want, "{path:?}, length {len}");
                assert_eq!(bytes::first_difference_with(path, b, &a), *want, "{path:?}, length {len}, swapped");
            }
        }
    }
}

#[test]
fn every_simd_path_finds_each_difference_in_a_block_and_the_overlapping_tail() {
    let paths = bytes::available();
    let mut rng = Rng::new(77);
    // Every length across the four-vector blocks (up to 256 bytes) and the tails after them;
    // a single difference at every offset, so each lane of each vector and of the last,
    // overlapping one is where the first difference is.
    for len in (0..=600).chain([1023, 1024, 1025, 4096 + 63]) {
        let a: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        for &path in &paths {
            assert_eq!(bytes::first_difference_with(path, &a, &a), None, "{path:?}, length {len}");
        }
        let offsets: Vec<usize> = if len <= 300 { (0..len).collect() } else { (0..64).map(|_| rng.below(len as u64) as usize).collect() };
        for at in offsets {
            let mut b = a.clone();
            b[at] = !b[at];
            for &path in &paths {
                assert_eq!(bytes::first_difference_with(path, &a, &b), Some(at), "{path:?}, length {len}, at {at}");
            }
        }
    }
}

#[test]
fn an_empty_file_maps_to_no_bytes() {
    let dir = std::env::temp_dir().join(format!("blaze-v2-proof-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::write(&a, b"").unwrap();
    std::fs::write(&b, b"x").unwrap();
    assert_eq!(bytes::Mapped::open(&a).unwrap().bytes(), b"");
    assert_eq!(bytes::first_file_difference(&a, &a).unwrap(), None);
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), Some(0));
    assert!(bytes::first_file_difference(&a, &dir.join("missing")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn files_are_compared_across_buffer_boundaries() {
    let dir = std::env::temp_dir().join(format!("blaze-v2-proof-bytes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a"), dir.join("b"));
    let body: Vec<u8> = (0..(3 << 20) + 5).map(|i: u32| (i % 251) as u8).collect();
    std::fs::write(&a, &body).unwrap();
    std::fs::write(&b, &body).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), None);
    let mut other = body.clone();
    other[(2 << 20) + 3] ^= 1;
    std::fs::write(&b, &other).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), Some((2 << 20) + 3));
    std::fs::write(&b, &body[..1 << 20]).unwrap();
    assert_eq!(bytes::first_file_difference(&a, &b).unwrap(), Some(1 << 20));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_builder_keeps_the_last_insert_and_names_every_read() {
    let mut trace = TraceBuilder::default();
    trace.insert(key(2), Fingerprint(20), &[edge(1, 10)], true);
    trace.insert(key(2), Fingerprint(21), &[edge(1, 11), edge(3, 30)], false);
    let trace = trace.finish();
    assert_eq!(trace.keys.len(), 3);
    assert_eq!(trace.results(), 1);
    let two = trace.index(&key(2)).unwrap();
    assert_eq!(trace.result(two), Some(Fingerprint(21)));
    assert!(!trace.ran.contains(two));
    let read: Vec<(QueryKey, Fingerprint)> = trace.edges(two).map(|(r, s)| (trace.keys[r as usize], s)).collect();
    assert_eq!(read, vec![(key(1), Fingerprint(11)), (key(3), Fingerprint(30))]);
    assert_eq!(trace.result(trace.index(&key(1)).unwrap()), None);
    assert_eq!(trace.edges_from.len(), trace.keys.len() + 1);
}

fn le_bytes(column: &[u32]) -> Vec<u8> {
    column.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn an_exact_encoding_round_trips_every_shape() {
    let decode = |bytes: &Vec<u8>| -> Result<Vec<u32>, String> {
        if bytes.len() % 4 != 0 {
            return Err(format!("{} bytes is no whole number of u32s", bytes.len()));
        }
        Ok(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    };
    let checked = columns::round_trips(le_bytes, decode, 17).unwrap();
    assert_eq!(checked, columns::shapes(17).len());
    assert_eq!(columns::shapes(17), columns::shapes(17));
}

#[test]
fn a_lossy_encoding_is_named_with_where_it_lost() {
    let encode = |column: &[u32]| -> Vec<u16> { column.iter().map(|&v| v as u16).collect() };
    let decode = |packed: &Vec<u16>| -> Result<Vec<u32>, String> { Ok(packed.iter().map(|&v| u32::from(v)).collect()) };
    let wrong = columns::round_trips(encode, decode, 17).unwrap_err();
    assert!(wrong.starts_with("max of 1:"), "{wrong}");
    let shapes = columns::shapes(3);
    assert!(shapes.iter().any(|(name, c)| *name == "CSR offsets" && c.windows(2).all(|w| w[0] <= w[1])));
    assert!(shapes.iter().any(|(name, c)| *name == "strictly increasing" && c.windows(2).all(|w| w[0] < w[1])));
}

#[test]
fn a_frame_of_reference_packing_reads_in_place() {
    // Each value as its offset from the column's minimum, in as many bits as the range needs.
    let encode = |column: &[u32]| -> (u32, u32, Vec<u64>) {
        let min = column.iter().copied().min().unwrap_or(0);
        let range = column.iter().map(|&v| v - min).max().unwrap_or(0);
        let width = 32 - range.leading_zeros();
        let mut words = vec![0u64; (column.len() * width as usize).div_ceil(64) + 1];
        // A constant column has no bits to write: `get` gives `min`.
        for (i, &v) in column.iter().enumerate().filter(|_| width > 0) {
            let bit = i * width as usize;
            let offset = u128::from(v - min) << (bit % 64);
            words[bit / 64] |= offset as u64;
            words[bit / 64 + 1] |= (offset >> 64) as u64;
        }
        (min, width, words)
    };
    let get = |(min, width, words): &(u32, u32, Vec<u64>), i: usize| -> u32 {
        if *width == 0 {
            return *min;
        }
        let bit = i * *width as usize;
        let pair = u128::from(words[bit / 64]) | (u128::from(words[bit / 64 + 1]) << 64);
        min + ((pair >> (bit % 64)) as u64 & ((1u64 << width) - 1)) as u32
    };
    assert_eq!(columns::reads_in_place(encode, get, 5).unwrap(), columns::shapes(5).len());
    let off_by_one = |packed: &(u32, u32, Vec<u64>), i: usize| get(packed, i).wrapping_add(u32::from(i == 3));
    assert!(columns::reads_in_place(encode, off_by_one, 5).unwrap_err().contains("element 3"));
}

/// The layout and joins before the speed pass (a `BTreeMap` builder, a binary search per edge
/// and per key, `apart` by lookups and a sort): the model the columns must agree with.
mod model {
    use std::collections::BTreeMap;

    use blaze_ids::{Fingerprint, bits::BitSet};
    use blaze_query::{Edge, QueryKey};

    use crate::{Apart, Stale, Trace, Work};

    pub fn finish(rows: &[(QueryKey, Fingerprint, Vec<Edge>, bool)]) -> Trace {
        let mut results: BTreeMap<QueryKey, (Fingerprint, bool, &[Edge])> = BTreeMap::new();
        for (key, fingerprint, edges, ran) in rows {
            results.insert(*key, (*fingerprint, *ran, edges));
        }
        let mut keys: Vec<QueryKey> = results.keys().copied().collect();
        for (_, _, edges) in results.values() {
            keys.extend(edges.iter().map(|e| e.read));
        }
        keys.sort_unstable();
        keys.dedup();
        let n = keys.len();
        let mut trace = Trace {
            has: BitSet::new(n),
            ran: BitSet::new(n),
            fingerprints: vec![Fingerprint(0); n],
            edges_from: vec![0],
            ..Trace::default()
        };
        for (i, key) in keys.iter().enumerate() {
            if let Some(&(fingerprint, ran, edges)) = results.get(key) {
                trace.has.insert(i as u32);
                if ran {
                    trace.ran.insert(i as u32);
                }
                trace.fingerprints[i] = fingerprint;
                for edge in edges {
                    trace.reads.push(keys.binary_search(&edge.read).unwrap() as u32);
                    trace.saw.push(edge.saw);
                }
            }
            trace.edges_from.push(trace.reads.len() as u32);
        }
        trace.keys = keys;
        trace
    }

    fn same_edges(a: &Trace, i: u32, b: &Trace, j: u32) -> bool {
        let x: Vec<_> = a.edges(i).map(|(r, s)| (a.keys[r as usize], s)).collect();
        let y: Vec<_> = b.edges(j).map(|(r, s)| (b.keys[r as usize], s)).collect();
        x == y
    }

    pub fn under(incremental: &Trace, scratch: &Trace, roots: &[QueryKey]) -> Vec<Stale> {
        let reached = scratch.reachable(roots);
        let mut stale = Vec::new();
        for i in 0..scratch.keys.len() as u32 {
            let Some(theirs) = scratch.result(i).filter(|_| reached.contains(i)) else { continue };
            let query = scratch.keys[i as usize];
            let Some((j, ours)) = incremental.index(&query).and_then(|j| Some((j, incremental.result(j)?))) else {
                stale.push(Stale::Absent { query });
                continue;
            };
            if !incremental.ran.contains(j)
                && let Some((read, saw)) = incremental.edges(j).find(|&(read, saw)| incremental.result(read) != Some(saw))
            {
                let now = incremental.result(read);
                stale.push(Stale::Unverified { query, read: incremental.keys[read as usize], saw, now });
                continue;
            }
            if ours == theirs {
                continue;
            }
            stale.push(if same_edges(incremental, j, scratch, i) {
                Stale::UnrecordedRead { query, incremental: ours, scratch: theirs }
            } else {
                Stale::Downstream { query, incremental: ours, scratch: theirs }
            });
        }
        stale.sort_by_key(|s| (s.rank(), s.query()));
        stale
    }

    pub fn over(before: &Trace, after: &Trace) -> (Work, Vec<QueryKey>) {
        let mut work = Work::default();
        let mut needless = Vec::new();
        for i in 0..after.keys.len() as u32 {
            let Some(now) = after.result(i).filter(|_| after.ran.contains(i)) else { continue };
            work.ran += 1;
            let key = after.keys[i as usize];
            match before.index(&key).and_then(|j| Some((j, before.result(j)?))) {
                None => work.new += 1,
                Some((j, was)) if was == now && same_edges(before, j, after, i) => {
                    work.needless += 1;
                    needless.push(key);
                }
                Some((_, was)) if was == now => work.cut_off += 1,
                Some(_) => {}
            }
        }
        (work, needless)
    }

    pub fn apart<K: Ord + Clone, V: PartialEq + Clone>(first: &BTreeMap<K, V>, second: &BTreeMap<K, V>) -> Vec<Apart<K, V>> {
        let mut out = Vec::new();
        for (key, value) in first {
            match second.get(key) {
                None => out.push(Apart::OnlyFirst { key: key.clone(), value: value.clone() }),
                Some(other) if other != value => {
                    out.push(Apart::Differ { key: key.clone(), first: value.clone(), second: other.clone() })
                }
                Some(_) => {}
            }
        }
        for (key, value) in second {
            if !first.contains_key(key) {
                out.push(Apart::OnlySecond { key: key.clone(), value: value.clone() });
            }
        }
        let key_of = |a: &Apart<K, V>| match a {
            Apart::OnlyFirst { key, .. } | Apart::OnlySecond { key, .. } | Apart::Differ { key, .. } => key.clone(),
        };
        out.sort_by_key(key_of);
        out
    }
}

/// `n` keys of a few kinds, where many share their first eight bytes (one slot of the key
/// table) and some differ only in kind, so probing and the kind's mixing are both exercised.
fn pool(rng: &mut Rng, n: usize) -> Vec<QueryKey> {
    (0..n)
        .map(|i| {
            let mut input = [0u8; 32];
            input.iter_mut().for_each(|b| *b = rng.next_u64() as u8);
            if i % 3 == 0 {
                input[..8].copy_from_slice(&[0xab; 8]);
            }
            QueryKey { kind: QueryKind(0x2000 + (i % 4) as u16), input: ContentId(input) }
        })
        .collect()
}

/// Rows of a random build over `keys`: some keys inserted twice, edges to keys with and
/// without results, fingerprints from a small range so equal and different ones both occur.
fn rows(rng: &mut Rng, keys: &[QueryKey], n: usize) -> Vec<(QueryKey, Fingerprint, Vec<Edge>, bool)> {
    (0..n)
        .map(|_| {
            let key = keys[rng.below(keys.len() as u64) as usize];
            let edges = (0..rng.below(6))
                .map(|_| Edge { read: keys[rng.below(keys.len() as u64) as usize], saw: Fingerprint(u128::from(rng.below(4) as u8)) })
                .collect();
            (key, Fingerprint(u128::from(rng.below(4) as u8)), edges, rng.below(2) == 0)
        })
        .collect()
}

fn built(rows: &[(QueryKey, Fingerprint, Vec<Edge>, bool)]) -> Trace {
    let mut builder = TraceBuilder::default();
    for (key, fingerprint, edges, ran) in rows {
        builder.insert(*key, *fingerprint, edges, *ran);
    }
    builder.finish()
}

#[test]
fn the_builder_gives_the_trace_the_sorted_map_gave() {
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let (n_keys, n_rows) = (1 + rng.below(300) as usize, rng.below(500) as usize);
        let keys = pool(&mut rng, n_keys);
        let rows = rows(&mut rng, &keys, n_rows);
        assert_eq!(built(&rows), model::finish(&rows), "seed {seed}");
    }
}

#[test]
fn the_key_table_grows_past_many_keys_sharing_a_slot() {
    let mut rng = Rng::new(9);
    let keys = pool(&mut rng, 20_000);
    let rows: Vec<_> = keys.iter().map(|&k| (k, Fingerprint(1), vec![], true)).collect();
    let trace = built(&rows);
    assert_eq!(trace.keys.len(), keys.len());
    assert!(trace.keys.windows(2).all(|w| w[0] < w[1]));
    assert!(keys.iter().all(|k| trace.index(k).is_some()));
}

#[test]
fn the_joins_find_what_a_lookup_per_key_found() {
    for seed in 0..200 {
        let mut rng = Rng::new(1000 + seed);
        let (n_keys, n_before, n_after) = (1 + rng.below(200) as usize, rng.below(300) as usize, rng.below(300) as usize);
        let keys = pool(&mut rng, n_keys);
        let before_rows = rows(&mut rng, &keys, n_before);
        // Part of the build before again as it was, so kept, cut-off and needless results occur.
        let kept = rng.below(before_rows.len() as u64 + 1) as usize;
        let mut after_rows = before_rows[..kept].to_vec();
        after_rows.extend(rows(&mut rng, &keys, n_after));
        let (before, after) = (built(&before_rows), built(&after_rows));
        let roots: Vec<QueryKey> = if seed % 4 == 0 { vec![keys[0], keys[keys.len() / 2]] } else { vec![] };
        assert_eq!(under(&after, &before, &roots), model::under(&after, &before, &roots), "seed {seed}");
        assert_eq!(over(&before, &after), model::over(&before, &after), "seed {seed}");
    }
}

#[test]
fn seek_finds_the_partition_point_from_any_cursor() {
    let mut rng = Rng::new(5);
    let mut keys = pool(&mut rng, 500);
    keys.sort_unstable();
    keys.dedup();
    let probes = pool(&mut rng, 50);
    for from in [0, 1, 7, 250, keys.len() - 1, keys.len()] {
        for probe in probes.iter().chain(&keys) {
            let want = from + keys[from..].partition_point(|k| k < probe);
            assert_eq!(seek(&keys, from, probe), want);
        }
    }
}

#[test]
fn apart_by_merge_is_apart_by_lookup() {
    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let map = |rng: &mut Rng| -> BTreeMap<u16, u8> {
            (0..rng.below(60)).map(|_| (rng.below(80) as u16, rng.below(3) as u8)).collect()
        };
        let (first, second) = (map(&mut rng), map(&mut rng));
        assert_eq!(apart(&first, &second), model::apart(&first, &second), "seed {seed}");
    }
}
