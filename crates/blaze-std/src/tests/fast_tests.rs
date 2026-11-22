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

#[test]
fn wasm64_sans_io_ring_batches_ops_in_one_host_crossing() {
    use core::mem::{align_of, size_of};
    use crate::wasm64::{
        CqEntry, HostCaps, OpCode, RingError, RingStatus, SimWasmHost, SqEntry, Wasm64Ring,
    };

    assert_eq!(size_of::<SqEntry>(), 64);
    assert_eq!(align_of::<SqEntry>(), 64);
    assert_eq!(size_of::<CqEntry>(), 32);
    assert_eq!(align_of::<CqEntry>(), 32);

    let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("frame");
    let caps = HostCaps::CLOCK
        .union(HostCaps::RANDOM)
        .union(HostCaps::KV)
        .union(HostCaps::STDIO);
    let mut host = SimWasmHost::new_in(caps, 0xC0FFEE, &frame);
    let mut ring = Wasm64Ring::new_in(&host, 16, &frame).expect("ring init");

    // Ungranted HTTP capability is refused immediately without a host call.
    assert!(matches!(
        ring.push(OpCode::HttpFetch, SqEntry::default()),
        Err(RingError::MissingCapability { opcode: OpCode::HttpFetch, .. })
    ));
    assert_eq!(ring.host_crossings(), 0);

    // Simulated 64-bit guest linear memory buffer.
    let mut mem = alloc::vec![0u8; 4096];
    mem[0..3].copy_from_slice(b"key");
    mem[16..27].copy_from_slice(b"hello-w64!!");
    mem[64..76].copy_from_slice(b"stdout-batch");

    // Queue 5 heterogeneous ops and flush ALL 5 in ONE single host boundary crossing!
    let t_clk = ring.queue_clock_monotonic().unwrap();
    let t_rng = ring.queue_random(128, 16).unwrap();
    let t_put = ring.queue_kv_put(0, 3, 16, 11).unwrap();
    let t_get = ring.queue_kv_get(0, 3, 256, 64).unwrap();
    let t_out = ring.queue_stdout(64, 12).unwrap();
    assert_eq!(ring.pending_submissions(), 5);

    let n = ring.flush_wait(&mut host, &mut mem, 5, 1_000_000_000).unwrap();
    assert_eq!(n, 5);
    assert_eq!(ring.host_crossings(), 1, "all 5 ops must amortize into 1 host crossing");

    let c1 = ring.pop_completion().unwrap();
    assert_eq!((c1.tag, RingStatus::from_i32(c1.status)), (t_clk, RingStatus::Ok));
    assert!(c1.result_u64 > 0);

    let c2 = ring.pop_completion().unwrap();
    assert_eq!((c2.tag, c2.result_u64), (t_rng, 16));
    assert!(mem[128..144].iter().any(|&b| b != 0));

    let c3 = ring.pop_completion().unwrap();
    assert_eq!((c3.tag, c3.result_u64), (t_put, 11));

    let c4 = ring.pop_completion().unwrap();
    assert_eq!((c4.tag, c4.result_u64), (t_get, 11));
    assert_eq!(&mem[256..267], b"hello-w64!!");

    let c5 = ring.pop_completion().unwrap();
    assert_eq!((c5.tag, c5.result_u64), (t_out, 12));
    assert_eq!(host.stdout(), b"stdout-batch");
}

#[test]
fn simd_primitives_match_scalar_across_block_boundaries() {
    use crate::mem::{simd_count_byte, simd_find_byte_any4, simd_mismatch, simd_prefix_sum_u32};

    // 1. simd_count_byte across > 255 * 32 = 8160 bytes (exercises multi-block accumulation).
    let mut big = alloc::vec![b'a'; 10_000];
    let mut expected_newlines = 0usize;
    for i in (7..big.len()).step_by(19) {
        big[i] = b'\n';
        expected_newlines += 1;
    }
    assert_eq!(simd_count_byte(&big, b'\n'), expected_newlines);
    assert_eq!(simd_count_byte(&big[..13], b'\n'), 1);

    // 2. simd_find_byte_any4 across aligned & unaligned positions.
    let hay = b"0123456789abcdef0123456789abcdef_hello\"world/end";
    assert_eq!(simd_find_byte_any4(hay, b'\n', b'"', b'\\', b'/'), Some(38));
    assert_eq!(simd_find_byte_any4(&hay[..38], b'\n', b'"', b'\\', b'/'), None);

    // 3. simd_mismatch at start, middle, tail, and equal slices.
    let mut copy = big.clone();
    assert_eq!(simd_mismatch(&big, &copy), big.len());
    copy[4099] ^= 1;
    assert_eq!(simd_mismatch(&big, &copy), 4099);
    assert_eq!(simd_mismatch(b"abc", b"abd"), 2);

    // 4. simd_prefix_sum_u32 on non-multiple-of-8 length.
    let mut vals = [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    simd_prefix_sum_u32(&mut vals);
    assert_eq!(vals, [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 66]);
}

#[test]
fn comptime_enum_tags_and_discriminant_tag_need_no_proc_macro() {
    use crate::comptime::{TaggedEnum, discriminant_tag};

    crate::enum_tags! {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Color {
            Red,
            Green,
            Blue,
        }
    }

    assert_eq!(Color::VARIANT_COUNT, 3);
    assert_eq!(Color::Red.tag(), 0);
    assert_eq!(Color::Green.tag(), 1);
    assert_eq!(Color::Blue.tag(), 2);
    assert_eq!(Color::Blue.variant_name(), "Blue");
    assert_ne!(discriminant_tag(&Color::Red), discriminant_tag(&Color::Blue));
}

#[test]
fn type_set_order_independent_disjoint_borrows_row_poly_and_lacks() {
    use crate::type_set::{Cons, ExtendSet, HasPairMut, Lacks, UniqueSet};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct MaxSpeed(u32);
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Health(u32);
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Armor(u32);
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Burning;
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Cargo(u32);

    crate::declare_distinct!(MaxSpeed, Health, Armor, Burning, Cargo);

    type Ship = crate::TypeSet![MaxSpeed, Health];
    type Frigate = ExtendSet<Cargo, Ship>;

    // Construct in arbitrary order {Health(500), MaxSpeed(100)} and reorder into Ship {MaxSpeed, Health}.
    let raw = crate::tset! { Health(500), MaxSpeed(100) };
    let mut s: Ship = raw.project();
    assert_eq!(core::mem::size_of::<Ship>(), core::mem::size_of::<(MaxSpeed, Health)>());

    // Access by type (`s.[Health].0 -= 10`).
    s.get_mut::<Health, _>().0 -= 10;
    assert_eq!(s.get::<Health, _>().0, 490);

    // Safe disjoint borrow (`(&mut s.[Health], &s.[MaxSpeed])`), zero unsafe!
    let (h, m) = s.borrow_mut_ref::<Health, MaxSpeed, _, _>();
    h.0 -= m.0 / 10;
    assert_eq!(s.get::<Health, _>().0, 480);

    // Row-polymorphic "at least {Health, Armor}" function works on any superset.
    fn damage<S, I1, I2>(e: &mut S, amount: u32)
    where
        S: HasPairMut<Health, Armor, I1, I2>,
    {
        let (health, armor) = e.borrow_pair_mut();
        health.0 = health.0.saturating_sub(amount.saturating_sub(armor.0));
    }

    let mut cruiser = s.extend(Armor(25));
    damage(&mut cruiser, 60);
    assert_eq!(cruiser.get::<Health, _>().0, 445);

    // Row-polymorphic extension requiring `Lacks<Burning>`.
    fn ignite<R>(e: Cons<Health, R>) -> Cons<Burning, Cons<Health, R>>
    where
        R: Lacks<Burning>,
        Cons<Health, R>: Lacks<Burning>,
    {
        e.extend_lacking(Burning)
    }

    let ship_h = crate::tset! { Health(500), MaxSpeed(100) };
    let burning_ship = ignite(ship_h);
    assert_eq!(*burning_ship.get::<Burning, _>(), Burning);
    assert_eq!(burning_ship.get::<Health, _>().0, 500);

    // Type-level extension Frigate = {Cargo, ..Ship}.
    let frigate: Frigate = crate::tset! { Health(300), MaxSpeed(80), Cargo(40) }.project();
    assert_eq!(frigate.get::<Cargo, _>().0, 40);
    assert_eq!(frigate.get::<MaxSpeed, _>().0, 80);
    assert!(<Frigate as UniqueSet>::ALL_DISTINCT);
    assert!(!<crate::TypeSet![Health, Health] as UniqueSet>::ALL_DISTINCT);
}
