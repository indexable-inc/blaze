use std::{
    collections::BTreeMap,
    convert::Infallible,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_cas::{Corrupt, Key, Mismatch, Ticket};
use blaze_codegen::ExecTable;
use blaze_core::Traced;
use blaze_ids::{ContentId, rng::SplitMix64 as Rng};
use blaze_testsel::{Basis, Bound, Cached, Choice, FnId, Ran, Record, Selector, Subject, Why};
use blaze_usercode::{
    test::{Attrs, Executed, Lines, Passes, Trace},
    trace::{self, Inputs, Seen},
};

use super::{
    image::{Selectors, image, record},
    traced::{Common, Splice},
};

// The budget tests here count through the test binary's one counting allocator,
// `crate::sandbox`'s.

/// The machine a test reads: `/w/p{n}` is nothing for `n % 3 == 0`, a file for 1 and a
/// directory for 2; the environment is `V0` to `V3`.
struct World {
    env: Vec<(OsString, OsString)>,
}

impl World {
    fn new() -> World {
        World { env: (0..4).map(|it| (OsString::from(format!("V{it}")), OsString::from(format!("value {it}")))).collect() }
    }
}

fn kind(path: &Path) -> u8 {
    path.to_str().and_then(|it| it.strip_prefix("/w/p")).and_then(|it| it.parse::<u8>().ok()).map_or(0, |it| it % 3)
}

fn hash(what: &'static str, path: &Path) -> [u8; 32] {
    ContentId::of(what, path.as_os_str().as_encoded_bytes()).0
}

impl Inputs for World {
    type Error = Infallible;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Infallible> {
        Ok(match kind(path) {
            1 => Some(hash("file", path)),
            2 => Some(trace::DIRECTORY),
            _ => None,
        })
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Infallible> {
        Ok((kind(path) == 2).then(|| hash("listing", path)))
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        &self.env
    }

    fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, Infallible> {
        Ok(false)
    }

    fn entry(&mut self, _: &Path) -> Result<trace::Entry, Infallible> {
        Ok(trace::Entry::Missing)
    }
}

/// Lines of every kind over a few paths and names, so the two parts share many.
fn lines(rng: &mut Rng, count: u64) -> Vec<(Seen, String)> {
    (0..count)
        .map(|_| {
            let n = rng.below(12);
            match rng.below(10) {
                0 | 1 => (Seen::Read, format!("/w/p{n}")),
                2 => (Seen::Looked, format!("/w/p{n}")),
                3 | 4 => (Seen::Listed, format!("/w/p{n}")),
                5 if n == 0 => (Seen::Env, trace::ALL_ENV.to_owned()),
                5 | 6 => (Seen::Env, format!("V{}", n % 6)),
                7 => (Seen::Untraceable, ["clock", "random", "network", "descriptor", "other"][n as usize % 5].to_owned()),
                8 => (Seen::Incomplete, "a record cut short".to_owned()),
                _ => (Seen::RefusedWrite, format!("/w/p{n}")),
            }
        })
        .collect()
}

fn assembled(lines: &[(Seen, String)]) -> Traced {
    let Ok(traced) = trace::assemble_seen(lines.iter().map(|it| (it.0, it.1.as_str())), &mut World::new());
    traced
}

fn increasing<T: Ord>(list: &[T]) -> bool {
    list.windows(2).all(|it| it[0] < it[1])
}

/// A server's shared lines assembled once with each test's own spliced in are what one
/// assembly of all of them gives, as the store keeps it (each list strictly increasing); taken
/// out again, the shared part is as it was.
#[test]
fn a_splice_is_what_one_assembly_of_both_parts_gives() {
    let mut rng = Rng::new(7);
    let mut splice = Splice::default();
    for _ in 0..2000 {
        let count = rng.below(48);
        let all = lines(&mut rng, count);
        let cut = rng.below(all.len() as u64 + 1) as usize;
        let mut base = assembled(&all[..cut]);
        let before = base.clone();
        splice.put(&mut base, assembled(&all[cut..]));
        assert_eq!(base, assembled(&all), "{all:?} cut at {cut}");
        assert!(increasing(&base.files) && increasing(&base.dirs), "{base:?}");
        assert!(increasing(&base.env) && increasing(&base.untraceable), "{base:?}");
        splice.undo(&mut base);
        assert_eq!(base, before, "{all:?} cut at {cut}");
    }
}

/// A file the shared part listed and found nothing at and the own part read keeps the read's
/// value while lent, and its listing's again after.
#[test]
fn a_read_of_what_a_listing_found_nothing_at_stands() {
    let listed = [(Seen::Listed, "/w/p1".to_owned())];
    let read = [(Seen::Read, "/w/p1".to_owned())];
    let mut base = assembled(&listed);
    assert_eq!(base.files, vec![(PathBuf::from("/w/p1"), None)]);
    let mut splice = Splice::default();
    splice.put(&mut base, assembled(&read));
    assert_eq!(base, assembled(&[listed[0].clone(), read[0].clone()]));
    assert_eq!(base.files, vec![(PathBuf::from("/w/p1"), Some(hash("file", Path::new("/w/p1"))))]);
    splice.undo(&mut base);
    assert_eq!(base, assembled(&listed));
}

/// Once its lists have grown to a result's size, a splice and its undo allocate nothing.
#[test]
fn a_warm_splice_allocates_nothing() {
    let shared: Vec<(Seen, String)> = (0..2000).map(|it| (Seen::Read, format!("/lib/{it:05}"))).collect();
    let own: Vec<(Seen, String)> = [7, 900, 1999].iter().map(|it| (Seen::Read, format!("/lib/{it:05}x"))).collect();
    let mut base = assembled(&shared);
    let mut splice = Splice::default();
    splice.put(&mut base, assembled(&own));
    splice.undo(&mut base);
    let own = assembled(&own);
    let ((), used) = blaze_alloc_count::measure(|| {
        splice.put(&mut base, own);
        splice.undo(&mut base);
    });
    assert_eq!(used.allocations, 0, "a warm splice of 3 files into 2000");
    assert_eq!(base, assembled(&shared));
}

/// A test whose pass is kept is reported as the agent API marks a cached result: not run now,
/// and the pass that stands with how long it took and what it was kept by.
#[test]
fn a_kept_pass_is_reported_cached() {
    use blaze_api::{Basis, TestOutcome};

    let kept = Cached { took: Duration::from_micros(9_999), basis: blaze_testsel::Basis::Executed };
    let result = blaze_testsel::api::result("tests/parse", 3, "parse::nested", &kept);
    assert_eq!((result.outcome, result.millis, result.output.len()), (TestOutcome::Unreached, 0, 0));
    assert_eq!((result.executable, result.outside), (3, false));
    assert_eq!((result.binary.as_str(), result.name.as_str()), ("tests/parse", "parse::nested"));
    let cached = result.cached.expect("a kept pass is cached");
    assert_eq!((cached.outcome, cached.millis, cached.basis), (TestOutcome::Passed, 9, Basis::Executed));
    let reached = Cached { took: Duration::ZERO, basis: blaze_testsel::Basis::Reached { why: "killed".to_owned() } };
    let cached = blaze_testsel::api::result("a", 0, "b", &reached).cached.expect("a kept pass is cached");
    assert_eq!((cached.millis, cached.basis), (0, Basis::Reached { why: "killed".to_owned() }));
}

/// The store, as a map that refuses to replace a value.
#[derive(Default)]
struct Mem {
    values: BTreeMap<Key, Vec<u8>>,
    puts: u64,
}

impl Passes for Mem {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt> {
        Ok(self.values.get(key).map(Vec::as_slice))
    }

    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => {
                return Err(Mismatch {
                    key,
                    kept: ContentId::of("blaze v2 cargo test mem 1", kept),
                    given: ContentId::of("blaze v2 cargo test mem 1", &bytes),
                });
            }
            Some(_) => {}
            None => _ = self.values.insert(key, bytes),
        }
        self.puts += 1;
        Ok(Ticket(self.puts))
    }
}

fn fn_id(n: u32) -> ContentId {
    ContentId::of("blaze v2 cargo test fn 1", &n.to_le_bytes())
}

fn bound(n: u32, version: u32) -> ContentId {
    ContentId::of("blaze v2 cargo test bound 1", &[n.to_le_bytes(), version.to_le_bytes()].concat())
}

/// The table of a binary of three functions at `versions`: `0` and `2` in slots 0 and 1, `1`
/// called directly, and one test `t` that reaches all three.
fn exec_table(versions: [u32; 3]) -> ExecTable {
    let mut fns: Vec<(ContentId, ContentId)> = (0..3).map(|n| (fn_id(n), bound(n, versions[n as usize]))).collect();
    fns.sort_unstable();
    let at = |n: u32| fns.iter().position(|it| it.0 == fn_id(n)).expect("in the table") as u32;
    let slot_fn = vec![at(0), at(2)];
    let mut reached = vec![at(0), at(1), at(2)];
    reached.sort_unstable();
    ExecTable { fns, slot_fn, beside: ContentId::default(), tests: vec![("t".to_owned(), reached)] }
}

/// A fork server's record is selection's: whole when its hits are strictly increasing and in
/// the table, else not whole, saying why.
#[test]
fn what_a_test_executed_is_its_record() {
    assert_eq!(record(Executed::Hits { slots: 4, hits: vec![1, 3] }), Record::Complete { slots: 4, hits: vec![1, 3] });
    assert_eq!(record(Executed::Hits { slots: 4, hits: Vec::new() }), Record::Complete { slots: 4, hits: Vec::new() });
    assert!(matches!(record(Executed::Hits { slots: 4, hits: vec![3, 1] }), Record::Incomplete { .. }));
    assert!(matches!(record(Executed::Hits { slots: 2, hits: vec![2] }), Record::Incomplete { .. }));
    let why = "the child was killed after 60s".to_owned();
    assert_eq!(record(Executed::Not(why.clone())), Record::Incomplete { why });
}

/// The image keeps every slot, an emptied one too, as a record names slots by place; a table
/// that is not one is refused naming its binary.
#[test]
fn a_table_is_its_image_slot_for_slot() {
    let table = exec_table([0, 0, 0]);
    let made = image("tests/parse", &table).expect("a table");
    assert_eq!((made.len(), made.slots()), (3, 2));
    assert_eq!(made.bound_of(&FnId(fn_id(1))), Some(Bound(bound(1, 0))));

    let mut emptied = exec_table([0, 0, 0]);
    emptied.slot_fn.insert(1, ExecTable::EMPTY);
    let made = image("tests/parse", &emptied).expect("an emptied slot is a slot of no function");
    assert_eq!(made.slots(), 3);

    let mut broken = exec_table([0, 0, 0]);
    broken.slot_fn.push(7);
    let why = image("tests/parse", &broken).err().expect("a slot of no function in the table");
    assert!(why.contains("`tests/parse`"), "{why}");
}

/// A suite's selector is kept between runs, moved to its binary only when it was relinked, and
/// held by one run at a time.
#[test]
fn a_suite_keeps_its_selector_and_moves_it_to_a_relinked_binary() {
    let selectors = Selectors::default();
    let given = ContentId::of("blaze v2 cargo test given 1", b"suite");
    let (first, second) = (ContentId::of("blaze v2 cargo test bin 1", b"1"), ContentId::of("blaze v2 cargo test bin 1", b"2"));
    let selector = selectors.take(given, first, || image("t", &exec_table([0, 0, 0]))).expect("made");
    assert_eq!((selector.given(), selectors.len()), (given, 0));
    selectors.give(first, selector);
    assert_eq!(selectors.len(), 1);
    let selector = selectors.take(given, first, || panic!("the selector's image is of this binary")).expect("kept");
    assert_eq!(selectors.len(), 0);
    selectors.give(first, selector);
    let mut made = false;
    let selector = selectors
        .take(given, second, || {
            made = true;
            image("t", &exec_table([0, 0, 1]))
        })
        .expect("moved");
    assert!(made);
    assert_eq!(selector.image().bound_of(&FnId(fn_id(2))), Some(Bound(bound(2, 1))));
    let refused = selectors.take(given, first, || Err("no table".to_owned()));
    assert_eq!(refused.err().as_deref(), Some("no table"), "none is kept while one is taken: a new one's image is asked for");
}

fn choose(selector: &mut Selector, store: &Mem, subject: &Subject<'_>) -> Choice {
    selector.choose(store, subject, false, &mut World::new()).expect("chosen")
}

/// Through the daemon's path (the image of a table, a server's record, a selector), a pass
/// kept by what it executed stands until a function it executed changes; one whose record is
/// not whole stands by what it reaches, saying why.
#[test]
fn a_pass_stands_until_what_it_executed_changes() {
    let attrs = Attrs::default();
    let table = exec_table([0, 0, 0]);
    let subject = Subject { name: "t", attrs: &attrs, outside: false, reached: &table.tests[0].1 };
    let traced = Traced::default();
    let given = ContentId::of("blaze v2 cargo test given 1", b"suite");

    let mut store = Mem::default();
    let mut selector = Selector::new(given, image("t", &table).expect("a table"));
    assert!(matches!(choose(&mut selector, &store, &subject), Choice::Run { why: Why::New, .. }));
    // It called slot 0's function, `0`, and not slot 1's, `2`.
    let executed = record(Executed::Hits { slots: 2, hits: vec![0] });
    let ran = Ran { passed: true, took: Duration::from_millis(3), traced: Some(&traced), record: &executed };
    selector.keep(&mut store, &subject, &ran).expect("kept");
    let cached = |choice: Choice| match choice {
        Choice::Cached(it) => it,
        other => panic!("ran: {other:?}"),
    };
    assert_eq!(cached(choose(&mut selector, &store, &subject)).basis, Basis::Executed);
    selector.advance(image("t", &exec_table([0, 0, 1])).expect("a table"));
    assert_eq!(cached(choose(&mut selector, &store, &subject)).took, Duration::from_millis(3));
    selector.advance(image("t", &exec_table([1, 0, 1])).expect("a table"));
    let choice = choose(&mut selector, &store, &subject);
    assert!(matches!(choice, Choice::Run { why: Why::Changed(FnId(id)), .. } if id == fn_id(0)), "{choice:?}");

    let mut store = Mem::default();
    let mut selector = Selector::new(given, image("t", &table).expect("a table"));
    let killed = record(Executed::Not("the child was killed".to_owned()));
    let ran = Ran { passed: true, took: Duration::from_millis(5), traced: Some(&traced), record: &killed };
    selector.keep(&mut store, &subject, &ran).expect("kept");
    let kept = cached(choose(&mut selector, &store, &subject));
    assert_eq!(kept.basis, Basis::Reached { why: "the child was killed".to_owned() });
    selector.advance(image("t", &exec_table([0, 1, 0])).expect("a table"));
    let choice = choose(&mut selector, &store, &subject);
    assert!(matches!(choice, Choice::Run { why: Why::Unlogged, .. }), "{choice:?}");
}

fn of(lines: &[(Seen, String)]) -> Lines {
    lines.iter().map(|it| (it.0, it.1.as_str())).collect()
}

/// Through the cache, every kind of trace is what [`Trace::traced`] says it is, result after
/// result.
#[test]
fn a_result_is_traced_as_its_trace_says() {
    let mut rng = Rng::new(11);
    let mut common = Common::default();
    let mut world = World::new();
    for _ in 0..200 {
        let count = rng.below(32);
        let all = lines(&mut rng, count);
        let Ok(expected) = Trace::Lines(of(&all)).traced(&mut world);
        let Ok(got) = common.traced(Trace::Lines(of(&all)), &mut world, |it| it.cloned());
        assert_eq!(got, expected, "{all:?}");
    }
    let whole = assembled(&lines(&mut rng, 20));
    let Ok(got) = common.traced(Trace::Whole(whole.clone()), &mut world, |it| it.cloned());
    assert_eq!(got, Some(whole));
    let Ok(got) = common.traced(Trace::Unknown("the sandbox's record was cut".to_owned()), &mut world, |it| it.cloned());
    assert_eq!(got, None);
}
