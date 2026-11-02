use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_cas::{Corrupt, Key, Mismatch, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_ids::ContentId;
use blaze_usercode::test::ShouldPanic;

use super::*;
use crate::table::{
    Version,
    tests::{bound, id},
};

/// The store, as a map that refuses to replace a value.
#[derive(Default)]
pub(crate) struct Mem {
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
                    kept: ContentId::of("blaze v2 testsel mem 1", kept),
                    given: ContentId::of("blaze v2 testsel mem 1", &bytes),
                });
            }
            Some(_) => {}
            None => _ = self.values.insert(key, bytes),
        }
        self.puts += 1;
        Ok(Ticket(self.puts))
    }
}

/// The files and environment a test sees.
#[derive(Clone, Default)]
pub(crate) struct World {
    pub(crate) files: BTreeMap<PathBuf, [u8; 32]>,
    pub(crate) env: Vec<(OsString, OsString)>,
}

impl Inputs for World {
    type Error = String;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(self.files.get(path).copied())
    }

    fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.sort();
        &self.env
    }

    fn names(&mut self, _: &Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> {
        Ok(false)
    }

    fn entry(&mut self, _: &Path) -> Result<blaze_usercode::trace::Entry, String> {
        Ok(blaze_usercode::trace::Entry::Missing)
    }
}

/// A binary of `versions.len()` functions, function `n` at version `versions[n]`; the first
/// `slots` are called through slots, slot `s` holding function `s`.
pub(crate) fn image(versions: &[u32], slots: u32, beside: u32) -> Image {
    let fns = versions.iter().enumerate().map(|(n, &v)| Version { id: id(n as u32), bound: bound(n as u32, v) }).collect();
    Image::new(fns, (0..slots).collect(), ContentId::of("blaze v2 testsel beside 1", &beside.to_le_bytes())).unwrap()
}

fn given() -> ContentId {
    ContentId::of("blaze v2 testsel given 1", b"parser lib x86_64")
}

const DATA: &str = "/ws/data";

fn world(data: u8) -> World {
    World { files: [(PathBuf::from(DATA), [data; 32])].into(), env: vec![("FOO".into(), "1".into())] }
}

fn read(world: &World) -> Traced {
    Traced {
        files: vec![(PathBuf::from(DATA), world.files.get(Path::new(DATA)).copied())],
        env: vec![("FOO".into(), world.env.iter().find(|it| it.0 == "FOO").map(|it| it.1.clone()))],
        ..Traced::default()
    }
}

struct Case {
    store: Mem,
    attrs: Attrs,
    reached: Vec<u32>,
    outside: bool,
}

impl Case {
    fn new() -> Case {
        Case { store: Mem::default(), attrs: Attrs::default(), reached: vec![0, 1, 2, 3], outside: false }
    }

    fn subject(&self) -> Subject<'_> {
        Subject { name: "parse::nested", attrs: &self.attrs, outside: self.outside, reached: &self.reached }
    }

    fn choose(&self, image: &Image, world: &World) -> Choice {
        choose(&self.store, given(), image, &self.subject(), false, &mut world.clone()).unwrap()
    }

    fn keep(&mut self, image: &Image, ran: &Ran<'_>) -> Result<Kept, RecordError> {
        let subject = Subject { name: "parse::nested", attrs: &self.attrs, outside: self.outside, reached: &self.reached };
        keep(&mut self.store, given(), image, &subject, ran)
    }

    fn ran(&mut self, image: &Image, world: &World, passed: bool, record: &Record) -> Kept {
        let traced = read(world);
        self.keep(image, &Ran { passed, took: Duration::from_millis(40), traced: Some(&traced), record }).unwrap()
    }
}

fn executed(hits: &[u32]) -> Record {
    Record::of_hits(4, hits.to_vec())
}

fn cached(choice: &Choice) -> Option<&Basis> {
    match choice {
        Choice::Cached(it) => Some(&it.basis),
        Choice::Run { .. } => None,
    }
}

fn why(choice: Choice) -> Why {
    match choice {
        Choice::Run { why, .. } => why,
        Choice::Cached(it) => panic!("cached, {it:?}"),
    }
}

#[test]
fn a_pass_stands_until_a_function_it_executed_changes() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    assert_eq!(why(case.choose(&at, &world(1))), Why::New);
    let kept = case.ran(&at, &world(1), true, &executed(&[0, 2]));
    assert!(kept.pass);
    assert_eq!(kept.basis, Basis::Executed);
    let choice = case.choose(&at, &world(1));
    assert_eq!(choice, Choice::Cached(Cached { took: Duration::from_millis(40), basis: Basis::Executed }));

    // Function 1 is reached and not executed: the reached set would run the test again.
    let edited = image(&[0, 1, 0, 0, 0], 4, 0);
    assert_eq!(cached(&case.choose(&edited, &world(1))), Some(&Basis::Executed));
    let edited = image(&[0, 0, 1, 0, 0], 4, 0);
    assert_eq!(why(case.choose(&edited, &world(1))), Why::Changed(id(2)));
}

#[test]
fn code_that_comes_back_finds_its_pass() {
    let mut case = Case::new();
    let first = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&first, &world(1), true, &executed(&[0, 2]));
    let second = image(&[0, 0, 1, 0, 0], 4, 0);
    case.ran(&second, &world(1), false, &executed(&[0, 2]));
    assert_eq!(why(case.choose(&second, &world(1))), Why::Failed);
    assert_eq!(cached(&case.choose(&first, &world(1))), Some(&Basis::Executed));
}

#[test]
fn a_failure_runs_again_with_the_inputs_it_failed_with() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    case.ran(&at, &world(2), false, &executed(&[0, 1]));
    assert_eq!(why(case.choose(&at, &world(2))), Why::Failed);
    assert_eq!(cached(&case.choose(&at, &world(1))), Some(&Basis::Executed));
    assert_eq!(why(case.choose(&at, &world(3))), Why::Read(Input::File(PathBuf::from(DATA))));
}

#[test]
fn a_failure_whose_reads_are_not_known_runs_again() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    let record = executed(&[0]);
    let ran = Ran { passed: false, took: Duration::from_millis(3), traced: None, record: &record };
    case.keep(&at, &ran).unwrap();
    assert_eq!(why(case.choose(&at, &world(1))), Why::Failed);
}

#[test]
fn an_incomplete_record_keeps_the_pass_under_the_reached_set_and_says_why() {
    let mut case = Case::new();
    case.reached = vec![0, 1, 4];
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    let killed = Record::incomplete("killed after 60s");
    let kept = case.ran(&at, &world(1), true, &killed);
    let basis = Basis::Reached { why: "killed after 60s".to_owned() };
    assert_eq!(kept.basis, basis);
    assert_eq!(cached(&case.choose(&at, &world(1))), Some(&basis));
    // Reached, so the edit runs it, though a whole record might have shown it never ran.
    assert_eq!(why(case.choose(&image(&[0, 1, 0, 0, 0], 4, 0), &world(1))), Why::Changed(id(1)));
    // Not reached and not called through a slot: in the unlogged code.
    assert_eq!(why(case.choose(&image(&[0, 0, 0, 0, 1], 4, 0), &world(1))), Why::Unlogged);
    // Neither reached nor executed.
    assert!(cached(&case.choose(&image(&[0, 0, 1, 0, 0], 4, 0), &world(1))).is_some());
}

#[test]
fn code_called_not_through_a_slot_reruns_every_test() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    assert_eq!(why(case.choose(&image(&[0, 0, 0, 0, 1], 4, 0), &world(1))), Why::Unlogged);
    assert_eq!(why(case.choose(&image(&[0, 0, 0, 0, 0], 4, 1), &world(1))), Why::Unlogged);
}

#[test]
fn a_function_gone_from_the_binary_runs_it() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[3]));
    let fns = [0u32, 1, 2, 4].iter().map(|&n| Version { id: id(n), bound: bound(n, 0) }).collect();
    let fewer = Image::new(fns, vec![0, 1, 2], ContentId::of("blaze v2 testsel beside 1", &0u32.to_le_bytes())).unwrap();
    assert_eq!(why(case.choose(&fewer, &world(1))), Why::Gone(id(3)));
}

#[test]
fn what_it_read_is_part_of_its_key() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    assert_eq!(why(case.choose(&at, &world(2))), Why::Read(Input::File(PathBuf::from(DATA))));
    let mut other = world(1);
    other.env[0].1 = "2".into();
    assert_eq!(why(case.choose(&at, &other)), Why::Read(Input::Env("FOO".into())));
    case.ran(&at, &world(2), true, &executed(&[0]));
    assert!(cached(&case.choose(&at, &world(2))).is_some());
    assert!(cached(&case.choose(&at, &world(1))).is_some());
}

#[test]
fn a_pass_that_cant_be_kept_is_not() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    let record = executed(&[0]);
    let ran = Ran { passed: true, took: Duration::from_millis(3), traced: None, record: &record };
    assert!(!case.keep(&at, &ran).unwrap().pass);
    assert_eq!(why(case.choose(&at, &world(1))), Why::NoPass);

    let network = Traced { untraceable: vec![Untraceable::Network], ..read(&world(1)) };
    let ran = Ran { passed: true, took: Duration::from_millis(3), traced: Some(&network), record: &record };
    assert!(!case.keep(&at, &ran).unwrap().pass);
    assert_eq!(why(case.choose(&at, &world(1))), Why::NoPass);
}

#[test]
fn outside_and_all_always_run() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    let all = choose(&case.store, given(), &at, &case.subject(), true, &mut world(1)).unwrap();
    assert_eq!(all, Choice::Run { why: Why::All, expected: Some(Duration::from_millis(40)) });
    case.outside = true;
    assert_eq!(why(case.choose(&at, &world(1))), Why::Outside);
    assert!(!case.ran(&at, &world(1), true, &executed(&[0])).pass);
    assert_eq!(why(case.choose(&at, &world(1))), Why::Outside);
}

#[test]
fn other_attributes_run_it() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&at, &world(1), true, &executed(&[0]));
    case.attrs.should_panic = ShouldPanic::WithMessage("overflow".to_owned());
    assert_eq!(why(case.choose(&at, &world(1))), Why::Attributes);
    case.attrs.should_panic = ShouldPanic::No;
    assert!(cached(&case.choose(&at, &world(1))).is_some());
}

#[test]
fn a_pass_older_than_the_lookback_is_not_found() {
    let mut case = Case::new();
    let first = image(&[0, 0, 0, 0, 0], 4, 0);
    case.ran(&first, &world(1), true, &executed(&[0]));
    for v in 1..LOOKBACK {
        case.ran(&image(&[v, 0, 0, 0, 0], 4, 0), &world(1), true, &executed(&[0]));
    }
    assert!(cached(&case.choose(&first, &world(1))).is_some());
    case.ran(&image(&[LOOKBACK, 0, 0, 0, 0], 4, 0), &world(1), true, &executed(&[0]));
    assert_eq!(why(case.choose(&first, &world(1))), Why::Changed(id(0)));
}

#[test]
fn the_end_of_a_history_is_found_at_every_length() {
    let mut store = Mem::default();
    let entry = Entry {
        passed: true,
        kept: false,
        attrs: Attrs::default().id(),
        set: Key(ContentId::default()),
        took: Duration::from_micros(5),
        basis: Basis::Executed,
        traced: None,
    }
    .encode();
    assert_eq!(end(&store, given(), "t").unwrap(), None);
    for n in 0..70u32 {
        store.put(history_slot(given(), "t", n), entry.clone()).unwrap();
        assert_eq!(end(&store, given(), "t").unwrap(), Some(n));
    }
}

#[test]
fn an_entry_round_trips_and_a_short_one_is_refused() {
    let entry = Entry {
        passed: false,
        kept: false,
        attrs: Attrs::default().id(),
        set: Key(ContentId::of("blaze v2 testsel set test 1", b"s")),
        took: Duration::from_micros(123_456),
        basis: Basis::Reached { why: "the record failed: short".to_owned() },
        traced: Some(read(&world(4))),
    };
    let bytes = entry.encode();
    assert_eq!(Entry::decode(&bytes).unwrap(), entry);
    for cut in [0, HISTORY.len(), bytes.len() - 1] {
        assert!(Entry::decode(&bytes[..cut]).is_err(), "{cut}");
    }
}

#[test]
fn a_record_of_another_binary_is_an_error() {
    let mut case = Case::new();
    let at = image(&[0, 0, 0, 0, 0], 4, 0);
    let traced = read(&world(1));
    let record = Record::of_hits(5, vec![0]);
    let ran = Ran { passed: true, took: Duration::ZERO, traced: Some(&traced), record: &record };
    assert!(matches!(
        case.keep(&at, &ran),
        Err(RecordError::Table(TableError::OtherBinary { record: 5, table: 4 }))
    ));
}
