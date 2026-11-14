//! The simulation held to the disk world ([`crate::world::disk`]): objective V14's refinement
//! test of [`Sim`]'s file system.
//!
//! A run draws file effects over four names (appends, writes at an offset and from memory,
//! truncations, syncs, barriers, whole-file writes and renames that wait for the drive and that
//! do not, removals), gives them to a `Sim` a few at a time, so they overlap and complete in
//! the simulation's order, and tells a [`Disk`] each completion as the simulation told it.
//! After every poll the simulation's files are the world's, change for change. At a crash the
//! cut the simulation drew ([`Sim::crash_cut`]) is one the world allows ([`Disk::allows`]) and
//! leaves the world's disk ([`Disk::crash`]).
//!
//! Beside the disk it keeps the asks' world ([`World`]): each effect is asked of it when it
//! is given to `Sim`, and each completion is an answer ([`Told`]) read off what `Sim` did. Every
//! answer `Sim` gives is one the world allows ([`World::allows`]) at the moment it is given
//! (a sync covers at least what was written before it was asked and is still not durable), the
//! world stays well formed, its disk is the disk told directly, and at a crash the sectors the
//! writes in flight landed, in the order the simulation landed them, and the cut drawn are a
//! crash the world allows.
//!
//! So a proof against the world and a simulation test under `Sim` are about one file system:
//! what `Sim` does, the world allows. The reverse (every cut the world allows, `Sim` draws on
//! some seed) is not checked here: where `Sim` draws fewer, a simulation test sees less than a
//! proof covers, never more.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::host::Plant;
use super::{Cut, FAULT, Faults, Op, Sim, SimFile};
use crate::world::asks::{Ask, Landing, Told, World, unsynced};
use crate::world::disk::{Barrier, Change, Disk, File, Kept, SECTOR, Torn, sectors};
use crate::{Cache, Completion, Done, Driver, Effect, FileName, Memory, Tag, wait::Deadline};

/// The names a run uses, in their order: a file's number in the world is its place here.
const NAMES: [&str; 4] = ["w/a", "w/b", "w/c", "w/d"];
/// The bytes of the memory a write from memory takes its bytes from.
const MEMORY: usize = 4096;
/// The most a poll may wait in virtual time: every effect here completes long before it.
const POLL: Duration = Duration::from_secs(1);

fn name(file: usize) -> FileName {
    FileName::new(NAMES[file]).expect("a name")
}

fn number(file: &FileName) -> u64 {
    NAMES.iter().position(|it| *it == file.as_str()).expect("one of the run's names") as u64
}

#[allow(unsafe_code, reason = "test memory: under `Sim`, which touches it only inside poll and crash")]
fn memory() -> Memory {
    // SAFETY: `Memory::new`'s contract holds under `Sim` on the test's one thread.
    let mut memory = unsafe { Memory::new("the disk world's refinement test", MEMORY) }.expect("a free region");
    memory.write(0, &pattern(0, MEMORY));
    memory
}

/// The bytes the test's memory holds at `at`: no two sectors alike, and none of them zero, so
/// a sector that landed is told from a hole and from another.
fn pattern(at: usize, len: usize) -> Vec<u8> {
    (at..at + len).map(|i| 1 + ((i * 7 + i / SECTOR) % 251) as u8).collect()
}

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

fn harness(why: String) -> Broke {
    Broke::Harness(why)
}

/// A change of the simulation's as the world's.
fn change(op: &Op) -> Change {
    // clone: a test's copy of a change's bytes
    match op {
        Op::Append(bytes) => Change::Append(bytes.clone()),
        Op::At(at, bytes) => Change::At(*at as usize, bytes.clone()),
        Op::Sectors(at, bytes) => Change::Sectors(*at as usize, bytes.clone()),
        Op::Cut(len) => Change::Cut(*len as usize),
    }
}

/// Whether the simulation's file is the world's: the same durable bytes, the same changes not
/// synced in the same epochs, the same barriers, the same thing to undo.
fn same_file(sim: &Sim, name: &FileName, ours: &SimFile, theirs: &File) -> Result<(), String> {
    if ours.durable != theirs.durable {
        return Err(format!(
            "{name}: {} durable bytes in the simulation, {} in the world",
            ours.durable.len(),
            theirs.durable.len()
        ));
    }
    let changes: Vec<Change> = ours.ops.iter().map(change).collect();
    let made: Vec<Change> = theirs.changes.iter().map(|it| it.change.clone()).collect();
    if changes != made {
        return Err(format!(
            "{name}: its changes not synced are {} in the simulation and {} in the world",
            said(&changes),
            said(&made)
        ));
    }
    // A change the simulation has not stamped yet is of the epoch that is now.
    let epochs: Vec<u64> =
        (0..ours.ops.len()).map(|at| ours.stamps.get(at).copied().unwrap_or(sim.epoch)).collect();
    let theirs_epochs: Vec<u64> = theirs.changes.iter().map(|it| it.epoch).collect();
    if epochs != theirs_epochs {
        return Err(format!(
            "{name}: its changes were made in epochs {epochs:?} in the simulation and \
             {theirs_epochs:?} in the world"
        ));
    }
    let barriers: Vec<Barrier> =
        ours.barriers.iter().map(|(covers, epoch)| Barrier { covers: *covers, epoch: *epoch }).collect();
    if barriers != theirs.barriers {
        return Err(format!(
            "{name}: its barriers are {barriers:?} in the simulation and {:?} in the world",
            theirs.barriers
        ));
    }
    let undo = ours.undo.as_ref().map(|undo| {
        // clone: a test's copy of what a crash would put back
        (undo.was.as_ref().map(|was| was.durable.clone()), undo.from.as_ref().map(number))
    });
    // clone: the same, of the world's
    let theirs_undo = theirs.undo.as_ref().map(|undo| (undo.was.clone(), undo.from));
    if undo != theirs_undo {
        return Err(format!(
            "{name}: a crash may undo {} in the simulation and {} in the world",
            undone(&undo),
            undone(&theirs_undo)
        ));
    }
    let read = sim.contents(name);
    let theirs_read = theirs.contents();
    if read != theirs_read {
        return Err(format!(
            "{name}: a read sees {} bytes in the simulation and {} in the world, of the same \
             durable bytes and changes",
            read.len(),
            theirs_read.len()
        ));
    }
    Ok(())
}

fn said(changes: &[Change]) -> String {
    let each: Vec<String> = changes
        .iter()
        .map(|it| match it {
            Change::Append(bytes) => format!("append {}", bytes.len()),
            Change::At(at, bytes) => format!("{} at {at}", bytes.len()),
            Change::Sectors(at, bytes) => format!("{} from memory at {at}", bytes.len()),
            Change::Cut(len) => format!("cut to {len}"),
        })
        .collect();
    format!("[{}]", each.join(", "))
}

fn undone(undo: &Option<(Option<Vec<u8>>, Option<u64>)>) -> String {
    match undo {
        None => "nothing".to_owned(),
        Some((was, from)) => format!(
            "a file that held {} and came from {}",
            was.as_ref().map_or("no file".to_owned(), |it| format!("{} bytes", it.len())),
            from.map_or("no name".to_owned(), |it| NAMES[it as usize].to_owned())
        ),
    }
}

/// The refinement's relation: the simulation's disk is the world's.
fn same(sim: &Sim, world: &Disk) -> Result<(), Broke> {
    if !world.wf() {
        return Err(oracle(format!("the world is not well formed: {world:?}")));
    }
    if sim.epoch != world.epoch {
        return Err(oracle(format!(
            "{} barriers have completed in the simulation and {} in the world",
            sim.epoch, world.epoch
        )));
    }
    let mut there = 0;
    for file in 0..NAMES.len() {
        let name = name(file);
        match (sim.files.get(&name), world.find(file as u64)) {
            (None, None) => {}
            (Some(ours), Some(at)) => {
                there += 1;
                same_file(sim, &name, ours, &world.files[at]).map_err(oracle)?;
            }
            (ours, theirs) => {
                return Err(oracle(format!(
                    "{name}: {} in the simulation and {} in the world",
                    if ours.is_some() { "a file" } else { "no file" },
                    if theirs.is_some() { "a file" } else { "no file" }
                )));
            }
        }
    }
    if sim.files.len() != there || world.files.len() != there {
        return Err(harness(format!(
            "the simulation holds {} files and the world {}, {there} of them under the run's names",
            sim.files.len(),
            world.files.len()
        )));
    }
    Ok(())
}

/// The changes the simulation's `file` has past the world's, which a failed write or one in
/// flight at a crash left: told to the world as what they are.
fn landed(sim: &Sim, world: &mut Disk, file: &FileName) -> Result<(), Broke> {
    let Some(ours) = sim.files.get(file) else { return Ok(()) };
    let has = world.find(number(file)).map_or(0, |at| world.files[at].changes.len());
    for op in ours.ops.iter().skip(has) {
        // clone: a test's copy of a change's bytes
        match op {
            Op::Append(bytes) => world.append(number(file), bytes.clone()),
            Op::Sectors(at, bytes) => {
                if !world.write_from(number(file), *at as usize, bytes.clone()) {
                    return Err(harness(format!("{file}: sectors landed on a file the world has not")));
                }
            }
            other => return Err(harness(format!("{file}: a failed write left {other:?}"))),
        }
    }
    Ok(())
}

/// One effect in flight, as the test has to remember it to tell the world its completion.
struct Flying {
    effect: Effect,
    /// For a sync and a barrier: how many changes the file had not synced when it was asked.
    covers: usize,
}

/// The world told one completion of the simulation's.
fn tell(sim: &Sim, world: &mut Disk, flying: Flying, done: &Done) -> Result<(), Broke> {
    let Flying { effect, covers } = flying;
    if let Done::Failed(_) = done {
        // A failed append wrote a part of its bytes; a failed write from memory landed any of
        // its sectors, which is `Disk::land`'s to say; a failed anything else changed nothing.
        match &effect {
            Effect::Append { file, .. } => landed(sim, world, file)?,
            Effect::WriteFrom { file, at, from, .. } => {
                let Some(ours) = sim.files.get(file) else { return Ok(()) };
                let Some(found) = world.find(number(file)) else { return Ok(()) };
                let bytes = pattern(from.offset as usize, from.len as usize);
                let mut mask = vec![false; sectors(bytes.len())];
                for op in ours.ops.iter().skip(world.files[found].changes.len()) {
                    match op {
                        Op::Sectors(sector, _) => mask[(*sector - *at) as usize / SECTOR] = true,
                        other => return Err(harness(format!("{file}: a failed write from memory left {other:?}"))),
                    }
                }
                world.land(number(file), *at as usize, &bytes, &mask);
            }
            _ => {}
        }
        return Ok(());
    }
    let done_as_asked = match effect {
        Effect::Append { file, bytes, .. } => {
            world.append(number(&file), bytes);
            true
        }
        Effect::WriteAt { file, at, bytes, .. } => world.write_at(number(&file), at as usize, bytes),
        Effect::WriteFrom { file, at, from, .. } => {
            world.write_from(number(&file), at as usize, pattern(from.offset as usize, from.len as usize))
        }
        Effect::Truncate { file, len, .. } => {
            world.truncate(number(&file), len as usize);
            true
        }
        Effect::Sync { file, .. } => {
            world.sync(number(&file), covers);
            true
        }
        Effect::Barrier { file, .. } => {
            world.barrier(number(&file), covers);
            true
        }
        // Written out and not durable: nothing a crash keeps changes.
        Effect::WriteBack { .. } => true,
        Effect::SyncDir { .. } => {
            world.flush();
            true
        }
        Effect::Write { file, bytes, .. } => {
            world.write(number(&file), bytes);
            true
        }
        Effect::WriteUnsynced { file, bytes, .. } => {
            world.write_unsynced(number(&file), bytes);
            true
        }
        Effect::Rename { from, to, .. } => world.rename(number(&from), number(&to)),
        Effect::RenameUnsynced { from, to, .. } => world.rename_unsynced(number(&from), number(&to)),
        Effect::Remove { file, .. } => {
            world.remove(number(&file));
            true
        }
        other => return Err(harness(format!("the run drew no {other:?}"))),
    };
    match done_as_asked {
        true => Ok(()),
        false => Err(oracle(format!("the simulation completed {done:?} and the world has no such file"))),
    }
}

/// The effect as the asks' world takes it; none for a write-back, which the world does not
/// have (nothing a crash keeps changes).
fn ask_of(effect: &Effect) -> Option<Ask> {
    // clone: the test's copy of an effect's bytes, which the world keeps while it is in flight
    Some(match effect {
        Effect::Append { file, bytes, .. } => Ask::Append { file: number(file), bytes: bytes.clone() },
        Effect::Sync { file, .. } => Ask::Sync { file: number(file) },
        Effect::Barrier { file, .. } => Ask::Barrier { file: number(file) },
        Effect::WriteAt { file, at, bytes, .. } => {
            Ask::WriteAt { file: number(file), at: *at as usize, bytes: bytes.clone() }
        }
        Effect::WriteFrom { file, at, from, .. } => Ask::WriteFrom {
            file: number(file),
            at: *at as usize,
            bytes: pattern(from.offset as usize, from.len as usize),
        },
        Effect::Truncate { file, len, .. } => Ask::Truncate { file: number(file), len: *len as usize },
        Effect::SyncDir { .. } => Ask::SyncDir,
        Effect::Write { file, bytes, .. } => Ask::Write { file: number(file), bytes: bytes.clone() },
        Effect::WriteUnsynced { file, bytes, .. } => {
            Ask::WriteUnsynced { file: number(file), bytes: bytes.clone() }
        }
        Effect::Rename { from, to, .. } => Ask::Rename { from: number(from), to: number(to) },
        Effect::RenameUnsynced { from, to, .. } => Ask::RenameUnsynced { from: number(from), to: number(to) },
        Effect::Remove { file, .. } => Ask::Remove { file: number(file) },
        _ => return None,
    })
}

/// Of a failed write from memory to `file`, each of its sectors the simulation landed: the
/// changes its file has past the world's `has`, each a sector of the write's.
fn mask(sim: &Sim, file: &FileName, at: u64, len: usize, has: usize) -> Result<Vec<bool>, Broke> {
    let mut mask = vec![false; sectors(len)];
    let Some(ours) = sim.files.get(file) else { return Ok(mask) };
    for op in ours.ops.iter().skip(has) {
        match op {
            Op::Sectors(sector, _) => mask[(*sector - at) as usize / SECTOR] = true,
            other => return Err(harness(format!("{file}: a failed write from memory left {other:?}"))),
        }
    }
    Ok(mask)
}

/// The answer the simulation gave to `flying`, read off what it did, the world's disk `world`
/// being as it was before it. `alone`: it is the only completion of its poll, so the
/// simulation's state after the poll is the state after it.
fn answer(sim: &Sim, world: &Disk, flying: &Flying, done: &Done, alone: bool) -> Result<Told, Broke> {
    let injected = |why: &crate::IoError| why.0 == FAULT;
    let told = match (&flying.effect, done) {
        (Effect::Append { file, .. }, Done::Failed(_)) => {
            // The prefix that landed, a change of its own, the one past the world's.
            let has = unsynced(world, number(file));
            match sim.files.get(file).and_then(|it| it.ops.get(has)) {
                Some(Op::Append(part)) => Told::FailedAfter(part.len()),
                other => return Err(harness(format!("{file}: a failed append left {other:?}"))),
            }
        }
        (Effect::WriteFrom { file, at, from, .. }, Done::Failed(why)) => match world.find(number(file)) {
            Some(found) => Told::FailedLanding(mask(sim, file, *at, from.len as usize, world.files[found].changes.len())?),
            None if injected(why) => Told::Failed,
            None => Told::Missing,
        },
        (Effect::WriteAt { .. } | Effect::Rename { .. } | Effect::RenameUnsynced { .. }, Done::Failed(why)) => {
            match injected(why) {
                true => Told::Failed,
                false => Told::Missing,
            }
        }
        (_, Done::Failed(_)) => Told::Failed,
        (Effect::Sync { file, .. }, _) => {
            let before = unsynced(world, number(file));
            let covered = match alone {
                // Read off the simulation's state: how many of the file's changes are durable
                // now that were not before (its own and, where the flush made more durable,
                // what its barriers covered: an answer the world gives the same disk for).
                true => {
                    let after = sim.files.get(file).map_or(0, |it| it.ops.len());
                    before.checked_sub(after).ok_or_else(|| {
                        harness(format!("{file}: a sync left {after} changes not synced of {before}"))
                    })?
                }
                // With another completion in the poll the state between them is not seen:
                // what the simulation's own rule says, `covers.min(ops.len())` at completion.
                false => flying.covers.min(before),
            };
            Told::Covered(covered)
        }
        (Effect::Barrier { file, .. }, _) => {
            let before = unsynced(world, number(file));
            let covered = match alone {
                // The barrier the simulation made, of the epoch it completed in; none where
                // it covered nothing.
                true => sim
                    .files
                    .get(file)
                    .and_then(|it| it.barriers.iter().find(|(_, epoch)| *epoch + 1 == sim.epoch))
                    .map_or(0, |(covers, _)| *covers),
                // As for a sync.
                false => flying.covers.min(before),
            };
            Told::Covered(covered)
        }
        _ => Told::Done,
    };
    Ok(told)
}

/// The sectors of the writes from memory in flight that the simulation landed at a crash, in
/// the order it landed them: each change its file has past the world's is one sector, made as
/// `Sim::crash_cut` landed it. Each is given to the first write in flight, in the order asked,
/// to that file whose sector it is (offset and bytes) and whose sector has not landed yet:
/// where two writes could have made it, their sectors are the same change, so the choice
/// leaves the same disk. The files are taken one after another, the run's names in order: a
/// landing on one file and one on another leave the same disk in either order.
fn landings(sim: &Sim, world: &World) -> Result<Vec<Landing>, Broke> {
    let mut out: Vec<Landing> = Vec::new();
    for (file, name) in NAMES.iter().enumerate() {
        let Some(ours) = sim.files.get(&name_of(name)) else { continue };
        let past = unsynced(&world.disk, file as u64);
        for op in ours.ops.iter().skip(past) {
            let Op::Sectors(sector, part) = op else {
                return Err(harness(format!("{name}: a write in flight at a crash left {op:?}")));
            };
            let sector = *sector as usize;
            let landing = world.asked.iter().enumerate().find_map(|(ask, asked)| {
                let Ask::WriteFrom { file: of, at, bytes } = &asked.ask else { return None };
                let from = sector.checked_sub(*at)?;
                let landing = Landing { ask, sector: from / SECTOR };
                let fits = *of == file as u64
                    && from % SECTOR == 0
                    && from < bytes.len()
                    && bytes[from..(from + SECTOR).min(bytes.len())] == part[..]
                    && !out.contains(&landing);
                fits.then_some(landing)
            });
            match landing {
                Some(landing) => out.push(landing),
                None => {
                    return Err(oracle(format!(
                        "{name}: the crash landed {} bytes at {sector}, a sector of no write in flight \
                         that has not landed: in flight {:?}",
                        part.len(),
                        in_flight(world)
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// Whether `landings` lands a sector of a write in flight after a sector of one asked after
/// it, both to one file.
fn out_of_order(world: &World, landings: &[Landing]) -> bool {
    let file = |landing: &Landing| match &world.asked[landing.ask].ask {
        Ask::WriteFrom { file, .. } => Some(*file),
        _ => None,
    };
    landings.iter().enumerate().any(|(i, early)| {
        landings[i + 1..].iter().any(|late| late.ask < early.ask && file(late) == file(early))
    })
}

/// What is in flight in the asks' world, as tags, kinds and offsets.
fn in_flight(asks: &World) -> Vec<String> {
    asks.asked
        .iter()
        .map(|it| match &it.ask {
            Ask::WriteFrom { file, at, bytes } => {
                format!("{}: {} from memory at {at} of {}", it.tag, bytes.len(), NAMES[*file as usize])
            }
            other => format!("{}: {other:?}", it.tag),
        })
        .collect()
}

fn name_of(name: &str) -> FileName {
    FileName::new(name).expect("a name")
}

/// One effect drawn. Calm is an append of a few bytes to the first file.
fn draw(tape: &Tape, memory: &Memory, tag: u64) -> Effect {
    let file = || name(tape.index(NAMES.len(), "the file"));
    let bytes = || match tape.one_in(4, "bytes over sectors") {
        true => tape.bytes(SECTOR - 2..=3 * SECTOR, "the bytes"),
        false => tape.bytes(1..=6, "the bytes"),
    };
    let at = || match tape.one_in(3, "past the first sector") {
        true => tape.below(2 * SECTOR as u64, "the offset"),
        false => tape.below(8, "the offset"),
    };
    let tag = Tag(tag);
    match tape.below(13, "the effect") {
        0 => Effect::Append { file: file(), bytes: bytes(), tag },
        1 => Effect::Sync { file: file(), tag },
        2 => Effect::Barrier { file: file(), tag },
        3 => Effect::WriteAt { file: file(), at: at(), bytes: bytes(), tag },
        4 => Effect::Truncate { file: file(), len: at(), tag },
        5 => Effect::WriteUnsynced { file: file(), bytes: bytes(), tag },
        6 => Effect::RenameUnsynced { from: file(), to: file(), tag },
        7 => Effect::Write { file: file(), bytes: bytes(), tag },
        8 => Effect::Rename { from: file(), to: file(), tag },
        9 => Effect::Remove { file: file(), tag },
        10 => Effect::SyncDir { dir: FileName::new("w").expect("a name"), tag },
        11 => Effect::WriteBack { file: file(), tag },
        _ => {
            // Whole sectors of the test's memory, at a sector's offset in the file.
            let sectors = 1 + tape.below(3, "the sectors written") as usize;
            let from = tape.below(4, "the memory's sector") as usize * SECTOR;
            let at = tape.below(3, "the file's sector") * SECTOR as u64;
            let cut_short = tape.below(2, "a last sector cut short") as usize * 100;
            Effect::WriteFrom {
                file: file(),
                at,
                from: memory.span(from as u64, (sectors * SECTOR - cut_short) as u32),
                cache: Cache::Kept,
                tag,
            }
        }
    }
}

fn file_of(effect: &Effect) -> Option<&FileName> {
    match effect {
        Effect::Sync { file, .. } | Effect::Barrier { file, .. } => Some(file),
        _ => None,
    }
}

/// The cut the simulation drew, as the world's: one [`Kept`] for each file in the order of
/// the names.
fn kept(sim: &Sim, cuts: &[Cut]) -> Result<Vec<Kept>, Broke> {
    let mut out = Vec::new();
    for cut in cuts {
        let file = sim.files.get(&cut.name).ok_or_else(|| harness(format!("a cut of {}, which is not there", cut.name)))?;
        let torn = match (file.ops.get(cut.kept), cut.torn.as_slice()) {
            (_, []) => Torn::Nothing,
            (Some(Op::Append(_)), [Op::Append(part)]) | (Some(Op::At(..)), [Op::At(_, part)]) => {
                Torn::Prefix(part.len())
            }
            (Some(Op::Sectors(at, bytes)), parts) => {
                let mut mask = vec![false; sectors(bytes.len())];
                for part in parts {
                    match part {
                        Op::At(sector, _) => mask[(*sector - *at) as usize / SECTOR] = true,
                        other => return Err(harness(format!("a write from memory torn to {other:?}"))),
                    }
                }
                Torn::Sectors(mask)
            }
            (next, torn) => return Err(harness(format!("{next:?} torn to {torn:?}"))),
        };
        out.push(Kept { whole: cut.kept, torn, undone: cut.undone });
    }
    Ok(out)
}

/// The asks' world told `told` of `tag`, which must be an answer it allows; and the points the
/// run must reach, seen.
fn told_to(
    asks: &mut World,
    effect: &Effect,
    tag: u64,
    told: &Told,
    overtaken: &mut BTreeSet<u64>,
    renamed: &mut BTreeSet<u64>,
    tape: &Tape,
) -> Result<(), Broke> {
    if !asks.allows(tag, told) {
        return Err(oracle(format!(
            "the simulation told {told:?} of {effect:?}, an answer the world does not allow: {asks:?}"
        )));
    }
    let syncs_of = |asks: &World, name: u64| -> Vec<u64> {
        asks.asked
            .iter()
            .filter(|it| matches!(it.ask, Ask::Sync { file } if file == name) && it.tag != tag)
            .map(|it| it.tag)
            .collect()
    };
    match (effect, told) {
        (Effect::Sync { file, .. }, Told::Covered(_)) => {
            if overtaken.remove(&tag) {
                tape.reached("a sync told after a sync of its file asked after it");
            }
            if renamed.remove(&tag) {
                tape.reached("a sync whose name was renamed while it was in flight");
            }
            overtaken.extend(syncs_of(asks, number(file)).into_iter().filter(|it| *it < tag));
        }
        (Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. }, Told::Done) => {
            renamed.extend(syncs_of(asks, number(from)));
            renamed.extend(syncs_of(asks, number(to)));
        }
        _ => {}
    }
    if !asks.tell(tag, told) {
        return Err(oracle(format!("the asks' world allowed {told:?} of {effect:?} and its tell refused it")));
    }
    Ok(())
}

/// One run: effects drawn and given to a simulation and, completion by completion, to the
/// world, with crashes between and in the middle of them. `Err` where the two disagree.
fn holds(tape: &Tape, plant: Option<Plant>) -> Result<(), Broke> {
    let faults = Faults {
        fail_one_in: match tape.one_in(3, "effects fail") {
            true => 5,
            false => 0,
        },
        ..Faults::default()
    };
    let mut sim = Sim::new(tape.next_u64("the simulation's seed"), faults);
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    let memory = memory();
    let mut world = Disk::new();
    let mut asks = World::new();
    // The syncs another sync of their file, asked after them, was told before; and the syncs
    // whose file's name a rename moved while they were in flight.
    let mut overtaken: BTreeSet<u64> = BTreeSet::new();
    let mut renamed: BTreeSet<u64> = BTreeSet::new();
    let mut tag = 0;
    let mut flying: BTreeMap<Tag, Flying> = BTreeMap::new();
    let mut done: Vec<Completion> = Vec::new();
    for _ in 0..1 + tape.below(12, "more rounds") {
        let mut effects = Vec::new();
        for _ in 0..1 + tape.below(4, "more effects at once") {
            tag += 1;
            effects.push(draw(tape, &memory, tag));
        }
        for effect in &effects {
            // What a sync covers is what had been written when it was asked: counted here as
            // the simulation counts it, at the submit.
            let covers = file_of(effect)
                .and_then(|file| world.find(number(file)))
                .map_or(0, |at| world.files[at].changes.len());
            // clone: the test's copy of the effect, to tell the world its completion
            flying.insert(effect.tag(), Flying { effect: copy(effect), covers });
            if let Some(ask) = ask_of(effect)
                && !asks.ask(effect.tag().0, ask)
            {
                return Err(harness(format!("{:?} asked twice", effect.tag())));
            }
        }
        sim.submit(&mut effects);
        // A crash in the middle of them: after this many polls, with the rest in flight.
        let crash_after = match tape.one_in(4, "a crash in flight") {
            true => Some(tape.below(3, "polls before the crash")),
            false => None,
        };
        let mut polls = 0;
        while sim.in_flight() > 0 && crash_after != Some(polls) {
            let until = Deadline::after(sim.now(), POLL);
            if sim.poll(&mut done, until) == 0 {
                return Err(harness(format!("{} effects in flight and none completed in {POLL:?}", sim.in_flight())));
            }
            let alone = done.len() == 1;
            for completion in done.drain(..) {
                let Some(was) = flying.remove(&completion.tag) else {
                    return Err(harness(format!("a completion of {:?}, which was not in flight", completion.tag)));
                };
                if ask_of(&was.effect).is_some() {
                    let told = answer(&sim, &world, &was, &completion.done, alone)?;
                    told_to(&mut asks, &was.effect, completion.tag.0, &told, &mut overtaken, &mut renamed, tape)?;
                }
                tell(&sim, &mut world, was, &completion.done)?;
                if !asks.wf() {
                    return Err(oracle(format!("the asks' world is not well formed: {asks:?}")));
                }
                if asks.disk != world {
                    return Err(oracle(format!(
                        "the asks' world's disk is not the disk told directly: {:?} and {world:?}",
                        asks.disk
                    )));
                }
            }
            same(&sim, &world)?;
            polls += 1;
        }
        if crash_after.is_some() || tape.one_in(3, "a crash") {
            tape.reached("a crash");
            let cuts = sim.crash_cut();
            let sectors_landed = landings(&sim, &asks)?;
            // What was in flight is gone, but for the sectors a write from memory landed.
            for was in std::mem::take(&mut flying).into_values() {
                if let Effect::WriteFrom { file, .. } = &was.effect {
                    landed(&sim, &mut world, file)?;
                }
            }
            same(&sim, &world)?;
            if asks.landed(&sectors_landed).as_ref() != Some(&world) {
                return Err(oracle(format!(
                    "the writes in flight at the crash landed {sectors_landed:?}, which on the asks' world \
                     is not the simulation's disk: in flight {:?}",
                    in_flight(&asks)
                )));
            }
            if out_of_order(&asks, &sectors_landed) {
                tape.reached("a crash that lands sectors of two writes in flight out of the order they were asked");
            }
            let cut = kept(&sim, &cuts)?;
            if !world.allows(&cut) {
                return Err(oracle(format!(
                    "the simulation drew a cut the world does not allow: {cut:?} of {world:?}"
                )));
            }
            if cut.iter().any(|it| it.undone) {
                tape.reached("a crash that undoes");
            }
            if cut.iter().any(|it| it.torn != Torn::Nothing) {
                tape.reached("a crash that tears");
            }
            if !asks.allows_crash(&sectors_landed, &cut) {
                return Err(oracle(format!(
                    "the simulation's crash landed {sectors_landed:?} of the writes in flight and kept {cut:?}, \
                     a crash the asks' world does not allow: {asks:?}"
                )));
            }
            sim.crash_with(cuts);
            world.crash(&cut);
            asks.crash(&sectors_landed, &cut);
            if !asks.wf() || !asks.asked.is_empty() || asks.disk != world {
                return Err(oracle(format!(
                    "after the crash that landed {sectors_landed:?} and kept {cut:?}, the asks' world is \
                     {asks:?} and the disk told directly {world:?}"
                )));
            }
            same(&sim, &world).map_err(|broke| match broke {
                Broke::Oracle(why) => oracle(format!("after the crash that kept {cut:?}: {why}")),
                other => other,
            })?;
        }
    }
    Ok(())
}

/// The test's copy of an effect it drew.
fn copy(effect: &Effect) -> Effect {
    // clone: a test's copy of an effect's names and bytes
    match effect {
        Effect::Append { file, bytes, tag } => Effect::Append { file: file.clone(), bytes: bytes.clone(), tag: *tag },
        Effect::Sync { file, tag } => Effect::Sync { file: file.clone(), tag: *tag },
        Effect::Barrier { file, tag } => Effect::Barrier { file: file.clone(), tag: *tag },
        Effect::WriteAt { file, at, bytes, tag } => {
            Effect::WriteAt { file: file.clone(), at: *at, bytes: bytes.clone(), tag: *tag }
        }
        Effect::Truncate { file, len, tag } => Effect::Truncate { file: file.clone(), len: *len, tag: *tag },
        Effect::WriteUnsynced { file, bytes, tag } => {
            Effect::WriteUnsynced { file: file.clone(), bytes: bytes.clone(), tag: *tag }
        }
        Effect::RenameUnsynced { from, to, tag } => {
            Effect::RenameUnsynced { from: from.clone(), to: to.clone(), tag: *tag }
        }
        Effect::Write { file, bytes, tag } => Effect::Write { file: file.clone(), bytes: bytes.clone(), tag: *tag },
        Effect::Rename { from, to, tag } => Effect::Rename { from: from.clone(), to: to.clone(), tag: *tag },
        Effect::Remove { file, tag } => Effect::Remove { file: file.clone(), tag: *tag },
        Effect::SyncDir { dir, tag } => Effect::SyncDir { dir: dir.clone(), tag: *tag },
        Effect::WriteBack { file, tag } => Effect::WriteBack { file: file.clone(), tag: *tag },
        Effect::WriteFrom { file, at, from, cache, tag } => {
            Effect::WriteFrom { file: file.clone(), at: *at, from: *from, cache: *cache, tag: *tag }
        }
        other => unreachable!("the run drew no {other:?}"),
    }
}

/// The cases of the refinement and of each control.
fn config() -> Config {
    Config {
        must_reach: &[
            "a crash",
            "a crash that undoes",
            "a crash that tears",
            "a sync told after a sync of its file asked after it",
            "a sync whose name was renamed while it was in flight",
            "a crash that lands sectors of two writes in flight out of the order they were asked",
        ],
        ..Config::cases(3000)
    }
}

/// The refinement: the simulation's file system is the disk world's. Whatever effects a run
/// draws, in whatever order the simulation completes them, with whichever of them fail, after
/// every poll each file of the simulation's is the world's file (the same durable bytes, the
/// same changes not synced in the same epochs, the same barriers, the same write or rename a
/// crash may undo, the same bytes read); and at every crash, in flight or not, the cut the
/// simulation drew is one `Disk::allows`, and the disk it leaves is `Disk::crash`'s.
#[test]
fn the_simulation_s_disk_is_the_world_s() {
    check(config(), |tape| holds(tape, None));
}

/// The planted bug: a crash keeps a change made after a barrier completed and not what the
/// barrier covers. The world's `allows` refuses that cut.
#[test]
fn the_oracle_catches_a_crash_that_keeps_a_later_change_without_what_a_barrier_covers() {
    control(config(), None, Some(Plant::BarriersNotKept), "a cut the world does not allow", holds);
}

/// The planted bug, which was the simulation's until 2026-11-14: a rename that waits for the
/// drive flushes every file but the one it renames, whose barriers' changes stay not durable.
#[test]
fn the_oracle_catches_a_rename_whose_flush_misses_its_own_file() {
    control(config(), None, Some(Plant::RenameFlushesOthers), "durable bytes in the simulation", holds);
}

/// The planted bug, which was the simulation's until 2026-11-14: a file a crash puts back
/// under its name keeps the barriers and epochs of the changes it lost, so its next changes
/// are taken for older than they are and for covered by a barrier that never saw them.
#[test]
fn the_oracle_catches_a_file_put_back_with_the_barriers_of_what_it_lost() {
    control(config(), None, Some(Plant::PutBackWithBarriers), "in the simulation and", holds);
}

/// The planted bug: a sync that completes makes durable one change fewer than it covered. With
/// nothing between its ask and its answer it owes every change it covered, and the world
/// refuses an answer that covers fewer.
#[test]
fn the_oracle_catches_a_sync_that_makes_less_durable_than_it_owes() {
    control(config(), None, Some(Plant::SyncCoversLess), "an answer the world does not allow", holds);
}

/// The asks' world alone: a sync asked after two appends were told owes both, so an answer
/// that covers one is refused and one that covers two is allowed; once an earlier sync of the
/// file, asked after the first append, is told to cover it, the later sync owes one, and an
/// answer that covers one is allowed.
#[test]
fn the_oracle_catches_a_sync_answer_that_covers_less_than_it_owes() {
    let mut asks = World::new();
    assert!(asks.ask(1, Ask::Append { file: 0, bytes: b"one".to_vec() }));
    assert!(asks.tell(1, &Told::Done));
    assert!(asks.ask(2, Ask::Sync { file: 0 }));
    assert!(asks.ask(3, Ask::Append { file: 0, bytes: b"two".to_vec() }));
    assert!(asks.tell(3, &Told::Done));
    assert!(asks.ask(4, Ask::Sync { file: 0 }));
    assert!(!asks.ask(4, Ask::SyncDir), "a tag in flight is not asked again");
    assert!(asks.wf());
    assert!(!asks.allows(4, &Told::Covered(1)));
    assert!(!asks.tell(4, &Told::Covered(1)), "a refused answer changes nothing");
    assert!(asks.allows(4, &Told::Covered(2)));
    assert!(!asks.allows(4, &Told::Covered(3)), "no more than the file has not synced");
    assert!(asks.tell(2, &Told::Covered(1)));
    assert!(asks.wf());
    assert!(asks.allows(4, &Told::Covered(1)));
    assert!(!asks.allows(4, &Told::Covered(0)));
    assert!(asks.tell(4, &Told::Covered(1)));
    assert!(asks.wf() && asks.asked.is_empty());
    assert_eq!(asks.disk.contents(0).as_deref(), Some(&b"onetwo"[..]));
    assert!(asks.disk.files[0].changes.is_empty());
}

/// The asks' world alone, in the shape of the refinement's seed 683: two writes from memory in
/// flight to one file, the one asked second landing its middle sector before the one asked
/// first lands its last two. The world allows it, and the bytes where they overlap are the
/// last landed: 1024..1436 the first write's, 1436..1536 the second's. A sector landed twice,
/// one a write does not have, and a landing of what is not a write from memory are refused.
#[test]
fn the_oracle_catches_a_crash_that_lands_what_no_write_in_flight_has() {
    let mut asks = World::new();
    assert!(asks.ask(1, Ask::Write { file: 0, bytes: vec![0; 2048] }));
    assert!(asks.tell(1, &Told::Done));
    assert!(asks.ask(14, Ask::WriteFrom { file: 0, at: 0, bytes: vec![14; 1436] }));
    assert!(asks.ask(15, Ask::Sync { file: 0 }));
    assert!(asks.ask(16, Ask::WriteFrom { file: 0, at: 512, bytes: vec![16; 1436] }));
    let landings = [Landing { ask: 2, sector: 1 }, Landing { ask: 0, sector: 1 }, Landing { ask: 0, sector: 2 }];
    let all = [Kept { whole: 3, torn: Torn::Nothing, undone: false }];
    for refused in [
        vec![Landing { ask: 0, sector: 1 }, Landing { ask: 0, sector: 1 }],
        vec![Landing { ask: 2, sector: 3 }],
        vec![Landing { ask: 1, sector: 0 }],
        vec![Landing { ask: 3, sector: 0 }],
    ] {
        assert!(!asks.allows_crash(&refused, &all), "{refused:?}");
    }
    assert!(asks.allows_crash(&landings, &all));
    asks.crash(&landings, &all);
    assert!(asks.wf() && asks.asked.is_empty());
    let read = asks.disk.contents(0).expect("there");
    assert_eq!(read.len(), 2048);
    assert!(read[..512].iter().all(|it| *it == 0));
    assert!(read[512..1024].iter().all(|it| *it == 14));
    assert!(read[1024..1436].iter().all(|it| *it == 14));
    assert!(read[1436..1536].iter().all(|it| *it == 16));
    assert!(read[1536..].iter().all(|it| *it == 0));
}
