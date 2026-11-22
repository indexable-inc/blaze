//! The roster on the simulation (remote-read.md section 3, piece 8): binaries written as files
//! in the simulated tree, each a 64-bit Mach-O header with its `LC_UUID`, read back through
//! bstd's effect layer as the command reads them.

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::host::BuildId;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::{RosterPlant, State, WINDOW_DAYS, live, live_with, write_entry, write_own};

/// The user's home in the simulation (`HOME=/h`); blaze's folder in it, as store.rs
/// `Machine::of` opens it.
const HOME: &str = "/h";
/// The simulated host's own program (bstd `HostFacts::simulated`).
const OWN: &str = "/sim/bin/blaze";
/// The simulated host's build id (bstd `HostFacts::simulated`).
const OWN_ID: &[u8] = b"bstd simulated!!";

/// A binary's first bytes as a 64-bit little-endian Mach-O with one load command, `LC_UUID`
/// holding `id` (mach-o/loader.h: `mach_header_64`, then `uuid_command`).
fn macho(id: &[u8; 16]) -> std::vec::Vec<u8> {
    let mut bytes = std::vec::Vec::new();
    // magic, cputype (arm64), cpusubtype, filetype (execute), ncmds 1, sizeofcmds 24, flags, reserved.
    for word in [0xfeed_facfu32, 0x0100_000c, 0, 2, 1, 24, 0, 0, 0x1b, 24] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.extend_from_slice(id);
    // Code after the header, so the file is longer than the commands.
    bytes.extend_from_slice(&[0xaa; 300]);
    bytes
}

/// The binary at the absolute `exe` written into the simulated tree (beneath its one root).
fn install(io: &Io, cx: &Cx, exe: &str, id: &[u8; 16]) -> Result<(), Broke> {
    let path = Path::new(exe.trim_start_matches('/')).map_err(|e| Broke::Harness(format!("a path: {e:?}")))?;
    let fs = io.need_fs().map_err(|e| Broke::Harness(format!("files: {e}")))?;
    fs.root().publish(cx, &path, Content::bytes(&macho(id)), Durable::No).map_err(|e| Broke::Harness(format!("install {exe}: {e}")))?;
    Ok(())
}

fn id(bytes: &[u8]) -> Result<BuildId, Broke> {
    BuildId::of(bytes).ok_or_else(|| Broke::Harness("a build id of 1 to 32 bytes".to_owned()))
}

/// One case: four binaries' entries, then a rebuild replaces the running one's file.
/// - `/sim/bin/blaze`, the host's own, entry by `write_own`, then its file replaced: Replaced.
/// - `/sim/bin/blaze-old`, installed, ran within the window: Live.
/// - `/sim/opt/blaze-gone`, ran within the window, its file never there: Gone.
/// - `/sim/bin/blaze-stale`, installed, last ran past the window: Stale.
///
/// Oracle: each entry's state is the one above, the rows are in the order of their names, and
/// the live ids are exactly blaze-old's.
fn roster_case(tape: &Tape, plant: RosterPlant) -> Result<(), Broke> {
    let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| Broke::Harness(format!("a root context: {e:?}")))?;
    let fs = io.need_fs().map_err(|e| Broke::Harness(format!("files: {e}")))?;
    let home: Dir<ReadWrite> = fs.home(&cx, HOME, &Path::new(".blaze").map_err(|e| Broke::Harness(format!("{e:?}")))?).map_err(|e| Broke::Harness(format!("the home: {e}")))?;
    let host = io.need_host().map_err(|e| Broke::Harness(format!("host: {e}")))?;
    let today = u32::try_from(tape.range(u64::from(WINDOW_DAYS) + 200..40_000, "today")).map_err(|_| Broke::Harness("a day".to_owned()))?;
    let recent = today - u32::try_from(tape.range(0..u64::from(WINDOW_DAYS) + 1, "blaze-old ran days ago")).unwrap_or(0);
    let old = today - WINDOW_DAYS - 1 - u32::try_from(tape.range(0..100, "blaze-stale ran past the window")).unwrap_or(0);

    let own_id: [u8; 16] = OWN_ID.try_into().map_err(|_| Broke::Harness("the simulated id is 16 bytes".to_owned()))?;
    install(&io, &cx, OWN, &own_id)?;
    install(&io, &cx, "/sim/bin/blaze-old", &[0xb0; 16])?;
    install(&io, &cx, "/sim/bin/blaze-stale", &[0xc0; 16])?;
    let wrote = |result: Result<(), super::RosterError>| result.map_err(|e| Broke::Harness(format!("a roster write: {e}")));
    wrote(write_own(&cx, &home, &host, today))?;
    // A second command the same day writes nothing new and leaves the entry as it was.
    wrote(write_own(&cx, &home, &host, today))?;
    wrote(write_entry(&cx, &home, "/sim/bin/blaze-old", &id(&[0xb0; 16])?, recent))?;
    wrote(write_entry(&cx, &home, "/sim/opt/blaze-gone", &id(&[0xd0; 16])?, recent))?;
    wrote(write_entry(&cx, &home, "/sim/bin/blaze-stale", &id(&[0xc0; 16])?, old))?;

    // Before the rebuild the running binary's entry lives.
    let before = live(&cx, &fs, &home, today, Alloc::global()).map_err(|e| Broke::Harness(format!("live: {e}")))?;
    let own_before = (0..before.len()).find(|&i| before.path(i) == OWN).ok_or_else(|| Broke::Oracle("the running binary has no entry".to_owned()))?;
    if before.row(own_before).state != State::Live {
        return Err(Broke::Oracle(format!("the running, installed binary is {:?}, not live", before.row(own_before).state)));
    }

    // A rebuild replaces the file at the running binary's path.
    install(&io, &cx, OWN, &[0xe0; 16])?;
    let after = live_with(&cx, &fs, &home, today, Alloc::global(), plant).map_err(|e| Broke::Harness(format!("live: {e}")))?;
    if after.len() != 4 || after.is_empty() {
        return Err(Broke::Oracle(format!("{} entries, not 4", after.len())));
    }
    for i in 0..after.len() {
        let want = match after.path(i) {
            OWN => State::Replaced,
            "/sim/bin/blaze-old" => State::Live,
            "/sim/opt/blaze-gone" => State::Gone,
            "/sim/bin/blaze-stale" => State::Stale,
            other => return Err(Broke::Oracle(format!("an entry for {other}, which no command wrote"))),
        };
        let got = after.row(i).state;
        if got != want {
            let says = if got.is_live() { "counted live" } else { "dead" };
            return Err(Broke::Oracle(format!("{}: {got:?} ({says}), not {want:?}", after.path(i))));
        }
        if i > 0 && after.row(i - 1).name >= after.row(i).name {
            return Err(Broke::Oracle("the rows are not in the order of their names".to_owned()));
        }
    }
    let live_ids: std::vec::Vec<&BuildId> = after.live_ids().collect();
    if live_ids != [&id(&[0xb0; 16])?] {
        return Err(Broke::Oracle(format!("live ids {live_ids:?}, not blaze-old's alone")));
    }
    Ok(())
}

/// The invariant: binary liveness is determined by build id and last-run window, never by path alone.
/// A rebuild that replaces the running binary's file makes its entry dead at once
/// (`Replaced`), while an installed second binary (`blaze-old`) that ran within the window
/// stays live; a missing binary is `Gone` and one past the window `Stale`.
///
/// Expected: `test roster::tests::a_replaced_binary_is_dead_and_an_installed_one_lives ... ok`
#[test]
fn a_replaced_binary_is_dead_and_an_installed_one_lives() {
    blaze_sim::check(Config::cases(64), |tape| roster_case(tape, RosterPlant::None));
}

/// The planted bug `RosterPlant::PathOnly` (liveness by a file at the path, its build id not
/// compared) counts the replaced binary live, and the oracle says so.
///
/// Expected: `test roster::tests::the_oracle_catches_liveness_by_path_only ... ok`
#[test]
fn the_oracle_catches_liveness_by_path_only() {
    blaze_sim::control(Config::cases(64), RosterPlant::None, RosterPlant::PathOnly, "counted live", roster_case);
}
