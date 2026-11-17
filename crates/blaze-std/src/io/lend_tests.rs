//! The lent driver on a simulated world: each property with its oracle and the planted bug it
//! catches (rule 43).

use std::string::{String, ToString};
use std::time::Duration;
use std::vec::Vec;
use std::{format, vec};

use blaze_core::wait::Deadline;
use blaze_core::{Completion, Done, Driver, Effect, FileName, LockMode, Tag};

use super::{LendPlant, LentDriver};
use crate::io::Io;
use crate::path::Path;

fn name(text: &str) -> FileName {
    FileName::new(text).expect("a name")
}

fn lent(io: &Io, plant: LendPlant) -> LentDriver {
    let root = io.need_fs().expect("files").root();
    root.lend(&Path::new("store").expect("a path")).planted(plant)
}

/// Polls `driver` until it has handed back `want` completions or ten polls passed.
fn collect(driver: &mut LentDriver, io: &Io, want: usize) -> Vec<Completion> {
    let mut out = Vec::new();
    for _ in 0..10 {
        if out.len() >= want {
            break;
        }
        let until = Deadline::after(io.now(), Duration::from_secs(1));
        driver.poll(&mut out, until);
    }
    out
}

/// The oracle: a completion of ANOTHER waiter's tag that the lent driver's poll brings stays
/// kept for that waiter; the lent driver hands back only its own tags, rewritten.
fn others_stay_kept(plant: LendPlant) -> Result<(), String> {
    let io = Io::sim(3);
    let mut driver = lent(&io, plant);
    // Another waiter's effect, asked of the world first so the same polls bring it.
    let other = io.submit(|tag| Effect::Stat { file: name("elsewhere"), tag });
    let mut effects = vec![
        Effect::WriteUnsynced { file: name("store/a"), bytes: b"a record".to_vec(), tag: Tag(7) },
        Effect::Stat { file: name("store"), tag: Tag(8) },
    ];
    driver.submit(&mut effects);
    let got = collect(&mut driver, &io, 2);
    let mut tags: Vec<u64> = got.iter().map(|it| it.tag.0).collect();
    tags.sort_unstable();
    if tags != [7, 8] {
        return Err(format!("the lent driver handed back tags {tags:?}, not its own 7 and 8"));
    }
    if driver.in_flight() != 0 {
        return Err("the lent driver still counts effects in flight".to_string());
    }
    match io.settle(other, Deadline::after(io.now(), Duration::from_secs(1)), "the other waiter") {
        Ok(Done::Stat(None)) => Ok(()),
        Ok(other) => Err(format!("the other waiter got {other:?}")),
        Err(_) => Err("the other waiter's completion was taken by the lent driver".to_string()),
    }
}

/// Expected: `test io::lend::tests::another_waiters_completion_stays_with_it ... ok`.
#[test]
fn another_waiters_completion_stays_with_it() {
    others_stay_kept(LendPlant::None).expect("kept");
    let why = others_stay_kept(LendPlant::ReturnsOthers).expect_err("the planted bug is caught");
    assert!(why.contains("handed back tags") || why.contains("taken by the lent driver"), "{why}");
}

/// The oracle: a file outside the folder is refused, naming it, and nothing is done to it.
fn fenced(plant: LendPlant) -> Result<(), String> {
    let io = Io::sim(4);
    let mut driver = lent(&io, plant);
    let mut effects = vec![Effect::WriteUnsynced { file: name("elsewhere/x"), bytes: b"no".to_vec(), tag: Tag(1) }];
    driver.submit(&mut effects);
    let got = collect(&mut driver, &io, 1);
    match got.first().map(|it| &it.done) {
        Some(Done::Failed(why)) if why.0.contains("elsewhere/x is outside store") => Ok(()),
        Some(other) => Err(format!("a write outside the folder completed as {other:?}")),
        None => Err("no completion".to_string()),
    }
}

/// Expected: `test io::lend::tests::a_file_outside_the_folder_is_refused_by_name ... ok`.
#[test]
fn a_file_outside_the_folder_is_refused_by_name() {
    fenced(LendPlant::None).expect("refused");
    let why = fenced(LendPlant::NoFence).expect_err("the planted bug is caught");
    assert!(why.contains("completed as"), "{why}");
}

/// A lock taken and let go through the lent driver: the unlock names the asker's lock tag and
/// the driver rewrites it to the world's (the plain store's flip lock does exactly this).
/// Expected: `test io::lend::tests::an_unlock_names_the_lock_by_the_askers_tag ... ok`.
#[test]
fn an_unlock_names_the_lock_by_the_askers_tag() {
    let io = Io::sim(5);
    let mut driver = lent(&io, LendPlant::None);
    let mut effects = vec![Effect::Lock { file: name("store/flip"), mode: LockMode::Exclusive, tag: Tag(1) }];
    driver.submit(&mut effects);
    let got = collect(&mut driver, &io, 1);
    assert!(matches!(got.first().map(|it| (&it.done, it.tag)), Some((Done::Locked, Tag(1)))), "locked: {got:?}");
    let mut effects = vec![Effect::Unlock { lock: Tag(1), tag: Tag(2) }];
    driver.submit(&mut effects);
    let got = collect(&mut driver, &io, 1);
    assert!(matches!(got.first().map(|it| (&it.done, it.tag)), Some((Done::Unlocked, Tag(2)))), "unlocked: {got:?}");
    // An unlock of a lock never taken here is refused by name.
    let mut effects = vec![Effect::Unlock { lock: Tag(9), tag: Tag(3) }];
    driver.submit(&mut effects);
    let got = collect(&mut driver, &io, 1);
    assert!(matches!(got.first().map(|it| &it.done), Some(Done::Failed(_))), "refused: {got:?}");
}
