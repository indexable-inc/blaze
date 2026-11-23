//! journal_ask.rs's sans-IO parts: the differential between the helper's names and the
//! replay's ([`differ`]), the checkout's key, and which stored helper place is asked from.
//! The ask's wire and its session are blaze-watch helper.rs's, simulated there (sim/helper.rs).
//!
//! THE ORACLE for [`differ`], independent of its sort and search: a name of one side, not the
//! checkout itself and not blaze's own traffic, is that side's alone exactly when no such name
//! of the other side is it or a folder above it (a quadratic scan with `since::covers`).
//! Planted, caught: a name covered only by itself ([`DifferPlant::NamesOnly`]).

use std::collections::BTreeSet;
use std::format;
use std::vec::Vec as StdVec;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::*;

/// The names either side may tell: folders and the files under them, a name that is a prefix of
/// another but no folder of it (`ab` beside `a`), the checkout itself, and blaze's own traffic.
const NAMES: [&str; 11] = ["", "a", "a/b", "a/b/c.rs", "a/d.rs", "ab", "ab/c.rs", "z.rs", ".blaze/v2/fsevents/cookie-1", ".blaze/v2/journal/helper.sock", ".blaze"];

fn test_frame() -> Frame {
    Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame")
}

/// The names one side tells: each of [`NAMES`] or not, some twice, in the tape's order.
fn side(tape: &Tape, label: &'static str) -> StdVec<&'static str> {
    let mut told = StdVec::new();
    for _ in 0..tape.below(8, label) {
        told.push(NAMES[tape.index(NAMES.len(), "a name told")]);
    }
    told
}

/// The oracle's answer: `ours`' compared names that no compared name of `theirs` covers.
fn alone(ours: &[&'static str], theirs: &[&'static str]) -> BTreeSet<&'static str> {
    // The socket is under the home since 2026-11-21: an old checkout's `.blaze/v2/journal` is
    // a name like any other.
    let own = |name: &str| name.is_empty() || name.starts_with(".blaze/v2/fsevents");
    ours.iter().copied().filter(|name| !own(name)).filter(|name| !theirs.iter().any(|other| !own(other) && since::covers(other, name))).collect()
}

fn differential(tape: &Tape, plant: DifferPlant) -> Result<(), Broke> {
    let frame = test_frame();
    let helper = side(tape, "the helper's names");
    let replay = side(tape, "the replay's names");
    let differs = differ_planted(&helper, replay.iter().copied(), &frame, plant);
    let got = (differs.only_helper.iter().copied().collect::<BTreeSet<_>>(), differs.only_replay.iter().copied().collect::<BTreeSet<_>>());
    let want = (alone(&helper, &replay), alone(&replay, &helper));
    if got != want {
        return Err(Broke::Oracle(format!("the differential misnamed a side's own names: got {got:?}, want {want:?} (helper {helper:?}, replay {replay:?}, {plant:?})")));
    }
    let sorted = |names: &[&str]| names.windows(2).all(|pair| pair[0] < pair[1]);
    if !sorted(&differs.only_helper) || !sorted(&differs.only_replay) {
        return Err(Broke::Oracle(format!("the differential misnamed a side's own names: a side's names are not sorted and unique ({plant:?})")));
    }
    Ok(())
}

#[test]
fn the_differential_names_what_only_one_side_told() {
    check(Config::cases(512), |tape| differential(tape, DifferPlant::None));
}

#[test]
fn control_a_name_covered_only_by_itself() {
    control(Config::cases(512), DifferPlant::None, DifferPlant::NamesOnly, "misnamed a side's own names", differential);
}

/// One checkout, one key; two checkouts, two; the key is a digest of the root's text, so a
/// link to another checkout (resolved by `realpath` on both sides) cannot share one.
#[test]
fn a_checkout_key_is_its_root() {
    let frame = test_frame();
    let tempo = checkout_key(Some("/checkout/project/tempo"), &frame);
    assert_eq!(tempo, checkout_key(Some("/checkout/project/tempo"), &frame), "one root, one key");
    assert_ne!(tempo, checkout_key(Some("/checkout/project/reth"), &frame), "two roots, two keys");
    assert_ne!(tempo, checkout_key(None, &frame), "a simulated world's key is no checkout's");
}

/// The socket's fit at its boundary: a home whose `<home>/.blaze/v2/journal/<16 hex>.sock` and
/// NUL are exactly the platform's most fits, one byte more does not, and the refusal names the
/// socket's folder, both counts and the limit. No home has no path to fit; the checkout's depth
/// counts for nothing.
#[test]
fn a_socket_path_one_byte_over_the_limit_is_too_long() {
    let room = bstd::local::SOCKET_PATH_MOST - PAST_HOME;
    let fits = format!("/{:r<width$}", "", width = room - 1);
    assert_eq!(fits.len() + 1 + crate::store::HOME_FOLDER.len() + 1 + helper::SOCKET_PATH_BYTES + 1, bstd::local::SOCKET_PATH_MOST);
    assert_eq!(SocketFits::of(Some(&fits)), SocketFits::Fits, "exactly the most fits");
    assert_eq!(SocketFits::of(Some(&format!("{fits}/"))), SocketFits::Fits, "a home's trailing slash is not counted");
    let over = format!("{fits}r");
    let too_long = SocketFits::of(Some(&over));
    assert_eq!(too_long, SocketFits::TooLong { bytes: bstd::local::SOCKET_PATH_MOST + 1, home: over.len() });
    let said = format!("{too_long}");
    assert!(said.contains(helper::SOCKET_FOLDER), "{said:?} does not name the socket's folder");
    for count in [bstd::local::SOCKET_PATH_MOST + 1, bstd::local::SOCKET_PATH_MOST, over.len()] {
        assert!(said.contains(&format!("{count}")), "{said:?} does not say {count}");
    }
    assert_eq!(SocketFits::of(None), SocketFits::Fits, "no home");
    assert_eq!(SocketFits::of(Some("/homes/someone")), SocketFits::Fits, "a home on macOS");
}

/// A checkout's socket is named by its key alone: one checkout, one name, under the folder;
/// two checkouts, two; no part of the checkout's path is in it, and a deep checkout's name is
/// as short as any.
#[test]
fn a_socket_is_named_by_its_checkouts_key() {
    let frame = test_frame();
    let build = bstd::host::BuildId::of(b"a build").expect("a build id");
    let deep = format!("/checkout/project/{:d<200}", "");
    let tempo = Me::of(&build, Some("/checkout/project/tempo"), Some("/homes/someone"), &frame);
    let reth = Me::of(&build, Some("/checkout/project/reth"), Some("/homes/someone"), &frame);
    let far = Me::of(&build, Some(&deep), Some("/homes/someone"), &frame);
    let text = |me: &Me| std::string::String::from(core::str::from_utf8(&me.socket_path).expect("UTF-8"));
    assert_eq!(text(&tempo), text(&Me::of(&build, Some("/checkout/project/tempo"), Some("/homes/other"), &frame)), "one checkout, one name");
    assert_ne!(text(&tempo), text(&reth), "two checkouts, two names");
    for me in [&tempo, &reth, &far] {
        let name = text(me);
        let hex = name.strip_prefix(&format!("{}/", helper::SOCKET_FOLDER)).and_then(|it| it.strip_suffix(".sock")).expect("in the folder, a .sock");
        assert!(hex.len() == 16 && hex.bytes().all(|it| it.is_ascii_hexdigit() && !it.is_ascii_uppercase()), "{name:?}: 16 lower hex digits");
        assert!(!name.contains("Projects") && me.socket == SocketFits::Fits, "{name:?}: no path in the name, and it fits");
        assert_eq!(me.socket_beneath_home().expect("a path").as_str(), name);
    }
}

/// A helper place is asked from only beside the checkout's own journal place (its pair); a
/// stored answer without one asks from no place (refused `NoPlace`, issuing one).
#[test]
fn the_helper_place_is_asked_from_only_with_its_pair() {
    let frame = test_frame();
    let place = Place { instance: 0x5eed, seq: 3 };
    let cursor = Cursor::Sim { journal: 1, next: 9 };
    let encoded = |units: &[(u8, &'static str, Cursor)], helper: Option<Place>| {
        let mut bytes = Vec::new_in(&frame);
        Stored::encode_units(units.iter().copied(), Some(1), &mut bytes);
        if let Some(helper) = helper {
            Stored::encode_helper(helper.encode(), &mut bytes);
        }
        StdVec::from(&bytes[..])
    };
    let checkout = crate::journal_sweep::Where::Checkout as u8;
    let home = crate::journal_sweep::Where::Home as u8;
    let paired = encoded(&[(checkout, "", cursor), (home, "", cursor)], Some(place));
    assert_eq!(paired_place(Stored::of(&paired).expect("whole")), Some(place), "paired: asked from");
    let unpaired = encoded(&[(checkout, "crates", cursor), (home, "", cursor)], Some(place));
    assert_eq!(paired_place(Stored::of(&unpaired).expect("whole")), None, "no checkout place of its own: not asked from");
    let none = encoded(&[(checkout, "", cursor)], None);
    assert_eq!(paired_place(Stored::of(&none).expect("whole")), None, "no helper place kept");
}

/// The detached helper is invoked with `journal-helper` alone (cwd is set to the checkout root),
/// never with an absolute path argument (which `bstd::path::Path` refuses).
#[cfg(unix)]
#[test]
fn detached_helper_args_use_cwd_not_absolute_path() {
    assert_eq!(HELPER_ARGS, ["journal-helper"]);
}
