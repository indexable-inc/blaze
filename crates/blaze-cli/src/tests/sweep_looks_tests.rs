//! The follow's batched looks (sweep.rs, "batched looks", thoughts2 1.1) against the control,
//! each row statted one by one ([`Serial`]), on bstd's simulated file system with the system's
//! stamps ([`FromMeta`]): a crate root, ten slots in one folder (eight or more asked names: a
//! folder listed whole where the platform lists), a `mod a;` slot (`src/a.rs` or
//! `src/a/mod.rs`), a manifest, a manifest probed absent and a listing. Each round edits files
//! at their length or not, deletes a slot, moves `a` between its two places, creates the absent
//! manifest or a file in the listed folder, and lets the clock pass a tick or not; the world's
//! seed orders the effects' completions.
//!
//! THE INVARIANTS: the batched follow's rows, their states before and the layout's move are
//! the serial follow's, every round (the result does not depend on completion order or on the
//! walk); a quiet round with the stamps settled proves rows by the looks alone (the batch ran:
//! rule 61); and a look proves a row exactly when a stat would, every part compared, the
//! device included (asked of the proof itself: the simulated world has one device). PLANTED,
//! CAUGHT: [`TimesIgnored`], a look that proves a file by its length alone, which an edit at
//! the file's length breaks; [`DevIgnored`], a look whose device is not compared.

use core::fmt;
use std::format;
use std::string::{String as StdString, ToString};
use std::time::Duration;
use std::vec::Vec as StdVec;

use blaze_sim::{Broken, Config, Tape, check, controls};
use bstd::fs::{Content, Durable, EntryKind, Look};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

use super::*;

/// What the looks' oracle can find broken.
#[derive(Clone, PartialEq, Eq, Debug)]
enum LooksBroken {
    /// The batched follow's rows, states before, layout move or stat count differ from the
    /// serial follow's.
    Differs { round: usize, first: StdString },
    /// A quiet round, the stamps settled, proved no row by the looks.
    NoneProved { counts: StdString },
    /// A look that differs from the row's stamp in `part` ("none": in nothing) proved the row
    /// (`proved`), or one equal did not.
    Proved { part: &'static str, proved: bool },
}

impl fmt::Display for LooksBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LooksBroken::Differs { round, first } => write!(f, "round {round}: the batched follow differs from the serial one: first {first}"),
            LooksBroken::NoneProved { counts } => write!(f, "a quiet settled round proved no row by the looks: {counts}"),
            LooksBroken::Proved { part, proved } => write!(f, "a look differing in {part} is proved {proved}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the batched follow, run on the simulated world.
    enum FollowPlant breaks LooksBroken {
        /// The batched walk as it is.
        None,
        /// A file's look proves it by its length alone: an edit at its length is missed.
        TimesIgnored => LooksBroken::Differs { .. },
    }
}

blaze_sim::plants! {
    /// The bugs planted in a look's proof of one row.
    enum ProofPlant breaks LooksBroken {
        /// The proof as it is.
        None,
        /// The times, inode and device unread.
        TimesIgnored => LooksBroken::Proved { proved: true, .. },
        /// The device unread: a file replaced by one on another mount with the same inode
        /// number, length and times is proved unchanged.
        DevIgnored => LooksBroken::Proved { part: "dev", proved: true },
    }
}

const SLOTS: usize = 10;

fn harness<E: fmt::Display>(what: &'static str) -> impl Fn(E) -> Broken<LooksBroken> {
    move |error| Broken::Harness(format!("{what}: {error}"))
}

fn slot(n: usize) -> StdString {
    format!("src/m{n}.rs")
}

/// A row as the oracle compares it.
type Row = (LeafKind, u8, StdString, Option<Stamp>, ContentId);

fn rows_of(followed: &Followed<'_>) -> StdVec<Row> {
    followed.now.rows.iter().map(|leaf| (leaf.kind, leaf.flags, followed.now.path(leaf).to_string(), leaf.stamp, leaf.id)).collect()
}

fn follow(tape: &Tape, plant: FollowPlant) -> Result<(), Broken<LooksBroken>> {
    let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("a root context"))?;
    let fs = io.need_fs().map_err(harness("the file system"))?;
    let clock = io.need_clock().map_err(harness("a clock"))?;
    let root = fs.open_dir(&cx, &Path::new("ws").map_err(harness("a path"))?).map_err(harness("open"))?;
    let write = |path: &str, bytes: &[u8]| -> Result<(), Broken<LooksBroken>> {
        root.publish(&cx, &Path::new(path).map_err(harness("a path"))?, Content::bytes(bytes), Durable::No).map(|_| ()).map_err(harness("a write"))
    };
    let remove = |path: &str| -> Result<(), Broken<LooksBroken>> {
        match root.remove(&cx, &Path::new(path).map_err(harness("a path"))?, Durable::No) {
            Ok(_) => Ok(()),
            Err(error) if error.kind == FsKind::NotFound => Ok(()),
            Err(error) => Err(harness("a remove")(error)),
        }
    };
    write("Cargo.toml", b"[package]\nname = \"w\"\n")?;
    write("src/lib.rs", b"mod a;\n")?;
    write("src/a.rs", b"pub fn a() {}\n")?;
    for n in 0..SLOTS {
        write(&slot(n), format!("pub fn f{n}() {{}}\n").as_bytes())?;
    }
    // The first answer's reads, as the cli makes them.
    let frame = cx.frame();
    let stamps = FromMeta::now(&clock);
    let mut first = Rows::new_in(frame);
    let mut text = Bytes::new_in(frame, "a manifest");
    read_manifest(&cx, &root, &stamps, "Cargo.toml", &mut first, &mut text).map_err(harness("a manifest"))?;
    read_manifest(&cx, &root, &stamps, "m/Cargo.toml", &mut first, &mut text).map_err(harness("a manifest"))?;
    read_listing(&cx, &root, &stamps, "src", &mut first, frame).map_err(harness("a listing"))?;
    let mut probes: StdVec<(StdString, bool)> = StdVec::from([("src/lib.rs".to_string(), false), ("src/a.rs".to_string(), true)]);
    probes.extend((0..SLOTS).map(|n| (slot(n), false)));
    for (path, module) in probes.iter() {
        let probed = probe_slot(&cx, &root, &stamps, path, *module, frame).map_err(CliError::from).map_err(harness("an open"))?;
        opened(path, probed, &mut first);
    }
    let mut table: StdVec<u8> = Leaves::bytes_in(&first, frame).map_err(harness("the table"))?.to_vec();
    let rounds = 1 + tape.index(6, "rounds");
    // The last two rounds are quiet, each after a tick. The first of them settles the table: a
    // row is kept with a stamp only when the stamp was settled at its follow (`look_at`), so
    // a table made before any tick passed (tape `0*2 1 4`: one round, no tick, the absent
    // manifest made) holds no stamp a look or a stat could prove from. The second is the
    // oracle's: every row now holds a settled stamp, and nothing changed.
    let last_round = rounds + 1;
    for round in 0..=last_round {
        let quiet = round >= rounds;
        if !quiet {
            for _ in 0..tape.index(4, "edits this round") {
                match tape.index(7, "an edit") {
                    0 => {
                        let n = tape.index(SLOTS, "a slot rewritten at its length");
                        write(&slot(n), format!("pub fn g{n}() {{}}\n").as_bytes())?;
                    }
                    1 => {
                        let n = tape.index(SLOTS, "a slot rewritten longer");
                        write(&slot(n), format!("pub fn long{n}() {{ let _ = {n}; }}\n").as_bytes())?;
                    }
                    2 => remove(&slot(tape.index(SLOTS, "a slot deleted")))?,
                    3 => {
                        // `a` moves to its other place (or is at both, a finding).
                        write("src/a/mod.rs", b"pub fn b() {}\n")?;
                        if tape.bool("and its first place goes") {
                            remove("src/a.rs")?;
                        }
                    }
                    4 => write("m/Cargo.toml", b"[package]\nname = \"m\"\n")?,
                    5 => write("src/new.rs", b"// a file the listing now names\n")?,
                    _ => write("Cargo.toml", b"[package]\nname = \"v\"\n")?,
                }
            }
        }
        if quiet || tape.bool("the clock passes a tick") {
            clock.sleep(&cx, Duration::from_millis(50)).map_err(harness("the clock"))?;
        }
        let (last, _) = Leaves::of(&table).map_err(harness("the leaf table"))?;
        let follow = Follow { stamps: FromMeta::now(&clock), plant: Plant::None };
        let (serial, serial_statted) = follow.follow_marked_by::<Serial>(&cx, &root, None, &last, None, frame).map_err(harness("the serial follow"))?;
        let (batched, batched_statted) = match plant {
            FollowPlant::None => follow.follow_marked_by::<Batched>(&cx, &root, None, &last, None, frame),
            FollowPlant::TimesIgnored => follow.follow_marked_by::<TimesIgnored>(&cx, &root, None, &last, None, frame),
        }
        .map_err(harness("the batched follow"))?;
        let (got, want) = (rows_of(&batched), rows_of(&serial));
        if got != want || batched.was.as_slice() != serial.was.as_slice() || batched.layout_moved != serial.layout_moved || batched_statted != serial_statted {
            let first = got.iter().zip(want.iter()).find(|(a, b)| a != b);
            return Err(Broken::Oracle(LooksBroken::Differs { round, first: format!("{first:?}") }));
        }
        if serial.looks != LookCounts::default() {
            return Err(Broken::Harness(format!("round {round}: the serial control asked looks: {:?}", serial.looks)));
        }
        if round == last_round && (batched.looks.batches == 0 || batched.looks.proved == 0 || batched.looks.asked == 0) {
            return Err(Broken::Oracle(LooksBroken::NoneProved { counts: format!("{:?}", batched.looks) }));
        }
        table = Leaves::bytes_in(&serial.now, frame).map_err(harness("the table's write"))?.to_vec();
    }
    Ok(())
}

/// The part of a look that differs from the row's stamp in [`one_part`].
const PARTS: [&str; 7] = ["none", "len", "mtime", "ctime", "ino", "dev", "kind"];

/// One settled slot row and one look of its path, equal but for one part the tape picks (or
/// none): the look proves the row exactly when nothing differs.
fn one_part(tape: &Tape, plant: ProofPlant) -> Result<(), Broken<LooksBroken>> {
    let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("a root context"))?;
    let frame = cx.frame();
    let stamps = FromMeta { swept_at_ns: 60_000_000_000 };
    let stamp = Stamp { len: 1 + tape.below(1 << 20, "a length"), mtime_ns: 1_000_000_000, ctime_ns: 2_000_000_000, ino: 7, dev: 0x5155 };
    let old = Leaf { kind: LeafKind::Slot, flags: 0, path: 0, stamp: Some(stamp), id: ContentId([3; 32]) };
    let mut look = Look { kind: EntryKind::File, len: stamp.len, modified_ns: stamp.mtime_ns, changed_ns: stamp.ctime_ns, ino: stamp.ino, dev: stamp.dev };
    let part = PARTS[tape.index(PARTS.len(), "the part that differs")];
    match part {
        "len" => look.len += 1,
        "mtime" => look.modified_ns += 1,
        "ctime" => look.changed_ns += 1,
        "ino" => look.ino += 1,
        "dev" => look.dev += 1,
        "kind" => look.kind = EntryKind::Dir,
        _ => {}
    }
    let mut looked = Looked::none(frame);
    looked.at.push([0, NO_LOOK]);
    looked.looks.push(look);
    let proved = match plant {
        ProofPlant::None => looked.proves::<Batched>(&stamps, 0, &old),
        ProofPlant::TimesIgnored => looked.proves::<TimesIgnored>(&stamps, 0, &old),
        ProofPlant::DevIgnored => looked.proves::<DevIgnored>(&stamps, 0, &old),
    }
    .is_some();
    if proved != (part == "none") {
        return Err(Broken::Oracle(LooksBroken::Proved { part, proved }));
    }
    Ok(())
}

#[test]
fn the_batched_follow_is_the_serial_one() {
    check(Config::cases(128), |tape| follow(tape, FollowPlant::None));
}

#[test]
fn every_follow_plant_is_caught_by_its_invariant() {
    controls(Config::cases(128), |tape, plant: FollowPlant| follow(tape, plant));
}

#[test]
fn a_look_proves_a_row_exactly_when_a_stat_would() {
    check(Config::cases(256), |tape| one_part(tape, ProofPlant::None));
}

#[test]
fn every_proof_plant_is_caught_by_its_invariant() {
    controls(Config::cases(256), |tape, plant: ProofPlant| one_part(tape, plant));
}
