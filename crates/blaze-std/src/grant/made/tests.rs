//! A parameter is made once (made.rs), in the simulated world:
//!
//! - [`a_folder_grant_is_opened_once`]: a `Dir` grant whose folder becomes a file right after
//!   its first open (the world changing between a check and a make, as an injected fault or a
//!   folder removed on a machine) starts `main` with the folder it opened, and it was opened
//!   ONCE. The control is the planted bug, the old launcher's second making put back: the
//!   count is 2 and the second open fails, which is the failure the old `Filled::make` met
//!   after its check had passed.
//! - [`a_refused_start_unmakes_what_it_made_in_order`]: a start refused by its third
//!   parameter drops the two made before it, in the order they were made, and the fourth,
//!   made only as its check, at once: three made, three dropped, `main` never run.

use core::sync::atomic::{AtomicU32, Ordering};

use blaze_core::FileName;

use super::super::{Given, Grant, Granting, Kind, LaunchAlloc, Manifest, Param, Refused, Want};
use crate::{
    cx::Cx,
    entry::{Status, launch_in},
    fs::{Dir, ReadOnly},
    io::Io,
    spec::{RunSpec, delivery, grant_arg},
};

/// The test entry's heap, as a test hands it (D102).
fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

/// What file descriptor 3 holds for a run spec granting `grants` (`name=value` each) to the
/// program of `manifest`.
fn spec_granting(manifest: &Manifest, grants: &[&str], alloc: LaunchAlloc) -> blaze_own::Vec<u8, LaunchAlloc> {
    let mut spec = RunSpec::defaults(
        blaze_own::Vec::new_in(alloc),
        blaze_own::Vec::new_in(alloc),
        blaze_own::String::from_str_in("/work", alloc),
        alloc,
    );
    for grant in grants {
        spec.put_grant(grant_arg(manifest, grant, alloc).expect("the test's grant reads as its parameter's kind"));
    }
    delivery(&spec, alloc)
}

/// How many times [`Watched`] was made: only [`a_folder_grant_is_opened_once`] makes one.
static OPENED: AtomicU32 = AtomicU32::new(0);

/// A read-only folder that counts its openings and, once opened, puts a FILE where its folder
/// was: any later open of it fails (`open_dir` refuses a file).
struct Watched(#[allow(dead_code)] Dir<ReadOnly>);

impl Grant for Watched {
    const KIND: Kind = Kind::Dir { write: false };

    fn grant(from: &Granting<'_>) -> Result<Watched, Want> {
        // Counted before the open: a second open that fails (the control below) is an opening too.
        OPENED.fetch_add(1, Ordering::SeqCst);
        let dir = <Dir<ReadOnly> as Grant>::grant(from)?;
        let replaced = from.process().with_sim(|sim| match FileName::new("data") {
            Some(name) => {
                sim.change(&name, b"a file where the folder was");
                true
            }
            None => false,
        });
        assert_eq!(replaced, Some(true), "the test's world is the simulation, and `data` is a name in it");
        Ok(Watched(dir))
    }
}

static WATCHED: Manifest = Manifest::new(&[Param::new("data", Kind::Dir { write: false })]);

fn reach(_cx: &Cx, data: Watched) -> Status {
    drop(data);
    Status::OK
}

/// Expected: `test grant::made::tests::a_folder_grant_is_opened_once ... ok`. Without the
/// fix (the launcher's check making and dropping, `Filled::make` making again) the first
/// assertion fails: two openings, and the start fails at the second.
#[test]
fn a_folder_grant_is_opened_once() {
    let alloc = alloc();
    let fd3 = spec_granting(&WATCHED, &["data=data"], alloc);

    OPENED.store(0, Ordering::SeqCst);
    let mut said = std::string::String::new();
    let status = launch_in(&WATCHED, Some(fd3.as_slice()), Io::sim(1), &mut said, crate::launch_body!(reach; data: Watched));
    assert_eq!((status, said.as_str(), OPENED.load(Ordering::SeqCst)), (Status::OK, "", 1), "one opening, and `main` ran");

    // Control, the planted bug: the old launcher's second making put back after the check.
    // The count says 2, and the second open meets the changed world and fails, after the
    // check passed: what made the old `make` unreachable, then an unwind.
    OPENED.store(0, Ordering::SeqCst);
    let mut second: Option<bool> = None;
    let twice = |given: &mut Given<'_>| -> Result<Status, Refused> {
        let filled = given.checking().check::<Watched>("data").filled()?;
        let cx = filled.cx();
        let ((), data) = filled.hand_out();
        let again = <Watched as Grant>::grant(&Granting { cx, name: "data", member: None, given: &*given });
        second = Some(matches!(again, Err(Want::Failed(_))));
        drop((data, again));
        Ok(Status::OK)
    };
    let mut said = std::string::String::new();
    let status = launch_in(&WATCHED, Some(fd3.as_slice()), Io::sim(1), &mut said, twice);
    assert_eq!((status, OPENED.load(Ordering::SeqCst), second), (Status::OK, 2, Some(true)), "{said}");
}

/// How many [`Noted`] were made, and the order they were dropped in (each drop appends its
/// digit): only [`a_refused_start_unmakes_what_it_made_in_order`] makes one.
static NOTED_MADE: AtomicU32 = AtomicU32::new(0);
static NOTED_DROPPED: AtomicU32 = AtomicU32::new(0);

/// A parameter that fills itself and notes its making and its drop: `a` is 1, `b` 2, any
/// other name 4.
struct Noted(u32);

impl Grant for Noted {
    const KIND: Kind = Kind::Clock;

    fn grant(from: &Granting<'_>) -> Result<Noted, Want> {
        NOTED_MADE.fetch_add(1, Ordering::SeqCst);
        Ok(Noted(match from.name {
            "a" => 1,
            "b" => 2,
            _ => 4,
        }))
    }
}

impl Drop for Noted {
    fn drop(&mut self) {
        let at = self.0;
        let _ = NOTED_DROPPED.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |seen| Some(seen * 10 + at));
    }
}

static FOUR: Manifest = Manifest::new(&[
    Param::new("a", Kind::Clock),
    Param::new("b", Kind::Clock),
    Param::new("c", Kind::Dir { write: false }),
    Param::new("d", Kind::Clock),
]);

fn never(_cx: &Cx, _a: Noted, _b: Noted, _c: Dir<ReadOnly>, _d: Noted) -> Status {
    Status::FAILED
}

/// Expected: `test grant::made::tests::a_refused_start_unmakes_what_it_made_in_order ... ok`.
#[test]
fn a_refused_start_unmakes_what_it_made_in_order() {
    let alloc = alloc();
    let fd3 = spec_granting(&FOUR, &[], alloc);
    NOTED_MADE.store(0, Ordering::SeqCst);
    NOTED_DROPPED.store(0, Ordering::SeqCst);
    let mut said = std::string::String::new();
    let status = launch_in(
        &FOUR,
        Some(fd3.as_slice()),
        Io::sim(1),
        &mut said,
        crate::launch_body!(never; a: Noted, b: Noted, c: Dir<ReadOnly>, d: Noted),
    );
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("- c (Dir<ReadOnly>): no grant"), "the refusal names `c`: {said}");
    // `a` and `b` dropped when `c` refused the start, in the order made; `d` made as its check
    // and dropped at once. Three made, three dropped: nothing leaked.
    assert_eq!((NOTED_MADE.load(Ordering::SeqCst), NOTED_DROPPED.load(Ordering::SeqCst)), (3, 124));
}
