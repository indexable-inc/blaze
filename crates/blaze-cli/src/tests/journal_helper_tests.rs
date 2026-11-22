//! journal_helper.rs's sans-IO decision of a look at its own executable ([`look_at`]).
//!
//! THE ORACLE, independent of its order of checks: a look ends the helper exactly when the
//! file is gone, or its stamp moved and the build read at it is not this one (another, none, or
//! unreadable); a moved stamp of this build is kept; an unmoved stamp reads nothing. Planted,
//! caught: a moved stamp taken as the same file ([`LookPlant::StampOnly`]), which would let a
//! helper of a replaced build live on.

use std::format;

use blaze_sim::{Broke, Config, Tape, check, control};
use bstd::fs::Stamp;
use bstd::host::BuildId;

use super::*;

fn stamp(tape: &Tape, label: &'static str) -> Stamp {
    // Few values, so equal stamps come up as often as moved ones.
    Stamp { len: tape.below(2, label), modified_ns: tape.below(2, label), changed_ns: tape.below(2, label), ino: tape.below(2, label), dev: 1 }
}

/// What the file's head says, as the tape picks it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Head {
    Own,
    Other,
    NoId,
    Unreadable,
}

fn one_look(tape: &Tape, plant: LookPlant) -> Result<(), Broke> {
    let own = BuildId::of(b"the helper's own build").expect("an id");
    let other = BuildId::of(b"a build installed over it").expect("an id");
    let was = stamp(tape, "the stamp kept");
    let now = (!tape.one_in(5, "the file gone")).then(|| stamp(tape, "the stamp now"));
    let head = *tape.pick(&[Head::Own, Head::Other, Head::NoId, Head::Unreadable], "its head").expect("four");
    let mut reads = 0u32;
    let (looked, read) = look_at(
        own,
        was,
        now,
        || {
            reads += 1;
            match head {
                Head::Own => Ok(Some(own)),
                Head::Other => Ok(Some(other)),
                Head::NoId => Ok(None),
                Head::Unreadable => Err(()),
            }
        },
        plant,
    );
    let moved = now.is_some_and(|now| now != was);
    let want = match now {
        None => Looked::Replaced,
        Some(now) if now == was => Looked::Same,
        Some(now) => match head {
            Head::Own => Looked::Moved(now),
            Head::Other | Head::NoId | Head::Unreadable => Looked::Replaced,
        },
    };
    if looked != want {
        return Err(Broke::Oracle(format!("a look at the executable misjudged it: got {looked:?}, want {want:?} (kept {was:?}, now {now:?}, head {head:?}, {plant:?})")));
    }
    if (reads == 1) != moved || read.is_some() != moved {
        return Err(Broke::Oracle(format!("a look at the executable misjudged it: its head read {reads} time(s) with the stamp moved: {moved} ({plant:?})")));
    }
    Ok(())
}

#[test]
fn a_look_ends_the_helper_exactly_when_its_build_is_gone() {
    check(Config::cases(2000), |tape| one_look(tape, LookPlant::None));
}

#[test]
fn control_a_moved_stamp_taken_as_the_same_file() {
    control(Config::cases(2000), LookPlant::None, LookPlant::StampOnly, "misjudged it", one_look);
}
