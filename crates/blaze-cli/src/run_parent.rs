//! `blaze run`'s parent (docs/v2/plan/design/run.md, "Pieces" item 6; bottom.md section 11):
//! the program runs in a process of its own, the launcher's run mode
//! (crates/blaze-launcher/src/main.rs `run_mode`), and this process answers its first calls
//! over the socket pair on its descriptor 3 with blaze-launch's run wire
//! (`blaze_launch::wire::run`: `Up::Need` in; `Down::Declare`, `Down::Publish`, `Down::Failed`
//! out), until it exits; its status is `blaze run`'s, unchanged.
//!
//! What is here, sans-IO and whole:
//!
//! - [`lay_out`]: the program's code records made by the run path's record walk
//!   (`blaze_ladder::run::build_records_new`, run.md piece 3), each record's bytes filed in the
//!   records folder the launcher reads (`blaze_launch_rt::run_mode::Folder`: one file a record,
//!   named by the 64 hex digits of its BLAKE3), and the index the parent answers from. The
//!   design's `Lookup`/`Compile` as drive `Code` jobs, one function at a time on first call,
//!   needs a compile of ONE instance from its symbol, which the tree does not have: the walk
//!   makes every instance the entry reaches (keyed in full, found again on a repeat run), so
//!   the "lookup" is the index and the "compile" ran before the start.
//! - [`Needs`]: one need answered. A symbol's first need is told its slot (`Declare`) before
//!   its record (`Publish`); a later need by that slot gets the record alone; a symbol the
//!   program's records do not hold is `Failed` with `Unresolved`, naming the need.
//! - [`status_of`]: how the child ended as `blaze run`'s status: an exit's status unchanged; an
//!   end by signal `n` is 128 + n, the shell's form (a Rust program's own status is never
//!   above 255, so it cannot be confused with an exit).
//! - [`launch_env`]: what the launcher's run mode reads from its environment: the command's
//!   own environment (LADDER.md L3), then `BLAZE_LAUNCH_FD=3`, `BLAZE_LAUNCH_STORE` and
//!   `BLAZE_LAUNCH_ENTRY`, which the launcher removes before the program's first instruction.
//!
//! NOT HERE, AND WHY (the driver loop: spawn, serve, wait). The tree has no surface for it that
//! blaze-cli may use (`#![forbid(unsafe_code)]`, and the system only through bstd):
//!
//! 1. bstd `Spawner::start_linked` gives a child descriptors to inherit, but nothing makes a
//!    socket pair, and nothing reads or writes a descriptor with a deadline: blaze-core's
//!    `wait::readable` takes `libc::pollfd`, and `socketpair` appears only in a test.
//! 2. blaze-core's one spawn path (`process::spawn::spawn_with`) always gives a linked child
//!    piped standard output and error (`Stdio::piped()`, spawn.rs), and bstd's `Linked` hands
//!    neither pipe to its caller: the program's output would fill a pipe nobody reads and the
//!    program would block on its next write. A run's program writes to the command's own
//!    standard output and error (`blaze run | consumer`).
//! 3. `argv[0]`: blaze-core's `Command` runs `program` with `argv[0]` = its path; L3's
//!    `argv[0]` = the binary target's name needs an `arg0` field.
//!
//! With those (a `bstd::procs::Channel`: a socket pair, one end given as descriptor 3, the
//! other read and written by deadline on the driver's clock; and `LinkedSpec::output:
//! Inherit`), the loop is: [`lay_out`]; `start_linked` with `own_group: false` (Ctrl-C reaches
//! the program as under any shell; a group of its own would keep the terminal's signals from
//! it), the launcher beside `Host::exe` as `program`, `launch.args` after it, [`launch_env`];
//! then until the child ends: wait up to [`TICK`] for the channel, hand what came to
//! [`Needs::serve`], write what it wrote by [`WRITE_BOUND`], and look at the child with a
//! passed deadline ([`status_of`]). A child cannot outlive the parent on any path that returns
//! or unwinds: bstd `Linked` dropped unreaped kills and reaps it; a parent ended by SIGKILL
//! closes the socket, and the child's next first call fails (`Down` never comes: the launcher's
//! `ASK_BOUND`), which ends it with its `STOPPED` (125).
//!
//! Every failure here comes before the program's first instruction and is a [`ParentError`],
//! which `blaze run` ends with `program::NOT_RUN` (125), never a status the program could have
//! returned.

use core::alloc::Allocator;
use core::fmt;

use blaze_emit::{Isa, Os};
use blaze_ladder::code_records::{Builder, CodeStore, EntryError, EntryView, MemoryStore, RecordRun, Settings, StoreError};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError, build_records_new};
use blaze_launch::wire::run::{Declared, Down, Failure, Needed, Symbol, Up};
use blaze_launch::wire::{List, RecordId, WireError, split};
use blaze_launch::{Slot, Tier};
use blaze_own::{AVec, Alloc, Vec};
use bstd::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, ReadWrite};
use bstd::path::{Path, PathError};
use bstd::procs::Ended;

use crate::program::ROOT;

/// How long the parent waits for the program's next message before it looks at the child
/// again (the launcher's own tick is 250 ms: `blaze_launch_rt::run_mode::TICK_MS`).
pub(crate) const TICK: core::time::Duration = core::time::Duration::from_millis(250);

/// How long a write of answers may wait for the program to read (rule 18): a program whose
/// control thread reads nothing for this long is stopped, named.
pub(crate) const WRITE_BOUND: core::time::Duration = core::time::Duration::from_secs(30);

/// The most slots a run numbers (`blaze_launch_rt::run_mode::MOST_SLOTS`, the launcher's
/// table): one past it is refused, never wrapped.
pub(crate) const MOST_SLOTS: u32 = 1 << 20;

/// The records folder beneath the home: content-named, so two runs of one program share it.
pub(crate) const RECORDS: &str = ".blaze/v2/run/records";

/// Why `blaze run` could not start the program in a process of its own. Each names the thing.
#[derive(Debug)]
pub enum ParentError {
    /// The record walk refused the program (the stage and the function, the ladder's words).
    Build(RunError),
    /// The store refused an instance's entry.
    Store { function: Box<str>, why: StoreError },
    /// The walk met an instance and its entry is not in the store.
    NotStored { function: Box<str> },
    /// An instance's entry does not read.
    Entry { function: Box<str>, why: EntryError },
    /// The walk met no instance: no entry to start.
    NoEntry,
    /// A symbol filed twice with two different records (rule 21).
    TwoRecords { symbol: [u8; 32] },
    /// Code records are AArch64 (blaze-emit): the run mode does not start on another machine.
    NotAarch64,
    /// A record's file name or the records folder.
    Path(PathError),
    /// Writing a record's file.
    Fs(FsError),
    /// The records folder beneath the home.
    Home(bstd::fs::HomeError),
    /// No `HOME` to put the records folder under.
    NoHome,
    /// A message of the run wire.
    Wire(WireError),
    /// The program named a slot this parent never declared (a launcher bug).
    UndeclaredSlot { slot: u32 },
    /// The run numbered [`MOST_SLOTS`] slots and the program asked for one more.
    Full,
}

impl fmt::Display for ParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "`blaze run` did not start the program (run.md piece 6)";
        match self {
            ParentError::Build(why) => write!(f, "{rule}: {why}"),
            ParentError::Store { function, why } => write!(f, "{rule}: the code record of `{function}`: {why}"),
            ParentError::NotStored { function } => write!(f, "{rule}: the record walk met `{function}` and its record is not in the store"),
            ParentError::Entry { function, why } => write!(f, "{rule}: the code record of `{function}` does not read: {why}"),
            ParentError::NoEntry => write!(f, "{rule}: the record walk met no function, so there is no entry to start"),
            ParentError::TwoRecords { symbol } => {
                write!(f, "{rule}: the symbol ")?;
                for byte in symbol {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, " has two different records (rule 21)")
            }
            ParentError::NotAarch64 => write!(f, "{rule}: code records are AArch64 and this machine is not"),
            ParentError::Path(why) => write!(f, "{rule}: a record's path: {why}"),
            ParentError::Fs(why) => write!(f, "{rule}: writing a code record: {why}"),
            ParentError::Home(why) => write!(f, "{rule}: the records folder `{RECORDS}`: {why}"),
            ParentError::NoHome => write!(f, "{rule}: `HOME` is not set, and the records folder `{RECORDS}` is beneath it"),
            ParentError::Wire(why) => write!(f, "{rule}: the program's run wire: {why}"),
            ParentError::UndeclaredSlot { slot } => write!(f, "{rule}: the program asked for slot {slot}, which this parent never declared"),
            ParentError::Full => write!(f, "{rule}: the program asked for more than {MOST_SLOTS} functions"),
        }
    }
}

impl core::error::Error for ParentError {}

impl From<WireError> for ParentError {
    fn from(why: WireError) -> ParentError {
        ParentError::Wire(why)
    }
}

/// The machine the records are made for: this one (the program runs on it).
fn isa() -> Result<Isa, ParentError> {
    if !cfg!(target_arch = "aarch64") {
        return Err(ParentError::NotAarch64);
    }
    Ok(Isa {
        lse: true,
        os: match cfg!(target_os = "macos") {
            true => Os::MacOs,
            false => Os::Linux,
        },
    })
}

/// 32 bytes as 64 lowercase hex digits, the form the launcher reads.
pub(crate) fn hex(bytes: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (index, byte) in bytes.iter().enumerate() {
        out[index * 2] = DIGITS[usize::from(byte >> 4)];
        out[index * 2 + 1] = DIGITS[usize::from(byte & 0xf)];
    }
    out
}

/// What [`lay_out`] made: the answers' index, and the entry's symbol.
pub(crate) struct Laid<'a> {
    pub needs: Needs<'a>,
    /// The entry's symbol: the first instance the walk met (it walks from the entry forward).
    pub entry: [u8; 32],
}

/// The program's records, made by the record walk and filed in `folder` (a file a record,
/// named by its BLAKE3's hex; a file already there holds equal bytes and is not written again),
/// and the index to answer its needs from.
pub(crate) fn lay_out<'a>(cx: &Cx, files: &[SourceFile<'_>], new: NewFront<'_>, folder: &Dir<ReadWrite>, alloc: Alloc<'a>) -> Result<Laid<'a>, ParentError> {
    let builder = Builder { isa: isa()?, tier: blaze_run::Tier::Fast, settings: Settings::baseline(new.overflow), toolchain: blaze_toolchain::PINNED };
    let mut store = MemoryStore::new_in(alloc);
    let met = {
        let mut run = RecordRun::new(&mut store, builder);
        build_records_new(files, ROOT, new, &mut run).map_err(ParentError::Build)?;
        core::mem::take(&mut run.met)
    };
    let entry = met.first().map(|first| first.symbol.0).ok_or(ParentError::NoEntry)?;
    let mut index: AVec<'a, ([u8; 32], RecordId)> = Vec::new_in(alloc);
    for each in met.iter() {
        let function = || each.function.clone(); // clone: an error's name
        let bytes = store.get(each.key).map_err(|why| ParentError::Store { function: function(), why })?.ok_or_else(|| ParentError::NotStored { function: function() })?;
        let view = EntryView::read(bytes).map_err(|why| ParentError::Entry { function: function(), why })?;
        let record = view.record.bytes();
        let id = blaze_blake3::hash(record);
        let name = hex(&id);
        // The digits are ASCII: always UTF-8.
        let name = core::str::from_utf8(&name).map_err(|_| ParentError::NoEntry)?;
        let path = Path::new(name).map_err(ParentError::Path)?;
        if folder.stat(cx, &path).map_err(ParentError::Fs)?.is_none() {
            folder.publish(cx, &path, Content::bytes(record), Durable::No).map_err(ParentError::Fs)?;
        }
        index.push((each.symbol.0, RecordId(id)));
    }
    Ok(Laid { needs: Needs::new(index, alloc)?, entry })
}

/// The launcher run mode's environment: the command's own (`env`, each pair in the world's
/// order), then the three names the launcher reads, each set once (a pair of the command's
/// own under one of those names is left out: the parent's word is the run's).
pub(crate) fn launch_env<'e, A: Allocator>(env: impl Iterator<Item = (&'e str, &'e str)>, store: &'e str, entry: &'e str, out: &mut Vec<(&'e str, &'e str), A>) {
    const OURS: [&str; 3] = ["BLAZE_LAUNCH_FD", "BLAZE_LAUNCH_STORE", "BLAZE_LAUNCH_ENTRY"];
    for (name, value) in env {
        if !OURS.contains(&name) {
            out.push((name, value));
        }
    }
    out.push(("BLAZE_LAUNCH_FD", "3"));
    out.push(("BLAZE_LAUNCH_STORE", store));
    out.push(("BLAZE_LAUNCH_ENTRY", entry));
}

/// `BLAZE_LAUNCH_ENTRY` for `symbol`: `unit:<hex>`, the form today's entry has
/// (blaze-ladder run.rs `entry_shape`: `fn main()` called directly). LADDER.md L1 makes a
/// Rust program's entry the C `main` shim through `lang_start`, whose form is `c-main:`; this
/// is the one place that changes then.
pub(crate) fn entry_line(symbol: &[u8; 32]) -> [u8; 69] {
    let mut out = [0u8; 69];
    out[..5].copy_from_slice(b"unit:");
    out[5..].copy_from_slice(&hex(symbol));
    out
}

/// How the child ended, as `blaze run`'s status; `None` while it runs.
pub(crate) fn status_of(ended: Ended) -> Option<u8> {
    match ended {
        // A process's exit status is its low 8 bits (`_exit`): what the kernel reports.
        Ended::Exited(status) => Some((status & 0xff) as u8),
        // The shell's form for an end by signal: 128 + its number.
        Ended::Signaled(signal) => Some(128u8.saturating_add((signal & 0x7f) as u8)),
        Ended::Deadline => None,
    }
}

/// The parent's answers: each symbol's record, and the slots it numbered.
pub(crate) struct Needs<'a> {
    /// (symbol, record), sorted by symbol, each symbol once.
    index: AVec<'a, ([u8; 32], RecordId)>,
    /// (symbol, slot) of every slot declared, sorted by symbol.
    declared: AVec<'a, ([u8; 32], u32)>,
    /// Each declared slot's symbol, by slot: slots are dense, in the order needs came.
    slots: AVec<'a, [u8; 32]>,
    /// The next message's id.
    next_id: u64,
}

impl<'a> Needs<'a> {
    /// `index` sorted; a symbol twice with one record is kept once, with two is refused.
    pub(crate) fn new(mut index: AVec<'a, ([u8; 32], RecordId)>, alloc: Alloc<'a>) -> Result<Needs<'a>, ParentError> {
        index.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
        let mut kept: AVec<'a, ([u8; 32], RecordId)> = Vec::with_capacity_in(index.len(), alloc);
        for row in index.iter() {
            match kept.last() {
                Some(last) if last.0 == row.0 && last.1 == row.1 => {}
                Some(last) if last.0 == row.0 => return Err(ParentError::TwoRecords { symbol: row.0 }),
                _ => kept.push(*row),
            }
        }
        Ok(Needs { index: kept, declared: Vec::new_in(alloc), slots: Vec::new_in(alloc), next_id: 0 })
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Every whole frame at the start of `inbox` answered onto the end of `out`: the bytes
    /// taken (the rest is a frame not all here yet). Each frame takes at least its 4-byte
    /// length, so the loop ends.
    pub(crate) fn serve<A: Allocator>(&mut self, inbox: &[u8], out: &mut std::vec::Vec<u8, A>) -> Result<usize, ParentError> {
        let mut taken = 0;
        while let Some((item, len)) = split(&inbox[taken..])? {
            let (_, up) = Up::decode(item)?;
            match up {
                Up::Need { need, .. } => self.answer(need, out)?,
            }
            taken += len;
        }
        Ok(taken)
    }

    /// One need answered onto the end of `out`.
    pub(crate) fn answer<A: Allocator>(&mut self, need: Needed, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        let symbol = match need {
            Needed::Symbol(symbol) => symbol.0,
            Needed::Slot(slot) => *self.slots.get(slot.0 as usize).ok_or(ParentError::UndeclaredSlot { slot: slot.0 })?,
        };
        let Ok(at) = self.index.binary_search_by(|row| row.0.cmp(&symbol)) else {
            let id = self.id();
            Down::Failed { need, why: Failure::Unresolved }.encode(id, out)?;
            return Ok(());
        };
        let record = self.index[at].1;
        let slot = match self.declared.binary_search_by(|row| row.0.cmp(&symbol)) {
            Ok(found) => self.declared[found].1,
            Err(place) => {
                let slot = u32::try_from(self.slots.len()).map_err(|_| ParentError::Full)?;
                if slot >= MOST_SLOTS {
                    return Err(ParentError::Full);
                }
                let pairs = [Declared { symbol: Symbol(symbol), slot: Slot(slot) }];
                let id = self.id();
                Down::Declare { pairs: List::of(&pairs) }.encode(id, out)?;
                self.slots.push(symbol);
                self.declared.insert_shift(place, (symbol, slot));
                slot
            }
        };
        let id = self.id();
        Down::Publish { slot: Slot(slot), tier: Tier::Zero, record }.encode(id, out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Rust's rule: a program's `main` runs in the program's own process, which ends with its
    //! status (the Reference, "Crates and source files": "the main function"; the library's
    //! process.rs: "`exit` ... the exit code is passed through to the underlying OS"). The
    //! parent's part is the code it hands that process: a function's slot is told once before
    //! its record, every later call of it gets the same slot, and a symbol with no record is
    //! refused by name, never answered with another's code.

    use super::*;

    fn frames(bytes: &[u8]) -> std::vec::Vec<std::string::String> {
        let mut said = std::vec::Vec::new();
        let mut at = 0;
        while let Some((item, len)) = split(&bytes[at..]).expect("a frame") {
            let (_, down) = Down::decode(item).expect("a run wire message");
            said.push(match down {
                Down::Declare { pairs } => {
                    let pairs: std::vec::Vec<_> = pairs.iter().map(|it| (it.symbol.0[0], it.slot.0)).collect();
                    std::format!("Declare {pairs:?}")
                }
                Down::Publish { slot, record, .. } => std::format!("Publish {} {}", slot.0, record.0[0]),
                Down::Failed { why, .. } => std::format!("Failed {why:?}"),
                Down::Personality { slot } => std::format!("Personality {}", slot.0),
                Down::Native { library, words } => std::format!("Native {} {}", library.0[0], words.len()),
            });
            at += len;
        }
        said
    }

    fn need(need: Needed) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::new();
        Up::Need { need, thread: blaze_launch::Thread(1) }.encode(7, &mut out).expect("a need encodes");
        out
    }

    /// Prints "run_parent: declared once, slot 0 published twice, unknown symbol Unresolved".
    /// A parent that declared on every need, numbered a second slot for one symbol, or answered
    /// an unknown symbol with a record fails it.
    #[test]
    fn a_symbol_is_declared_once_and_an_unknown_one_refused() {
        let alloc = Alloc::global();
        let mut index = Vec::new_in(alloc);
        index.push(([2u8; 32], RecordId([20; 32])));
        index.push(([1u8; 32], RecordId([10; 32])));
        let mut needs = Needs::new(index, alloc).expect("one record a symbol");
        let mut inbox = need(Needed::Symbol(Symbol([1; 32])));
        inbox.extend_from_slice(&need(Needed::Slot(Slot(0))));
        inbox.extend_from_slice(&need(Needed::Symbol(Symbol([9; 32]))));
        let half = need(Needed::Symbol(Symbol([2; 32])));
        inbox.extend_from_slice(&half[..3]);
        let mut out = std::vec::Vec::new();
        let taken = needs.serve(&inbox, &mut out).expect("served");
        assert_eq!(taken, inbox.len() - 3, "a frame not all here is left for the next read");
        assert_eq!(frames(&out), ["Declare [(1, 0)]", "Publish 0 10", "Publish 0 10", "Failed Unresolved"]);
        let mut two = Vec::new_in(alloc);
        two.push(([1u8; 32], RecordId([10; 32])));
        two.push(([1u8; 32], RecordId([11; 32])));
        assert!(matches!(Needs::new(two, alloc), Err(ParentError::TwoRecords { .. })), "two records under one symbol are refused");
        assert_eq!(status_of(Ended::Exited(3)), Some(3));
        assert_eq!(status_of(Ended::Signaled(9)), Some(137));
        assert_eq!(status_of(Ended::Deadline), None);
        assert_eq!(&entry_line(&[0xab; 32])[..7], b"unit:ab");
        std::println!("run_parent: declared once, slot 0 published twice, unknown symbol Unresolved");
    }
}
