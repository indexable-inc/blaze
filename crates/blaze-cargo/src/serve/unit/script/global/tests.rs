use std::path::PathBuf;

use blaze_cas::{Kind, Ticket};
use blaze_global::Root;
use blaze_ids::ContentId;
use blaze_usercode::RunKind;
use blaze_hash::HashMap;

use super::*;

const TOOLCHAIN: ContentId = ContentId([1; 32]);
const BUILD_RS: &str = "/reg/serde-1.0.219/build.rs";

fn serde() -> Origin {
    Origin::CratesIo { name: "serde".into(), version: "1.0.219".into(), checksum: [3; 32] }
}

fn rule() -> Rule {
    let roots = vec![
        Root { path: "/reg/serde-1.0.219".into(), origin: serde() },
        Root { path: "/work".into(), origin: Origin::Workspace },
        Root { path: "/work/target/debug/build".into(), origin: Origin::Outputs },
    ];
    Rule::new(TOOLCHAIN, ContentId([11; 32]), Vec::new(), roots, Vec::new())
}

fn given(manifest_dir: &str) -> Given {
    let mut given = Given::new(RunKind::BuildScript);
    given.env("CARGO_MANIFEST_DIR", manifest_dir).env("OUT_DIR", "/work/target/debug/build/serde-0123/out");
    given
}

fn run_of(given: &Given) -> RunKey {
    RunKey { code: ContentId::of("blaze v2 test script 1", b"serde's build script"), given: given.finish() }
}

/// The files as they are now; nothing else is there.
struct Now(Vec<(PathBuf, [u8; 32])>);

impl Inputs for Now {
    type Error = String;

    fn file(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
        Ok(self.0.iter().find(|it| it.0 == path).map(|it| it.1))
    }

    fn dir(&mut self, _: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    fn env(&mut self, _: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
        None
    }

    fn env_all(&mut self) -> &[(std::ffi::OsString, std::ffi::OsString)] {
        &[]
    }

    fn names(&mut self, _: &std::path::Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> {
        Ok(false)
    }

    fn entry(&mut self, _: &std::path::Path) -> Result<blaze_usercode::trace::Entry, String> {
        Ok(blaze_usercode::trace::Entry::Missing)
    }
}

fn now() -> Now {
    Now(vec![(PathBuf::from(BUILD_RS), [5; 32])])
}

/// A shape that read `path`.
fn shape(path: &str) -> Vec<u8> {
    let traced = Traced { files: vec![(PathBuf::from(path), Some([9; 32]))], ..Traced::default() };
    Shape::of(&traced).encode()
}

fn step(fetch: &mut Fetch, made: &Made<'_>, store: &HashMap<Key, Vec<u8>>) -> Result<Step, String> {
    fetch.step(&rule(), made, |key| Ok(store.get(key).map(Vec::as_slice)), &mut now())
}

fn fetched(step: Result<Step, String>) -> Public {
    match step {
        Ok(Step::Fetch(public)) => public,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_run_the_store_has_nothing_of_asks_for_its_first_shape_once() {
    let given = given("/reg/serde-1.0.219");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let store = HashMap::default();
    let mut fetch = Fetch::default();
    let public = fetched(step(&mut fetch, &made, &store));
    assert_eq!(public.key(), index::slot(&run, 0));
    assert_eq!(public.kind(), Kind::Run);
    assert_eq!(fetch.waiting(), Some(public.key()));
    // Asked again before its answer is in: an error, not a second fetch.
    assert!(step(&mut fetch, &made, &store).is_err());
    // Another run's answer is not this one's.
    assert_eq!(fetch.fetched(&index::slot(&run, 1), &Fetched::Missing), Ok(false));
    assert_eq!(fetch.fetched(&public.key(), &Fetched::Missing), Ok(true));
    assert_eq!(step(&mut fetch, &made, &store), Ok(Step::Run));
}

#[test]
fn a_shape_kept_here_asks_for_its_result_for_the_inputs_as_they_are_now() {
    let given = given("/reg/serde-1.0.219");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let mut store = HashMap::default();
    store.insert(index::slot(&run, 0), shape(BUILD_RS));
    let mut fetch = Fetch::default();
    let public = fetched(step(&mut fetch, &made, &store));
    let values = Shape::decode(&store[&index::slot(&run, 0)]).unwrap().values(&mut now()).unwrap();
    assert_eq!(public.key(), index::result_key(&run, &values));
    // The cache had it: the store keeps it, and the run begins and finds it.
    store.insert(public.key(), b"the run's value".to_vec());
    assert_eq!(fetch.fetched(&public.key(), &Fetched::Kept(Ticket(0))), Ok(true));
    assert_eq!(step(&mut fetch, &made, &store), Ok(Step::Run));
}

#[test]
fn a_result_missing_from_the_cache_moves_on_to_the_next_shape() {
    let given = given("/reg/serde-1.0.219");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let mut store = HashMap::default();
    store.insert(index::slot(&run, 0), shape(BUILD_RS));
    let mut fetch = Fetch::default();
    let result = fetched(step(&mut fetch, &made, &store));
    assert_eq!(fetch.fetched(&result.key(), &Fetched::Missing), Ok(true));
    let next = fetched(step(&mut fetch, &made, &store));
    assert_eq!(next.key(), index::slot(&run, 1));
    assert_eq!(fetch.fetched(&next.key(), &Fetched::Missing), Ok(true));
    assert_eq!(step(&mut fetch, &made, &store), Ok(Step::Run));
}

#[test]
fn a_shape_that_read_what_is_not_public_is_never_asked_for_its_result() {
    let given = given("/reg/serde-1.0.219");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let mut store = HashMap::default();
    store.insert(index::slot(&run, 0), shape("/home/me/.ssh/config"));
    let mut fetch = Fetch::default();
    assert_eq!(fetched(step(&mut fetch, &made, &store)).key(), index::slot(&run, 1));
}

#[test]
fn a_run_given_a_private_path_asks_nothing() {
    let given = given("/home/me/serde");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let mut fetch = Fetch::default();
    assert_eq!(step(&mut fetch, &made, &HashMap::default()), Ok(Step::Run));
    assert_eq!(fetch.waiting(), None);
}

#[test]
fn a_refused_value_is_an_error_naming_its_key() {
    let given = given("/reg/serde-1.0.219");
    let run = run_of(&given);
    let origin = serde();
    let made = Made { run: &run, given: &given, origin: &origin };
    let mut fetch = Fetch::default();
    let public = fetched(step(&mut fetch, &made, &HashMap::default()));
    let error = fetch.fetched(&public.key(), &Fetched::Refused("a bad signature".into())).unwrap_err();
    assert!(error.contains(&format!("{:x}", public.key().0)), "{error}");
    assert!(error.contains("a bad signature"), "{error}");
}

#[test]
fn only_a_crates_io_package_with_a_checksum_has_a_public_origin() {
    let package = |is_member: bool, source: Option<&str>| PackagePlan {
        id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219".into(),
        name: "serde".into(),
        version: "1.0.219".into(),
        manifest_path: "/reg/serde-1.0.219/Cargo.toml".into(),
        source: source.map(str::to_owned),
        is_member,
        default_run: None,
        checksum: None,
        tests: blaze_units::query::TestSandbox::default(),
        const_macros: None,
    };
    assert_eq!(origin(&package(false, None), Some([3; 32])), Some(serde()));
    assert_eq!(origin(&package(false, None), None), None);
    assert_eq!(origin(&package(true, None), Some([3; 32])), None);
    assert_eq!(origin(&package(false, Some("(/work/serde)")), Some([3; 32])), None);
    assert_eq!(origin(&package(false, Some("(registry `mine`)")), Some([3; 32])), None);
}
