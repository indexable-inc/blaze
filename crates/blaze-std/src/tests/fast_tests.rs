use core::mem::align_of;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::mem::{Frame, GenMap, KindSet, SmallVec, TASK_FRAME, Tag, interp_search_by_prefix};
use crate::sync::CachePadded;

fn test_frame() -> Frame {
    Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("test frame")
}

#[test]
fn small_vec_inline_and_spill_on_frame() {
    let frame = test_frame();
    let mut v: SmallVec<u32, 4, &Frame> = SmallVec::new_in(&frame);
    for i in 0..4u32 {
        v.push(i * 10);
    }
    assert!(!v.is_spilled());
    assert_eq!(&*v, &[0, 10, 20, 30]);
    v.push(40);
    assert!(v.is_spilled());
    assert_eq!(&*v, &[0, 10, 20, 30, 40]);
    assert_eq!(v.pop(), Some(40));
    v.extend_from_slice(&[50, 60]);
    assert_eq!(&*v, &[0, 10, 20, 30, 50, 60]);
    v.clear();
    assert!(v.is_empty());
}

#[test]
fn gen_map_o1_clear_and_single_probe_on_frame() {
    let frame = test_frame();
    let mut map: GenMap<u32, u64, &Frame> = GenMap::with_capacity_in(32, &frame);
    for i in 0..50u32 {
        assert_eq!(map.insert(i, u64::from(i) * 7), None);
    }
    assert_eq!(map.len(), 50);
    assert_eq!(map.get(&42), Some(294));
    let cap_before = map.capacity();
    let gen_before = map.generation();

    // O(1) clear bumps generation without dropping capacity.
    map.clear();
    assert!(map.is_empty());
    assert_eq!(map.capacity(), cap_before);
    assert_eq!(map.generation(), gen_before + 1);
    assert_eq!(map.get(&42), None);

    // Single-probe get_or_insert_with computes only once.
    let mut calls = 0u32;
    let a = map.get_or_insert_with(7, || {
        calls += 1;
        999
    });
    let b = map.get_or_insert_with(7, || {
        calls += 1;
        111
    });
    assert_eq!((a, b, calls), (999, 999, 1));
}

#[test]
fn kind_set_const_eval_and_o1_contains() {
    const ASCII_IDENT_START: KindSet<4> = KindSet::empty()
        .with_range(b'A' as usize, b'Z' as usize)
        .with(b'_' as usize)
        .with_range(b'a' as usize, b'z' as usize);
    assert_eq!(ASCII_IDENT_START.count(), 53);
    assert!(ASCII_IDENT_START.contains(b'a' as usize));
    assert!(ASCII_IDENT_START.contains(b'_' as usize));
    assert!(ASCII_IDENT_START.contains(b'Z' as usize));
    assert!(!ASCII_IDENT_START.contains(b'0' as usize));
    assert!(!ASCII_IDENT_START.contains(300));
}

#[test]
fn interp_search_matches_binary_search_on_uniform_and_skewed_keys() {
    let frame = test_frame();
    // 1. Uniform 64-bit hash keys.
    let mut uniform = alloc::vec::Vec::with_capacity_in(1024, &frame);
    let mut x = 0x1234_5678_9abc_def0u64;
    for _ in 0..1024 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        uniform.push(x);
    }
    uniform.sort_unstable();
    uniform.dedup();
    for (idx, &k) in uniform.iter().enumerate() {
        let got = interp_search_by_prefix(&uniform, k, |&v| v, |v| v.cmp(&k));
        assert_eq!(got, Ok(idx));
        let miss = k.wrapping_add(1);
        let got_miss = interp_search_by_prefix(&uniform, miss, |&v| v, |v| v.cmp(&miss));
        assert_eq!(got_miss, uniform.binary_search(&miss));
    }

    // 2. Adversarial exponential skew (tests 3-stale-step binary fallback).
    let mut skewed = alloc::vec::Vec::with_capacity_in(64, &frame);
    for i in 0..63u32 {
        skewed.push(1u64 << i);
    }
    for (idx, &k) in skewed.iter().enumerate() {
        assert_eq!(interp_search_by_prefix(&skewed, k, |&v| v, |v| v.cmp(&k)), Ok(idx));
    }
    assert_eq!(interp_search_by_prefix(&skewed, 3, |&v| v, |v| v.cmp(&3)), Err(2));
}

#[test]
fn cache_padded_is_128_byte_aligned_everywhere() {
    assert_eq!(align_of::<CachePadded<AtomicU64>>(), 128);
    let c = CachePadded::new(AtomicU64::new(42));
    assert_eq!(c.load(Ordering::Relaxed), 42);
    assert_eq!(c.into_inner().into_inner(), 42);
}

#[test]
fn flat_dir_capability_narrows_and_rejects_nested_leaf() {
    use crate::fs::{Content, Flat, Leaf, LeafError, ReadOnly, ReadWrite, Scope, Subtree};
    use crate::io::{Io, Limits, SimWorld};
    use crate::path::Path;

    assert!(Subtree::RECURSIVE);
    assert!(!Flat::RECURSIVE);

    // Leaf rejects nested paths, parent traversal, roots, and empty names.
    assert!(Leaf::new("config.toml").is_ok());
    assert!(matches!(Leaf::new("sub/config.toml"), Err(LeafError::Nested { .. })));
    assert!(matches!(Leaf::new("../secret"), Err(LeafError::Path(_))));
    assert!(matches!(Leaf::new("/etc/passwd"), Err(LeafError::Path(_))));
    assert!(matches!(Leaf::new(""), Err(LeafError::Path(_))));

    // Simulated OS run: Subtree -> subdir -> flat -> quota -> read_only.
    let io = Io::simulated(SimWorld::new(42, Limits::USUAL));
    let cx = super::root(&io);
    let root: crate::fs::Dir<ReadWrite, Subtree> = io.need_fs().expect("sim offers fs").root();
    let pkg = root.subdir(&Path::new("pkg").unwrap());
    let flat_rw: crate::fs::Dir<ReadWrite, Flat> = pkg.flat().quota(1024);
    assert_eq!(flat_rw.quota_left(), Some(1024));

    let leaf = Leaf::new("manifest.toml").unwrap();
    flat_rw.publish_leaf_synced(&cx, &leaf, Content::bytes(b"name = \"demo\"")).unwrap();
    assert_eq!(flat_rw.quota_left(), Some(1024 - 13));

    let flat_ro: crate::fs::Dir<ReadOnly, Flat> = flat_rw.read_only();
    let meta = flat_ro.stat_leaf(&cx, &leaf).unwrap().expect("file exists");
    assert_eq!(meta.len, 13);
    let entries = flat_ro.own_entries(&cx).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries.name(0), "manifest.toml");

    let file = flat_ro.open_leaf(&cx, &leaf).unwrap();
    let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).unwrap();
    let mut buf = crate::mem::Bytes::new_in(&frame, "flat read");
    file.read_all(&cx, &mut buf).unwrap();
    assert_eq!(buf.as_slice(), b"name = \"demo\"");
}
