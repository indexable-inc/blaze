use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_codegen::ExecTable;
use blaze_core::{Traced, Untraceable};
use blaze_ids::{ContentId, rng::SplitMix64 as Rng};
use blaze_keyed::kind::{Key, TestIndex};
use blaze_testsel::{
    Bound, FnId, Record,
    ask::Basis,
    machine::{By, NotKept, Verdict},
};
use blaze_usercode::{
    test::{Executed, Lines, Trace},
    trace::{self, Inputs, Seen},
};

use super::{
    image::{Held, Selectors, image, record},
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
        World {
            env: (0..4)
                .map(|it| (OsString::from(format!("V{it}")), OsString::from(format!("value {it}"))))
                .collect(),
        }
    }
}

fn kind(path: &Path) -> u8 {
    path.to_str()
        .and_then(|it| it.strip_prefix("/w/p"))
        .and_then(|it| it.parse::<u8>().ok())
        .map_or(0, |it| it % 3)
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
                7 => (
                    Seen::Untraceable,
                    ["clock", "random", "network", "descriptor", "other"][n as usize % 5]
                        .to_owned(),
                ),
                8 => (Seen::Incomplete, "a record cut short".to_owned()),
                _ => (Seen::RefusedWrite, format!("/w/p{n}")),
            }
        })
        .collect()
}

fn assembled(lines: &[(Seen, String)]) -> Traced {
    let Ok(traced) =
        trace::assemble_seen(lines.iter().map(|it| (it.0, it.1.as_str())), &mut World::new());
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
    let shared: Vec<(Seen, String)> =
        (0..2000).map(|it| (Seen::Read, format!("/lib/{it:05}"))).collect();
    let own: Vec<(Seen, String)> =
        [7, 900, 1999].iter().map(|it| (Seen::Read, format!("/lib/{it:05}x"))).collect();
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

/// A cached test is reported passed, in no time, with the pass that stands (how long its last
/// run took, and what it was kept by); a run not kept names why in a word and a sentence, and
/// a failure or a kept run says nothing of keeping.
#[test]
fn a_cached_test_is_a_pass_and_a_run_not_kept_says_why() {
    use blaze_api::TestOutcome;

    let result = super::cached("tests/parse", 3, "parse::nested", Basis::Executed, Some(9_999));
    assert_eq!(
        (result.outcome, result.millis, result.output.len(), result.not_kept),
        (TestOutcome::Passed, 0, 0, None)
    );
    assert_eq!((result.executable, result.outside), (3, false));
    assert_eq!((result.binary.as_str(), result.name.as_str()), ("tests/parse", "parse::nested"));
    let cached = result.cached.expect("a kept pass is cached");
    assert_eq!(
        (cached.outcome, cached.millis, cached.basis),
        (TestOutcome::Passed, 9, blaze_api::Basis::Executed)
    );
    let cached = super::cached("a", 0, "b", Basis::Reached, None).cached.expect("cached");
    assert_eq!(cached.millis, 0);
    assert!(matches!(cached.basis, blaze_api::Basis::Reached { .. }), "{cached:?}");

    let clock = Verdict::NotKept { passed: true, micros: 5, why: NotKept::Untraceable(Untraceable::Clock) };
    let said = super::not_kept(clock).expect("a pass not kept says why");
    assert_eq!((said.no.as_str(), said.why.as_str()), ("clock", "it read the wall clock"));
    let failed = Verdict::NotKept { passed: false, micros: 5, why: NotKept::Failed };
    assert_eq!(super::not_kept(failed), None);
    assert_eq!(super::not_kept(Verdict::Ran { micros: 5, basis: Basis::Executed }), None);
    let unchanged = Verdict::Cached { by: By::Unchanged, basis: Basis::Executed, micros: None };
    assert_eq!(super::not_kept(unchanged), None);
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
    let mut fns: Vec<(ContentId, ContentId)> =
        (0..3).map(|n| (fn_id(n), bound(n, versions[n as usize]))).collect();
    fns.sort_unstable();
    let at = |n: u32| fns.iter().position(|it| it.0 == fn_id(n)).expect("in the table") as u32;
    let slot_fn = vec![at(0), at(2)];
    let mut reached = vec![at(0), at(1), at(2)];
    reached.sort_unstable();
    let (graph, roots) = blaze_codegen::exec::TestGraph::listing(3, &[reached]);
    ExecTable {
        fns,
        slot_fn,
        beside: ContentId::default(),
        tests: vec![("t".to_owned(), roots[0])],
        graph,
    }
}

/// A fork server's record is selection's: whole when its hits are strictly increasing and in
/// the table, else not whole, saying why.
#[test]
fn what_a_test_executed_is_its_record() {
    assert_eq!(
        record(Executed::Hits { slots: 4, hits: vec![1, 3] }),
        Record::Complete { slots: 4, hits: vec![1, 3] }
    );
    assert_eq!(
        record(Executed::Hits { slots: 4, hits: Vec::new() }),
        Record::Complete { slots: 4, hits: Vec::new() }
    );
    assert!(matches!(
        record(Executed::Hits { slots: 4, hits: vec![3, 1] }),
        Record::Incomplete { .. }
    ));
    assert!(matches!(
        record(Executed::Hits { slots: 2, hits: vec![2] }),
        Record::Incomplete { .. }
    ));
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

/// A suite's head is held between runs by its settings, with the image of the binary it was
/// filed at: a run of the same binary reuses the image, a relinked one makes its own and
/// keeps the held one as the image before (whose changes the head's rows are checked
/// against), and one run holds it at a time.
#[test]
fn a_suite_holds_its_head_and_the_image_it_was_filed_at() {
    let selectors = Selectors::default();
    let given = ContentId::of("blaze v2 cargo test given 1", b"suite");
    let (first, second) = (
        ContentId::of("blaze v2 cargo test bin 1", b"1"),
        ContentId::of("blaze v2 cargo test bin 1", b"2"),
    );
    let head = Key::<TestIndex>::from_id(ContentId::of("blaze v2 cargo test head 1", b"h"));
    let taken = selectors.take(given, first, || image("t", &exec_table([0, 0, 0]))).expect("made");
    assert!(taken.head.is_none() && taken.before.is_none() && taken.held.is_none());
    assert_eq!(selectors.len(), 0);
    selectors.give(given, Held { binary: first, image: taken.now, head: Some(head) });
    assert_eq!(selectors.len(), 1);
    let taken = selectors
        .take(given, first, || panic!("the held image is of this binary"))
        .expect("held");
    assert_eq!((taken.head, taken.before.is_none(), selectors.len()), (Some(head), true, 0));
    let held = taken.held.expect("what was held is given back when the run does not end");
    let state = held.image.state();
    selectors.give(given, held);
    let mut made = false;
    let taken = selectors
        .take(given, second, || {
            made = true;
            image("t", &exec_table([0, 0, 1]))
        })
        .expect("relinked");
    assert!(made);
    assert_eq!(taken.now.bound_of(&FnId(fn_id(2))), Some(Bound(bound(2, 1))));
    let before = taken.before.expect("the image the head was filed at");
    assert_eq!((before.state(), taken.head), (state, Some(head)));
    assert!(!Arc::ptr_eq(&before, &taken.now));
    let refused = selectors.take(given, first, || Err("no table".to_owned()));
    assert_eq!(
        refused.err().as_deref(),
        Some("no table"),
        "none is held while one run holds it: a new image is asked for"
    );
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
    let Ok(got) = common.traced(
        Trace::Unknown("the sandbox's record was cut".to_owned()),
        &mut world,
        |it| it.cloned(),
    );
    assert_eq!(got, None);
}

/// Keep-going (`each`): a test run whose binaries are `edition`'s, refused as a dev test binary
/// with no start hook, an artifact with no tests, `blaze-hash`'s, which runs, and `stdx`'s,
/// refused too, names both refusals in order and still runs `blaze-hash`'s tests. Stopping at
/// the first refusal ran none (coord-114).
#[test]
fn a_binary_that_cant_run_is_named_and_keeps_no_other_from_running() {
    let built = ["edition", "no tests", "blaze-hash", "stdx"];
    let (made, refused) = super::each(built.into_iter(), |name| match name {
        "blaze-hash" => Ok(Some(name)),
        "no tests" => Ok(None),
        _ => Err(blaze_api::ApiError::new(
            format!(
                "`{name}` runs under profile `test`, so on the daemon's code memory, and can't"
            ),
            None,
        )),
    });
    assert_eq!(made, ["blaze-hash"]);
    let refused: Vec<&str> = refused.iter().map(|it| it.what.as_str()).collect();
    assert_eq!(
        refused,
        [
            "`edition` runs under profile `test`, so on the daemon's code memory, and can't",
            "`stdx` runs under profile `test`, so on the daemon's code memory, and can't",
        ]
    );
}
