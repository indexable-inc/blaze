use std::{
    collections::BTreeMap,
    hash::{BuildHasher, BuildHasherDefault, DefaultHasher},
    mem::offset_of,
};

use blaze_format::{Aligned, FormatError, Part, Persist, Record, Rel, SchemaHash, Slice, Writer, buffer_id, open_as, schema_of, to_bytes};
use blaze_ids::{ContentId, Domain, Hasher, Idx, Interner, rng::SplitMix64 as Rng};

use crate::{
    Consed, HashCons, Interned, Load, content_of,
    table::{CHUNKS, FIRST, chunk_of},
};
use records::{LeafRoot, PairRoot};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Leaf {
    n: u64,
    name: String,
}

fn leaf(n: u64, name: &str) -> Leaf {
    Leaf { n, name: name.to_owned() }
}

impl Consed for Leaf {
    fn domain() -> &'static Domain {
        static DOMAIN: Domain = Domain::new("blaze v2 hashcons test leaf 1");
        &DOMAIN
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        hasher.update(&self.n.to_le_bytes()).part(self.name.as_bytes());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pair {
    left: Interned<'static, Leaf>,
    right: Interned<'static, Leaf>,
}

impl Consed for Pair {
    fn domain() -> &'static Domain {
        static DOMAIN: Domain = Domain::new("blaze v2 hashcons test pair 1");
        &DOMAIN
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        self.left.hash_into(hasher);
        self.right.hash_into(hasher);
    }
}

/// `record!` writes each record's `unsafe impl Record`.
#[allow(unsafe_code)]
mod records {
    use blaze_format::{Rel, Str, record};

    record! {
        #[derive(Clone, Copy)]
        pub(super) struct LeafRoot: 1 {
            pub(super) n: u64,
            pub(super) name: Str,
        }
    }

    record! {
        #[derive(Clone, Copy)]
        pub(super) struct PairRoot: 1 {
            pub(super) left: Rel<LeafRoot>,
            pub(super) right: Rel<LeafRoot>,
        }
    }
}

fn write_leaf(leaf: &Leaf, out: &mut Writer) -> u64 {
    let name = out.push_str(&leaf.name);
    let at = out.next::<LeafRoot>();
    let name = match leaf.name.is_empty() {
        true => Slice::EMPTY,
        false => Slice::between(at + offset_of!(LeafRoot, name) as u64, name, leaf.name.len()),
    };
    let written = out.push(&LeafRoot { n: leaf.n, name });
    assert_eq!(written, at);
    at
}

fn read_leaf<'a>(root: &'a LeafRoot, buffer: &'a [u8]) -> Result<(u64, &'a str), FormatError> {
    Ok((root.n, root.name.as_str(buffer)?))
}

impl Persist for Leaf {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("hashcons test Leaf"), Part::U64(1), Part::Schema(<LeafRoot as Record>::SCHEMA)]);
    type Ref<'a> = (u64, &'a str);

    fn write(&self, out: &mut Writer) -> u64 {
        write_leaf(self, out)
    }

    fn read(buffer: &[u8]) -> Result<(u64, &str), FormatError> {
        read_leaf(open_as::<LeafRoot>(buffer, Self::SCHEMA)?, buffer)
    }
}

impl Load for Leaf {
    type Parts = ();

    fn load((n, name): (u64, &str), (): ()) -> Leaf {
        leaf(n, name)
    }
}

impl Persist for Pair {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("hashcons test Pair"), Part::U64(1), Part::Schema(<PairRoot as Record>::SCHEMA)]);
    type Ref<'a> = ((u64, &'a str), (u64, &'a str));

    fn write(&self, out: &mut Writer) -> u64 {
        let left = write_leaf(self.left.get(), out);
        let right = write_leaf(self.right.get(), out);
        let at = out.next::<PairRoot>();
        let root = PairRoot {
            left: Rel::between(at + offset_of!(PairRoot, left) as u64, left),
            right: Rel::between(at + offset_of!(PairRoot, right) as u64, right),
        };
        let written = out.push(&root);
        assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, FormatError> {
        let root = open_as::<PairRoot>(buffer, Self::SCHEMA)?;
        let missing = FormatError::Invalid { at: 0, what: "a pair without one of its leaves" };
        let left = root.left.follow(buffer)?.ok_or(missing.clone())?;
        let right = root.right.follow(buffer)?.ok_or(missing)?;
        Ok((read_leaf(left, buffer)?, read_leaf(right, buffer)?))
    }
}

impl Load for Pair {
    type Parts = &'static HashCons<Leaf>;

    fn load((left, right): Self::Ref<'_>, leaves: &'static HashCons<Leaf>) -> Pair {
        Pair { left: leaves.intern(Leaf::load(left, ())), right: leaves.intern(Leaf::load(right, ())) }
    }
}

fn leaked<T: Consed>() -> &'static HashCons<T> {
    Box::leak(Box::new(HashCons::new()))
}

const NAMES: [&str; 5] = ["", "a", "ab", "a longer name than the others", "b"];

fn random_leaf(rng: &mut Rng) -> Leaf {
    leaf(rng.below(40), NAMES[rng.index(NAMES.len())])
}

#[test]
fn equal_values_intern_once() {
    let cons = HashCons::<Leaf>::new();
    let a = cons.intern(leaf(1, "a"));
    let again = cons.intern(leaf(1, "a"));
    let b = cons.intern(leaf(1, "b"));
    assert_eq!(a, again);
    assert_eq!(a.idx(), again.idx());
    assert_ne!(a, b);
    assert_ne!(a.idx(), b.idx());
    assert_eq!(*a.get(), leaf(1, "a"));
    assert_eq!(a.content(), content_of(&leaf(1, "a")));
    assert_eq!(a.fingerprint(), content_of(&leaf(1, "a")).fingerprint());
    assert_eq!(cons.get(b.idx()), &leaf(1, "b"));
    assert_eq!(cons.handle(a.idx()), a);
    assert_eq!(cons.lookup(&a.content()), Some(a));
    assert_eq!(cons.lookup(&content_of(&leaf(2, "a"))), None);
    assert_eq!(cons.len(), 2);
    assert!(!cons.is_empty());
    assert!((a.idx().raw() as usize) < cons.end() && (b.idx().raw() as usize) < cons.end());
}

/// The hot paths allocate nothing: an intern that finds its value, an index or a content ID to
/// a value, and comparing or hashing two handles. The values are made outside the count.
#[cfg(not(blaze_check))]
#[test]
fn hits_reads_and_compares_allocate_nothing() {
    use std::hash::{Hash, Hasher as _};

    let cons = HashCons::<Leaf>::new();
    let handles: Vec<_> = (0..1_000).map(|n| cons.intern(leaf(n, "hit"))).collect();
    let (a, b) = (handles[17], handles[400]);
    let again: Vec<Leaf> = (0..1_000).map(|n| leaf(n, "hit")).collect();
    let (content, absent) = (a.content(), content_of(&leaf(5_000, "hit")));
    blaze_alloc_count::assert_at_most(0, "interning values already in", || {
        for (value, handle) in again.into_iter().zip(&handles) {
            assert_eq!(cons.intern(value), *handle);
        }
    });
    blaze_alloc_count::assert_at_most(0, "an index to a value and a handle", || {
        assert_eq!(cons.get(a.idx()).n, 17);
        assert_eq!(cons.handle(b.idx()), b);
    });
    blaze_alloc_count::assert_at_most(0, "a content ID to its handle", || {
        assert_eq!(cons.lookup(&content), Some(a));
        assert_eq!(cons.lookup(&absent), None);
    });
    blaze_alloc_count::assert_at_most(0, "comparing and hashing handles", || {
        assert!(a == a && a != b);
        let mut hasher = DefaultHasher::new();
        a.hash(&mut hasher);
        hasher.finish()
    });
}

#[test]
fn chunks_cover_every_place() {
    let mut expected = (0, 0);
    for local in 0..100_000u32 {
        assert_eq!(chunk_of(local), expected, "{local}");
        expected.1 += 1;
        if expected.1 == FIRST << expected.0 {
            expected = (expected.0 + 1, 0);
        }
    }
    assert!(chunk_of(u32::MAX).0 < CHUNKS);
}

/// intern(a) == intern(b) exactly when a == b, checked against a sequential model: a map from
/// each distinct value to the first handle it got.
#[test]
fn handles_are_equal_exactly_when_values_are() {
    let leaves = leaked::<Leaf>();
    let pairs = HashCons::<Pair>::new();
    let mut rng = Rng::new(7);
    let mut leaf_model: BTreeMap<Leaf, Interned<'static, Leaf>> = BTreeMap::new();
    let mut pair_model: BTreeMap<(Leaf, Leaf), Interned<'_, Pair>> = BTreeMap::new();
    for _ in 0..20_000 {
        let value = random_leaf(&mut rng);
        let handle = leaves.intern(value.clone());
        assert_eq!(*handle.get(), value);
        assert_eq!(handle.content(), content_of(&value));
        assert_eq!(*leaf_model.entry(value).or_insert(handle), handle);

        let (left, right) = (random_leaf(&mut rng), random_leaf(&mut rng));
        let pair = Pair { left: leaves.intern(left.clone()), right: leaves.intern(right.clone()) };
        let handle = pairs.intern(pair);
        assert_eq!(*handle.get(), pair);
        assert_eq!(*pair_model.entry((left, right)).or_insert(handle), handle);
    }
    let mut raws: Vec<u32> = leaf_model.values().map(|it| it.idx().raw()).collect();
    raws.sort_unstable();
    raws.dedup();
    assert_eq!(raws.len(), leaf_model.len(), "unequal leaves share an index");
    assert_eq!(leaves.len(), leaf_model.len());
    let mut raws: Vec<u32> = pair_model.values().map(|it| it.idx().raw()).collect();
    raws.sort_unstable();
    raws.dedup();
    assert_eq!(raws.len(), pair_model.len(), "unequal pairs share an index");
    assert_eq!(pairs.len(), pair_model.len());
    for (value, handle) in &leaf_model {
        for (other, other_handle) in leaf_model.range(value.clone()..).take(8) {
            assert_eq!(value == other, handle == other_handle);
        }
    }
}

#[test]
fn a_composite_hashes_its_parts_content_ids() {
    let leaves = leaked::<Leaf>();
    let (left, right) = (leaves.intern(leaf(1, "a")), leaves.intern(leaf(2, "b")));
    let mut expected = Hasher::of(Pair::domain());
    expected.id(&left.content()).id(&right.content());
    assert_eq!(content_of(&Pair { left, right }), expected.finish());
    assert_ne!(content_of(&Pair { left, right }), content_of(&Pair { left: right, right: left }));
}

#[test]
fn values_never_move() {
    let cons = HashCons::<Leaf>::new();
    let first = cons.intern(leaf(0, "zero"));
    let at = std::ptr::from_ref(first.get());
    for n in 1..100_000 {
        cons.intern(leaf(n, "n"));
    }
    assert!(std::ptr::eq(at, cons.get(first.idx())));
    assert!(std::ptr::eq(at, cons.lookup(&first.content()).unwrap().get()));
}

#[test]
fn tables_grow_and_keep_every_value() {
    let cons = HashCons::<Leaf>::new();
    let handles: Vec<_> = (0..200_000).map(|n| cons.intern(leaf(n, "grow"))).collect();
    assert_eq!(cons.len(), 200_000);
    let end = cons.end();
    for (n, handle) in handles.iter().enumerate() {
        assert_eq!(cons.lookup(&content_of(&leaf(n as u64, "grow"))), Some(*handle));
        assert!((handle.idx().raw() as usize) < end);
        assert_eq!(cons.handle(handle.idx()), *handle);
    }
}

#[test]
fn the_same_value_from_many_threads_is_one_index() {
    let cons = HashCons::<Leaf>::new();
    let threads = 16u64;
    let seen: Vec<(Vec<Idx<Leaf>>, Vec<Idx<Leaf>>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|worker| {
                let cons = &cons;
                scope.spawn(move || {
                    let same: Vec<_> = (0..2_000).map(|_| cons.intern(leaf(7, "shared")).idx()).collect();
                    let many: Vec<_> = (0..10_000u64)
                        .map(|n| (n + worker * 997) % 10_000)
                        .map(|n| cons.intern(leaf(n, "many")).idx())
                        .collect();
                    (same, many)
                })
            })
            .collect();
        workers.into_iter().map(|it| it.join().unwrap()).collect()
    });
    assert_eq!(cons.len(), 10_001);
    let shared = cons.lookup(&content_of(&leaf(7, "shared"))).unwrap().idx();
    for (worker, (same, many)) in seen.iter().enumerate() {
        assert!(same.iter().all(|&idx| idx == shared));
        for (i, &idx) in many.iter().enumerate() {
            let n = (i as u64 + worker as u64 * 997) % 10_000;
            assert_eq!(*cons.get(idx), leaf(n, "many"));
            assert_eq!(cons.lookup(&content_of(&leaf(n, "many"))).unwrap().idx(), idx);
        }
    }
    let mut raws: Vec<u32> = cons.sorted().iter().map(|it| it.idx().raw()).collect();
    raws.sort_unstable();
    raws.dedup();
    assert_eq!(raws.len(), 10_001);
}

#[derive(PartialEq, Eq, Debug)]
struct Sloppy {
    hashed: u32,
    ignored: u32,
}

impl Consed for Sloppy {
    fn domain() -> &'static Domain {
        static DOMAIN: Domain = Domain::new("blaze v2 hashcons test sloppy 1");
        &DOMAIN
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        hasher.update(&self.hashed.to_le_bytes());
    }
}

#[test]
#[should_panic(expected = "share content ID")]
fn unequal_values_of_one_content_id_stop() {
    let cons = HashCons::<Sloppy>::new();
    cons.intern(Sloppy { hashed: 1, ignored: 1 });
    cons.intern(Sloppy { hashed: 1, ignored: 2 });
}

#[test]
fn the_interner_trait_checks_the_content_id_it_is_given() {
    let cons = HashCons::<Leaf>::new();
    let id = content_of(&leaf(3, "c"));
    let idx = Interner::intern(&cons, id, || leaf(3, "c"));
    assert_eq!(Interner::intern(&cons, id, || unreachable!("interned twice")), idx);
    assert_eq!(Interner::find(&cons, &id), Some(idx));
    assert_eq!(Interner::content(&cons, idx), id);
    assert_eq!(Interner::get(&cons, idx), &leaf(3, "c"));
    assert_eq!(Interner::len(&cons), 1);
    assert_eq!(cons.intern(leaf(3, "c")).idx(), idx);
    let wrong = std::panic::catch_unwind(|| {
        let cons = HashCons::<Leaf>::new();
        Interner::intern(&cons, content_of(&leaf(4, "d")), || leaf(5, "e"));
    });
    assert!(wrong.is_err());
}

#[test]
fn a_handle_hashes_as_its_fingerprint() {
    let cons = HashCons::<Leaf>::new();
    let a = cons.intern(leaf(1, "a"));
    let other = HashCons::<Leaf>::new();
    for n in 0..1_000 {
        other.intern(leaf(n + 10, "others first"));
    }
    let b = other.intern(leaf(1, "a"));
    fn hash(handle: Interned<'_, Leaf>) -> u64 {
        BuildHasherDefault::<DefaultHasher>::default().hash_one(handle)
    }
    assert_eq!(hash(a), hash(b), "two interners, one hash");
    let mut expected = DefaultHasher::new();
    std::hash::Hasher::write_u128(&mut expected, a.fingerprint().0);
    assert_eq!(hash(a), std::hash::Hasher::finish(&expected));
}

#[test]
fn a_handle_persists_as_its_value() {
    let leaves = leaked::<Leaf>();
    let handle = leaves.intern(leaf(5, "five"));
    let bytes = to_bytes(&handle);
    assert_eq!(bytes, to_bytes(handle.get()));
    let buffer = Aligned::copy_from(&bytes);
    assert_eq!(<Interned<'static, Leaf> as Persist>::read(&buffer).unwrap(), (5, "five"));
    let fresh = HashCons::<Leaf>::new();
    let loaded = fresh.load(&buffer, ()).unwrap();
    assert_eq!(*loaded.get(), leaf(5, "five"));
    assert_eq!(loaded.content(), handle.content());
    assert_eq!(fresh.load_verified(&buffer, buffer_id(&bytes), ()).unwrap(), loaded);
    assert!(matches!(fresh.load_verified(&buffer, ContentId([0; 32]), ()), Err(FormatError::Hash { .. })));
}

fn fill(leaves: &'static HashCons<Leaf>, pairs: &HashCons<Pair>, values: &[(Leaf, Leaf)]) {
    for (left, right) in values {
        let left = leaves.intern(left.clone());
        let right = leaves.intern(right.clone());
        pairs.intern(Pair { left, right });
    }
}

fn values(seed: u64, count: usize) -> Vec<(Leaf, Leaf)> {
    let mut rng = Rng::new(seed);
    (0..count).map(|_| (random_leaf(&mut rng), random_leaf(&mut rng))).collect()
}

/// Two runs that intern the same values in different orders, one across threads, give the
/// same content IDs and the same pack bytes, whatever indices they handed out.
#[test]
fn interleavings_give_the_same_fingerprints_and_bytes() {
    let all = values(11, 6_000);

    let threaded_leaves = leaked::<Leaf>();
    let threaded = HashCons::<Pair>::new();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8u64)
            .map(|worker| {
                let mut mine = all.clone();
                Rng::new(100 + worker).shuffle(&mut mine);
                let pairs = &threaded;
                scope.spawn(move || fill(threaded_leaves, pairs, &mine))
            })
            .collect();
        workers.into_iter().for_each(|it| it.join().unwrap());
    });

    let serial_leaves = leaked::<Leaf>();
    let serial = HashCons::<Pair>::new();
    let mut reversed = all.clone();
    reversed.reverse();
    fill(serial_leaves, &serial, &reversed);

    assert_eq!(threaded.len(), serial.len());
    assert_eq!(threaded_leaves.len(), serial_leaves.len());
    let contents = |pairs: &HashCons<Pair>| -> Vec<(ContentId, ContentId, ContentId)> {
        pairs.sorted().iter().map(|it| (it.content(), it.get().left.content(), it.get().right.content())).collect()
    };
    assert_eq!(contents(&threaded), contents(&serial));
    for (left, right) in &all {
        let a = threaded_leaves.lookup(&content_of(left)).unwrap();
        let b = serial_leaves.lookup(&content_of(left)).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        let pair_a = threaded.lookup(&content_of(&Pair { left: a, right: threaded_leaves.intern(right.clone()) }));
        let pair_b = serial.lookup(&content_of(&Pair { left: b, right: serial_leaves.intern(right.clone()) }));
        assert_eq!(pair_a.unwrap().fingerprint(), pair_b.unwrap().fingerprint());
    }
    assert_eq!(threaded_leaves.write_pack(), serial_leaves.write_pack());
    assert_eq!(threaded.write_pack(), serial.write_pack());
}

#[test]
fn a_pack_loads_into_a_fresh_interner_as_the_same_bytes() {
    let leaves = leaked::<Leaf>();
    let pairs = HashCons::<Pair>::new();
    fill(leaves, &pairs, &values(5, 2_000));
    let leaf_pack = leaves.write_pack();
    let pair_pack = pairs.write_pack();

    let fresh_leaves = leaked::<Leaf>();
    let fresh_pairs = HashCons::<Pair>::new();
    let leaf_buffer = Aligned::copy_from(&leaf_pack);
    let pair_buffer = Aligned::copy_from(&pair_pack);
    assert_eq!(fresh_leaves.load_pack_verified(&leaf_buffer, buffer_id(&leaf_pack), ()).unwrap(), leaves.len());
    assert_eq!(fresh_pairs.load_pack(&pair_buffer, fresh_leaves).unwrap(), pairs.len());
    assert_eq!(fresh_leaves.len(), leaves.len());
    assert_eq!(fresh_pairs.len(), pairs.len());
    assert_eq!(fresh_leaves.write_pack(), leaf_pack);
    assert_eq!(fresh_pairs.write_pack(), pair_pack);
    for pair in pairs.sorted() {
        let loaded = fresh_pairs.lookup(&pair.content()).unwrap();
        assert_eq!(*loaded.get().left.get(), *pair.get().left.get());
        assert_eq!(*loaded.get().right.get(), *pair.get().right.get());
    }

    let empty = HashCons::<Leaf>::new().write_pack();
    assert_eq!(HashCons::<Leaf>::new().load_pack(&Aligned::copy_from(&empty), ()).unwrap(), 0);
}

#[test]
fn a_pack_refuses_what_it_was_not_written_with() {
    let cons = HashCons::<Leaf>::new();
    cons.intern(leaf(1, "the one name to change"));
    cons.intern(leaf(2, "another"));
    let pack = cons.write_pack();

    assert!(matches!(
        HashCons::<Leaf>::new().load_pack_verified(&Aligned::copy_from(&pack), ContentId([0; 32]), ()),
        Err(FormatError::Hash { .. })
    ));

    let leaves = leaked::<Leaf>();
    let as_pairs = HashCons::<Pair>::new().load_pack(&Aligned::copy_from(&pack), leaves);
    assert!(matches!(as_pairs, Err(FormatError::Schema { .. })), "{as_pairs:?}");

    let mut tampered = pack.clone();
    let needle = b"the one name to change";
    let at = tampered.windows(needle.len()).position(|it| it == needle).expect("the name in the pack");
    tampered[at] = b'T';
    let fresh = HashCons::<Leaf>::new();
    let loaded = fresh.load_pack(&Aligned::copy_from(&tampered), ());
    assert!(matches!(loaded, Err(FormatError::Invalid { .. })), "{loaded:?}");
    assert_eq!(fresh.lookup(&content_of(&leaf(1, "The one name to change"))), None, "interned before it was checked");
}

#[derive(Hash, PartialEq, Eq, Debug)]
enum Shape {
    Scalar { size: usize, align: u32 },
    Many(Vec<Shape>),
}

impl Consed for Shape {
    fn domain() -> &'static Domain {
        static DOMAIN: Domain = Domain::new("blaze v2 hashcons test shape 1");
        &DOMAIN
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        crate::hash_std(self, hasher);
    }
}

#[test]
fn hash_std_writes_fixed_width_little_endian() {
    let mut fed = Hasher::new("blaze v2 hashcons test std 1");
    crate::hash_std(&(7usize, -2isize, "ab", 3u16), &mut fed);
    let mut expected = Hasher::new("blaze v2 hashcons test std 1");
    expected
        .update(&7u64.to_le_bytes())
        .update(&(-2i64).to_le_bytes())
        .update(b"ab")
        .update(&[0xff])
        .update(&3u16.to_le_bytes());
    assert_eq!(fed.finish(), expected.finish());
}

#[test]
fn a_derived_hash_is_content() {
    let cons = HashCons::<Shape>::new();
    let shape = || Shape::Many(vec![Shape::Scalar { size: 4, align: 4 }, Shape::Scalar { size: 8, align: 8 }]);
    let a = cons.intern(shape());
    assert_eq!(cons.intern(shape()), a);
    assert_ne!(cons.intern(Shape::Scalar { size: 4, align: 4 }), a);
    assert_ne!(content_of(&Shape::Many(vec![])), content_of(&Shape::Many(vec![Shape::Many(vec![])])));
    assert_eq!(cons.len(), 2);
}

#[test]
fn sorted_is_in_content_order() {
    let cons = HashCons::<Leaf>::new();
    for n in (0..500).rev() {
        cons.intern(leaf(n, "sorted"));
    }
    let sorted = cons.sorted();
    assert_eq!(sorted.len(), 500);
    assert!(sorted.windows(2).all(|it| it[0].content() < it[1].content()));
}
