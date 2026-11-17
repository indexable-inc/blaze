//! blaze-impls against a naive model: a world of crates, their dependencies and their impls,
//! drawn from a tape, with every answer of the records compared to a scan of the world.
//!
//! - `candidates` and `inherent_candidates` equal a scan of every impl the asking crate loads,
//!   blanket impls included, in the documented order;
//! - a lookup that finds nothing reads no bucket, and each crate's directory at most once, and
//!   every record read is noted;
//! - adding an impl changes its one bucket and the directory of its crate, no other bucket;
//! - every record's bytes are the same under any order of their inputs;
//! - `NoImplsOf` never says "none" of a trait or bucket that has an impl, with the control
//!   that drops a bucket's marks and must be caught;
//! - a lang item is found across three crates, and two definitions are the typed error;
//! - the graph's closure, `depends_on` and `dependents` equal a naive traversal.

#![feature(allocator_api)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_impls::{
    BucketKey, BucketView, CrateFlags, CrateGraphView, CrateImpls, CrateIndex, CrateInput,
    DepInput, DirectoryView, EdgeFlags, Edition, ImplFlags, ImplIndexError, ImplRead,
    ImplRecords, ImplRow,
    Incoherent, IntWidth, LangError, LangItem, LangItemsError, LangItemsView, LangName,
    LangNameError, LangRead, LangRecords, MAX_LANG_NAME,
    Mutability, NoImplsOfView, RecordKind, SelfShape, Shape, build_crate_graph, build_impl_index,
    build_lang_items, candidates, inherent_candidates, lang_item,
};
use blaze_sim::{Broke, Config, Tape};

const TRAITS: usize = 3;
const ADTS: usize = 3;

const KIND_TRAIT: u8 = 1;
const KIND_ADT: u8 = 2;
const KIND_IMPL: u8 = 3;
const KIND_LANG: u8 = 4;

fn harness(what: impl fmt::Display) -> Broke {
    Broke::Harness(what.to_string())
}

/// The key of the `n`th crate: its prefix `n + 1`, never zero.
fn crate_key(n: usize) -> CrateItemKey {
    let mut bytes = ContentId::of("blaze-impls test crate", &n.to_le_bytes()).0;
    bytes[..8].copy_from_slice(&(n as u64 + 1).to_le_bytes());
    CrateItemKey(ContentId(bytes))
}

/// The key of the `n`th item of `kind` in `krate`.
fn item(krate: CrateItemKey, kind: u8, n: usize) -> ItemKey {
    let mut seed = [0u8; 41];
    seed[..32].copy_from_slice(&krate.0.0);
    seed[32] = kind;
    seed[33..].copy_from_slice(&(n as u64).to_le_bytes());
    let mut bytes = ContentId::of("blaze-impls test item", &seed).0;
    bytes[..8].copy_from_slice(&krate.prefix());
    bytes[8] &= !ItemKey::INNER;
    ItemKey(ContentId(bytes))
}

fn shape(of: Shape) -> SelfShape {
    match SelfShape::new(of) {
        Ok(shape) => shape,
        Err(error) => panic!("the test's shape {of:?} is refused: {error:?}"),
    }
}

/// The shapes with no definition the world files impls under.
fn plain_shapes() -> [SelfShape; 4] {
    [
        shape(Shape::Bool),
        shape(Shape::Int(IntWidth::I32)),
        shape(Shape::Ref(Mutability::Not)),
        shape(Shape::Tuple(2)),
    ]
}

/// Crates, their dependencies (on crates before them) and their impls.
#[derive(Clone, Debug)]
struct World {
    keys: Vec<CrateItemKey>,
    flags: Vec<CrateFlags>,
    deps: Vec<Vec<usize>>,
    impls: Vec<Vec<ImplRow>>,
}

impl World {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn trait_of(&self, krate: usize, n: usize) -> ItemKey {
        item(self.keys[krate], KIND_TRAIT, n)
    }

    fn adt_of(&self, krate: usize, n: usize) -> ItemKey {
        item(self.keys[krate], KIND_ADT, n)
    }

    /// What `of` loads, itself included: its dependencies' closures, through no proc-macro
    /// crate (one is loaded for its macros alone).
    fn closure(&self, of: usize) -> Vec<bool> {
        let mut seen = vec![false; self.len()];
        let mut stack = vec![of];
        seen[of] = true;
        while let Some(at) = stack.pop() {
            for &dep in &self.deps[at] {
                if !seen[dep] && !self.flags[dep].contains(CrateFlags::PROC_MACRO) {
                    seen[dep] = true;
                    stack.push(dep);
                }
            }
        }
        seen
    }

    fn members(&self, of: usize) -> Vec<usize> {
        let closure = self.closure(of);
        (0..self.len()).filter(|&at| closure[at]).collect()
    }

    /// A new impl of crate `krate`, its `n`th: of a trait and for a type it can name.
    fn draw_impl(&self, tape: &Tape, krate: usize, n: usize) -> ImplRow {
        let members = self.members(krate);
        let impl_ = item(self.keys[krate], KIND_IMPL, n);
        let header = ContentId::of("blaze-impls test header", &impl_.0.0);
        let lang = self.flags[krate].contains(CrateFlags::LANG);
        if tape.one_in(4, "inherent") {
            let shape = match lang && tape.coin("primitive") {
                true => plain_shapes()[tape.index(4, "primitive shape")],
                false => shape(Shape::Def(self.adt_of(krate, tape.index(ADTS, "own adt")))),
            };
            return ImplRow { impl_, trait_: None, shape, flags: ImplFlags::NONE, header };
        }
        let trait_crate = members[tape.index(members.len(), "trait crate")];
        let trait_ = self.trait_of(trait_crate, tape.index(TRAITS, "trait"));
        let shape = match tape.below(3, "shape kind") {
            0 => SelfShape::BLANKET,
            1 => plain_shapes()[tape.index(4, "plain shape")],
            _ => {
                let adt_crate = members[tape.index(members.len(), "adt crate")];
                shape(Shape::Def(self.adt_of(adt_crate, tape.index(ADTS, "adt"))))
            }
        };
        let flags = match ImplFlags::from_bits(tape.below(64, "flags") as u32) {
            Ok(flags) => flags,
            Err(error) => panic!("the test drew unknown impl flags: {error:?}"),
        };
        ImplRow { impl_, trait_: Some(trait_), shape, flags, header }
    }

    /// A shape a lookup may ask for: any the world files, a placeholder, or a type of any crate.
    fn draw_goal_shape(&self, tape: &Tape) -> SelfShape {
        match tape.below(4, "goal shape kind") {
            0 => SelfShape::BLANKET,
            1 => SelfShape::PLACEHOLDER,
            2 => plain_shapes()[tape.index(4, "goal plain shape")],
            _ => {
                let krate = tape.index(self.len(), "goal adt crate");
                shape(Shape::Def(self.adt_of(krate, tape.index(ADTS, "goal adt"))))
            }
        }
    }
}

fn draw_world(tape: &Tape) -> World {
    let n = 1 + tape.index(6, "crates");
    let mut world = World {
        keys: (0..n).map(crate_key).collect(),
        flags: Vec::new(),
        deps: Vec::new(),
        impls: Vec::new(),
    };
    for krate in 0..n {
        world.deps.push((0..krate).filter(|_| tape.coin("depends")).collect());
        world.flags.push(match krate {
            0 => CrateFlags::LANG.union(CrateFlags::LANG_ITEMS),
            _ if tape.one_in(5, "proc macro") => CrateFlags::PROC_MACRO,
            _ => CrateFlags::NONE,
        });
    }
    for krate in 0..n {
        let count = tape.index(7, "impls");
        let rows = (0..count).map(|at| world.draw_impl(tape, krate, at)).collect();
        world.impls.push(rows);
    }
    world
}

/// The world's crate graph's buffer, its dependencies named `d<n>`.
fn graph_bytes(world: &World, order: &[usize], dep_order: &[Vec<usize>]) -> Result<Aligned, Broke> {
    let names: Vec<Vec<String>> =
        dep_order.iter().map(|deps| deps.iter().map(|dep| format!("d{dep}")).collect()).collect();
    let deps: Vec<Vec<DepInput<'_>>> = dep_order
        .iter()
        .zip(&names)
        .map(|(deps, names)| {
            deps.iter()
                .zip(names)
                .map(|(&dep, name)| DepInput { on: world.keys[dep], name, flags: EdgeFlags::NONE })
                .collect()
        })
        .collect();
    let inputs: Vec<CrateInput<'_>> = order
        .iter()
        .map(|&krate| CrateInput {
            key: world.keys[krate],
            edition: Edition::E2021,
            flags: world.flags[krate],
            deps: &deps[krate],
        })
        .collect();
    let bytes = build_crate_graph(&inputs, blaze_own::testing::heap().alloc()).map_err(harness)?;
    Ok(Aligned::copy_from(&bytes))
}

fn graph_of(world: &World) -> Result<Aligned, Broke> {
    let order: Vec<usize> = (0..world.len()).collect();
    graph_bytes(world, &order, &world.deps)
}

fn open_graph(bytes: &Aligned) -> Result<CrateGraphView<'_>, Broke> {
    CrateGraphView::read(bytes).map_err(harness)
}

fn index_of(graph: &CrateGraphView<'_>, key: CrateItemKey) -> Result<CrateIndex, Broke> {
    graph.index_of(key).ok_or_else(|| harness(format!("the crate {key:x} is not in its graph")))
}

/// A crate's records as the store holds them.
struct Records {
    no_impls_of: Aligned,
    directory: Aligned,
    buckets: BTreeMap<ContentId, Aligned>,
    lang_items: Option<Aligned>,
}

impl Records {
    fn of(built: &CrateImpls<'_>) -> Records {
        Records {
            no_impls_of: Aligned::copy_from(&built.no_impls_of),
            directory: Aligned::copy_from(&built.directory),
            buckets: built.buckets.iter().map(|it| (it.id, Aligned::copy_from(&it.bytes))).collect(),
            lang_items: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Missing(&'static str);

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the store has no {}", self.0)
    }
}

/// Every crate's records, with the reads made of them and the reads noted.
#[derive(Default)]
struct Store {
    crates: BTreeMap<CrateItemKey, Records>,
    accessed: RefCell<Vec<(CrateItemKey, RecordKind)>>,
    noted: RefCell<Vec<ImplRead>>,
    /// Each lang read noted: the crate read, the name asked and the answer.
    lang_noted: RefCell<Vec<(CrateItemKey, String, Option<ItemKey>)>>,
    lang_accessed: RefCell<Vec<CrateItemKey>>,
}

impl Store {
    fn of(world: &World) -> Result<Store, Broke> {
        let mut store = Store::default();
        for (krate, rows) in world.keys.iter().zip(&world.impls) {
            let built = build_impl_index(*krate, rows, blaze_own::testing::heap().alloc()).map_err(harness)?;
            store.crates.insert(*krate, Records::of(&built));
        }
        Ok(store)
    }

    fn records(&self, krate: CrateItemKey) -> Result<&Records, Missing> {
        self.crates.get(&krate).ok_or(Missing("crate"))
    }

    fn clear(&self) {
        self.accessed.borrow_mut().clear();
        self.noted.borrow_mut().clear();
    }
}

impl ImplRecords for Store {
    type Error = Missing;

    fn no_impls_of(&self, krate: CrateItemKey) -> Result<&[u8], Missing> {
        self.accessed.borrow_mut().push((krate, RecordKind::NoImplsOf));
        Ok(&*self.records(krate)?.no_impls_of)
    }

    fn directory(&self, krate: CrateItemKey) -> Result<&[u8], Missing> {
        self.accessed.borrow_mut().push((krate, RecordKind::Directory));
        Ok(&*self.records(krate)?.directory)
    }

    fn bucket(&self, krate: CrateItemKey, bucket: ContentId) -> Result<&[u8], Missing> {
        self.accessed.borrow_mut().push((krate, RecordKind::Bucket));
        self.records(krate)?.buckets.get(&bucket).map(|it| &**it).ok_or(Missing("bucket"))
    }

    fn note(&self, read: ImplRead) {
        self.noted.borrow_mut().push(read);
    }
}

impl LangRecords for Store {
    type Error = Missing;

    fn lang_items(&self, krate: CrateItemKey) -> Result<&[u8], Missing> {
        self.lang_accessed.borrow_mut().push(krate);
        self.records(krate)?.lang_items.as_deref().ok_or(Missing("LangItems record"))
    }

    fn note(&self, read: LangRead<'_>) {
        self.lang_noted.borrow_mut().push((read.krate, read.name.as_str().to_owned(), read.found));
    }
}

/// The impls of `trait_` for `shape` that `from` loads, by a scan of the world, in the
/// documented order: by crate index, the shape's rows before the blanket ones, by impl key.
fn naive_candidates(
    world: &World,
    graph: &CrateGraphView<'_>,
    from: usize,
    trait_: ItemKey,
    goal: SelfShape,
) -> Result<Vec<ImplRow>, Broke> {
    let mut found = Vec::new();
    for krate in world.members(from) {
        let at = index_of(graph, world.keys[krate])?.get();
        for row in &world.impls[krate] {
            let exact = !goal.files_no_impl() && row.shape == goal;
            if row.trait_ == Some(trait_) && (exact || row.is_blanket()) {
                found.push((at, row.is_blanket(), row.impl_, *row));
            }
        }
    }
    found.sort_by_key(|it| (it.0, it.1, it.2));
    Ok(found.into_iter().map(|it| it.3).collect())
}

/// The inherent impls of `goal` that `from` loads, by a scan.
fn naive_inherent(
    world: &World,
    graph: &CrateGraphView<'_>,
    from: usize,
    goal: SelfShape,
) -> Result<Vec<ImplRow>, Broke> {
    let mut found = Vec::new();
    for krate in world.members(from) {
        let at = index_of(graph, world.keys[krate])?.get();
        for row in &world.impls[krate] {
            if row.trait_.is_none() && row.shape == goal && !goal.files_no_impl() {
                found.push((at, row.impl_, *row));
            }
        }
    }
    found.sort_by_key(|it| (it.0, it.1));
    Ok(found.into_iter().map(|it| it.2).collect())
}

struct Lookup {
    from: usize,
    trait_: ItemKey,
    goal: SelfShape,
}

fn draw_lookup(world: &World, tape: &Tape) -> Lookup {
    let from = tape.index(world.len(), "from");
    let trait_crate = tape.index(world.len(), "goal trait crate");
    // Half the time a trait an impl of the asking crate's closure has: a likely hit.
    let filed: Vec<ItemKey> =
        world.members(from).iter().flat_map(|&at| &world.impls[at]).filter_map(|row| row.trait_).collect();
    let trait_ = match tape.pick(&filed, "filed trait") {
        Some(&trait_) if tape.coin("filed") => trait_,
        _ => world.trait_of(trait_crate, tape.index(TRAITS, "goal trait")),
    };
    Lookup { from, trait_, goal: world.draw_goal_shape(tape) }
}

fn run_candidates(
    store: &Store,
    graph: &CrateGraphView<'_>,
    world: &World,
    lookup: &Lookup,
) -> Result<Vec<ImplRow>, Broke> {
    let from = index_of(graph, world.keys[lookup.from])?;
    candidates(graph, from, lookup.trait_, lookup.goal, store)
        .map_err(harness)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(harness)
}

#[test]
fn candidates_equal_a_naive_scan() {
    blaze_sim::check(Config::cases(512), |tape| {
        let world = draw_world(tape);
        let graph_buffer = graph_of(&world)?;
        let graph = open_graph(&graph_buffer)?;
        let store = Store::of(&world)?;
        for _ in 0..4 {
            let lookup = draw_lookup(&world, tape);
            let found = run_candidates(&store, &graph, &world, &lookup)?;
            let wanted = naive_candidates(&world, &graph, lookup.from, lookup.trait_, lookup.goal)?;
            if found != wanted {
                return Err(Broke::Oracle(format!(
                    "candidates of {} for {:?} from crate {} found {found:?}, a scan {wanted:?}",
                    lookup.trait_,
                    lookup.goal.shape(),
                    lookup.from
                )));
            }
            let from = index_of(&graph, world.keys[lookup.from])?;
            let found = inherent_candidates(&graph, from, lookup.goal, Incoherent::No, &store)
                .map_err(harness)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(harness)?;
            let wanted = naive_inherent(&world, &graph, lookup.from, lookup.goal)?;
            if found != wanted {
                return Err(Broke::Oracle(format!(
                    "inherent candidates for {:?} from crate {} found {found:?}, a scan {wanted:?}",
                    lookup.goal.shape(),
                    lookup.from
                )));
            }
        }
        Ok(())
    });
}

#[test]
fn a_miss_reads_no_bucket_and_every_read_is_noted() {
    blaze_sim::check(Config::cases(512), |tape| {
        let world = draw_world(tape);
        let graph_buffer = graph_of(&world)?;
        let graph = open_graph(&graph_buffer)?;
        let store = Store::of(&world)?;
        for _ in 0..4 {
            let lookup = draw_lookup(&world, tape);
            store.clear();
            let found = run_candidates(&store, &graph, &world, &lookup)?;
            let accessed = store.accessed.borrow().clone();
            let noted = store.noted.borrow().clone();
            // Each record read has its note, of the same crate and kind, in the same order.
            let noted_reads: Vec<(CrateItemKey, RecordKind)> = noted
                .iter()
                .filter_map(|read| match *read {
                    ImplRead::Filter { krate, .. } => Some((krate, RecordKind::NoImplsOf)),
                    ImplRead::Directory { krate, .. } => Some((krate, RecordKind::Directory)),
                    ImplRead::Bucket { krate, .. } => Some((krate, RecordKind::Bucket)),
                })
                .collect();
            for read in &accessed {
                if !noted_reads.contains(read) {
                    return Err(Broke::Oracle(format!("the read {read:?} was not noted: {noted:?}")));
                }
            }
            for &krate in &world.keys {
                let directories =
                    accessed.iter().filter(|it| **it == (krate, RecordKind::Directory)).count();
                if directories > 1 {
                    return Err(Broke::Oracle(format!(
                        "one lookup read the directory of {krate} {directories} times"
                    )));
                }
            }
            if found.is_empty() {
                if accessed.iter().any(|it| it.1 == RecordKind::Bucket) {
                    return Err(Broke::Oracle(format!("a miss read a bucket: {accessed:?}")));
                }
                if noted.iter().any(|read| matches!(read, ImplRead::Directory { found: Some(_), .. })) {
                    return Err(Broke::Oracle(format!("a miss found a bucket: {noted:?}")));
                }
            }
        }
        Ok(())
    });
}

/// `const impl<I: [const] Iterator<Item: IntoIterator>> Flatten<I> { .. }`
/// (core/src/iter/adapters/flatten.rs:189) is an inherent impl's own form: filed as any
/// inherent impl. An impl refused (an inherent `unsafe impl`, an impl given twice) is a
/// finding on that impl, and the crate's other impls are filed: one failure fails one impl.
#[test]
fn an_inherent_const_impl_is_filed_and_a_refused_impl_fails_alone() {
    let krate = crate_key(0);
    let flatten = shape(Shape::Def(item(krate, KIND_ADT, 0)));
    let row = |n: usize, trait_: Option<ItemKey>, flags: ImplFlags| ImplRow {
        impl_: item(krate, KIND_IMPL, n),
        trait_,
        shape: flatten,
        flags,
        header: ContentId::of("blaze-impls test header", &n.to_le_bytes()),
    };
    let const_inherent = row(0, None, ImplFlags::CONST);
    let unsafe_inherent = row(1, None, ImplFlags::UNSAFE);
    let twice = row(2, Some(item(krate, KIND_TRAIT, 0)), ImplFlags::NONE);
    let plain = row(3, Some(item(krate, KIND_TRAIT, 1)), ImplFlags::CONST);
    let rows = [twice, const_inherent, unsafe_inherent, plain, twice];
    let built = build_impl_index(krate, &rows, blaze_own::testing::heap().alloc())
        .unwrap_or_else(|error| panic!("one refused impl refused the crate's index: {error}"));
    let mut refused: Vec<ImplIndexError> = built.refused.iter().copied().collect();
    refused.sort_by_key(|it| format!("{it:?}"));
    let mut expected = vec![
        ImplIndexError::InherentFlags { impl_: unsafe_inherent.impl_, flags: ImplFlags::UNSAFE },
        ImplIndexError::DuplicateImpl { impl_: twice.impl_ },
    ];
    expected.sort_by_key(|it| format!("{it:?}"));
    assert_eq!(refused, expected);
    let filed: Vec<ImplRow> = built
        .buckets
        .iter()
        .flat_map(|bucket| {
            let bytes = Aligned::copy_from(&bucket.bytes);
            let view = BucketView::read(&bytes).expect("a built bucket reads back");
            (0..view.len())
                .map(|at| view.row(at).expect("a row in range").expect("a built row reads back"))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(filed.contains(&const_inherent), "{filed:?}");
    assert!(filed.contains(&plain), "{filed:?}");
    assert_eq!(filed.len(), 2, "{filed:?}");
    // The same rows in another order write the same index and the same findings.
    let reversed: Vec<ImplRow> = rows.iter().rev().copied().collect();
    let again = build_impl_index(krate, &reversed, blaze_own::testing::heap().alloc()).expect("as above");
    assert_eq!(again.directory, built.directory);
    assert_eq!(again.refused[..], built.refused[..]);
}

/// Each bucket's key and content id, by key.
fn bucket_ids(built: &CrateImpls<'_>) -> BTreeMap<BucketKey, ContentId> {
    built.buckets.iter().map(|it| (it.key, it.id)).collect()
}

#[test]
fn adding_an_impl_changes_its_one_bucket_and_the_directory() {
    blaze_sim::check(Config::cases(512), |tape| {
        let world = draw_world(tape);
        let krate = tape.index(world.len(), "crate");
        let key = world.keys[krate];
        let rows = &world.impls[krate];
        let added = world.draw_impl(tape, krate, rows.len());
        let before = build_impl_index(key, rows, blaze_own::testing::heap().alloc()).map_err(harness)?;
        let mut more = rows.clone();
        more.push(added);
        let after = build_impl_index(key, &more, blaze_own::testing::heap().alloc()).map_err(harness)?;
        let changed = BucketKey::of(&added);
        let mut old = bucket_ids(&before);
        let mut new = bucket_ids(&after);
        let (was, is) = (old.remove(&changed), new.remove(&changed));
        if old != new {
            return Err(Broke::Oracle(format!(
                "adding {added:?} changed buckets other than its own {changed:?}"
            )));
        }
        if is.is_none() || was == is {
            return Err(Broke::Oracle(format!("adding {added:?} left its bucket as it was")));
        }
        if before.directory == after.directory {
            return Err(Broke::Oracle(format!("adding {added:?} left the directory as it was")));
        }
        Ok(())
    });
}

#[test]
fn bytes_are_the_same_under_any_order() {
    blaze_sim::check(Config::cases(512), |tape| {
        let world = draw_world(tape);
        for (krate, rows) in world.keys.iter().zip(&world.impls) {
            let mut shuffled = rows.clone();
            tape.shuffle(&mut shuffled, "rows");
            let first = build_impl_index(*krate, rows, blaze_own::testing::heap().alloc()).map_err(harness)?;
            let second = build_impl_index(*krate, &shuffled, blaze_own::testing::heap().alloc()).map_err(harness)?;
            let same = first.directory == second.directory
                && first.no_impls_of == second.no_impls_of
                && first.buckets.len() == second.buckets.len()
                && first.buckets.iter().zip(&second.buckets).all(|(a, b)| a.bytes == b.bytes);
            if !same {
                return Err(Broke::Oracle(format!("the impl index of {krate} moved with its rows' order")));
            }
        }
        let mut order: Vec<usize> = (0..world.len()).collect();
        tape.shuffle(&mut order, "crates");
        let mut deps = world.deps.clone();
        for list in &mut deps {
            tape.shuffle(list, "deps");
        }
        if *graph_of(&world)? != *graph_bytes(&world, &order, &deps)? {
            return Err(Broke::Oracle("the crate graph moved with its inputs' order".to_owned()));
        }
        Ok(())
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    AsBuilt,
    /// `NoImplsOf` built without one bucket's rows: a bucket it says is not there.
    DropsABucket,
}

fn no_impls_of_never_says_none(tape: &Tape, filter: Filter) -> Result<(), Broke> {
    let world = draw_world(tape);
    for (krate, rows) in world.keys.iter().zip(&world.impls) {
        let built = build_impl_index(*krate, rows, blaze_own::testing::heap().alloc()).map_err(harness)?;
        let filter_bytes = match (filter, tape.pick(rows, "dropped bucket")) {
            (Filter::DropsABucket, Some(dropped)) => {
                let dropped = BucketKey::of(dropped);
                let kept: Vec<ImplRow> =
                    rows.iter().copied().filter(|row| BucketKey::of(row) != dropped).collect();
                build_impl_index(*krate, &kept, blaze_own::testing::heap().alloc()).map_err(harness)?.no_impls_of
            }
            _ => built.no_impls_of.clone(),
        };
        let bytes = Aligned::copy_from(&filter_bytes);
        let view = NoImplsOfView::read(&bytes).map_err(harness)?;
        for row in rows {
            if let Some(trait_) = row.trait_ {
                if !view.may_hold_trait(trait_) {
                    return Err(Broke::Oracle(format!("NoImplsOf of {krate} said none of {trait_}, which {row:?} is of")));
                }
            }
            let bucket = BucketKey::of(row);
            if !view.may_hold_bucket(bucket.hash()) {
                return Err(Broke::Oracle(format!("NoImplsOf of {krate} said none of {bucket:?}, which {row:?} is in")));
            }
        }
        // And the directory agrees: every row is in the bucket it names.
        let directory_bytes = Aligned::copy_from(&built.directory);
        let directory = DirectoryView::read(&directory_bytes).map_err(harness)?;
        for row in rows {
            let key = BucketKey::of(row);
            let hit = directory
                .lookup(key.hash())
                .ok_or_else(|| Broke::Oracle(format!("the directory of {krate} has no bucket for {row:?}")))?;
            let bucket = built
                .buckets
                .iter()
                .find(|it| it.id == hit.bucket)
                .ok_or_else(|| harness("a directory names a bucket not built"))?;
            let bucket_bytes = Aligned::copy_from(&bucket.bytes);
            let view = BucketView::read(&bucket_bytes).map_err(harness)?;
            let read = view.find(row.impl_).map(|it| it.map_err(harness)).transpose()?;
            if view.key() != key || read != Some(*row) {
                return Err(Broke::Oracle(format!("the bucket of {row:?} read back {read:?}")));
            }
        }
    }
    Ok(())
}

#[test]
fn no_impls_of_never_says_none_when_there_is_one() {
    blaze_sim::control(
        Config::cases(256),
        Filter::AsBuilt,
        Filter::DropsABucket,
        "said none",
        no_impls_of_never_says_none,
    );
}

/// core, alloc and std, each defining lang items, and a user crate depending on std.
struct Sysroot {
    store: Store,
    graph: Aligned,
    keys: [CrateItemKey; 4],
}

fn lang(text: &str) -> LangName {
    match LangName::new(text) {
        Ok(name) => name,
        Err(error) => panic!("the test's lang item name `{text}` is refused: {error}"),
    }
}

fn sysroot(extra_in_alloc: &[&str]) -> Sysroot {
    let keys = [crate_key(0), crate_key(1), crate_key(2), crate_key(3)];
    let defined: [&[&str]; 3] = [&["sized", "copy"], &["owned_box"], &["termination"]];
    let mut store = Store::default();
    for (at, names) in defined.iter().enumerate() {
        let mut items: Vec<(LangName, ItemKey)> =
            names.iter().enumerate().map(|(n, name)| (lang(name), item(keys[at], KIND_LANG, n))).collect();
        if at == 1 {
            items.extend(extra_in_alloc.iter().enumerate().map(|(n, name)| (lang(name), item(keys[at], KIND_LANG, 100 + n))));
        }
        let bytes = match build_lang_items(keys[at], &items, blaze_own::testing::heap().alloc()) {
            Ok(bytes) => bytes,
            Err(error) => panic!("the sysroot's lang items are refused: {error}"),
        };
        store.crates.insert(
            keys[at],
            Records {
                no_impls_of: Aligned::copy_from(&[]),
                directory: Aligned::copy_from(&[]),
                buckets: BTreeMap::new(),
                lang_items: Some(Aligned::copy_from(&bytes)),
            },
        );
    }
    let lang_items = CrateFlags::LANG.union(CrateFlags::LANG_ITEMS);
    let dep = |on: usize, name: &'static str| DepInput { on: keys[on], name, flags: EdgeFlags::SYSROOT };
    let alloc_deps = [dep(0, "core")];
    let std_deps = [dep(0, "core"), dep(1, "alloc")];
    let user_deps = [dep(2, "std"), dep(0, "core")];
    let inputs = [
        CrateInput { key: keys[3], edition: Edition::E2024, flags: CrateFlags::NONE, deps: &user_deps },
        CrateInput { key: keys[0], edition: Edition::E2024, flags: lang_items, deps: &[] },
        CrateInput { key: keys[2], edition: Edition::E2024, flags: lang_items, deps: &std_deps },
        CrateInput { key: keys[1], edition: Edition::E2024, flags: lang_items, deps: &alloc_deps },
    ];
    let graph = match build_crate_graph(&inputs, blaze_own::testing::heap().alloc()) {
        Ok(bytes) => Aligned::copy_from(&bytes),
        Err(error) => panic!("the sysroot's graph is refused: {error}"),
    };
    Sysroot { store, graph, keys }
}

#[test]
fn a_lang_item_is_found_across_three_crates() {
    let sysroot = sysroot(&[]);
    let graph = match CrateGraphView::read(&sysroot.graph) {
        Ok(graph) => graph,
        Err(error) => panic!("the sysroot's graph does not open: {error}"),
    };
    let index = |key| match graph.index_of(key) {
        Some(index) => index,
        None => panic!("the crate {key} is not in the sysroot's graph"),
    };
    let user = index(sysroot.keys[3]);
    let find = |from, name| match lang_item(&graph, from, lang(name), &sysroot.store) {
        Ok(found) => found,
        Err(error) => panic!("the lang item `{name}` was not looked up: {error}"),
    };
    let at = |krate: usize, n| Some(LangItem { krate: sysroot.keys[krate], item: item(sysroot.keys[krate], KIND_LANG, n) });
    assert_eq!(find(user, "sized"), at(0, 0));
    assert_eq!(find(user, "copy"), at(0, 1));
    assert_eq!(find(user, "owned_box"), at(1, 0));
    assert_eq!(find(user, "termination"), at(2, 0));
    assert_eq!(find(user, "unknown"), None);
    // core loads none of the others, so it sees none of theirs.
    assert_eq!(find(index(sysroot.keys[0]), "owned_box"), None);
    // The user crate defines no lang items, so it is not read; the three are, each noted.
    let accessed = sysroot.store.lang_accessed.borrow().clone();
    assert!(!accessed.contains(&sysroot.keys[3]), "a crate without lang items was read");
    let noted = sysroot.store.lang_noted.borrow().clone();
    let termination: Vec<_> = noted.into_iter().filter(|read| read.1 == "termination").collect();
    assert_eq!(termination.len(), 3, "each of the three crates is read once: {termination:?}");
    assert!(termination.iter().filter(|read| read.2.is_some()).count() == 1);
}

/// A lang item name is stored out of line: `core`'s `panic_misaligned_pointer_dereference`
/// (36 bytes) was refused by version 1's fixed 32-byte field. Names of 1 to 256 bytes go in
/// and are found again by a binary search over the record; 257 bytes and an empty name are
/// refused by name.
#[test]
fn a_long_lang_item_name_round_trips() {
    let krate = crate_key(7);
    let long = "x".repeat(blaze_surface::MAX_LANG_NAME);
    let names = ["panic_misaligned_pointer_dereference", "sized", "a", long.as_str()];
    let items: Vec<(LangName<'_>, ItemKey)> =
        names.iter().enumerate().map(|(n, name)| (lang(name), item(krate, KIND_LANG, n))).collect();
    let bytes = match build_lang_items(krate, &items, blaze_own::testing::heap().alloc()) {
        Ok(bytes) => Aligned::copy_from(&bytes),
        Err(error) => panic!("the long names are refused: {error}"),
    };
    let view = match LangItemsView::read(&bytes) {
        Ok(view) => view,
        Err(error) => panic!("the record does not read back: {error}"),
    };
    for (n, name) in names.iter().enumerate() {
        assert_eq!(view.get(lang(name)), Some(item(krate, KIND_LANG, n)), "{name}");
    }
    assert_eq!(view.get(lang("panic_misaligned_pointer_dereferenc")), None);
    let listed: Vec<&str> = view.iter().map(|(name, _)| name.as_str()).collect();
    let mut sorted = names.to_vec();
    sorted.sort_unstable();
    assert_eq!(listed, sorted);
    let too_long = "x".repeat(blaze_surface::MAX_LANG_NAME + 1);
    assert_eq!(LangName::new(&too_long), Err(LangNameError::TooLong { len: too_long.len() }));
    assert_eq!(LangName::new(""), Err(LangNameError::Empty));
}

#[test]
fn a_duplicate_lang_item_is_the_typed_error() {
    let sysroot = sysroot(&["sized"]);
    let graph = match CrateGraphView::read(&sysroot.graph) {
        Ok(graph) => graph,
        Err(error) => panic!("the sysroot's graph does not open: {error}"),
    };
    let user = match graph.index_of(sysroot.keys[3]) {
        Some(index) => index,
        None => panic!("the user crate is not in the sysroot's graph"),
    };
    let found = lang_item(&graph, user, lang("sized"), &sysroot.store);
    let first = LangItem { krate: sysroot.keys[0], item: item(sysroot.keys[0], KIND_LANG, 0) };
    let second = LangItem { krate: sysroot.keys[1], item: item(sysroot.keys[1], KIND_LANG, 100) };
    assert_eq!(found, Err(LangError::Duplicate { name: lang("sized"), first, second }));
    // Within one crate, the builder refuses it.
    let krate = crate_key(9);
    let (a, b) = (item(krate, KIND_LANG, 0), item(krate, KIND_LANG, 1));
    let refused = build_lang_items(krate, &[(lang("copy"), b), (lang("copy"), a)], blaze_own::testing::heap().alloc());
    let (first, second) = if a < b { (a, b) } else { (b, a) };
    assert_eq!(refused, Err(LangItemsError::Duplicate { krate, name: lang("copy"), first, second }));
}

#[test]
fn graph_closure_and_depends_on_match_a_naive_traversal() {
    blaze_sim::check(Config::cases(512), |tape| {
        let world = draw_world(tape);
        let graph_buffer = graph_of(&world)?;
        let graph = open_graph(&graph_buffer)?;
        if graph.len() != world.len() {
            return Err(Broke::Oracle(format!("a graph of {} crates has {}", world.len(), graph.len())));
        }
        let indices: Vec<CrateIndex> =
            world.keys.iter().map(|&key| index_of(&graph, key)).collect::<Result<_, _>>()?;
        for a in 0..world.len() {
            let ia = indices[a];
            if graph.key(ia) != world.keys[a] || graph.flags(ia) != world.flags[a] {
                return Err(Broke::Oracle(format!("crate {a} reads back as another")));
            }
            if graph.crate_of_prefix(world.keys[a].prefix()) != Some(ia) {
                return Err(Broke::Oracle(format!("crate {a}'s prefix names another crate")));
            }
            let naive = world.closure(a);
            let mut wanted: Vec<CrateIndex> =
                (0..world.len()).filter(|&b| naive[b]).map(|b| indices[b]).collect();
            wanted.sort();
            let found: Vec<CrateIndex> = graph.closure(ia).collect();
            if found != wanted {
                return Err(Broke::Oracle(format!("the closure of {a} is {found:?}, a traversal {wanted:?}")));
            }
            let mut dependents: Vec<CrateIndex> =
                (0..world.len()).filter(|&b| world.closure(b)[a]).map(|b| indices[b]).collect();
            dependents.sort();
            let found: Vec<CrateIndex> = graph.dependents(ia).collect();
            if found != dependents {
                return Err(Broke::Oracle(format!("the dependents of {a} are {found:?}, a traversal {dependents:?}")));
            }
            for b in 0..world.len() {
                if graph.depends_on(ia, indices[b]) != naive[b] {
                    return Err(Broke::Oracle(format!("depends_on({a}, {b}) is not {}", naive[b])));
                }
            }
            let mut deps: Vec<(String, CrateIndex)> =
                world.deps[a].iter().map(|&dep| (format!("d{dep}"), indices[dep])).collect();
            deps.sort();
            let found: Vec<(String, CrateIndex)> = graph.deps(ia).map(|dep| (dep.name.to_owned(), dep.on)).collect();
            if found != deps {
                return Err(Broke::Oracle(format!("the deps of {a} read back {found:?}, not {deps:?}")));
            }
            if graph.deps(ia).any(|dep| dep.on >= ia) {
                return Err(Broke::Oracle(format!("crate {a} comes before a crate it depends on")));
            }
        }
        Ok(())
    });
}
