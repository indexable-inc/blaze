use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Barrier, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{FormatError, Persist, Record, SchemaHash, Slice, Writer};
use blaze_ids::{
    ContentId, Fingerprint,
    bits::{BitSet, Bits},
};
use blaze_image::{Builder, Cause, Corrupt, Image, Refused, SectionId};
use blaze_vfs::{Bytes, Mode, View, ViewId};
use intern::Live;
use parking_lot::Mutex;

use crate::{
    Cancelled, Changed, Config, Cx, Database, DbError, Edge, Input, Mark, MemTree, Names as Lent,
    Query, QueryKey, QueryKind, Stored, Tree, kinds,
    memo::MemoId,
    subtree::{Digest, Subtree, SubtreeKey},
    table::{Aligned, EdgeRecord},
};

#[derive(Clone, Copy)]
#[repr(C)]
struct Word(u64);

const WORD: SchemaHash = SchemaHash(ContentId([0x57; 32]));

// SAFETY: a `u64` in a `repr(C)` struct: no padding, no pointer, every bit pattern valid.
unsafe impl Record for Word {
    const SCHEMA: SchemaHash = WORD;
    fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }
}

struct Num(u64);

impl Persist for Num {
    const SCHEMA: SchemaHash = WORD;
    type Ref<'a> = u64;
    fn write(&self, out: &mut Writer) -> u64 {
        out.push(&Word(self.0))
    }
    fn read(buffer: &[u8]) -> Result<u64, FormatError> {
        blaze_format::open::<Word>(buffer).map(|word| word.0)
    }
}

thread_local! {
    static RUNS: RefCell<HashMap<&'static str, usize>> = RefCell::new(HashMap::new());
}

fn ran(name: &'static str) {
    RUNS.with(|runs| *runs.borrow_mut().entry(name).or_default() += 1);
}

fn runs(name: &'static str) -> usize {
    RUNS.with(|runs| runs.borrow().get(name).copied().unwrap_or(0))
}

fn reset() {
    RUNS.with(|runs| runs.borrow_mut().clear());
}

fn key_id(key: &str) -> ContentId {
    ContentId::of("blaze v2 db test key 1", key.as_bytes())
}

/// The length of a file, `u64::MAX` for none.
struct Len;

impl Query for Len {
    const KIND: QueryKind = QueryKind(0x2f00);
    const NAME: &'static str = "len";
    const PRODUCER: ContentId = ContentId([1; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("len");
        Ok(Num(match cx.file(key)? {
            Some(file) => file.bytes.len() as u64,
            None => u64::MAX,
        }))
    }
}

/// Twice [`Len`].
struct Double;

impl Query for Double {
    const KIND: QueryKind = QueryKind(0x2f01);
    const NAME: &'static str = "double";
    const PRODUCER: ContentId = ContentId([2; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("double");
        Ok(Num(cx.get::<Len>(key)?.wrapping_mul(2)))
    }
}

/// The file names another file; 1 + [`Must`] of it, 0 for an empty file.
struct Follow;

impl Query for Follow {
    const KIND: QueryKind = QueryKind(0x2f02);
    const NAME: &'static str = "follow";
    const PRODUCER: ContentId = ContentId([3; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("follow");
        let name = match cx.file(key)? {
            Some(file) => String::from_utf8(file.bytes.to_vec()).expect("a name"),
            None => String::new(),
        };
        if name.is_empty() {
            return Ok(Num(0));
        }
        Ok(Num(1 + cx.get::<Must>(&name)?))
    }
}

/// The length of a file that must be there: asked of one that is gone, it panics, as a query
/// asked of an item that no longer exists does (#350's `AstIdMap` panic).
struct Must;

impl Query for Must {
    const KIND: QueryKind = QueryKind(0x2f03);
    const NAME: &'static str = "must";
    const PRODUCER: ContentId = ContentId([4; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("must");
        let file = cx.file(key)?.unwrap_or_else(|| panic!("must was asked of {key}, which is gone"));
        Ok(Num(file.bytes.len() as u64))
    }
}

static GATE: AtomicUsize = AtomicUsize::new(0);
static BARRIER: OnceLock<Barrier> = OnceLock::new();

/// The file names another; 1 + [`Chain`] of it, 0 for an empty file. Its first two runs wait
/// for each other, to make two runs hold a key each before asking the other's.
struct Chain;

impl Query for Chain {
    const KIND: QueryKind = QueryKind(0x2f04);
    const NAME: &'static str = "chain";
    const PRODUCER: ContentId = ContentId([5; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        if GATE.fetch_add(1, Ordering::SeqCst) < 2 {
            BARRIER.get_or_init(|| Barrier::new(2)).wait();
        }
        let name = match cx.file(key)? {
            Some(file) => String::from_utf8(file.bytes.to_vec()).expect("a name"),
            None => String::new(),
        };
        if name.is_empty() {
            return Ok(Num(0));
        }
        Ok(Num(1 + cx.get::<Chain>(&name)?))
    }
}

/// The length of the view's configuration.
struct Conf;

impl Query for Conf {
    const KIND: QueryKind = QueryKind(0x2f05);
    const NAME: &'static str = "conf";
    const PRODUCER: ContentId = ContentId([6; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ran("conf");
        Ok(Num(cx.config()?.len() as u64))
    }
}

/// How many names a directory has.
struct Names;

impl Query for Names {
    const KIND: QueryKind = QueryKind(0x2f06);
    const NAME: &'static str = "names";
    const PRODUCER: ContentId = ContentId([7; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("names");
        Ok(Num(cx.dir(key)?.map_or(u64::MAX, |names| names.len() as u64)))
    }
}

const FACT: QueryKey = QueryKey { kind: QueryKind(0x1fff), input: ContentId([9; 32]) };

/// Reads a fact no query answers, as an index the caller holds would.
struct ReadsFact;

impl Query for ReadsFact {
    const KIND: QueryKind = QueryKind(0x2f07);
    const NAME: &'static str = "reads fact";
    const PRODUCER: ContentId = ContentId([8; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ran("reads fact");
        cx.read(FACT, Fingerprint(1));
        Ok(Num(1))
    }
}

const TAGS: u64 = 1 << 40;

fn config(tree: &Arc<MemTree>) -> Config {
    Config { tree: Some(tree.clone()), dir: FileName::new("db"), tags: TAGS }
}

fn register(db: &mut Database) {
    db.register::<Len>();
    db.register::<Double>();
    db.register::<Follow>();
    db.register::<Must>();
    db.register::<Chain>();
    db.register::<Conf>();
    db.register::<Names>();
    db.register::<ReadsFact>();
    db.register::<TreeOf>();
}

fn setup() -> (Database, Arc<MemTree>) {
    reset();
    let tree = Arc::new(MemTree::default());
    let mut db = Database::new(config(&tree));
    register(&mut db);
    (db, tree)
}

fn view(db: &Database, tree: &MemTree, files: &[(&str, &str)]) -> ViewId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (*path, bytes.as_bytes())).collect();
    db.open(View { root: tree.root(&files) })
}

fn key<Q: Query<Key = String>>(name: &str) -> QueryKey {
    QueryKey { kind: Q::KIND, input: Q::input(&name.to_owned()) }
}

fn double(db: &Database, view: ViewId, name: &str) -> u64 {
    db.pin().get::<Double>(view, &name.to_owned()).expect("double")
}

fn mapping(bytes: &[u8]) -> Mapping {
    Mapping::copy(bytes)
}

/// Takes the one write in `effects`.
fn written(effects: &mut Vec<Effect>) -> (FileName, Vec<u8>, Tag) {
    assert_eq!(effects.len(), 1, "{effects:?}");
    match effects.pop() {
        Some(Effect::Write { file, bytes, tag }) => (file, bytes, tag),
        other => panic!("not a write: {other:?}"),
    }
}

fn mapped(effects: &mut Vec<Effect>) -> (FileName, Tag) {
    assert_eq!(effects.len(), 1, "{effects:?}");
    match effects.pop() {
        Some(Effect::Map { file, tag }) => (file, tag),
        other => panic!("not a map: {other:?}"),
    }
}

#[test]
fn equal_inputs_share_results_across_views() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, one, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (1, 1));

    let other_files = view(&db, &tree, &[("a", "xyz"), ("b", "else")]);
    assert_eq!(double(&db, other_files, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (1, 1), "a's inputs are equal: nothing reruns");

    let shorter = view(&db, &tree, &[("a", "xy")]);
    assert_eq!(double(&db, shorter, "a"), 4);
    assert_eq!((runs("len"), runs("double")), (2, 2));

    assert_eq!(double(&db, one, "a"), 6, "the first view still has its own result");
    let again = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, again, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (2, 2), "both results are kept at once");
}

#[test]
fn an_equal_fingerprint_stops_the_rerun() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, one, "a"), 6);
    let edited = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, edited, "a"), 6);
    assert_eq!(runs("len"), 2, "the file changed: its length is asked again");
    assert_eq!(runs("double"), 1, "its length did not: early cutoff");
}

#[test]
fn verification_stops_at_the_first_changed_edge() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "b"), ("b", "12")]);
    assert_eq!(db.pin().get::<Follow>(one, &"a".to_owned()), Ok(3));
    // `b` is gone and `a` names `c`: verifying follow(a) must stop at `a`, never ask must(b).
    let two = view(&db, &tree, &[("a", "c"), ("c", "1")]);
    assert_eq!(db.pin().get::<Follow>(two, &"a".to_owned()), Ok(2));
    assert_eq!(runs("must"), 2);
    assert_eq!(runs("follow"), 2);
}

#[test]
fn why_names_the_chain_back_to_the_input() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let first = db.why(one, &key::<Double>("a")).expect("it ran");
    assert!(first.chain.is_empty(), "a first run has nothing that changed: {first:?}");

    let longer = view(&db, &tree, &[("a", "abcd")]);
    double(&db, longer, "a");
    let why = db.why(longer, &key::<Double>("a")).expect("it reran");
    let keys: Vec<QueryKey> = why.chain.iter().map(|changed| changed.key).collect();
    assert_eq!(keys, vec![key::<Len>("a"), Input::File("a".to_owned()).key()]);
    assert_eq!(why.input, Some(Input::File("a".to_owned())));
    let Changed { was, now, .. } = why.chain[0];
    assert_ne!(was, now);

    let same = view(&db, &tree, &[("a", "abc")]);
    double(&db, same, "a");
    assert_eq!(db.why(same, &key::<Double>("a")), None, "verified, not run");
}

#[test]
fn an_evicted_value_is_made_again_and_cutoff_holds() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    assert_eq!(db.evict(&key::<Len>("a")), 1);
    assert_eq!(db.memo(&key::<Len>("a")).map(|memo| memo.value), Some(Stored::Evicted));

    let edited = view(&db, &tree, &[("a", "xyz")]);
    assert_eq!(double(&db, edited, "a"), 6);
    assert_eq!(runs("double"), 1, "eviction kept len's fingerprint, so double is cut off");

    let same = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(db.pin().get::<Len>(same, &"a".to_owned()), Ok(3));
    assert_eq!(runs("len"), 3, "the evicted value was made again");
}

#[test]
fn collection_drops_what_nothing_reaches() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let mut effects = Vec::new();
    db.collect(&[], &mut effects);
    assert!(db.memo(&key::<Double>("a")).is_some(), "an open view reaches it");
    assert!(effects.is_empty(), "what an open view uses stays in memory: {effects:?}");

    db.close(one);
    db.collect(&[key::<Double>("a")], &mut effects);
    assert!(db.memo(&key::<Double>("a")).is_some(), "kept");
    assert!(db.memo(&key::<Len>("a")).is_some(), "reached from a kept key");

    effects.clear();
    db.collect(&[], &mut effects);
    assert_eq!(db.memo(&key::<Double>("a")), None);
    assert_eq!(db.memo(&key::<Len>("a")), None);
}

#[test]
fn cold_values_move_to_disk_and_read_from_it() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    db.close(one);
    let mut effects = Vec::new();
    db.collect(&[key::<Double>("a")], &mut effects);
    let (file, bytes, tag) = written(&mut effects);
    assert_eq!(file.as_str(), "db/cold/0");
    assert!(db.owns(tag));
    db.complete(Completion { tag, done: Done::Written }, &mut effects);
    let (file, tag) = mapped(&mut effects);
    assert_eq!(file.as_str(), "db/cold/0");
    db.complete(Completion { tag, done: Done::Mapped(mapping(&bytes)) }, &mut effects);
    assert!(matches!(db.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));

    let again = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, again, "a"), 6);
    assert_eq!(runs("double"), 1, "read from the cold pack, not made again");
}

/// The database's sections of an image, as the daemon writes them (it adds the symbol table
/// and its own sections, which the database never reads).
fn image_of(db: &Database, builder: Builder) -> (Vec<u8>, Live) {
    let mut builder = builder;
    let mut live = Live::new();
    db.write_image(&mut builder, &mut live).expect("every kind is registered");
    (builder.finish().expect("a small image"), live)
}

fn opened(bytes: &[u8]) -> Arc<Image<Mapping>> {
    Arc::new(Image::open(mapping(bytes)).expect("an image this blaze wrote"))
}

#[test]
fn the_image_holds_the_database() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());

    let mut started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    register(&mut started);
    reset();
    let same = view(&started, &tree, &[("a", "abc")]);
    assert_eq!(double(&started, same, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (0, 0), "every memo verified from the image");
    assert!(matches!(started.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));

    let longer = view(&started, &tree, &[("a", "abcd")]);
    assert_eq!(double(&started, longer, "a"), 8);
    assert_eq!((runs("len"), runs("double")), (1, 1));
    assert_eq!(started.take_corrupt(), vec![], "nothing read was corrupt");

    assert_eq!(image_of(&db, Builder::new()).0, bytes, "the same database writes the same image");
}

/// Values name symbols by their IDs in the image's symbol table, so an image is used only
/// after the table was restored from it.
#[test]
fn an_image_whose_symbols_were_not_restored_is_refused() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    assert_eq!(intern::restored_from(), None, "no test restores the process's symbols");
    assert_eq!(Database::from_image_with(config(&tree), opened(&bytes)).err(), Some(DbError::Symbols));
}

#[test]
fn an_image_without_the_databases_sections_is_refused() {
    let empty = Builder::new().finish().expect("an image");
    assert!(matches!(
        Database::open_image(Config::memory(), opened(&empty)),
        Err(DbError::Refused(Refused::Missing { id: SectionId::DB_MEMOS }))
    ));
    let mut other = Builder::new();
    other.add(SectionId::DB_MEMOS, blaze_format::schema("blaze v2 db memos 1"), &[]);
    let other = other.finish().expect("an image");
    let error = Database::open_image(Config::memory(), opened(&other)).err();
    assert!(matches!(error, Some(DbError::Refused(Refused::Schema { id: SectionId::DB_MEMOS, .. }))), "{error:?}");
    assert!(error.is_some_and(|error| error.is_image()));
}

#[test]
fn a_kind_with_memos_must_be_registered_to_write_an_image() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    let started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    let mut live = Live::new();
    let error = started.write_image(&mut Builder::new(), &mut live).err();
    assert!(matches!(error, Some(DbError::Unknown(_))), "{error:?}");
}

#[test]
fn settling_swaps_to_the_mapped_image() {
    let (mut db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let (bytes, _) = image_of(&db, Builder::new());
    let mut effects = Vec::new();
    assert_eq!(db.settle(opened(&bytes), &mut effects), Ok(()));
    assert!(effects.is_empty(), "no cold packs to remove: {effects:?}");

    assert_eq!(double(&db, one, "a"), 6, "the view stays open across the swap");
    assert_eq!((runs("len"), runs("double")), (1, 1));
    assert!(matches!(db.memo(&key::<Double>("a")).map(|memo| memo.value), Some(Stored::Disk(_))));
    db.reclaim();
}

/// A cold pack still being written (or, `mapping_it`, mapped) when the database settles names the
/// memos of the store it left (01251): once its I/O is done it is removed, never read into
/// the image's store, and the image's memos serve.
fn settle_with_a_cold_pack_in_flight(mapping_it: bool) {
    let (mut db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    db.close(one);
    let mut effects = Vec::new();
    db.collect(&[key::<Double>("a")], &mut effects);
    let (file, bytes, mut tag) = written(&mut effects);
    if mapping_it {
        db.complete(Completion { tag, done: Done::Written }, &mut effects);
        tag = mapped(&mut effects).1;
    }
    let (image, _) = image_of(&db, Builder::new());
    assert_eq!(db.settle(opened(&image), &mut effects), Ok(()));
    assert!(effects.is_empty(), "no pack was mapped yet: {effects:?}");

    let done = if mapping_it { Done::Mapped(mapping(&bytes)) } else { Done::Written };
    db.complete(Completion { tag, done }, &mut effects);
    assert_eq!(effects.len(), 1, "{effects:?}");
    let Some(Effect::Remove { file: removed, tag }) = effects.pop() else { panic!("not a remove") };
    assert_eq!(removed.as_str(), file.as_str());
    db.complete(Completion { tag, done: Done::Removed }, &mut effects);
    assert!(effects.is_empty(), "{effects:?}");

    let again = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, again, "a"), 6);
    assert_eq!((runs("len"), runs("double")), (1, 1), "verified from the image");
    db.reclaim();
}

#[test]
fn a_cold_pack_written_across_a_settle_is_removed() {
    settle_with_a_cold_pack_in_flight(false);
}

#[test]
fn a_cold_pack_mapped_across_a_settle_is_removed() {
    settle_with_a_cold_pack_in_flight(true);
}

/// One byte flipped in a group of a database section: each request reading a key whose memos
/// it held fails naming the section and group, every time, and nothing is computed again; every
/// other key is used from the image, the group is reported once, and no image is written
/// without it. A group holding an input's path is read at start, so the database does not
/// start, naming it.
#[test]
fn a_corrupt_group_fails_the_keys_in_it() {
    const KEYS: usize = 300;
    let (db, tree) = setup();
    let names: Vec<String> = (0..KEYS).map(|n| format!("f{n}")).collect();
    let contents: Vec<String> = (0..KEYS).map(|n| "x".repeat(n % 7)).collect();
    let files: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(contents.iter().map(String::as_str)).collect();
    let one = view(&db, &tree, &files);
    for name in &names {
        double(&db, one, name);
    }
    // Groups of 4 KiB, so each section spans several.
    let (bytes, _) = image_of(&db, Builder::with_group_shift(12));
    let sections = opened(&bytes).sections().to_vec();
    let mut tried = 0;
    for section in sections.iter().filter(|it| it.id != SectionId::DB_INPUTS) {
        for group in 0..section.groups {
            let mut flipped = bytes.clone();
            flipped[(section.at + u64::from(group) * 4096) as usize] ^= 0x40;
            let image = opened(&flipped);
            let mut started = match Database::open_image(config(&tree), image) {
                Ok(started) => started,
                Err(DbError::Corrupt(corrupt)) => {
                    assert_eq!((corrupt.section, corrupt.group), (SectionId::DB_BLOB, group), "only paths are read at start");
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            register(&mut started);
            reset();
            let same = view(&started, &tree, &files);
            let the_group = |error: DbError, name: &str| match error {
                DbError::Corrupt(corrupt) => assert_eq!(
                    (corrupt.section, corrupt.group),
                    (section.id, group),
                    "{name} fails naming the corrupt group"
                ),
                error => panic!("{name}: {error}"),
            };
            let mut failed = Vec::new();
            for (n, name) in names.iter().enumerate() {
                match started.pin().get::<Double>(same, name) {
                    Ok(value) => assert_eq!(value, 2 * (n % 7) as u64, "{name} is right from the image"),
                    Err(error) => {
                        the_group(error, name);
                        failed.push(name);
                    }
                }
            }
            assert!(!failed.is_empty(), "{} group {group} held memos: a key of it fails", section.id);
            assert!(failed.len() < KEYS, "{} group {group}: the other keys answer from the image", section.id);
            for &name in &failed {
                the_group(started.pin().get::<Double>(same, name).expect_err("asked again, it fails again"), name);
            }
            assert_eq!(runs("len") + runs("double"), 0, "{} group {group}: nothing is computed again", section.id);
            let mut live = Live::new();
            the_group(
                started.write_image(&mut Builder::new(), &mut live).expect_err("no image without the group"),
                "the image",
            );
            let found = started.take_corrupt();
            assert_eq!(
                found.iter().map(|it| (it.section, it.group)).collect::<Vec<_>>(),
                vec![(section.id, group)],
                "{} group {group}",
                section.id
            );
            assert_eq!(started.take_corrupt(), vec![], "each group is reported once");
            tried += 1;
        }
    }
    assert!(tried > 4 * 2, "every database section spans groups: {tried} tried");
}

#[test]
fn a_wait_that_would_close_a_cycle_is_cancelled() {
    let (db, tree) = setup();
    let x = view(&db, &tree, &[("a", "b"), ("b", "")]);
    let y = view(&db, &tree, &[("a", ""), ("b", "a")]);
    let cancelled = AtomicUsize::new(0);
    let ask = |view: ViewId, name: &str| loop {
        match db.pin().get::<Chain>(view, &name.to_owned()) {
            Ok(n) => break n,
            Err(DbError::Cancelled) => {
                cancelled.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => panic!("{error}"),
        }
    };
    let (in_x, in_y) = std::thread::scope(|scope| {
        let in_x = scope.spawn(|| ask(x, "a"));
        let in_y = scope.spawn(|| ask(y, "b"));
        (in_x.join().expect("x"), in_y.join().expect("y"))
    });
    assert_eq!((in_x, in_y), (1, 1));
    assert!(cancelled.load(Ordering::SeqCst) >= 1, "one of the two waits closed a cycle");
}

#[test]
fn configuration_and_directories_are_inputs() {
    let (db, tree) = setup();
    let root = tree.root(&[("src/a.rs", b"1".as_slice()), ("src/b.rs", b"2")]);
    let x86 = db.open_with(View { root }, b"x86_64");
    let arm = db.open_with(View { root }, b"aarch64");
    assert_eq!(db.pin().get::<Conf>(x86, &()), Ok(6));
    assert_eq!(db.pin().get::<Conf>(arm, &()), Ok(7));
    let x86_again = db.open_with(View { root }, b"x86_64");
    assert_eq!(db.pin().get::<Conf>(x86_again, &()), Ok(6));
    assert_eq!(runs("conf"), 2);

    assert_eq!(db.pin().get::<Names>(x86, &"src".to_owned()), Ok(2));
    let edited = view(&db, &tree, &[("src/a.rs", "11"), ("src/b.rs", "2")]);
    assert_eq!(db.pin().get::<Names>(edited, &"src".to_owned()), Ok(2));
    assert_eq!(runs("names"), 1, "an edit inside the directory leaves its names");
    let added = view(&db, &tree, &[("src/a.rs", "1"), ("src/b.rs", "2"), ("src/c.rs", "3")]);
    assert_eq!(db.pin().get::<Names>(added, &"src".to_owned()), Ok(3));
    assert_eq!(db.pin().get::<Names>(added, &"nowhere".to_owned()), Ok(u64::MAX));
}

/// A file's length under another root, times 100, plus the names at that root's top;
/// `u64::MAX` for no file.
struct RootLen;

impl Query for RootLen {
    const KIND: QueryKind = QueryKind(0x2f20);
    const NAME: &'static str = "root len";
    const PRODUCER: ContentId = ContentId([0x20; 32]);
    type Key = (ContentId, String);
    type Value = Num;
    fn input(key: &(ContentId, String)) -> ContentId {
        Input::file_at_key(key.0, &key.1).input
    }
    fn execute(cx: &mut Cx<'_>, (root, path): &(ContentId, String)) -> Result<Num, Cancelled> {
        ran("root len");
        let Some(file) = cx.file_at(*root, path)? else { return Ok(Num(u64::MAX)) };
        let names = cx.dir_at(*root, "")?.map_or(0, |names| names.len() as u64);
        Ok(Num(file.bytes.len() as u64 * 100 + names))
    }
}

#[test]
fn another_roots_files_are_inputs_shared_by_every_view() {
    let (mut db, tree) = setup();
    db.register::<RootLen>();
    let library = tree.root(&[("core/lib.rs", b"abcd".as_slice()), ("std/lib.rs", b"x")]);
    let core = (library, "core/lib.rs".to_owned());
    let one = view(&db, &tree, &[("a", "1")]);
    assert_eq!(db.pin().get::<RootLen>(one, &core), Ok(402));
    assert_eq!(db.pin().get::<RootLen>(one, &(library, "nope".to_owned())), Ok(u64::MAX));
    assert_eq!(db.pin().get::<RootLen>(one, &(library, "core".to_owned())), Ok(u64::MAX), "a directory");
    assert_eq!(runs("root len"), 3);

    let two = view(&db, &tree, &[("a", "22")]);
    assert_eq!(db.pin().get::<RootLen>(two, &core), Ok(402));
    assert_eq!(runs("root len"), 3, "the view changed, the root did not: verified, not run");
    assert_eq!(db.why(two, &QueryKey { kind: RootLen::KIND, input: RootLen::input(&core) }), None);

    let newer = tree.root(&[("core/lib.rs", b"abcde".as_slice())]);
    assert_eq!(db.pin().get::<RootLen>(two, &(newer, "core/lib.rs".to_owned())), Ok(501));
    assert_ne!(Input::file_at_key(library, "core/lib.rs"), Input::file_at_key(newer, "core/lib.rs"));
    assert_ne!(Input::file_at_key(library, "core/lib.rs"), Input::file_key("core/lib.rs"));

    let (bytes, _) = image_of(&db, Builder::new());
    let mut started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    register(&mut started);
    started.register::<RootLen>();
    reset();
    let three = view(&started, &tree, &[("b", "3")]);
    assert_eq!(started.pin().get::<RootLen>(three, &core), Ok(402));
    assert_eq!(runs("root len"), 0, "the root's inputs came back from the image");
    assert_eq!(started.take_corrupt(), vec![], "nothing read was corrupt");
}

#[test]
fn a_fact_no_query_answers_is_never_reused_unverified() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.pin().get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(db.pin().get::<ReadsFact>(one, &()), Ok(1));
    assert_eq!(runs("reads fact"), 1, "within a view it is resolved once");
    let two = view(&db, &tree, &[("a", "x")]);
    assert_eq!(db.pin().get::<ReadsFact>(two, &()), Ok(1));
    assert_eq!(runs("reads fact"), 2, "another view can't verify the fact, so it runs again");
}

#[test]
fn asking_what_is_not_there_is_an_error() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "x")]);
    db.close(one);
    assert_eq!(db.pin().get::<Len>(one, &"a".to_owned()), Err(DbError::NoView(one)));
    let empty = Database::empty();
    let none = empty.open(View { root: ContentId([0; 32]) });
    assert_eq!(empty.pin().get::<Len>(none, &"a".to_owned()), Err(DbError::Unknown(Len::KIND)));
}

#[test]
fn the_database_is_shared_by_reference() {
    fn shared<T: Send + Sync>() {}
    shared::<Database>();
}

static HELD: AtomicUsize = AtomicUsize::new(0);
static RELEASED: AtomicUsize = AtomicUsize::new(0);
static ASKING: AtomicUsize = AtomicUsize::new(0);

/// Runs until [`RELEASED`] is set, holding its key.
struct Hold;

impl Query for Hold {
    const KIND: QueryKind = QueryKind(0x2f08);
    const NAME: &'static str = "hold";
    const PRODUCER: ContentId = ContentId([10; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("hold")
    }
    fn execute(_: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        HELD.store(1, Ordering::SeqCst);
        blaze_graph::spin::SpinWait::within("the test to release the held query", std::time::Duration::from_secs(60), || {
            RELEASED.load(Ordering::SeqCst) != 0
        });
        Ok(Num(5))
    }
}

/// 1 + [`Hold`]: a query that waits, inside its run, for a key another task runs.
struct AsksHold;

impl Query for AsksHold {
    const KIND: QueryKind = QueryKind(0x2f09);
    const NAME: &'static str = "asks hold";
    const PRODUCER: ContentId = ContentId([11; 32]);
    type Key = ();
    type Value = Num;
    fn input(_: &()) -> ContentId {
        key_id("asks hold")
    }
    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Num, Cancelled> {
        ASKING.store(1, Ordering::SeqCst);
        Ok(Num(1 + cx.get::<Hold>(&())?))
    }
}

/// A query waiting on the one worker of a pool frees it: another job runs there meanwhile and may
/// ask the database (it is a task of its own, not the query), and one that asks the waiting
/// query's key waits for it like any other task, and gets its value once the query has it.
#[test]
fn a_query_waiting_on_the_one_worker_frees_it_for_other_jobs() {
    use blaze_graph::{pool::Pool, topology::Topology};
    use std::time::Duration;

    let tree = Arc::new(MemTree::default());
    let mut db = Database::new(config(&tree));
    register(&mut db);
    db.register::<Hold>();
    db.register::<AsksHold>();
    let db = Arc::new(db);
    let one = view(&db, &tree, &[("a", "abc")]);
    let pool = Pool::new(&Topology::flat(1), false).expect("a pool");
    let wait = |flag: &AtomicUsize| {
        blaze_graph::spin::SpinWait::within("a query to set its flag", Duration::from_secs(60), || flag.load(Ordering::SeqCst) != 0);
    };

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| db.pin().get::<Hold>(one, &()));
        wait(&HELD);

        let (asked, heard) = crossbeam_channel::unbounded();
        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(None, move || db.pin().get::<AsksHold>(one, &()), move |it| _ = asked.send(("asks", it)));
        }
        wait(&ASKING);
        blaze_graph::spin::SpinWait::within("the asking job to park on the held query", Duration::from_secs(60), || {
            pool.parked() == 1
        });

        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(
                None,
                move || db.pin().get::<Len>(one, &"a".to_owned()),
                move |it| _ = asked.send(("len", it)),
            );
        }
        let (name, len) = heard.recv_timeout(Duration::from_secs(60)).expect("the worker ran another job");
        assert_eq!(name, "len", "the waiting query holds no worker");
        assert_eq!(len.expect("no panic"), Ok(3));

        {
            let (db, asked) = (db.clone(), asked.clone());
            pool.submit(None, move || db.pin().get::<AsksHold>(one, &()), move |it| _ = asked.send(("again", it)));
        }
        blaze_graph::spin::SpinWait::within("the second asking job to park on the held query", Duration::from_secs(60), || {
            pool.parked() == 2
        });

        RELEASED.store(1, Ordering::SeqCst);
        assert_eq!(holder.join().expect("the holder"), Ok(5));
        let mut ended: Vec<_> = (0..2)
            .map(|_| heard.recv_timeout(Duration::from_secs(60)).expect("the waiting queries ended"))
            .map(|(name, it)| (name, it.expect("no panic")))
            .collect();
        ended.sort_by_key(|it| it.0);
        assert_eq!(ended, [("again", Ok(6)), ("asks", Ok(6))]);
    });
}

static COUNTED: AtomicUsize = AtomicUsize::new(0);

/// The length of a file, its runs counted across threads.
struct Counted;

impl Query for Counted {
    const KIND: QueryKind = QueryKind(0x2f0a);
    const NAME: &'static str = "counted";
    const PRODUCER: ContentId = ContentId([12; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        COUNTED.fetch_add(1, Ordering::SeqCst);
        Ok(Num(cx.file(key)?.map_or(u64::MAX, |file| file.bytes.len() as u64)))
    }
}

#[test]
fn threads_asking_overlapping_keys_run_each_once() {
    let (mut db, tree) = setup();
    db.register::<Counted>();
    let files: Vec<(String, String)> = (0..64).map(|i| (format!("f{i}"), "x".repeat(i))).collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_str())).collect();
    let one = view(&db, &tree, &files);
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let db = &db;
            scope.spawn(move || {
                let pinned = db.pin();
                for round in 0..64 {
                    let i = (round * 7 + thread * 5) % 64;
                    assert_eq!(pinned.get::<Counted>(one, &format!("f{i}")), Ok(i as u64));
                }
            });
        }
    });
    assert_eq!(COUNTED.load(Ordering::SeqCst), 64, "each key ran once, whichever thread asked first");
}

const TEXT: SchemaHash = SchemaHash(ContentId([0x54; 32]));

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct TextRoot: 1 {
        text: Slice<u8>,
    }
}

/// Bytes read in place: a value whose [`Persist::Ref`] borrows the database.
struct Text(String);

impl Persist for Text {
    const SCHEMA: SchemaHash = TEXT;
    type Ref<'a> = &'a [u8];
    fn write(&self, out: &mut Writer) -> u64 {
        let at = out.push_str(&self.0);
        let root = out.next::<TextRoot>();
        out.push(&TextRoot { text: Slice::between(root, at, self.0.len()) })
    }
    fn read(buffer: &[u8]) -> Result<&[u8], FormatError> {
        blaze_format::open_as::<TextRoot>(buffer, TEXT)?.text.follow(buffer)
    }
}

/// A file's bytes.
struct Contents;

impl Query for Contents {
    const KIND: QueryKind = QueryKind(0x2f0b);
    const NAME: &'static str = "contents";
    const PRODUCER: ContentId = ContentId([13; 32]);
    type Key = String;
    type Value = Text;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Text, Cancelled> {
        ran("contents");
        let file = cx.file(key)?.expect("a file");
        Ok(Text(String::from_utf8(file.bytes.to_vec()).expect("text")))
    }
}

/// Eviction swaps the value out; the buffer a task read stays until the task ends, and is
/// freed once every task that could hold it has.
#[test]
fn a_value_held_by_a_task_outlives_its_eviction() {
    use std::time::{Duration, Instant};

    let (mut db, tree) = setup();
    db.register::<Contents>();
    let one = view(&db, &tree, &[("a", "the bytes of a")]);
    let pinned = db.pin();
    let held = pinned.get::<Contents>(one, &"a".to_owned()).expect("contents");
    assert_eq!(db.evict(&key::<Contents>("a")), 1);
    assert_eq!(db.memo(&key::<Contents>("a")).map(|memo| memo.value), Some(Stored::Evicted));
    assert_eq!(db.held(), 1, "the evicted buffer waits for this task");
    assert_eq!(held, b"the bytes of a");
    drop(pinned);

    assert_eq!(db.pin().get::<Contents>(one, &"a".to_owned()), Ok(&b"the bytes of a"[..]));
    assert_eq!(runs("contents"), 2, "made again");
    let until = Instant::now() + Duration::from_secs(60);
    while db.held() > 0 {
        assert!(Instant::now() < until, "the evicted buffer was never freed");
        std::thread::yield_now();
    }
}

#[test]
fn edges_round_trip_through_their_record() {
    let edge = Edge {
        read: QueryKey { kind: QueryKind(0x1234), input: ContentId([0xab; 32]) },
        saw: Fingerprint(0x0123_4567_89ab_cdef_0011_2233_4455_6677),
    };
    let record = EdgeRecord::new(edge.read, edge.saw);
    assert_eq!(record.edge(), edge);
    let bytes = crate::image::encode_edge(&edge);
    assert_eq!(bytes.len(), crate::table::EDGE_RECORD);
    assert_eq!(EdgeRecord::as_bytes(&[record]), bytes.as_slice(), "the image's layout, read in place");
    let aligned = Aligned::copy(&bytes);
    // SAFETY: `Aligned` is at a 16-byte boundary.
    let read = unsafe { EdgeRecord::from_bytes(aligned.bytes()) };
    assert_eq!(read.iter().map(EdgeRecord::edge).collect::<Vec<_>>(), vec![edge]);
}

/// A memo of no edges reads an empty range of the image, which is `&[]` at address 1: no
/// records, never a cast of an unaligned pointer (the daemon's settle of a 14:13 home).
#[test]
fn no_edges_read_from_an_empty_range() {
    let empty: &[u8] = &[];
    assert_ne!(empty.as_ptr().addr() % align_of::<EdgeRecord>(), 0, "the case this covers");
    // SAFETY: empty, which `from_bytes` takes at any address.
    assert!(unsafe { EdgeRecord::from_bytes(empty) }.is_empty());
}

/// Counts each thread's allocations for the budgets below and in `memo` and `table`'s tests.
#[global_allocator]
static ALLOCATOR: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

static WARM_CX: AtomicU64 = AtomicU64::new(u64::MAX);

/// [`Double`] asked twice: the second time is a warm hit inside a query, its allocations
/// counted into [`WARM_CX`].
struct AskTwice;

impl Query for AskTwice {
    const KIND: QueryKind = QueryKind(0x2f0c);
    const NAME: &'static str = "ask twice";
    const PRODUCER: ContentId = ContentId([14; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        let first = cx.get::<Double>(key)?;
        let (second, used) = blaze_alloc_count::measure(|| cx.get::<Double>(key));
        WARM_CX.store(used.allocations, Ordering::SeqCst);
        Ok(Num(first + second?))
    }
}

/// A hit on a key the view resolved, from outside a query and from inside one, allocates
/// nothing: no key bytes, no run, no edge buffer, no clone of the value.
#[test]
fn a_warm_hit_allocates_nothing() {
    let (mut db, tree) = setup();
    db.register::<AskTwice>();
    let one = view(&db, &tree, &[("a", "abc")]);
    let name = "a".to_owned();
    let pinned = db.pin();
    assert_eq!(pinned.get::<Double>(one, &name), Ok(6));
    let got = blaze_alloc_count::assert_at_most(0, "a warm Pinned::get", || pinned.get::<Double>(one, &name));
    assert_eq!(got, Ok(6));
    drop(pinned);

    assert_eq!(db.pin().get::<AskTwice>(one, &name), Ok(12));
    assert_eq!(WARM_CX.load(Ordering::SeqCst), 0, "a warm Cx::get allocated");
}

/// The key bytes of a key asked once stay beside its memos, so another view can verify an
/// edge on it with no caller naming it again; a key asked again is not encoded again.
#[test]
fn a_key_is_kept_once_and_runs_again_by_its_bytes() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    double(&db, one, "a");
    let task = blaze_reclaim::enter();
    let slot = db.slot(&task, &key::<Len>("a")).expect("no image").expect("len(a) ran");
    let kept = slot.key().expect("its key's bytes").as_ptr();
    drop(task);
    double(&db, one, "a");
    let task = blaze_reclaim::enter();
    let again = db.slot(&task, &key::<Len>("a")).expect("no image").and_then(|slot| slot.key()).expect("still kept");
    assert_eq!(again.as_ptr(), kept, "not encoded and swapped in again");
    drop(task);

    // Another view: double(a) verifies its edge on len(a), whose memo no longer holds, so
    // len runs again from its kept bytes alone.
    let longer = view(&db, &tree, &[("a", "abcd")]);
    assert_eq!(db.pin().get::<Double>(longer, &"a".to_owned()), Ok(8));
    assert_eq!(runs("len"), 2);

    // Its bytes are read back once: the next view's run of len(a) takes the key decoded then.
    let task = blaze_reclaim::enter();
    let decoded: *const crate::AnyKey = &**db.keys.get(&task, &key::<Len>("a")).expect("decoded for its run");
    drop(task);
    let longest = view(&db, &tree, &[("a", "abcde")]);
    assert_eq!(db.pin().get::<Double>(longest, &"a".to_owned()), Ok(10));
    assert_eq!(runs("len"), 3);
    let task = blaze_reclaim::enter();
    let again: *const crate::AnyKey = &**db.keys.get(&task, &key::<Len>("a")).expect("still kept");
    assert!(std::ptr::addr_eq(again, decoded), "not decoded again");
    drop(task);

    for open in [one, longer, longest] {
        db.close(open);
    }
    collect_until_freed(&db);
    let task = blaze_reclaim::enter();
    assert!(matches!(db.slot(&task, &key::<Len>("a")), Ok(None)), "len(a)'s slot is collected");
    assert!(db.keys.get(&task, &key::<Len>("a")).is_none(), "and its decoded key with it");
}

/// Its runs are tasks of `Tag::Parse`: it answers its frame's tag and [`Double`] of its key.
struct Tagged;

impl Query for Tagged {
    const KIND: QueryKind = QueryKind(0x2f0d);
    const NAME: &'static str = "tagged";
    const PRODUCER: ContentId = ContentId([15; 32]);
    const TAG: blaze_mem::Tag = blaze_mem::Tag::Parse;
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        // A miss: `double`, then `len`, run in tasks of their own and give the frame back.
        let inner = cx.get::<Double>(key)?;
        let tag = blaze_mem::frame::current(|frame| {
            let mut bytes = Vec::new_in(frame);
            bytes.extend_from_slice(key.as_bytes());
            assert_eq!(bytes.len(), key.len());
            frame.tag().index() as u64
        });
        Ok(Num(tag << 32 | inner))
    }
}

#[test]
fn a_run_is_a_frame_task_of_its_querys_tag() {
    let (mut db, tree) = setup();
    db.register::<Tagged>();
    let one = view(&db, &tree, &[("a", "abc")]);
    let parse = blaze_mem::Tag::Parse.index() as u64;
    assert_eq!(db.pin().get::<Tagged>(one, &"a".to_owned()), Ok(parse << 32 | 6));
}

/// A view opened after one closed takes its state, emptied with its tables: revalidating a key
/// whose inputs did not change there allocates nothing, and runs nothing.
#[test]
fn a_revalidation_that_changes_nothing_allocates_nothing() {
    let (db, tree) = setup();
    let first = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, first, "a"), 6);
    let root = tree.root(&[("a", b"abc".as_slice())]);
    let name = "a".to_owned();
    // Warm: the closed state's grace passes once other tests' tasks that began before it end.
    let mut warm = 0;
    while db.ready_views() == 0 {
        let again = db.open(View { root });
        assert_eq!(db.pin().get::<Double>(again, &name), Ok(6));
        db.close(again);
        warm += 1;
        assert!(warm < 100_000, "a closed view was never ready again");
        std::thread::yield_now();
    }
    let reused = db.open(View { root });
    let pinned = db.pin();
    let got = blaze_alloc_count::assert_at_most(0, "revalidating double(a) in a reused view", || {
        pinned.get::<Double>(reused, &name)
    });
    assert_eq!(got, Ok(6));
    assert_eq!((runs("double"), runs("len")), (1, 1), "verified, not run again");
}

/// A new view of a tree that an open view resolved verifies with nothing allocated: its tables
/// are sized like that view's, and a directory's names are hashed as the tree lends them.
#[test]
fn verifying_in_a_new_view_of_the_same_tree_allocates_nothing() {
    let (db, tree) = setup();
    let files = [("a", "abc"), ("d/x", "1"), ("d/y", "2")];
    let first = view(&db, &tree, &files);
    let (name, dir) = ("a".to_owned(), "d".to_owned());
    assert_eq!(double(&db, first, "a"), 6);
    assert_eq!(db.pin().get::<Names>(first, &dir), Ok(2));
    let again = view(&db, &tree, &files);
    let pinned = db.pin();
    let got = blaze_alloc_count::assert_at_most(0, "verifying in a new view", || {
        (pinned.get::<Double>(again, &name), pinned.get::<Names>(again, &dir))
    });
    assert_eq!(got, (Ok(6), Ok(2)));
    assert_eq!((runs("double"), runs("names")), (1, 1), "verified, not run again");
}

/// Once warm, a collection that finds nothing new to drop allocates nothing.
#[test]
fn a_warm_collection_allocates_nothing() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, one, "a"), 6);
    let mut effects = Vec::new();
    for _ in 0..3 {
        db.collect(&[], &mut effects);
    }
    blaze_alloc_count::assert_at_most(0, "a warm collection", || db.collect(&[], &mut effects));
    assert!(effects.is_empty(), "an in-memory database writes nothing");
    assert_eq!(double(&db, one, "a"), 6);
}

/// Once warm, a miss allocates no more than a segment the store opens for what it copies: the
/// run's frame, its value buffer, its key bytes, the key its dependency's run decoded from
/// them and its slot's list are all reused.
#[test]
fn a_warm_miss_allocates_at_most_a_segment() {
    let (db, tree) = setup();
    let name = "a".to_owned();
    for n in 1..40usize {
        let text = "x".repeat(n);
        let root = tree.root(&[("a", text.as_bytes())]);
        // A closed view's state is ready again once other tests' tasks that began before it end.
        while n > 1 && db.ready_views() == 0 {
            std::thread::yield_now();
        }
        let view = db.open(View { root });
        let pinned = db.pin();
        let budget = if n < 32 { u64::MAX } else { 1 };
        let got = blaze_alloc_count::assert_at_most(budget, "a warm miss of double(a)", || {
            pinned.get::<Double>(view, &name)
        });
        assert_eq!(got, Ok(2 * n as u64));
        drop(pinned);
        db.close(view);
    }
    assert_eq!(runs("len"), 39, "each new content a miss");
}

/// Collection's marks are a bit a memo: an ID past the end they were made for grows them, and
/// they list what was marked in order.
#[test]
fn marks_are_a_bit_a_memo() {
    let mut marks = BitSet::<u32>::new(70);
    assert!(marks.insert_growing(3));
    assert!(!marks.insert_growing(3), "marked once");
    assert!(marks.insert_growing(64));
    assert!(marks.insert_growing(200), "past the end it was made for");
    assert!(marks.contains(64) && marks.contains(200) && !marks.contains(65) && !marks.contains(1000));
    assert_eq!(marks.iter().collect::<Vec<_>>(), [3, 64, 200]);
}

const LAYERS: u32 = 6;
const WIDTH: u32 = 400;

/// The three children in the next layer of node `i` of `layer`, picked by a hash of the node.
fn children(layer: u32, i: u32) -> impl Iterator<Item = (u32, u32)> {
    let mut x = (u64::from(layer) << 32 | u64::from(i)).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..3).filter_map(move |_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (layer + 1 < LAYERS).then_some((layer + 1, (x % u64::from(WIDTH)) as u32))
    })
}

/// A node of a layered graph: its file's length plus its [`children`]'.
struct Layered;

impl Query for Layered {
    const KIND: QueryKind = QueryKind(0x2f30);
    const NAME: &'static str = "layered";
    const PRODUCER: ContentId = ContentId([0x30; 32]);
    type Key = (u32, u32);
    type Value = Num;
    fn input(&(layer, i): &(u32, u32)) -> ContentId {
        key_id(&format!("{layer}.{i}"))
    }
    fn execute(cx: &mut Cx<'_>, &(layer, i): &(u32, u32)) -> Result<Num, Cancelled> {
        let mut sum = cx.file(&format!("{layer}.{i}"))?.map_or(0, |file| file.bytes.len() as u64);
        for child in children(layer, i) {
            sum = sum.wrapping_add(cx.get::<Layered>(&child)?);
        }
        Ok(Num(sum))
    }
}

/// A database of [`Layered`] asked from its first layer in three views whose files' lengths
/// differ, so a key has up to three memos and an edge's fingerprint picks among them; the
/// first two views are closed, their memos still held. With every memo it holds.
fn layered() -> (Database, Vec<MemoId>) {
    let (mut db, tree) = setup();
    db.register::<Layered>();
    let names: Vec<String> =
        (0..LAYERS).flat_map(|layer| (0..WIDTH).map(move |i| format!("{layer}.{i}"))).collect();
    for round in 0..3usize {
        let texts: Vec<String> = (0..names.len()).map(|at| "x".repeat(1 + at * (round + 1) % 7)).collect();
        let files: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(texts.iter().map(String::as_str)).collect();
        let one = view(&db, &tree, &files);
        let pinned = db.pin();
        for i in 0..WIDTH {
            pinned.get::<Layered>(one, &(0, i)).expect("layered");
        }
        drop(pinned);
        if round < 2 {
            db.close(one);
        }
    }
    let task = blaze_reclaim::enter();
    let mut all = Vec::new();
    db.slots.for_each(&task, |_, slot| all.extend_from_slice(slot.memos()));
    drop(task);
    (db, all)
}

/// What [`Database::mark`] reaches from `roots`, and from the open views' results when `views`,
/// with marks made for `capacity` memos: the memos reached, ascending; those the results name;
/// and the input keys, sorted.
fn marked(db: &Database, views: bool, roots: &[MemoId], capacity: usize, how: Mark) -> (Vec<MemoId>, Vec<MemoId>, Vec<QueryKey>) {
    let task = blaze_reclaim::enter();
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut states = Vec::new_in(frame);
        if views {
            db.views.for_each(&task, |state| states.push(state));
        }
        let mut work = Vec::new_in(frame);
        work.extend_from_slice(roots);
        let (mut reached, mut hot) =
            (Bits::<_, MemoId>::from_words(Vec::new_in(frame)), Bits::<_, MemoId>::from_words(Vec::new_in(frame)));
        reached.grow(capacity);
        hot.grow(capacity);
        let mut inputs = hashbrown::HashSet::with_hasher_in(blaze_hash::FixedState, frame);
        db.mark(&task, frame, &states, &mut work, &mut reached, &mut hot, &mut inputs, how);
        assert!(work.is_empty(), "a mark leaves no work");
        let mut inputs: Vec<QueryKey> = inputs.iter().copied().collect();
        inputs.sort_unstable();
        (reached.iter().collect(), hot.iter().collect(), inputs)
    })
}

/// Marking on the plane's workers reaches exactly what one thread's walk does, from random
/// roots with and without the open view's results, at every batch size (1 hands off at every
/// second memo), with marks too small for some memos (those finish on the caller, as memos
/// made during a collection do) or for none.
#[test]
fn parallel_marking_reaches_what_serial_marking_does() {
    let (db, all) = layered();
    let end = db.store.end() as usize;
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let (mut reached_some, mut hot_some) = (false, false);
    for round in 0..24u64 {
        let roots: Vec<MemoId> = all
            .iter()
            .copied()
            .filter(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                round % 4 != 0 && x % (round + 2) == 0
            })
            .collect();
        let views = round % 2 == 1;
        let serial = marked(&db, views, &roots, end, Mark::Serial);
        reached_some |= !serial.0.is_empty();
        hot_some |= !serial.1.is_empty();
        let capacity = [end, end / 2, 0][(round % 3) as usize];
        for batch in [1, 4, 64, crate::MARK_BATCH] {
            assert_eq!(
                marked(&db, views, &roots, capacity, Mark::Parallel { batch }),
                serial,
                "round {round}, views {views}, batch {batch}, marks for {capacity} of {end} memos"
            );
        }
    }
    assert!(reached_some && hot_some, "some round's roots reach memos, and the open view names some");
}

/// A parallel mark allocates nothing on the thread that asks it: its marks and what it keeps
/// are in the caller's frame, and the workers' lists are their own, once each a collection.
#[test]
fn a_parallel_mark_allocates_nothing_on_its_caller() {
    let (db, all) = layered();
    let end = db.store.end() as usize;
    let warm = marked(&db, true, &all, end, Mark::Parallel { batch: 64 });
    let task = blaze_reclaim::enter();
    let again = blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut states = Vec::new_in(frame);
        db.views.for_each(&task, |state| states.push(state));
        let mut work = Vec::new_in(frame);
        work.extend_from_slice(&all);
        let (mut reached, mut hot) =
            (Bits::<_, MemoId>::from_words(Vec::new_in(frame)), Bits::<_, MemoId>::from_words(Vec::new_in(frame)));
        reached.grow(end);
        hot.grow(end);
        let mut inputs = hashbrown::HashSet::with_hasher_in(blaze_hash::FixedState, frame);
        blaze_alloc_count::assert_at_most(0, "a parallel mark, on its caller", || {
            db.mark(&task, frame, &states, &mut work, &mut reached, &mut hot, &mut inputs, Mark::Parallel { batch: 64 });
        });
        reached.count()
    });
    assert_eq!(again, warm.0.len());
}

/// The image's radix sort of keys orders them as a comparison sort does: random keys of a few
/// kinds, with keys repeated and keys sharing their whole 8-byte prefix, at sizes on both sides
/// of the radix sort's insertion and parallel cut-offs.
#[test]
fn keys_sort_for_the_image_as_by_comparison() {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for len in [0usize, 1, 2, 63, 64, 65, 1000, 70_000, 150_000] {
        let mut keys: Vec<QueryKey> = (0..len)
            .map(|_| {
                let mut input = [0u8; 32];
                for chunk in input.chunks_mut(8) {
                    chunk.copy_from_slice(&next().to_le_bytes());
                }
                QueryKey { kind: QueryKind((next() % 4) as u16), input: ContentId(input) }
            })
            .collect();
        for at in 0..len / 10 {
            let from = keys[(next() as usize) % len];
            let to = (next() as usize) % len;
            keys[to] = if at % 2 == 0 {
                from
            } else {
                let mut shared = from;
                shared.input.0[31] ^= 1;
                shared
            };
        }
        let mut expected = keys.clone();
        expected.sort_unstable();
        crate::sort_keys(&mut keys);
        assert_eq!(keys, expected, "{len} keys");
    }
}

/// A memo is stamped with the collection generation it was given in, in its cold row; an equal
/// memo given later restamps the one its key keeps, and a stamp is never lowered.
#[test]
fn a_memo_is_born_in_the_generation_its_key_last_resolved_to_it() {
    let db = Database::empty();
    let task = blaze_reclaim::enter();
    let key = QueryKey { kind: QueryKind(0x2f00), input: ContentId([4; 32]) };
    let code = db.store.code(WORD, ContentId([1; 32]));
    let first = db.store.make(key, Fingerprint(1), code, &[], &[]);
    assert_eq!(db.add_memo(&task, key, first), Ok(first));
    assert_eq!(db.store.born(&task, first), 0);
    db.generation.store(3, Ordering::SeqCst);
    let equal = db.store.make(key, Fingerprint(1), code, &[], &[]);
    assert_eq!(db.add_memo(&task, key, equal), Ok(first), "the key keeps the equal memo it had");
    assert_eq!(db.store.born(&task, first), 3, "restamped as its key resolved to it again");
    let other = db.store.make(key, Fingerprint(2), code, &[], &[]);
    assert_eq!(db.add_memo(&task, key, other), Ok(other));
    assert_eq!(db.store.born(&task, other), 3);
    db.store.reborn_in(&task, first, 2);
    assert_eq!(db.store.born(&task, first), 3, "never lowered");
}

mod some_kinds {
    crate::query_kinds! {
        range: 0x7000..0x7004, owner: "the test";
        FIRST = 0x7000, "first";
        /// A method of a tracked `impl`.
        METHOD = 0x7002, "Type::method";
    }
}

/// `query_kinds!` names each kind both ways, and a number it doesn't give, or one outside its
/// range, has no name.
#[test]
fn query_kinds_name_each_kind_both_ways() {
    use some_kinds::*;
    assert_eq!(RANGE, 0x7000..0x7004);
    assert_eq!(ALL, &[(FIRST, "first"), (METHOD, "Type::method")]);
    for &(kind, query) in ALL {
        assert_eq!(name(kind), Some(query));
        assert_eq!(of(query), Some(kind));
    }
    assert_eq!(name(QueryKind(0x7001)), None);
    assert_eq!(name(QueryKind(0x7004)), None);
    assert_eq!(name(QueryKind(0x6fff)), None);
    assert_eq!(of("second"), None);
}

const BROKEN: Corrupt = Corrupt { section: SectionId::VFS_BLOBS, group: 7, cause: Cause::Crc { found: 1, wanted: 2 } };

/// A [`MemTree`] whose file `broken` names fails its check while it is set.
struct Breaking {
    files: MemTree,
    broken: Mutex<Option<&'static str>>,
}

impl Tree for Breaking {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        if *self.broken.lock() == Some(path) {
            return Err(BROKEN);
        }
        self.files.file(root, path)
    }
    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Lent<'_, '_>)) -> bool {
        self.files.read_dir(root, path, read)
    }
}

#[test]
fn a_file_that_fails_its_check_fails_the_request_and_is_read_again() {
    reset();
    let tree = Arc::new(Breaking { files: MemTree::default(), broken: Mutex::new(Some("a")) });
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: FileName::new("db"), tags: TAGS });
    register(&mut db);
    let view = db.open(View { root: tree.files.root(&[("a", b"four".as_slice()), ("b", b"xy".as_slice())]) });
    assert_eq!(db.pin().get::<Double>(view, &"a".to_owned()).err(), Some(DbError::Corrupt(BROKEN)));
    assert_eq!(double(&db, view, "b"), 4, "another file of the view still reads");
    *tree.broken.lock() = None;
    assert_eq!(double(&db, view, "a"), 8, "the failed read was not kept");
    assert_eq!(runs("len"), 3);
}

/// [`Cx::tree`] of a directory, with or without `package`.
struct TreeOf;

impl Query for TreeOf {
    const KIND: QueryKind = QueryKind(0x2f0e);
    const NAME: &'static str = "tree of";
    const PRODUCER: ContentId = ContentId([15; 32]);
    type Key = (String, bool);
    type Value = Digest;
    fn input(key: &(String, bool)) -> ContentId {
        key_id(&format!("{}{}", u8::from(key.1), key.0))
    }
    fn execute(cx: &mut Cx<'_>, key: &(String, bool)) -> Result<Digest, Cancelled> {
        ran("tree of");
        Ok(Digest(cx.tree(&key.0, key.1)?))
    }
}

fn tree_of(db: &Database, view: ViewId, dir: &str, package: bool) -> Option<[u8; 32]> {
    db.pin().get::<TreeOf>(view, &(dir.to_owned(), package)).expect("tree of")
}

/// The key of the query for `dir` when the tree is asked of the root without `package`.
fn subtree(dir: &str) -> QueryKey {
    let key = SubtreeKey { dir: dir.to_owned(), package: false, top: dir.is_empty() };
    QueryKey { kind: kinds::TREE, input: Subtree::input(&key) }
}

const NESTED: &[(&str, &str)] = &[("a/b/x", "1"), ("a/c/y", "2"), ("d/z", "3"), ("top", "4")];

#[test]
fn an_edit_reruns_only_the_directories_above_it() {
    let (db, tree) = setup();
    let one = view(&db, &tree, NESTED);
    let first = tree_of(&db, one, "", false).expect("the root is a directory");

    let edited = view(&db, &tree, &[("a/b/x", "9"), ("a/c/y", "2"), ("d/z", "3"), ("top", "4")]);
    assert_ne!(tree_of(&db, edited, "", false), Some(first));
    for dir in ["", "a", "a/b"] {
        assert!(db.why(edited, &subtree(dir)).is_some(), "{dir:?} is above the edit: it reran");
    }
    for dir in ["a/c", "d"] {
        assert_eq!(db.why(edited, &subtree(dir)), None, "{dir:?} is beside the edit: verified, not run");
    }
    assert_eq!(tree_of(&db, edited, "a/c", false), tree_of(&db, one, "a/c", false));

    let again = view(&db, &tree, NESTED);
    assert_eq!(tree_of(&db, again, "", false), Some(first), "equal trees hash equal");
    assert_eq!(db.why(again, &subtree("")), None);
}

#[test]
fn a_hash_sees_names_kinds_and_contents() {
    let (db, tree) = setup();
    let base = tree_of(&db, view(&db, &tree, &[("a/x", "1")]), "", false);
    for files in [&[("a/y", "1")][..], &[("a/x", "2")], &[("b/x", "1")], &[("a/x/x", "1")], &[("a", "1")]] {
        assert_ne!(tree_of(&db, view(&db, &tree, files), "", false), base, "{files:?}");
    }
    let one = view(&db, &tree, &[("a/x", "1")]);
    assert_ne!(tree_of(&db, one, "", true), base, "package hashes apart from a plain tree");
}

#[test]
fn a_package_leaves_out_hidden_names_and_the_top_target() {
    let (db, tree) = setup();
    let source = [("src/lib.rs", "fn f() {}"), ("src/target/t.rs", "1"), ("Cargo.toml", "[package]")];
    let mut cluttered = source.to_vec();
    cluttered.extend([(".git/HEAD", "ref"), (".env", "x"), ("target/debug/out", "bin"), ("src/.cache/c", "y")]);
    let clean = view(&db, &tree, &source);
    let noisy = view(&db, &tree, &cluttered);
    assert_eq!(tree_of(&db, noisy, "", true), tree_of(&db, clean, "", true));
    assert_ne!(tree_of(&db, noisy, "", false), tree_of(&db, clean, "", false), "a plain tree keeps them");

    let mut nested = source;
    nested[1].1 = "2";
    let nested = view(&db, &tree, &nested);
    assert_ne!(tree_of(&db, nested, "", true), tree_of(&db, clean, "", true), "only the top `target` is left out");
}

#[test]
fn no_directory_has_no_tree() {
    let (db, tree) = setup();
    let one = view(&db, &tree, NESTED);
    assert_eq!(tree_of(&db, one, "nowhere", false), None);
    assert_eq!(tree_of(&db, one, "top", true), None, "a file is not a directory");
    assert!(tree_of(&db, one, "a/", false).is_some());
    assert_eq!(tree_of(&db, one, "a/", false), tree_of(&db, one, "a", false));
}

/// A [`MemTree`] that lists every directory backwards.
struct Backwards(MemTree);

impl Tree for Backwards {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        self.0.file(root, path)
    }
    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Lent<'_, '_>)) -> bool {
        let Some(mut names) = self.0.list(root, path) else { return false };
        names.reverse();
        read(&mut names.iter().map(|(name, dir)| (name.as_str(), *dir)));
        true
    }
}

/// The names of a directory, in the order [`Cx::dir`] gives them, joined by `,`.
struct Listing;

impl Query for Listing {
    const KIND: QueryKind = QueryKind(0x2f0f);
    const NAME: &'static str = "listing";
    const PRODUCER: ContentId = ContentId([16; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        let names = cx.dir(key)?.expect("a directory");
        let joined: Vec<&str> = names.iter().map(|(name, _)| name.as_str()).collect();
        LISTED.with(|listed| *listed.borrow_mut() = joined.join(","));
        Ok(Num(names.len() as u64))
    }
}

thread_local! {
    static LISTED: RefCell<String> = const { RefCell::new(String::new()) };
}

#[test]
fn a_query_reads_each_directorys_names_sorted() {
    let (mut db, tree) = setup();
    db.register::<Listing>();
    let one = view(&db, &tree, &[("b", "1"), ("c/x", "2"), ("a", "3")]);
    assert_eq!(db.pin().get::<Listing>(one, &String::new()), Ok(3));
    assert_eq!(LISTED.with(|listed| listed.borrow().clone()), "a,b,c");
    assert_eq!(db.pin().get::<Listing>(one, &"c".to_owned()), Ok(1));
    assert_eq!(LISTED.with(|listed| listed.borrow().clone()), "x");
}

#[test]
fn a_tree_that_lists_out_of_order_stops_the_read_naming_the_directory() {
    reset();
    let files: &[(&str, &[u8])] = &[("b", b"1"), ("c/x", b"2"), ("a", b"3")];
    let backwards = Arc::new(Backwards(MemTree::default()));
    let root = backwards.0.root(files);
    let mut db = Database::new(Config { tree: Some(backwards.clone()), dir: FileName::new("db"), tags: TAGS });
    register(&mut db);
    db.register::<Listing>();
    let one = db.open(View { root });
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.pin().get::<Listing>(one, &String::new())));
    let panic = failed.expect_err("a listing out of order is the tree's error");
    let message = panic.downcast_ref::<String>().map(String::as_str).unwrap_or("");
    assert!(message.contains("listed directory \"\"") && message.contains("out of order"), "{message}");
}

static SPREAD: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

fn spread_runs(name: &str) -> usize {
    SPREAD.lock().as_ref().and_then(|runs| runs.get(name).copied()).unwrap_or(0)
}

/// The length of a file, `u64::MAX` for none, its runs counted by name across threads: the
/// key `no` is cancelled, and the key `gone` panics.
struct Spread;

impl Query for Spread {
    const KIND: QueryKind = QueryKind(0x2f10);
    const NAME: &'static str = "spread";
    const PRODUCER: ContentId = ContentId([0x10; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        match key.as_str() {
            "no" => return Err(Cancelled),
            "gone" => panic!("spread was asked of {key}, which is gone"),
            _ => {}
        }
        *SPREAD.lock().get_or_insert_with(HashMap::new).entry(key.clone()).or_default() += 1;
        Ok(Num(cx.file(key)?.map_or(u64::MAX, |file| file.bytes.len() as u64)))
    }
}

fn fold(values: impl IntoIterator<Item = u64>) -> u64 {
    values.into_iter().fold(0, |folded, value| folded.wrapping_mul(1_000_003).wrapping_add(value))
}

/// [`Spread`] of each name by one [`Cx::get_all`], folded in order.
struct All;

impl Query for All {
    const KIND: QueryKind = QueryKind(0x2f11);
    const NAME: &'static str = "all";
    const PRODUCER: ContentId = ContentId([0x11; 32]);
    type Key = Vec<String>;
    type Value = Num;
    fn input(key: &Vec<String>) -> ContentId {
        key_id(&key.join("\0"))
    }
    fn execute(cx: &mut Cx<'_>, key: &Vec<String>) -> Result<Num, Cancelled> {
        Ok(Num(fold(cx.get_all::<Spread>(key)?)))
    }
}

/// [`All`] as a loop of [`Cx::get`].
struct Each;

impl Query for Each {
    const KIND: QueryKind = QueryKind(0x2f12);
    const NAME: &'static str = "each";
    const PRODUCER: ContentId = ContentId([0x12; 32]);
    type Key = Vec<String>;
    type Value = Num;
    fn input(key: &Vec<String>) -> ContentId {
        key_id(&key.join("\0"))
    }
    fn execute(cx: &mut Cx<'_>, key: &Vec<String>) -> Result<Num, Cancelled> {
        let mut values = Vec::new();
        for name in key {
            values.push(cx.get::<Spread>(name)?);
        }
        Ok(Num(fold(values)))
    }
}

static WARM_ALL: AtomicU64 = AtomicU64::new(u64::MAX);

/// [`Spread`] of each name by one [`Cx::get_all`], its allocations counted into [`WARM_ALL`];
/// the round only makes another key.
struct CountedAll;

impl Query for CountedAll {
    const KIND: QueryKind = QueryKind(0x2f13);
    const NAME: &'static str = "counted all";
    const PRODUCER: ContentId = ContentId([0x13; 32]);
    type Key = (Vec<String>, u32);
    type Value = Num;
    fn input((names, round): &(Vec<String>, u32)) -> ContentId {
        key_id(&format!("{}\0{round}", names.join("\0")))
    }
    fn execute(cx: &mut Cx<'_>, (names, _): &(Vec<String>, u32)) -> Result<Num, Cancelled> {
        let (values, used) = blaze_alloc_count::measure(|| cx.get_all::<Spread>(names));
        WARM_ALL.store(used.allocations, Ordering::SeqCst);
        Ok(Num(fold(values?)))
    }
}

fn spread_setup(files: &[(String, String)]) -> (Database, ViewId, Arc<MemTree>) {
    let (mut db, tree) = setup();
    db.register::<Spread>();
    db.register::<All>();
    db.register::<Each>();
    db.register::<CountedAll>();
    let files: Vec<(&str, &str)> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_str())).collect();
    let one = view(&db, &tree, &files);
    (db, one, tree)
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|&name| name.to_owned()).collect()
}

fn list_key<Q: Query<Key = Vec<String>>>(names: &[String]) -> QueryKey {
    QueryKey { kind: Q::KIND, input: Q::input(&names.to_vec()) }
}

/// The values and edges of one `get_all` are a loop of `get`'s: in the order of the keys, a
/// repeated key read again where it repeats, each key run once whichever task ran it. Another
/// view verifies them as it verifies the loop's.
#[test]
fn get_all_gives_the_values_and_edges_a_loop_of_get_gives() {
    let files: Vec<(String, String)> = (0..32).map(|i| (format!("all{i}"), "x".repeat(i))).collect();
    let (db, one, tree) = spread_setup(&files);
    let mut asked: Vec<String> = (0..32).map(|i| format!("all{i}")).collect();
    asked.extend(names(&["all3", "all-missing", "all3", "all0"]));
    let expected = fold(asked.iter().map(|name| match name.strip_prefix("all").and_then(|n| n.parse::<u64>().ok()) {
        Some(n) => n,
        None => u64::MAX,
    }));

    assert_eq!(db.pin().get::<All>(one, &asked), Ok(expected));
    assert_eq!(db.pin().get::<Each>(one, &asked), Ok(expected));
    let edges = |key: QueryKey| db.memo(&key).expect("it ran").edges;
    let all = edges(list_key::<All>(&asked));
    assert_eq!(all, edges(list_key::<Each>(&asked)), "the same edges in the same order");
    let read: Vec<QueryKey> = all.iter().map(|edge| edge.read).collect();
    assert_eq!(read, asked.iter().map(|name| key::<Spread>(name)).collect::<Vec<_>>());
    for name in &asked {
        assert_eq!(spread_runs(name), 1, "{name} ran once");
    }

    let mut edited = files.clone();
    edited[5].1.push('y');
    let edited: Vec<(&str, &str)> = edited.iter().map(|(path, bytes)| (path.as_str(), bytes.as_str())).collect();
    let two = view(&db, &tree, &edited);
    // all5 is one longer, and it is read once, at position 5.
    let changed = expected.wrapping_add(1_000_003u64.wrapping_pow((asked.len() - 1 - 5) as u32));
    assert_eq!(db.pin().get::<All>(two, &asked), Ok(changed));
    assert_eq!(db.pin().get::<Each>(two, &asked), Ok(changed));
    assert_eq!(spread_runs("all5"), 2, "only the edited file's key ran again");
    assert_eq!(spread_runs("all6"), 1);
    assert_eq!(db.why(two, &list_key::<All>(&asked)).map(|why| why.chain[0].key), Some(key::<Spread>("all5")));
}

/// A key whose run panics fails the call with its panic, raised once the jobs ahead have ended;
/// what they resolved stays as memos.
#[test]
fn a_key_that_fails_fails_get_all_and_the_jobs_results_are_kept() {
    let files: Vec<(String, String)> = (0..8).map(|i| (format!("fail{i}"), "x".repeat(i))).collect();
    let (db, one, _) = spread_setup(&files);
    let mut asked = names(&["fail0", "gone"]);
    asked.extend((1..8).map(|i| format!("fail{i}")));
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.pin().get::<All>(one, &asked)));
    let panic = failed.expect_err("the key that panics fails the call");
    let message = panic.downcast_ref::<String>().map(String::as_str).unwrap_or("");
    assert!(message.contains("gone"), "the key's own panic: {message}");
    assert!(db.memo(&list_key::<All>(&asked)).is_none());
    assert!(db.memo(&key::<Spread>("gone")).is_none());
    for i in 1..8 {
        assert!(db.memo(&key::<Spread>(&format!("fail{i}"))).is_some(), "fail{i} was resolved ahead and kept");
    }
    assert_eq!(db.pin().get::<Spread>(one, &"fail7".to_owned()), Ok(7));
    assert_eq!(spread_runs("fail7"), 1, "kept, not run again");
}

/// The first key that is cancelled cancels the call; the jobs ahead's results stay as memos,
/// and the request asked again without it reuses them.
#[test]
fn a_cancelled_key_cancels_get_all_and_the_jobs_results_are_kept() {
    let files: Vec<(String, String)> = (0..8).map(|i| (format!("cancel{i}"), "x".repeat(i))).collect();
    let (db, one, _) = spread_setup(&files);
    let mut asked = names(&["cancel0", "no"]);
    asked.extend((1..8).map(|i| format!("cancel{i}")));
    assert_eq!(db.pin().get::<All>(one, &asked), Err(DbError::Cancelled));
    assert!(db.memo(&list_key::<All>(&asked)).is_none());
    assert!(db.memo(&key::<Spread>("no")).is_none());
    for i in 1..8 {
        assert!(db.memo(&key::<Spread>(&format!("cancel{i}"))).is_some(), "cancel{i} was resolved ahead and kept");
    }
    let rest: Vec<String> = (0..8).map(|i| format!("cancel{i}")).collect();
    assert_eq!(db.pin().get::<All>(one, &rest), Ok(fold(0..8)));
    for name in &rest {
        assert_eq!(spread_runs(name), 1, "{name} ran once");
    }
}

/// A `get_all` of keys the view resolved allocates its list of values and nothing else.
#[test]
fn a_warm_get_all_allocates_only_its_list() {
    let files: Vec<(String, String)> = (0..16).map(|i| (format!("warm{i}"), "x".repeat(i))).collect();
    let (db, one, _) = spread_setup(&files);
    let asked: Vec<String> = (0..16).map(|i| format!("warm{i}")).collect();
    assert_eq!(db.pin().get::<CountedAll>(one, &(asked.clone(), 0)), Ok(fold(0..16)));
    assert_eq!(db.pin().get::<CountedAll>(one, &(asked, 1)), Ok(fold(0..16)));
    assert_eq!(WARM_ALL.load(Ordering::SeqCst), 1, "a warm get_all allocated more than its list");
}

/// A file's length, of a kind whose equal values are kept once.
struct SameLen;

impl Query for SameLen {
    const KIND: QueryKind = QueryKind(0x2f14);
    const NAME: &'static str = "same len";
    const PRODUCER: ContentId = ContentId([0x14; 32]);
    const SHARE: bool = true;
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        ran("same len");
        Ok(Num(cx.file(key)?.map_or(u64::MAX, |file| file.bytes.len() as u64)))
    }
}

fn same_len(db: &Database, view: ViewId, name: &str) -> u64 {
    db.pin().get::<SameLen>(view, &name.to_owned()).expect("same len")
}

/// Each memo's value word of `name`'s key.
fn words(db: &Database, name: &str) -> Vec<u64> {
    let task = blaze_reclaim::enter();
    db.slot(&task, &key::<SameLen>(name))
        .expect("no image")
        .map_or_else(Vec::new, |slot| slot.memos().iter().map(|&memo| db.store.value_word(&task, memo)).collect())
}

/// Collects until every memo let go of is freed: other tests' threads may still be in tasks
/// that began before it was unlinked.
fn collect_until_freed(db: &Database) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut effects = Vec::new();
    loop {
        db.collect(&[], &mut effects);
        assert!(effects.is_empty(), "nothing cold to write: {effects:?}");
        if db.store.dying() == 0 {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "what the collections let go of was never freed");
        std::thread::yield_now();
    }
}

/// A shared kind's equal values, from two keys and from one key's memos in two views (its
/// file differs, its length doesn't), are one value row: every memo's word names it.
fn shared_three() -> (Database, Arc<MemTree>, ViewId, ViewId) {
    let (mut db, tree) = setup();
    db.register::<SameLen>();
    let one = view(&db, &tree, &[("a", "xyz"), ("b", "abc")]);
    let two = view(&db, &tree, &[("a", "pqr")]);
    assert_eq!((same_len(&db, one, "a"), same_len(&db, one, "b"), same_len(&db, two, "a")), (3, 3, 3));
    assert_eq!(runs("same len"), 3, "a's file differs in the two views: it runs in each");
    (db, tree, one, two)
}

#[test]
fn equal_values_of_a_shared_kind_from_two_views_and_two_keys_hold_one_row() {
    let (db, _tree, _, _) = shared_three();
    let (a, b) = (words(&db, "a"), words(&db, "b"));
    assert_eq!(a.len(), 2, "a has a memo for each view's file");
    assert!(a.iter().chain(&b).all(|&word| word != 0 && word == b[0]), "one row: {a:?} {b:?}");
    assert_eq!(db.store.shared_rows(), 1);
}

#[test]
fn evicting_or_collecting_one_memo_keeps_the_others_shared_value() {
    let (db, _tree, one, two) = shared_three();
    let row = words(&db, "b")[0];
    assert_eq!(db.evict(&key::<SameLen>("b")), 1);
    assert_eq!(words(&db, "b"), [0], "b's memo let go of the row");
    assert_eq!(words(&db, "a"), [row, row], "a's memos still name it");
    assert_eq!((same_len(&db, one, "a"), runs("same len")), (3, 3), "a's value is read, not made again");
    assert_eq!(same_len(&db, one, "b"), 3);
    assert_eq!(runs("same len"), 4, "b's evicted value is made again");
    assert_eq!(words(&db, "b"), [row], "and refilled into the shared row, not a row of its own");

    db.close(two);
    collect_until_freed(&db);
    assert_eq!(words(&db, "a").len(), 1, "a's memo of the closed view is collected");
    assert_eq!(db.store.shared_rows(), 1, "the row stays while a memo names it");
    assert_eq!((same_len(&db, one, "a"), same_len(&db, one, "b"), runs("same len")), (3, 3, 4));

    db.close(one);
    collect_until_freed(&db);
    assert_eq!(db.store.shared_rows(), 0, "no memo names the row: it is freed");
    let task = blaze_reclaim::enter();
    assert!(matches!(db.slot(&task, &key::<SameLen>("a")), Ok(None)) && matches!(db.slot(&task, &key::<SameLen>("b")), Ok(None)));
}

/// The image writes equal values once, whatever their kind: a shared kind's memos and a plain
/// kind's map the same bytes of the blob once started from it.
#[test]
fn equal_values_go_into_the_image_once() {
    let (db, tree, one, _) = shared_three();
    assert_eq!((db.pin().get::<Len>(one, &"a".to_owned()), db.pin().get::<Len>(one, &"b".to_owned())), (Ok(3), Ok(3)));
    let (bytes, _) = image_of(&db, Builder::new());

    let mut started = Database::open_image(config(&tree), opened(&bytes)).expect("an image");
    register(&mut started);
    started.register::<SameLen>();
    reset();
    let same = view(&started, &tree, &[("a", "xyz"), ("b", "abc")]);
    assert_eq!((same_len(&started, same, "a"), same_len(&started, same, "b")), (3, 3));
    let len = |name: &str| started.pin().get::<Len>(same, &name.to_owned());
    assert_eq!((len("a"), len("b")), (Ok(3), Ok(3)));
    assert_eq!((runs("same len"), runs("len")), (0, 0), "every memo verified from the image");

    let task = blaze_reclaim::enter();
    let at = |key: QueryKey| {
        let slot = started.slot(&task, &key).expect("not corrupt").expect("a key of the image");
        let bytes = started.store.bytes(&task, *slot.memos().last().expect("a memo")).expect("mapped");
        (bytes.as_ptr(), bytes.len())
    };
    let shared = at(key::<SameLen>("b"));
    assert_eq!(at(key::<SameLen>("a")), shared, "a shared kind's equal values are one copy in the blob");
    assert_eq!(at(key::<Len>("a")), shared, "so are any kind's: Len's value has the same bytes");
    assert_eq!(at(key::<Len>("b")), shared);
}

/// A cold pack writes equal values once, whatever their kind, and every memo maps them.
#[test]
fn equal_values_go_into_a_cold_pack_once() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "xyz"), ("b", "abc")]);
    assert_eq!((db.pin().get::<Len>(one, &"a".to_owned()), db.pin().get::<Len>(one, &"b".to_owned())), (Ok(3), Ok(3)));
    db.close(one);
    let mut effects = Vec::new();
    db.collect(&[key::<Len>("a"), key::<Len>("b")], &mut effects);
    let (_, bytes, tag) = written(&mut effects);
    assert_eq!(bytes, crate::write_value(&Num(3)), "the pack holds the one value once");
    db.complete(Completion { tag, done: Done::Written }, &mut effects);
    let (_, tag) = mapped(&mut effects);
    db.complete(Completion { tag, done: Done::Mapped(mapping(&bytes)) }, &mut effects);
    for name in ["a", "b"] {
        assert!(matches!(db.memo(&key::<Len>(name)).map(|memo| memo.value), Some(Stored::Disk(_))), "{name}");
    }
}

/// A context [`Database::provide`] gives every query.
struct Scale(u64);

/// A file's length times the database's [`Scale`]; `u64::MAX` with none given.
struct Scaled;

impl Query for Scaled {
    const KIND: QueryKind = QueryKind(0x2f15);
    const NAME: &'static str = "scaled";
    const PRODUCER: ContentId = ContentId([0x15; 32]);
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        let Some(&Scale(scale)) = cx.context::<Scale>() else { return Ok(Num(u64::MAX)) };
        Ok(Num(cx.file(key)?.map_or(0, |file| file.bytes.len() as u64) * scale))
    }
}

#[test]
fn a_query_reads_the_context_of_its_type_the_database_was_given() {
    let (mut db, tree) = setup();
    db.register::<Scaled>();
    let files = [("a", "abcd")];
    let one = view(&db, &tree, &files);
    assert_eq!(db.pin().get::<Scaled>(one, &"a".to_owned()), Ok(u64::MAX), "none given yet");

    let (mut db, tree) = setup();
    db.register::<Scaled>();
    db.provide(Scale(3));
    db.provide("a context of another type");
    let one = view(&db, &tree, &files);
    assert_eq!(db.pin().get::<Scaled>(one, &"a".to_owned()), Ok(12));
}

/// What a job on a fiber carries across a wait (`carry_with_fiber_jobs`): taking its marks off
/// leaves the thread outside any query with empty stacks, for the next job to run there; putting
/// them back gives the job its own again.
#[test]
fn a_switching_job_takes_its_query_marks_and_stacks_with_it() {
    let key = crate::QueryKey { kind: crate::QueryKind(0x2f01), input: ContentId([4; 32]) };
    let inside = crate::Inside::enter();
    let frame = crate::Frame::push(key);
    assert!(crate::inside_a_query());
    _ = crate::unreadable(BROKEN);

    let taken = (crate::take_inside(), crate::take_stacks(), crate::take_unreadable());
    assert!(!crate::inside_a_query(), "the thread is outside any query once the job is off it");
    assert_eq!(crate::Run::new().base, 0, "the thread's key stack is empty for the next job");
    assert!(crate::take_unreadable_corrupt().is_none());

    crate::put_inside(taken.0);
    crate::put_stacks(taken.1);
    crate::put_unreadable(taken.2);
    assert!(crate::inside_a_query());
    assert_eq!(crate::Run::new().base, 1, "the job's own key is back");
    assert_eq!(crate::take_unreadable_corrupt(), Some(BROKEN));
    drop(frame);
    drop(inside);
    assert!(!crate::inside_a_query());
}

/// Model tests of the database's own lock-free state (docs/v2/plan/lanes/safety/model.md): the
/// collection generation with each memo's `born`, and the thread-local that marks a query
/// running.
#[cfg(blaze_check)]
mod model {
    use std::sync::{Arc, atomic::Ordering};

    use blaze_check::{Builder, thread};
    use blaze_ids::{ContentId, Fingerprint};

    use super::WORD;
    use crate::{Database, Inside, QueryKey, QueryKind, inside_a_query};

    const KEY: QueryKey = QueryKey { kind: QueryKind(0x2f00), input: ContentId([3; 32]) };

    /// `Database::add_memo` from two threads racing `Database::collect`: a memo given by a thread
    /// that saw the collection's bump of the generation is young to it and kept; one given
    /// before may go (no view reaches it). A key keeps only memos given, each once. Finding 0 of
    /// the safety lane's request: before `born` was only raised, the giver that loaded the
    /// generation first could lower the other's stamp, and this schedule dropped a young memo.
    #[test]
    fn model_collect_keeps_a_memo_given_after_it_bumped_the_generation() {
        let mut builder = Builder::new();
        // `collect` loads or locks each shard of each table a few times: each is a branch.
        builder.max_branches = builder.max_branches.max(32 * blaze_ids::shard_count() + 10_000);
        builder.check(|| {
            let db = Arc::new(Database::empty());
            let give = |fingerprint: u128| {
                let db = db.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    let code = db.store.code(WORD, ContentId([1; 32]));
                    let made = db.store.make(KEY, Fingerprint(fingerprint), code, &[], &[]);
                    let saw_bump = db.generation.load(Ordering::SeqCst) > 0;
                    db.add_memo(&task, KEY, made).expect("no image");
                    saw_bump
                })
            };
            let givers = [give(1), give(2)];
            // The one thread here in a `blaze_mem::frame::task` (`collect`'s scratch is the OS
            // thread's, which every model thread shares).
            db.collect(&[], &mut Vec::new());
            let saw = givers.map(|giver| giver.join().expect("a giver"));
            let task = blaze_reclaim::enter();
            let kept: Vec<u128> = db.slot(&task, &KEY).expect("no image").map_or_else(Vec::new, |slot| {
                slot.memos().iter().map(|&memo| db.store.fingerprint(&task, memo).0).collect()
            });
            assert!(kept.iter().all(|it| *it == 1 || *it == 2), "only memos given are kept: {kept:?}");
            for (fingerprint, saw_bump) in [1, 2].into_iter().zip(saw) {
                let count = kept.iter().filter(|it| **it == fingerprint).count();
                assert!(count <= 1, "memo {fingerprint} is kept {count} times: {kept:?}");
                assert!(
                    !saw_bump || count == 1,
                    "memo {fingerprint}, given after the collection bumped the generation, was dropped: {kept:?}"
                );
            }
        });
    }

    /// `INSIDE` is each thread's own: one thread's running query is never another's, at any
    /// interleaving of their marks.
    #[test]
    fn model_inside_a_query_is_per_thread() {
        blaze_check::model(|| {
            let other = thread::spawn(|| {
                assert!(!inside_a_query(), "another thread's query is not this one's");
                thread::yield_now();
                let inside = Inside::enter();
                thread::yield_now();
                assert!(inside_a_query());
                drop(inside);
                assert!(!inside_a_query());
            });
            let inside = Inside::enter();
            thread::yield_now();
            assert!(inside_a_query(), "this thread's mark is its own");
            drop(inside);
            thread::yield_now();
            assert!(!inside_a_query());
            other.join().expect("the other thread");
        });
    }
}
