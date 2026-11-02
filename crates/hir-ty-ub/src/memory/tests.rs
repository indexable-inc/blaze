use std::ops::Range;

use blaze_ids::rng::SplitMix64;

use super::{
    AlignMsg, Bytes, InAlloc, Kind, Leak, Leaks, MemError, Memory, Uninit, init_mask::InitMask,
};
use crate::ids::{AccessKind, AllocId, BorTag, Prov, Span};

fn id(n: u32) -> AllocId {
    AllocId::new(n).unwrap()
}

fn prov(alloc: AllocId, tag: u64) -> Prov {
    Prov::Concrete { alloc, tag: BorTag::new(tag) }
}

fn ptr(size: usize, p: Prov) -> Bytes {
    Bytes::pointer(size, Some(p))
}

fn err(r: Result<impl std::fmt::Debug, MemError>) -> String {
    r.unwrap_err().to_string()
}

// Miri: rustc_middle/src/mir/interpret/allocation/init_mask/tests.rs, every test.

#[test]
fn uninit_mask() {
    let mut mask = InitMask::new(500, false);
    assert!(!mask.get(499));
    mask.set_range(499..500, true);
    assert!(mask.get(499));
    mask.set_range(100..256, true);
    for i in 0..100 {
        assert!(!mask.get(i), "{i} should not be set");
    }
    for i in 100..256 {
        assert!(mask.get(i), "{i} should be set");
    }
    for i in 256..499 {
        assert!(!mask.get(i), "{i} should not be set");
    }
}

#[test]
fn materialize_mask_within_range() {
    let mut mask = InitMask::new(16, false);
    assert_eq!(mask.materialized_block_count(), 0);
    mask.set_range(8..16, true);
    assert_eq!(mask.materialized_block_count(), 1);
    for i in 0..8 {
        assert!(!mask.get(i), "{i} should not be set");
    }
    for i in 8..16 {
        assert!(mask.get(i), "{i} should be set");
    }
}

#[test]
fn grow_within_unused_bits_with_full_overwrite() {
    let mut mask = InitMask::new(16, true);
    for i in 0..16 {
        assert!(mask.get(i), "{i} should be set");
    }
    mask.set_range(0..32, true);
    for i in 0..32 {
        assert!(mask.get(i), "{i} should be set");
    }
    assert_eq!(mask.materialized_block_count(), 0);
}

#[test]
fn grow_same_state_within_unused_bits() {
    let mut mask = InitMask::new(16, true);
    for i in 0..16 {
        assert!(mask.get(i), "{i} should be set");
    }
    mask.set_range(24..32, true);
    for i in 16..24 {
        assert!(mask.get(i), "{i} should be set");
    }
    for i in 24..32 {
        assert!(mask.get(i), "{i} should be set");
    }
    assert_eq!(1, mask.chunks(0..32).count());
    assert_eq!(mask.materialized_block_count(), 0);
}

#[test]
fn grow_mixed_state_within_unused_bits() {
    let mut mask = InitMask::new(16, true);
    for i in 0..16 {
        assert!(mask.get(i), "{i} should be set");
    }
    mask.set_range(24..32, false);
    for i in 16..24 {
        assert!(!mask.get(i), "{i} should not be set");
    }
    for i in 24..32 {
        assert!(!mask.get(i), "{i} should not be set");
    }
    assert_eq!(1, mask.chunks(0..16).count());
    assert_eq!(2, mask.chunks(0..32).count());
    assert_eq!(mask.materialized_block_count(), 1);
}

#[test]
fn grow_within_unused_bits_with_overlap() {
    let mut mask = InitMask::new(16, true);
    for i in 0..16 {
        assert!(mask.get(i), "{i} should be set");
    }
    mask.set_range(8..24, false);
    for i in 8..24 {
        assert!(!mask.get(i), "{i} should not be set");
    }
    assert_eq!(1, mask.chunks(0..8).count());
    assert_eq!(2, mask.chunks(0..24).count());
    assert_eq!(mask.materialized_block_count(), 1);
}

#[test]
fn grow_mixed_state_within_unused_bits_and_full_overwrite() {
    let mut mask = InitMask::new(16, true);
    assert!(mask.is_range_initialized(0..16).is_ok());
    mask.set_range(8..24, false);
    assert!(mask.is_range_initialized(8..24).is_err());
    assert_eq!(mask.materialized_block_count(), 1);
    mask.set_range(0..32, true);
    assert!(mask.is_range_initialized(0..32).is_ok());
    assert_eq!(1, mask.chunks(0..32).count());
    assert_eq!(mask.materialized_block_count(), 0);
}

#[test]
fn grow_same_state_outside_capacity() {
    let mut mask = InitMask::new(16, true);
    for i in 0..16 {
        assert!(mask.get(i), "{i} should be set");
    }
    assert_eq!(mask.materialized_block_count(), 0);
    mask.set_range(24..640, true);
    assert_eq!(1, mask.chunks(0..640).count());
    assert_eq!(mask.materialized_block_count(), 0);
}

/// The mask against a `Vec<bool>` under random writes, copies and bit blits: states, the first
/// uninitialized run, chunks, and the bits `Bytes` reads.
#[test]
fn init_mask_matches_bool_model() {
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for round in 0..200 {
        let len = 1 + rng.below(300);
        let start = round % 2 == 0;
        let mut mask = InitMask::new(len, start);
        let mut model = vec![start; len as usize];
        for _ in 0..40 {
            let a = rng.below(len);
            let b = a + 1 + rng.below(len - a);
            match rng.below(3) {
                0 => {
                    let state = rng.below(2) == 0;
                    mask.set_range(a..b, state);
                    model[a as usize..b as usize].fill(state);
                }
                1 => {
                    let n = b - a;
                    let dst = rng.below(len - n + 1);
                    let mut ranges = Vec::new();
                    let initial = mask.prepare_copy(a..b, &mut ranges);
                    mask.apply_copy(initial, &ranges, dst, n, 1);
                    let src: Vec<bool> = model[a as usize..b as usize].to_vec();
                    model[dst as usize..(dst + n) as usize].copy_from_slice(&src);
                }
                _ => {
                    let n = b - a;
                    let bits: Vec<u64> = (0..n.div_ceil(64)).map(|_| rng.next_u64()).collect();
                    mask.set_bits(a, &bits, n);
                    for k in 0..n {
                        model[(a + k) as usize] = bits[(k / 64) as usize] & (1 << (k % 64)) != 0;
                    }
                }
            }
            for (i, &state) in model.iter().enumerate() {
                assert_eq!(mask.get(i as u64), state, "byte {i}");
            }
            let a = rng.below(len);
            let b = a + rng.below(len - a + 1);
            let first_uninit = (a..b).find(|&i| !model[i as usize]);
            match mask.is_range_initialized(a..b) {
                Ok(()) => assert_eq!(first_uninit, None),
                Err(bad) => {
                    assert_eq!(Some(bad.start), first_uninit);
                    let end = (bad.start..b).find(|&i| model[i as usize]).unwrap_or(b);
                    assert_eq!(bad.end, end);
                }
            }
            let mut pos = a;
            let mut last = None;
            for chunk in mask.chunks(a..b) {
                let r = chunk.range();
                assert_eq!(r.start, pos);
                assert!(r.start < r.end);
                assert_ne!(last, Some(chunk.is_init()));
                assert!((r.start..r.end).all(|i| model[i as usize] == chunk.is_init()));
                last = Some(chunk.is_init());
                pos = r.end;
            }
            assert_eq!(pos, b);
            let mut out = vec![0; (b - a).div_ceil(64) as usize];
            mask.bits(a..b, &mut out);
            for k in 0..b - a {
                assert_eq!(out[(k / 64) as usize] & (1 << (k % 64)) != 0, model[(a + k) as usize]);
            }
        }
    }
}

// The reference model of provenance: one cell per byte, as Miri's `ProvenanceMap` means it.

const PS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    Empty,
    Whole { prov: Prov, start: usize },
    Frag { prov: Prov, idx: u8 },
}

fn model_clear(cells: &mut [Cell], range: Range<usize>) {
    for i in 0..cells.len() {
        match cells[i] {
            Cell::Whole { prov, start } if start < range.end && start + PS > range.start => {
                cells[i] = if range.contains(&i) {
                    Cell::Empty
                } else {
                    Cell::Frag { prov, idx: (i - start) as u8 }
                };
            }
            _ if range.contains(&i) => cells[i] = Cell::Empty,
            _ => {}
        }
    }
}

fn model_slice(cells: &[Cell], range: Range<usize>) -> Vec<Cell> {
    range
        .clone()
        .map(|i| match cells[i] {
            Cell::Whole { prov, start } if start >= range.start && start + PS <= range.end => {
                Cell::Whole { prov, start: start - range.start }
            }
            Cell::Whole { prov, start } => Cell::Frag { prov, idx: (i - start) as u8 },
            other => other,
        })
        .collect()
}

fn model_put(cells: &mut [Cell], at: usize, snap: &[Cell]) {
    model_clear(cells, at..at + snap.len());
    for (k, cell) in snap.iter().enumerate() {
        cells[at + k] = match *cell {
            Cell::Whole { prov, start } => Cell::Whole { prov, start: start + at },
            other => other,
        };
    }
}

fn model_read_ptr(cells: &[Cell], at: usize) -> Option<Prov> {
    if at + PS > cells.len() {
        return None;
    }
    if let Cell::Whole { prov, start } = cells[at]
        && start == at
    {
        return Some(prov);
    }
    let mut expected = None;
    for k in 0..PS {
        match cells[at + k] {
            Cell::Empty | Cell::Whole { .. } => return None,
            Cell::Frag { prov: Prov::Wildcard, .. } => {}
            Cell::Frag { prov, idx } => {
                if usize::from(idx) != k {
                    return None;
                }
                match expected {
                    Some(e) if e != prov => return None,
                    _ => expected = Some(prov),
                }
            }
        }
    }
    Some(expected.unwrap_or(Prov::Wildcard))
}

/// `Memory` (Miri's sorted maps of pointers and fragments, and the run-length init mask) against
/// the per-byte model, under random pointer and integer writes, padding resets, copies within and
/// across allocations (overlapping too), moves through `Bytes`, and retags.
#[test]
fn memory_matches_byte_model() {
    const N: usize = 72;
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for _ in 0..300 {
        let mut mem = Memory::new();
        let a = mem.alloc(0x1000, N as u64, 8, Kind::Rust, false).unwrap();
        let b = mem.alloc(0x2000, N as u64, 8, Kind::Rust, false).unwrap();
        let allocs = [a, b];
        let provs = [prov(a, 1), prov(b, 2), prov(a, 3), Prov::Wildcard];
        let mut cells = [vec![Cell::Empty; N], vec![Cell::Empty; N]];
        let mut init = [vec![false; N], vec![false; N]];
        for _ in 0..60 {
            let w = rng.below(2) as usize;
            match rng.below(6) {
                0 => {
                    let at = rng.below((N - PS + 1) as u64) as usize;
                    let p = provs[rng.below(4) as usize];
                    mem.write(allocs[w], at as u64, &ptr(PS, p));
                    model_put(&mut cells[w], at, &[Cell::Whole { prov: p, start: 0 }; PS]);
                    init[w][at..at + PS].fill(true);
                }
                1 => {
                    let at = rng.below(N as u64) as usize;
                    let len = 1 + rng.below((N - at) as u64) as usize;
                    mem.write(allocs[w], at as u64, &Bytes::init(len));
                    model_clear(&mut cells[w], at..at + len);
                    init[w][at..at + len].fill(true);
                }
                2 => {
                    let at = rng.below(N as u64) as usize;
                    let len = 1 + rng.below((N - at) as u64) as usize;
                    mem.uninit(allocs[w], Span::new(at as u64, len as u64));
                    model_clear(&mut cells[w], at..at + len);
                    init[w][at..at + len].fill(false);
                }
                3 | 4 => {
                    let s = rng.below(2) as usize;
                    let len = 1 + rng.below(N as u64 / 2) as usize;
                    let src = rng.below((N - len + 1) as u64) as usize;
                    let dst = rng.below((N - len + 1) as u64) as usize;
                    let snap = model_slice(&cells[s], src..src + len);
                    let snap_init: Vec<bool> = init[s][src..src + len].to_vec();
                    if rng.below(3) == 0 {
                        let bytes = mem.read(allocs[s], Span::new(src as u64, len as u64));
                        for k in 0..(len + 1).saturating_sub(PS) {
                            assert_eq!(bytes.prov_at(k, PS), model_read_ptr(&snap, k));
                        }
                        mem.write(allocs[w], dst as u64, &bytes);
                        model_put(&mut cells[w], dst, &snap);
                    } else {
                        mem.copy((allocs[w], dst as u64), (allocs[s], src as u64), len as u64);
                        if snap_init.iter().all(|i| !i) {
                            model_clear(&mut cells[w], dst..dst + len);
                        } else {
                            model_put(&mut cells[w], dst, &snap);
                        }
                    }
                    init[w][dst..dst + len].copy_from_slice(&snap_init);
                }
                _ => {
                    let at = rng.below((N - PS + 1) as u64) as usize;
                    let p = provs[rng.below(3) as usize];
                    mem.set_prov(allocs[w], at as u64, PS as u64, p);
                    model_put(&mut cells[w], at, &[Cell::Whole { prov: p, start: 0 }; PS]);
                    init[w][at..at + PS].fill(true);
                }
            }
            for x in 0..2 {
                for at in 0..=N - PS {
                    assert_eq!(
                        mem.prov_at(allocs[x], at as u64),
                        model_read_ptr(&cells[x], at),
                        "alloc {x} at {at}: {:?}",
                        &cells[x]
                    );
                }
                for (i, &state) in init[x].iter().enumerate() {
                    assert_eq!(mem.require_init(allocs[x], Span::new(i as u64, 1)).is_ok(), state);
                }
                let read = mem.read(allocs[x], Span::new(0, N as u64));
                for (i, &state) in init[x].iter().enumerate() {
                    assert_eq!(read.is_init(i), state);
                }
            }
        }
    }
}

/// `Bytes` against the per-byte model under `put`, `slice`, `clear_prov` and `set_uninit`.
#[test]
fn bytes_match_byte_model() {
    let mut rng = SplitMix64::new(0xdead_beef_cafe_f00d);
    let a = id(1);
    let provs = [prov(a, 1), prov(a, 2), Prov::Wildcard];
    for _ in 0..500 {
        let mut bytes = Bytes::init(0);
        let mut cells: Vec<Cell> = Vec::new();
        let mut init: Vec<bool> = Vec::new();
        for _ in 0..30 {
            let other_len;
            let (other, other_cells, other_init) = match rng.below(4) {
                0 => {
                    let p = provs[rng.below(3) as usize];
                    other_len = PS;
                    (ptr(PS, p), vec![Cell::Whole { prov: p, start: 0 }; PS], vec![true; PS])
                }
                1 => {
                    other_len = 1 + rng.below(80) as usize;
                    (Bytes::init(other_len), vec![Cell::Empty; other_len], vec![true; other_len])
                }
                2 => {
                    other_len = 1 + rng.below(80) as usize;
                    (Bytes::uninit(other_len), vec![Cell::Empty; other_len], vec![false; other_len])
                }
                _ if !cells.is_empty() => {
                    let s = rng.below(cells.len() as u64) as usize;
                    let e = s + 1 + rng.below((cells.len() - s) as u64) as usize;
                    other_len = e - s;
                    (bytes.slice(s..e), model_slice(&cells, s..e), init[s..e].to_vec())
                }
                _ => {
                    other_len = 3;
                    (Bytes::init(3), vec![Cell::Empty; 3], vec![true; 3])
                }
            };
            assert_eq!(other.len(), other_len);
            let at = rng.below(cells.len() as u64 + 4) as usize;
            bytes.put(at, &other);
            if at + other_len > cells.len() {
                cells.resize(at + other_len, Cell::Empty);
                init.resize(at + other_len, false);
            }
            model_put(&mut cells, at, &other_cells);
            init[at..at + other_len].copy_from_slice(&other_init);
            if rng.below(4) == 0 && !cells.is_empty() {
                let s = rng.below(cells.len() as u64) as usize;
                let e = s + rng.below((cells.len() - s) as u64 + 1) as usize;
                if rng.below(2) == 0 {
                    bytes.clear_prov(s..e);
                } else {
                    bytes.set_uninit(s..e);
                    init[s..e].fill(false);
                }
                model_clear(&mut cells, s..e);
            }
            assert_eq!(bytes.len(), cells.len());
            for (i, &state) in init.iter().enumerate() {
                assert_eq!(bytes.is_init(i), state, "byte {i}");
            }
            for at in 0..=cells.len().saturating_sub(PS) {
                assert_eq!(bytes.prov_at(at, PS), model_read_ptr(&cells, at), "at {at}");
            }
            let first_uninit = init.iter().position(|i| !i);
            assert_eq!(bytes.require_init(0..cells.len()).is_ok(), first_uninit.is_none());
        }
    }
}

// Bytes.

#[test]
fn small_values_stay_inline() {
    let p = ptr(8, prov(id(1), 1));
    assert!(!p.init.spilled() && !p.prov.spilled());
    let mut pair = Bytes::init(16);
    pair.put(0, &p);
    assert!(!pair.init.spilled() && !pair.prov.spilled());
    let scalar = Bytes::init(16);
    assert!(scalar.init.is_empty());
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, true).unwrap();
    mem.write(a, 0, &p);
    let read = mem.read(a, Span::new(0, 16));
    assert!(read.init.is_empty() && !read.prov.spilled());
    assert_eq!(read.prov_at(0, 8), Some(prov(id(1), 1)));
}

#[test]
fn bytes_put_grows_and_joins_fragments() {
    let p = prov(id(1), 1);
    let whole = ptr(8, p);
    let mut b = Bytes::init(0);
    b.put(0, &whole.slice(0..3));
    b.put(3, &whole.slice(3..8));
    assert_eq!(b.len(), 8);
    assert_eq!(b.prov.len(), 1);
    assert_eq!(b.prov_at(0, 8), Some(p));
    let mut gap = Bytes::init(2);
    gap.put(4, &Bytes::init(2));
    assert!(gap.is_init(1) && !gap.is_init(2) && !gap.is_init(3) && gap.is_init(4));
    assert_eq!(gap.require_init(0..6), Err(MemError::Uninit(None)));
}

#[test]
fn bytes_require_init_names_memory() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 4, 4, Kind::Rust, false).unwrap();
    mem.write(a, 0, &Bytes::init(2));
    let read = mem.read(a, Span::new(0, 4));
    let bad = Uninit { alloc: a, access: Span::new(0, 4), bad: Span::new(2, 2) };
    assert_eq!(read.require_init(0..4), Err(MemError::Uninit(Some(bad))));
    assert_eq!(
        err(read.require_init(0..4)),
        "reading memory at alloc1[0x0..0x4], but memory is uninitialized at [0x2..0x4], and \
         this operation requires initialized memory"
    );
    let tail = read.slice(1..4);
    let bad = Uninit { alloc: a, access: Span::new(1, 3), bad: Span::new(2, 2) };
    assert_eq!(tail.require_init(0..3), Err(MemError::Uninit(Some(bad))));
    assert_eq!(mem.require_init(a, Span::new(0, 4)), read.require_init(0..4));
    let mut joined = read.slice(0..2);
    joined.put(2, &read.slice(2..4));
    assert_eq!(joined.require_init(0..4), read.require_init(0..4));
    let mut mixed = Bytes::init(4);
    mixed.put(2, &Bytes::uninit(2));
    assert_eq!(
        err(mixed.require_init(0..4)),
        "using uninitialized data, but this operation requires initialized memory"
    );
    assert!(Bytes::init(4).require_init(0..4).is_ok());
}

// Miri: tests/pass/provenance.rs and tests/fail/provenance/, as the memory operations their
// programs come down to.

#[test]
fn bytewise_custom_memcpy() {
    let mut mem = Memory::new();
    let from = mem.alloc(0x1000, 8, 8, Kind::Stack, false).unwrap();
    let to = mem.alloc(0x2000, 8, 8, Kind::Stack, false).unwrap();
    let p = prov(id(9), 4);
    mem.write(from, 0, &ptr(8, p));
    for i in 0..8 {
        let b = mem.read(from, Span::new(i, 1));
        mem.write(to, i, &b);
    }
    assert_eq!(mem.prov_at(to, 0), Some(p));
}

#[test]
fn bytewise_ptr_swap() {
    let mut mem = Memory::new();
    let x = mem.alloc(0x1000, 8, 8, Kind::Stack, false).unwrap();
    let y = mem.alloc(0x2000, 8, 8, Kind::Stack, false).unwrap();
    let (p1, p2) = (prov(id(7), 1), prov(id(8), 2));
    mem.write(x, 0, &ptr(8, p1));
    mem.write(y, 0, &ptr(8, p2));
    for i in 0..8 {
        let bx = mem.read(x, Span::new(i, 1));
        let by = mem.read(y, Span::new(i, 1));
        mem.write(x, i, &by);
        mem.write(y, i, &bx);
    }
    assert_eq!(mem.prov_at(x, 0), Some(p2));
    assert_eq!(mem.prov_at(y, 0), Some(p1));
}

#[test]
fn bytewise_custom_memcpy_chunked() {
    let mut mem = Memory::new();
    let data = mem.alloc(0x1000, 128, 8, Kind::Stack, true).unwrap();
    let data2 = mem.alloc(0x2000, 128, 8, Kind::Stack, true).unwrap();
    let p = prov(id(9), 4);
    let offsets: Vec<u64> = (0..8).map(|i| i * 16 + i).collect();
    for &o in &offsets {
        mem.write(data, o, &ptr(8, p));
    }
    for k in 0..16 {
        let chunk = mem.read(data, Span::new(k * 8, 8));
        mem.write(data2, k * 8, &chunk);
    }
    for &o in &offsets {
        assert_eq!(mem.prov_at(data2, o), Some(p), "offset {o}");
    }
}

#[test]
fn maybe_uninit_preserves_partial_provenance() {
    let mut mem = Memory::new();
    let bytes = mem.alloc(0x1000, 16, 1, Kind::Stack, false).unwrap();
    let copy = mem.alloc(0x2000, 16, 8, Kind::Stack, false).unwrap();
    let p = prov(id(9), 4);
    mem.write(bytes, 0, &Bytes::init(16));
    mem.write(bytes, 4, &ptr(8, p));
    let mut value = mem.read(bytes, Span::new(0, 8));
    value.put(8, &mem.read(bytes, Span::new(8, 8)));
    assert_eq!(value.prov_at(4, 8), Some(p));
    mem.write(copy, 0, &value);
    assert_eq!(mem.prov_at(copy, 4), Some(p));
}

#[test]
fn mix_ptrs_has_no_provenance() {
    let mut mem = Memory::new();
    let target = mem.alloc(0x1000, 24, 8, Kind::Stack, true).unwrap();
    let p = prov(id(9), 4);
    let mut arr = ptr(8, p);
    arr.put(8, &ptr(8, p));
    mem.write(target, 4, &arr);
    assert_eq!(mem.prov_at(target, 8), None);
    assert_eq!(mem.prov_at(target, 4), Some(p));
    assert_eq!(mem.prov_at(target, 12), Some(p));
}

#[test]
fn pointer_partial_overwrite() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 8, 8, Kind::Stack, false).unwrap();
    let p = prov(id(9), 4);
    mem.write(a, 0, &ptr(8, p));
    mem.write(a, 0, &Bytes::init(1));
    assert_eq!(mem.prov_at(a, 0), None);
    assert!(mem.has_prov(a, Span::new(1, 7)));
}

#[test]
fn copy_with_partial_pointers_at_both_ends() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 32, 8, Kind::Rust, true).unwrap();
    let b = mem.alloc(0x2000, 32, 8, Kind::Rust, true).unwrap();
    let p = prov(a, 1);
    mem.write(a, 4, &ptr(8, p));
    mem.copy((b, 0), (a, 0), 8);
    mem.copy((b, 8), (a, 8), 8);
    assert_eq!(mem.prov_at(b, 4), Some(p));
    mem.copy((b, 20), (a, 6), 4);
    assert_eq!(mem.prov_at(b, 18), None);
    assert!(mem.has_prov(b, Span::new(20, 4)));
    mem.copy((b, 16), (a, 16), 8);
    assert!(!mem.has_prov(b, Span::new(16, 8)));
}

#[test]
fn overlapping_copy_moves_pointers() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 32, 8, Kind::Rust, true).unwrap();
    let p = prov(a, 1);
    mem.write(a, 0, &ptr(8, p));
    mem.write(a, 8, &ptr(8, prov(a, 2)));
    assert_eq!(mem.check_nonoverlapping((a, 4), (a, 0), 16), Err(MemError::CopyOverlapping));
    assert_eq!(
        err(mem.check_nonoverlapping((a, 4), (a, 0), 16)),
        "`copy_nonoverlapping` called on overlapping ranges"
    );
    mem.copy((a, 4), (a, 0), 16);
    assert_eq!(mem.prov_at(a, 4), Some(p));
    assert_eq!(mem.prov_at(a, 12), Some(prov(a, 2)));
    assert_eq!(mem.prov_at(a, 0), None);
    mem.copy((a, 0), (a, 4), 16);
    assert_eq!(mem.prov_at(a, 0), Some(p));
    assert_eq!(mem.prov_at(a, 8), Some(prov(a, 2)));
    assert!(mem.check_nonoverlapping((a, 16), (a, 0), 16).is_ok());
    let u = mem.alloc(0x2000, 32, 8, Kind::Rust, false).unwrap();
    assert!(mem.check_nonoverlapping((u, 4), (u, 0), 16).is_ok());
}

#[test]
fn copy_of_uninit_drops_provenance() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, false).unwrap();
    let b = mem.alloc(0x2000, 16, 8, Kind::Rust, false).unwrap();
    mem.write(b, 0, &ptr(8, prov(b, 1)));
    mem.copy((b, 0), (a, 0), 8);
    assert_eq!(mem.prov_at(b, 0), None);
    assert!(mem.require_init(b, Span::new(0, 1)).is_err());
    mem.write(a, 0, &Bytes::init(2));
    mem.copy((b, 0), (a, 0), 4);
    assert!(mem.require_init(b, Span::new(0, 2)).is_ok());
    assert!(mem.require_init(b, Span::new(2, 1)).is_err());
}

#[test]
fn uninit_resets_padding() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, true).unwrap();
    mem.write(a, 0, &ptr(8, prov(a, 1)));
    mem.uninit(a, Span::new(4, 8));
    assert!(mem.require_init(a, Span::new(0, 4)).is_ok());
    assert!(mem.require_init(a, Span::new(4, 1)).is_err());
    assert!(mem.require_init(a, Span::new(12, 4)).is_ok());
    assert_eq!(mem.prov_at(a, 0), None);
    assert!(mem.has_prov(a, Span::new(0, 4)));
    mem.clear_prov(a, Span::new(0, 16));
    assert!(!mem.holds_pointers(a));
}

#[test]
fn retag_rewrites_tag_in_place() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, true).unwrap();
    mem.write(a, 8, &ptr(8, prov(a, 1)));
    mem.set_prov(a, 8, 8, prov(a, 7));
    assert_eq!(mem.prov_at(a, 8), Some(prov(a, 7)));
    let mut tags = Vec::new();
    mem.tags(&mut |alloc, tag| tags.push((alloc, tag)));
    assert_eq!(tags, [(a, BorTag::new(7))]);
}

// Every error, with Miri's wording (its tests/fail/*.stderr).

#[test]
fn out_of_bounds() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 4, 4, Kind::Rust, false).unwrap();
    let pa = Some(prov(a, 1));
    assert_eq!(
        err(mem.access(0x1000, pa, 8, 4, AccessKind::Read)),
        "memory access failed: attempting to access 8 bytes, but got alloc1 which is only 4 \
         bytes from the end of the allocation"
    );
    assert_eq!(
        err(mem.access(0x1005, pa, 2, 1, AccessKind::Read)),
        "memory access failed: attempting to access 2 bytes, but got alloc1+0x5 which is at or \
         beyond the end of the allocation of size 4 bytes"
    );
    assert_eq!(
        err(mem.access(0x1000 - 5, pa, 2, 1, AccessKind::Read)),
        "memory access failed: attempting to access 2 bytes, but got alloc1-0x5 which points to \
         before the beginning of the allocation"
    );
    assert!(mem.access(0x1003, pa, 1, 1, AccessKind::Write).is_ok());
    assert_eq!(
        err(mem.access(0x1003, pa, 2, 1, AccessKind::Write)),
        "memory access failed: attempting to access 2 bytes, but got alloc1+0x3 which is only 1 \
         byte from the end of the allocation"
    );
    assert_eq!(mem.access(0x1000, pa, 4, 4, AccessKind::Read), Ok(Some((a, 0))));
    assert_eq!(mem.access(0x1002, pa, 2, 2, AccessKind::Write), Ok(Some((a, 2))));
    let one = mem.alloc(0x2000, 1, 1, Kind::Rust, false).unwrap();
    assert_eq!(
        err(mem.access(0x2001, Some(prov(one, 2)), 1, 1, AccessKind::Read)),
        "memory access failed: attempting to access 1 byte, but got alloc2+0x1 which is at or \
         beyond the end of the allocation of size 1 byte"
    );
}

#[test]
fn out_of_bounds_on_a_32_bit_target() {
    let mut mem = Memory::with_pointer_size(4);
    let a = mem.alloc(0x10, 4, 4, Kind::Rust, false).unwrap();
    assert_eq!(
        err(mem.access(0x8, Some(prov(a, 1)), 4, 4, AccessKind::Read)),
        "memory access failed: attempting to access 4 bytes, but got alloc1-0x8 which points to \
         before the beginning of the allocation"
    );
}

#[test]
fn use_after_free() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 4, 4, Kind::Rust, false).unwrap();
    let pa = Some(prov(a, 1));
    assert_eq!(mem.free((0x1000, pa), Some(4), Some(4), Kind::Rust), Ok(a));
    assert!(!mem.info(a).live);
    assert_eq!(
        err(mem.access(0x1000, pa, 4, 4, AccessKind::Read)),
        "memory access failed: alloc1 has been freed, so this pointer is dangling"
    );
    assert_eq!(
        err(mem.offset_inbounds(0x1000, pa, 1)),
        "in-bounds pointer arithmetic failed: alloc1 has been freed, so this pointer is dangling"
    );
    assert_eq!(
        err(mem.free((0x1000, pa), None, None, Kind::Rust)),
        "memory access failed: alloc1 has been freed, so this pointer is dangling"
    );
    assert_eq!(mem.access(0x1000, pa, 0, 4, AccessKind::Read), Ok(None));
    let b = mem.alloc(0x1000, 4, 4, Kind::Rust, false).unwrap();
    assert_eq!(mem.live_at(0x1002), Some(b));
    assert!(mem.access(0x1000, pa, 4, 4, AccessKind::Read).is_err());
    assert_eq!(mem.access(0x1000, Some(prov(b, 2)), 4, 4, AccessKind::Read), Ok(Some((b, 0))));
}

#[test]
fn dangling_integer_pointers() {
    let mem = Memory::new();
    assert_eq!(
        err(mem.access(0, None, 4, 4, AccessKind::Read)),
        "memory access failed: attempting to access 4 bytes, but got null pointer"
    );
    assert_eq!(
        err(mem.access(0x10, None, 4, 4, AccessKind::Write)),
        "memory access failed: attempting to access 4 bytes, but got 0x10[noalloc] which is a \
         dangling pointer (it has no provenance)"
    );
    assert_eq!(
        err(mem.offset_inbounds(0x10, None, 1)),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by 1 byte, but got \
         0x10[noalloc] which is a dangling pointer (it has no provenance)"
    );
    assert_eq!(mem.access(0x10, None, 0, 1, AccessKind::Read), Ok(None));
    assert_eq!(mem.offset_inbounds(0, None, 0), Ok(()));
}

#[test]
fn alignment() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 8, 4, Kind::Rust, false).unwrap();
    let pa = Some(prov(a, 1));
    assert_eq!(
        err(mem.access(0x1001, pa, 2, 2, AccessKind::Read)),
        "accessing memory based on pointer with alignment 1, but alignment 2 is required"
    );
    assert_eq!(
        err(mem.access_as(0x1002, pa, 4, 4, AccessKind::Read, AlignMsg::AccessedPtr)),
        "accessing memory with alignment 2, but alignment 4 is required"
    );
    assert_eq!(
        mem.access(0x1001, None, 0, 4, AccessKind::Read),
        Err(MemError::Misaligned { has: 1, required: 4, msg: AlignMsg::BasedOn })
    );
    assert_eq!(
        err(mem.access(0x1007, pa, 2, 2, AccessKind::Read)),
        "memory access failed: attempting to access 2 bytes, but got alloc1+0x7 which is only 1 \
         byte from the end of the allocation",
        "bounds are checked before alignment"
    );
    assert!(mem.access(0x1001, pa, 1, 1, AccessKind::Read).is_ok());
    assert!(mem.check_align(0x1008, 8, AlignMsg::BasedOn).is_ok());
}

#[test]
fn read_only_memory() {
    let mut mem = Memory::new();
    let g = mem.alloc(0x1000, 4, 4, Kind::Global, true).unwrap();
    mem.set_immutable(g);
    let pg = Some(prov(g, 1));
    assert!(mem.access(0x1000, pg, 4, 4, AccessKind::Read).is_ok());
    assert_eq!(
        err(mem.access(0x1000, pg, 4, 4, AccessKind::Write)),
        "writing to alloc1 which is read-only"
    );
    assert_eq!(mem.access(0x1000, pg, 0, 4, AccessKind::Write), Ok(None));
    let s = mem.alloc(0x2000, 4, 4, Kind::Global, true).unwrap();
    assert!(mem.access(0x2000, Some(prov(s, 2)), 4, 4, AccessKind::Write).is_ok());
}

#[test]
fn free_errors() {
    let mut mem = Memory::new();
    let s = mem.alloc(0x1000, 1, 1, Kind::Stack, false).unwrap();
    let r = mem.alloc(0x2000, 1, 1, Kind::Rust, false).unwrap();
    let g = mem.alloc(0x3000, 4, 4, Kind::Global, true).unwrap();
    assert_eq!(
        err(mem.free((0x1000, Some(prov(s, 1))), Some(1), Some(1), Kind::Rust)),
        "deallocating alloc1, which is stack variable memory, using Rust heap deallocation \
         operation"
    );
    assert_eq!(
        err(mem.free((0x2000, Some(prov(r, 2))), None, None, Kind::C)),
        "deallocating alloc2, which is Rust heap memory, using C heap deallocation operation"
    );
    assert_eq!(
        err(mem.free((0x2000, Some(prov(r, 2))), Some(2), Some(1), Kind::Rust)),
        "incorrect layout on deallocation: alloc2 has size 1 and alignment 1, but gave size 2 \
         and alignment 1"
    );
    assert_eq!(
        err(mem.free((0x2000, Some(prov(r, 2))), Some(1), Some(2), Kind::Rust)),
        "incorrect layout on deallocation: alloc2 has size 1 and alignment 1, but gave size 1 \
         and alignment 2"
    );
    assert_eq!(
        err(mem.free((0x2001, Some(prov(r, 2))), None, None, Kind::Rust)),
        "deallocating 0x2001[alloc2]<2> which does not point to the beginning of an object"
    );
    assert_eq!(
        err(mem.free((0x3000, Some(prov(g, 3))), None, None, Kind::Rust)),
        "deallocating alloc3, which is static memory"
    );
    assert_eq!(
        err(mem.free((0, None), None, None, Kind::C)),
        "pointer not dereferenceable: pointer must point to some allocation, but got null pointer"
    );
    assert_eq!(
        err(mem.free((0x2000, None), None, None, Kind::Rust)),
        "pointer not dereferenceable: pointer must point to some allocation, but got \
         0x2000[noalloc] which is a dangling pointer (it has no provenance)"
    );
    assert_eq!(
        err(mem.free((0x2000, Some(Prov::Wildcard)), None, None, Kind::Rust)),
        "pointer not dereferenceable: pointer must point to some allocation, but got \
         0x2000[noalloc] which is a dangling pointer (it has no provenance)"
    );
    assert_eq!(mem.free((0x2000, Some(prov(r, 2))), Some(1), Some(1), Kind::Rust), Ok(r));
    assert_eq!(mem.free((0x1000, Some(prov(s, 1))), None, None, Kind::Stack), Ok(s));
    assert_eq!(mem.live_at(0x2000), None);
}

#[test]
fn munmap_frees_only_mmap_memory_and_neither_leaks() {
    let mut mem = Memory::new();
    let errno = mem.alloc(0x1000, 4, 4, Kind::Machine, true).unwrap();
    let mapped = mem.alloc(0x4000, 0x1000, 0x1000, Kind::Mmap, true).unwrap();
    let kept = mem.alloc(0x8000, 0x1000, 0x1000, Kind::Mmap, true).unwrap();
    assert_eq!(
        err(mem.free((0x1000, Some(prov(errno, 1))), None, None, Kind::Mmap)),
        "deallocating alloc1, which is machine-managed memory memory, using mmap deallocation \
         operation"
    );
    assert_eq!(
        err(mem.free((0x4000, Some(prov(mapped, 2))), None, None, Kind::Machine)),
        "deallocating alloc2, which is mmap memory, using machine-managed memory deallocation \
         operation"
    );
    assert_eq!(mem.free((0x4000, Some(prov(mapped, 2))), None, None, Kind::Mmap), Ok(mapped));
    assert_eq!(mem.free((0x1000, Some(prov(errno, 1))), None, None, Kind::Machine), Ok(errno));
    assert_eq!(mem.live_at(0x8000), Some(kept));
    assert_eq!(mem.leaks(), Ok(()));
}

#[test]
fn const_heap_made_global() {
    let mut mem = Memory::new();
    let h = mem.alloc(0x1000, 8, 8, Kind::ConstHeap, false).unwrap();
    let s = mem.alloc(0x2000, 8, 8, Kind::Stack, false).unwrap();
    let ph = Some(prov(h, 1));
    assert_eq!(
        err(mem.make_global((0x1004, ph))),
        "making 0x1004[alloc1]<1> global which does not point to the beginning of an object"
    );
    assert_eq!(
        err(mem.make_global((0x2000, Some(prov(s, 2))))),
        "pointer passed to `const_make_global` does not point to a heap allocation: \
         0x2000[alloc2]<2>"
    );
    assert_eq!(mem.make_global((0x1000, ph)), Ok(h));
    assert_eq!(
        err(mem.make_global((0x1000, ph))),
        "attempting to call `const_make_global` twice on the same allocation alloc1"
    );
    assert_eq!(err(mem.access(0x1000, ph, 8, 8, AccessKind::Write)), "writing to alloc1 which is read-only");
    assert_eq!(
        err(mem.free((0x1000, ph), None, None, Kind::ConstHeap)),
        "deallocating immutable allocation alloc1"
    );
    let d = mem.alloc(0x3000, 8, 8, Kind::ConstHeap, false).unwrap();
    mem.free((0x3000, Some(prov(d, 3))), None, None, Kind::ConstHeap).unwrap();
    assert_eq!(
        err(mem.make_global((0x3000, Some(prov(d, 3))))),
        "pointer passed to `const_make_global` is dangling: 0x3000[alloc3]<3>"
    );
    mem.free((0x2000, Some(prov(s, 2))), None, None, Kind::Stack).unwrap();
    assert_eq!(mem.leaks(), Ok(()), "const heap memory made global may stay");
}

#[test]
fn offset_inbounds() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 4, 4, Kind::Rust, false).unwrap();
    let pa = Some(prov(a, 1));
    assert_eq!(mem.offset_inbounds(0x1000, pa, 4), Ok(()));
    assert_eq!(mem.offset_inbounds(0x1004, pa, -4), Ok(()));
    assert_eq!(
        err(mem.offset_inbounds(0x1000, pa, 8)),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by 8 bytes, but got \
         alloc1 which is only 4 bytes from the end of the allocation"
    );
    assert_eq!(
        err(mem.offset_inbounds(0x1000, pa, -1)),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by -1 bytes, but got \
         alloc1 which is at the beginning of the allocation"
    );
    assert_eq!(
        err(mem.offset_inbounds(0x1002, pa, -3)),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by -3 bytes, but got \
         alloc1+0x2 which is only 2 bytes from the beginning of the allocation"
    );
    assert_eq!(
        err(mem.offset_inbounds(0x0fff, pa, 1)),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by 1 byte, but got \
         alloc1-0x1 which points to before the beginning of the allocation"
    );
}

#[test]
fn internal_and_allocation_errors() {
    let mut mem = Memory::new();
    assert_eq!(
        err(mem.alloc(0x1000, 4, 3, Kind::Rust, false)),
        "creating allocation with non-power-of-two alignment 3"
    );
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, false).unwrap();
    assert_eq!(
        mem.alloc(0x1008, 8, 8, Kind::Rust, false),
        Err(MemError::Overlap { addr: 0x1008, size: 8, with: a, base: 0x1000, with_size: 16 })
    );
    assert_eq!(
        err(mem.alloc(0x0ff8, 16, 8, Kind::Rust, false)),
        "internal error: the evaluator allocated 16 bytes at 0xff8, over live alloc1 of 16 bytes \
         at 0x1000"
    );
    assert!(matches!(
        mem.alloc(u64::MAX - 1, 4, 1, Kind::Rust, false),
        Err(MemError::AddressOverflow { .. })
    ));
    let below = mem.alloc(0x0800, 16, 8, Kind::Rust, false).unwrap();
    let after = mem.alloc(0x1010, 16, 8, Kind::Rust, false).unwrap();
    let zst = mem.alloc(0x1004, 0, 4, Kind::Stack, false).unwrap();
    assert_eq!(mem.live_at(0x0805), Some(below));
    assert_eq!(mem.live_at(0x1004), Some(a));
    assert_eq!(mem.live_at(0x1010), Some(after));
    assert_eq!(mem.live_at(0x101f), Some(after));
    assert_eq!(mem.live_at(0x1020), None);
    assert_eq!(mem.info(zst).size, 0);
    assert_eq!(mem.count(), 4);
}

// Exposure and wildcard pointers. Miri: alloc_addresses/mod.rs; tests/pass/ptr_int_casts.rs,
// tests/fail/provenance/ptr_int_unexposed.rs.

#[test]
fn wildcard_resolves_to_exposed_allocation() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 16, 8, Kind::Rust, false).unwrap();
    let b = mem.alloc(0x2000, 16, 8, Kind::Rust, false).unwrap();
    let w = Some(Prov::Wildcard);
    assert_eq!(
        err(mem.access(0x1004, w, 4, 4, AccessKind::Read)),
        "memory access failed: attempting to access 4 bytes, but got 0x1004[noalloc] which is a \
         dangling pointer (it has no provenance)"
    );
    mem.expose(a, BorTag::new(1));
    mem.expose(a, BorTag::new(1));
    mem.expose(a, BorTag::new(2));
    assert_eq!(mem.exposed_tags(a), [BorTag::new(1), BorTag::new(2)]);
    assert!(mem.info(a).exposed && !mem.info(b).exposed);
    assert_eq!(mem.access(0x1004, w, 4, 4, AccessKind::Read), Ok(Some((a, 4))));
    assert_eq!(mem.exposed_at(0x100f), Some(a));
    assert_eq!(mem.exposed_at(0x1010), None, "the end of an allocation is not in it");
    assert_eq!(mem.offset_inbounds(0x1010, w, -4), Ok(()), "a negative size looks to the left");
    assert_eq!(mem.exposed_at(0x2000), None);
    assert_eq!(
        err(mem.access(0x100c, w, 8, 4, AccessKind::Read)),
        "memory access failed: attempting to access 8 bytes, but got alloc1+0xc which is only 4 \
         bytes from the end of the allocation"
    );
    let mut tags = Vec::new();
    mem.tags(&mut |alloc, tag| tags.push((alloc, tag)));
    assert_eq!(tags, [(a, BorTag::new(1)), (a, BorTag::new(2))]);
    assert_eq!(mem.free((0x1000, w), None, None, Kind::Rust), Ok(a));
    assert!(!mem.info(a).exposed);
    assert_eq!(mem.exposed_at(0x1004), None);
    mem.expose(a, BorTag::new(3));
    assert!(!mem.info(a).exposed, "exposing freed memory does nothing");
    let c = mem.alloc(0x1000, 16, 8, Kind::Rust, false).unwrap();
    assert_eq!(mem.exposed_at(0x1004), None, "a reused address is not exposed");
    mem.expose(c, BorTag::new(4));
    assert_eq!(mem.exposed_at(0x1004), Some(c));
}

#[test]
fn wildcard_zero_sized_allocations() {
    let mut mem = Memory::new();
    let z1 = mem.alloc(8, 0, 8, Kind::Stack, false).unwrap();
    let z2 = mem.alloc(8, 0, 8, Kind::Stack, false).unwrap();
    let a = mem.alloc(0x1000, 8, 8, Kind::Rust, false).unwrap();
    let z3 = mem.alloc(0x1008, 0, 8, Kind::Stack, false).unwrap();
    assert_eq!(mem.live_at(8), Some(z2));
    assert_eq!(mem.exposed_at(8), None);
    mem.expose(z1, BorTag::new(1));
    assert_eq!(mem.exposed_at(8), Some(z1));
    mem.expose(z3, BorTag::new(2));
    mem.expose(a, BorTag::new(3));
    assert_eq!(mem.exposed_at(0x1008), Some(z3), "a zero-sized allocation at another's end");
    assert_eq!(mem.access(8, Some(prov(z1, 1)), 0, 8, AccessKind::Read), Ok(None));
    assert_eq!(
        err(mem.access(8, Some(prov(z1, 1)), 1, 1, AccessKind::Read)),
        "memory access failed: attempting to access 1 byte, but got alloc1 which is at or \
         beyond the end of the allocation of size 0 bytes"
    );
    assert_eq!(mem.free((8, Some(Prov::Wildcard)), Some(0), Some(8), Kind::Stack), Ok(z1));
    assert_eq!(mem.exposed_at(8), None);
    assert_eq!(mem.live_at(8), Some(z2));
    assert_eq!(mem.free((8, Some(prov(z2, 9))), None, None, Kind::Stack), Ok(z2));
    assert_eq!(mem.live_at(8), None);
}

// The leak check. Miri: interpret/memory.rs, `take_leaked_allocations`; tests/fail/memleak*.rs,
// tests/pass/leak-in-static.rs.

#[test]
fn leaks_reachable_from_globals_and_roots() {
    let mut mem = Memory::new();
    let g = mem.alloc(0x100, 8, 8, Kind::Global, true).unwrap();
    let kept = mem.alloc(0x1000, 16, 8, Kind::Rust, false).unwrap();
    let chained = mem.alloc(0x2000, 4, 4, Kind::C, false).unwrap();
    let leaked = mem.alloc(0x3000, 4, 4, Kind::Rust, false).unwrap();
    let cycle1 = mem.alloc(0x4000, 8, 8, Kind::Rust, false).unwrap();
    let cycle2 = mem.alloc(0x5000, 8, 8, Kind::Rust, false).unwrap();
    let rooted = mem.alloc(0x6000, 8, 8, Kind::Rust, false).unwrap();
    let env = mem.alloc(0x7000, 8, 8, Kind::Machine, false).unwrap();
    let from_env = mem.alloc(0x8000, 8, 8, Kind::Rust, false).unwrap();
    let by_fragment = mem.alloc(0x9000, 8, 8, Kind::Rust, false).unwrap();
    let wild = mem.alloc(0xa000, 8, 8, Kind::Rust, false).unwrap();
    let freed = mem.alloc(0xb000, 8, 8, Kind::Rust, false).unwrap();
    let const_heap = mem.alloc(0xc000, 8, 8, Kind::ConstHeap, false).unwrap();
    let stack = mem.alloc(0xd000, 8, 8, Kind::Stack, false).unwrap();

    mem.write(g, 0, &ptr(8, prov(kept, 1)));
    mem.write(kept, 0, &ptr(8, prov(chained, 2)));
    mem.write(kept, 8, &ptr(8, prov(by_fragment, 3)));
    mem.write(kept, 8, &Bytes::init(4));
    mem.write(cycle1, 0, &ptr(8, prov(cycle2, 4)));
    mem.write(cycle2, 0, &ptr(8, prov(cycle1, 5)));
    mem.add_root(rooted);
    mem.write(env, 0, &ptr(8, prov(from_env, 6)));
    mem.expose(wild, BorTag::new(7));
    mem.write(rooted, 0, &ptr(8, Prov::Wildcard));
    mem.free((0xb000, Some(prov(freed, 8))), None, None, Kind::Rust).unwrap();

    let leak = |alloc: AllocId, kind: Kind, size: u64, align: u64| Leak { alloc, kind, size, align };
    let found = mem.leaks().unwrap_err();
    assert_eq!(
        found,
        Leaks {
            leaked: vec![
                leak(leaked, Kind::Rust, 4, 4),
                leak(cycle1, Kind::Rust, 8, 8),
                leak(cycle2, Kind::Rust, 8, 8),
                leak(wild, Kind::Rust, 8, 8),
                leak(const_heap, Kind::ConstHeap, 8, 8),
                leak(stack, Kind::Stack, 8, 8),
            ]
        }
    );
    assert_eq!(
        found.to_string(),
        "alloc4 (Rust heap, size: 4, align: 4)\n\
         memory leaked: alloc5 (Rust heap, size: 8, align: 8)\n\
         memory leaked: alloc6 (Rust heap, size: 8, align: 8)\n\
         memory leaked: alloc11 (Rust heap, size: 8, align: 8)\n\
         memory leaked: alloc13 (heap allocation, size: 8, align: 8)\n\
         memory leaked: alloc14 (stack variable, size: 8, align: 8)"
    );
    assert_eq!(
        crate::Ub::Leak(found).to_string().lines().next(),
        Some("memory leaked: alloc4 (Rust heap, size: 4, align: 4)")
    );
}

#[test]
fn no_leaks() {
    let mut mem = Memory::new();
    assert_eq!(mem.leaks(), Ok(()));
    let h = mem.alloc(0x1000, 4, 4, Kind::C, false).unwrap();
    let g = mem.alloc(0x2000, 8, 8, Kind::Global, false).unwrap();
    mem.write(g, 0, &ptr(8, prov(h, 1)));
    assert_eq!(mem.leaks(), Ok(()));
    mem.uninit(g, Span::new(0, 8));
    assert_eq!(
        mem.leaks().unwrap_err().to_string(),
        "alloc1 (C heap, size: 4, align: 4)"
    );
}

#[test]
fn tags_held_in_memory() {
    let mut mem = Memory::new();
    let a = mem.alloc(0x1000, 32, 8, Kind::Rust, false).unwrap();
    let b = mem.alloc(0x2000, 8, 8, Kind::Rust, false).unwrap();
    mem.write(a, 0, &ptr(8, prov(b, 5)));
    mem.write(a, 8, &ptr(8, prov(a, 6)));
    mem.write(a, 12, &Bytes::init(4));
    mem.write(a, 16, &ptr(8, Prov::Wildcard));
    mem.write(b, 0, &ptr(8, prov(a, 7)));
    mem.expose(b, BorTag::new(8));
    mem.free((0x2000, Some(prov(b, 5))), None, None, Kind::Rust).unwrap();
    let mut tags = Vec::new();
    mem.tags(&mut |alloc, tag| tags.push((alloc, tag.get())));
    tags.sort();
    assert_eq!(tags, [(a, 6), (a, 6), (a, 6), (a, 6), (b, 5)]);
}

#[test]
fn displays_of_kinds_and_messages() {
    assert_eq!(Kind::Machine.to_string(), "machine-managed memory");
    assert_eq!(Kind::Global.to_string(), "global (static or const)");
    assert_eq!(InAlloc::Dereferenceable.to_string(), "pointer not dereferenceable");
    assert_eq!(
        MemError::DanglingInt { addr: 0x10, inbounds_size: 4, msg: InAlloc::Dereferenceable }
            .to_string(),
        "pointer not dereferenceable: pointer must be dereferenceable for 4 bytes, but got \
         0x10[noalloc] which is a dangling pointer (it has no provenance)"
    );
    assert_eq!(
        MemError::FreeNotStart { addr: 0x10, prov: Some(Prov::Wildcard) }.to_string(),
        "deallocating 0x10[wildcard] which does not point to the beginning of an object"
    );
    assert!(MemError::TooManyAllocations.to_string().starts_with("internal error"));
}
