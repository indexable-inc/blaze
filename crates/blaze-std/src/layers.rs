//! The layers a run is held in besides record and replay (run.md, "Dry, ask and fake"):
//! `dry`, `ask` and the `fake` providers. Each is a sans-IO state machine over the effect
//! stream: it is told an effect and says what to do with it, and never performs one. The
//! host's seam ([`crate::hosting::Hosting`]) is the one driver today, the same seam record and
//! replay sit at, so the program does not change.
//!
//! **What changes the world** is one rule, [`changes`], written over every [`Effect`] (a new
//! effect does not compile until it is placed): an effect changes the world when it leaves
//! something outside the run that the run did not have before, or takes something away: a
//! file's bytes, names, modes, times or durability, a directory, a lock, a process, a
//! listening or connected socket, bytes sent. Reading, listing, watching, the clocks, timers,
//! randomness and name lookups do not. Standard output and error do not: they are the run's
//! report to the person who started it, so a dry run still says what the program says, and
//! `ask` never asks about a line of output.
//!
//! - **Dry**: every changing effect is withheld (reported in [`Gate::withheld`], not
//!   performed) and answered with the completion it has when it succeeds ([`performed`]), so
//!   the program goes on as it would. Reads are real: a read after a withheld write sees the
//!   world as it is, not as the write would have left it (the overlay that would show it is
//!   owed: run.md).
//! - **Ask**: every changing effect waits for a [`Decision`] from the host's [`Decide`]. `Yes`
//!   performs it, so a run answered `Yes` throughout is the plain run. `No` refuses it: it is
//!   not performed and the program gets the wire's typed refusal (`Refusal::Refused`, the
//!   shape an ungranted capability gives), never a success, so a person who says no to
//!   "remove this file?" never leaves the program believing it is gone. `Pretend` withholds it
//!   as dry does, so a run answered `Pretend` throughout is the dry run.
//! - **Fake**: a capability's provider is a stand-in ([`Fakes`]): a folder in memory
//!   ([`Folder`]), a clock fixed at one time, or the escaped effects of one capability answered
//!   from a record ([`Recorded`]). Layers sit outside the provider: a dry run of a fake folder
//!   leaves the folder as it was.

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};

use blaze_core::{Done, Effect, FileName, IoError};

use crate::hosting::Called;

/// Whether `effect` changes the world (the module's rule). Every effect is placed here by
/// name, so a new one is placed before it compiles.
pub fn changes(effect: &Effect) -> bool {
    match effect {
        Effect::ReadAll { .. }
        | Effect::Read { .. }
        | Effect::ReadInto { .. }
        | Effect::ReadMany { .. }
        | Effect::ReadLink { .. }
        | Effect::List { .. }
        | Effect::Private { .. }
        | Effect::Prewarm { .. }
        | Effect::Stat { .. }
        | Effect::Lstat { .. }
        | Effect::Watch { .. }
        | Effect::Journal { .. }
        | Effect::Unwatch { .. }
        | Effect::Stamp { .. }
        | Effect::Entries { .. }
        | Effect::Timer { .. }
        | Effect::Map { .. }
        | Effect::Recv { .. }
        | Effect::Pressure { .. }
        | Effect::RecvFrom { .. }
        | Effect::Resolve { .. }
        | Effect::ReadStdin { .. }
        | Effect::WriteOut { .. }
        | Effect::Now { .. }
        | Effect::Random { .. }
        | Effect::Roots { .. }
        | Effect::Ancestors { .. }
        | Effect::Above { .. } => false,
        Effect::WriteFrom { .. }
        | Effect::Append { .. }
        | Effect::Sync { .. }
        | Effect::Barrier { .. }
        | Effect::WriteBack { .. }
        | Effect::SyncDir { .. }
        | Effect::Truncate { .. }
        | Effect::MapWrite { .. }
        | Effect::Write { .. }
        | Effect::WriteUnsynced { .. }
        | Effect::WriteAt { .. }
        | Effect::Rename { .. }
        | Effect::RenameUnsynced { .. }
        | Effect::Mode { .. }
        | Effect::Seal { .. }
        | Effect::Times { .. }
        | Effect::Clone { .. }
        | Effect::CopyData { .. }
        | Effect::Link { .. }
        | Effect::Remove { .. }
        // A lock makes its file when it is missing, and holds it against other processes.
        | Effect::Lock { .. }
        | Effect::Unlock { .. }
        | Effect::Flush { .. }
        | Effect::Spawn { .. }
        | Effect::Kill { .. }
        | Effect::Listen { .. }
        | Effect::Unlisten { .. }
        | Effect::Connect { .. }
        | Effect::Send { .. }
        | Effect::Hangup { .. }
        | Effect::Bind { .. }
        | Effect::SendTo { .. }
        | Effect::Unbind { .. }
        | Effect::MakeDir { .. }
        | Effect::RemoveDir { .. } => true,
    }
}

/// The kind of an effect the hosted wire carries, as a person reads it: the first half of
/// what `ask` asks and keeps.
pub fn kind(effect: &Effect) -> &'static str {
    match effect {
        Effect::Timer { .. } => "timer",
        Effect::Random { .. } => "random",
        Effect::Roots { .. } => "roots",
        Effect::ReadAll { .. } => "read",
        Effect::Write { .. } => "write",
        Effect::Append { .. } => "append",
        Effect::Remove { .. } => "remove",
        Effect::Rename { .. } => "rename",
        Effect::MakeDir { .. } => "make dir",
        _ => "an effect the hosted wire does not carry",
    }
}

/// The capability an effect the hosted wire carries is a reach of: what a fake stands in for.
pub fn capability(effect: &Effect) -> &'static str {
    match effect {
        Effect::Timer { .. } | Effect::Now { .. } => "clock",
        Effect::Random { .. } => "rng",
        Effect::ReadAll { .. } | Effect::Write { .. } | Effect::Append { .. } | Effect::Remove { .. } | Effect::Rename { .. } | Effect::MakeDir { .. } => "fs",
        _ => "an effect the hosted wire does not carry",
    }
}

/// What an effect reaches, as a person reads it: the second half of what `ask` asks and keeps.
pub fn scope(effect: &Effect) -> String {
    match effect {
        Effect::ReadAll { file, .. } | Effect::Write { file, .. } | Effect::Append { file, .. } | Effect::Remove { file, .. } => String::from(file.as_str()),
        Effect::Rename { from, to, .. } => alloc::format!("{from} -> {to}"),
        Effect::MakeDir { dir, .. } => String::from(dir.as_str()),
        _ => String::new(),
    }
}

/// The completion a changing effect has when it succeeds: what a withheld one is answered with.
/// An effect whose success carries what only the world could say (a spawned process, a
/// connection) cannot be made up, and fails, naming why.
pub fn performed(effect: &Effect) -> Done {
    match effect {
        Effect::Write { .. } | Effect::WriteUnsynced { .. } => Done::Written,
        Effect::Append { .. } => Done::Appended,
        Effect::Remove { .. } => Done::Removed,
        Effect::Rename { .. } | Effect::RenameUnsynced { .. } => Done::Renamed,
        Effect::MakeDir { .. } => Done::MadeDir,
        Effect::RemoveDir { .. } => Done::RemovedDir,
        other => Done::Failed(IoError(alloc::format!(
            "bstd: `{}` was withheld (not performed), and what it would answer cannot be made up",
            kind(other)
        ))),
    }
}

/// Why a changing effect was not performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    /// `--dry`: the program was told it succeeded.
    Dry,
    /// `--ask`, answered `Pretend`: the program was told it succeeded.
    Pretended,
    /// `--ask`, answered `No`: the program got a typed refusal.
    Refused,
}

/// A changing effect not performed: the `at`th call the program made, the effect, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Withheld {
    pub at: usize,
    pub effect: Effect,
    pub by: By,
}

/// What `ask` asks: the `at`th call the program made would perform `effect`.
pub struct Question<'a> {
    pub at: usize,
    pub kind: &'static str,
    pub scope: &'a str,
    pub effect: &'a Effect,
}

/// The answer to a [`Question`]: exactly these three, and a prompt offers no default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Perform it.
    Yes,
    /// Do not perform it; the program gets a typed refusal and the report says so.
    No,
    /// Do not perform it, and tell the program it succeeded, as `--dry` does.
    Pretend,
}

/// What answers `ask`'s questions: the terminal in the CLI, a script in a test. It is called
/// on the program's thread, inside the call that would perform the effect, and the program
/// waits for it.
pub trait Decide {
    fn decide(&mut self, question: &Question<'_>) -> Decision;
}

/// What a [`Gate`] says of one changing effect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    Perform,
    /// Not performed; the program is told it succeeded ([`performed`]).
    Pretend,
    /// Not performed; the program gets the wire's typed refusal.
    Refuse,
}

/// The `dry` and `ask` layers, as one state machine: told each changing effect, it says
/// whether it is performed, and keeps what it did not perform and which reads may differ.
pub struct Gate {
    dry: bool,
    ask: Option<Box<dyn Decide + Send>>,
    withheld: Vec<Withheld>,
    /// The names a pretended effect would have changed: a later read of one may differ from
    /// a real run's, as there is no overlay yet.
    pretended: Vec<FileName>,
    stale_reads: usize,
    /// A test's control: under dry, the first changing effect is let through.
    planted_lets_through: bool,
    /// A test's control: under ask, the first changing effect is performed unasked.
    planted_unasked: bool,
    /// A test's control: a `No` is answered as performed.
    planted_no_says_ok: bool,
    seen: usize,
}

/// What a dry or asked run did not perform, for the person: [`Gate::report`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    pub withheld: Vec<Withheld>,
    /// Reads, after a pretended effect, of a name it would have changed: these may differ
    /// from a real run, which would have seen the change.
    pub stale_reads: usize,
}

impl core::fmt::Display for Report {
    /// The first line says how many later reads may differ, when any may; then one line per
    /// effect not performed, in order.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.stale_reads > 0 {
            writeln!(f, "{} later reads may differ from a real run", self.stale_reads)?;
        }
        for withheld in &self.withheld {
            let how = match withheld.by {
                By::Dry => "not performed (dry)",
                By::Pretended => "not performed (pretended)",
                By::Refused => "refused",
            };
            let scope = scope(&withheld.effect);
            writeln!(f, "call {}: {} {scope}: {how}", withheld.at, kind(&withheld.effect))?;
        }
        Ok(())
    }
}

/// The names `effect` would change.
fn changed_names(effect: &Effect) -> Vec<FileName> {
    match effect {
        // clone: names, refcounts.
        Effect::Write { file, .. } | Effect::Append { file, .. } | Effect::Remove { file, .. } => alloc::vec![file.clone()],
        Effect::Rename { from, to, .. } => alloc::vec![from.clone(), to.clone()],
        Effect::MakeDir { dir, .. } => alloc::vec![dir.clone()],
        _ => Vec::new(),
    }
}

impl Gate {
    /// The gate of a run: `dry` withholds every changing effect (no question is asked then);
    /// else `ask`, when given, decides each.
    pub fn new(dry: bool, ask: Option<Box<dyn Decide + Send>>) -> Gate {
        Gate {
            dry,
            ask,
            withheld: Vec::new(),
            pretended: Vec::new(),
            stale_reads: 0,
            planted_lets_through: false,
            planted_unasked: false,
            planted_no_says_ok: false,
            seen: 0,
        }
    }

    /// `effect`, which changes nothing, is about to be performed: a read of a name a pretended
    /// effect would have changed is counted, as it may differ from a real run's.
    pub fn read(&mut self, effect: &Effect) {
        if let Effect::ReadAll { file, .. } = effect
            && self.pretended.contains(file)
        {
            self.stale_reads += 1;
        }
    }

    /// What was not performed, and how many reads may differ.
    pub fn report(&self) -> Report {
        // clone: a report.
        Report { withheld: self.withheld.clone(), stale_reads: self.stale_reads }
    }

    /// `effect`, the `at`th call's, is not performed, for `by`.
    fn hold_back(&mut self, at: usize, effect: &Effect, by: By) {
        if by != By::Refused {
            self.pretended.extend(changed_names(effect));
        }
        // clone: the report's copy of the effect.
        self.withheld.push(Withheld { at, effect: effect.clone(), by });
    }

    /// Whether this gate holds anything back: a gate that never asks or withholds passes
    /// every effect, and the host skips it.
    pub fn is_open(&self) -> bool {
        !self.dry && self.ask.is_none()
    }

    /// What becomes of `effect`, the `at`th call's, which changes the world ([`changes`]).
    pub fn pass(&mut self, at: usize, effect: &Effect) -> Pass {
        self.seen += 1;
        let first = self.seen == 1;
        if self.dry {
            if self.planted_lets_through && first {
                return Pass::Perform;
            }
            self.hold_back(at, effect, By::Dry);
            return Pass::Pretend;
        }
        let Some(decide) = &mut self.ask else { return Pass::Perform };
        if self.planted_unasked && first {
            return Pass::Perform;
        }
        let scope = scope(effect);
        match decide.decide(&Question { at, kind: kind(effect), scope: &scope, effect }) {
            Decision::Yes => Pass::Perform,
            Decision::No => {
                self.hold_back(at, effect, By::Refused);
                if self.planted_no_says_ok { Pass::Pretend } else { Pass::Refuse }
            }
            Decision::Pretend => {
                self.hold_back(at, effect, By::Pretended);
                Pass::Pretend
            }
        }
    }

    /// Every effect not performed so far, in the order the program made them.
    pub fn withheld(&self) -> &[Withheld] {
        &self.withheld
    }

    #[cfg(test)]
    pub(crate) fn plant_no_says_ok(&mut self) {
        self.planted_no_says_ok = true;
    }

    #[cfg(test)]
    pub(crate) fn plant_lets_through(&mut self) {
        self.planted_lets_through = true;
    }

    #[cfg(test)]
    pub(crate) fn plant_unasked(&mut self) {
        self.planted_unasked = true;
    }
}

/// The stand-in providers of a run (`--fake name=provider`): each answers its capability's
/// calls in place of the world. A capability with none is the world's.
#[derive(Default)]
pub struct Fakes {
    /// `--fake clock=fixed:NS`: both clocks read `NS` for the whole run, and a timer fires at
    /// once (time does not pass).
    pub clock: Option<u64>,
    /// `--fake fs=memory[:DIR]`: the file effects answered from a folder in memory, which the
    /// CLI fills from `DIR` at the start.
    pub files: Option<Folder>,
    /// `--fake CAP=recorded:FILE`: the escaped effects of one capability answered from a
    /// record (`--record`'s file).
    pub recorded: Option<Recorded>,
}

/// One entry of a [`Folder`].
#[derive(Clone, PartialEq, Eq, Debug)]
enum Entry {
    File(Vec<u8>),
    Dir,
}

/// A folder in memory, standing in for the file tree: the hosted wire's file effects
/// answered as a driver answers them, and nothing else touched.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Folder {
    entries: BTreeMap<FileName, Entry>,
}

impl Folder {
    /// An empty folder.
    pub fn new() -> Folder {
        Folder::default()
    }

    /// The folder with `bytes` at `name`, and the directories above it.
    pub fn with_file(mut self, name: FileName, bytes: Vec<u8>) -> Folder {
        self.make_parents(&name);
        self.entries.insert(name, Entry::File(bytes));
        self
    }

    /// The bytes of the file `name`, if one is there.
    pub fn file(&self, name: &FileName) -> Option<&[u8]> {
        match self.entries.get(name) {
            Some(Entry::File(bytes)) => Some(bytes),
            Some(Entry::Dir) | None => None,
        }
    }

    /// Every directory above `name`, made.
    fn make_parents(&mut self, name: &FileName) {
        let text = name.as_str();
        let mut at = 0;
        while let Some(slash) = text[at..].find('/') {
            if let Some(dir) = FileName::new(&text[..at + slash]) {
                self.entries.entry(dir).or_insert(Entry::Dir);
            }
            at += slash + 1;
        }
    }

    /// Whether the directory `name` would be in is there: the root, or a directory.
    fn parent_is_there(&self, name: &FileName) -> bool {
        match name.as_str().rfind('/') {
            None => true,
            Some(slash) => FileName::new(&name.as_str()[..slash]).is_some_and(|dir| self.entries.get(&dir) == Some(&Entry::Dir)),
        }
    }

    /// `effect` answered from the folder; `None` for an effect that is not a file's.
    pub fn answer(&mut self, effect: &Effect) -> Option<Done> {
        let failed = |why: String| Some(Done::Failed(IoError(why)));
        match effect {
            Effect::ReadAll { file, .. } => match self.entries.get(file) {
                Some(Entry::File(bytes)) => Some(Done::Read(bytes.clone())),
                // A file that is not there reads as empty (`Effect::ReadAll`).
                None => Some(Done::Read(Vec::new())),
                Some(Entry::Dir) => failed(alloc::format!("bstd fake fs: `{file}` is a directory")),
            },
            Effect::Write { file, bytes, .. } | Effect::Append { file, bytes, .. } => {
                if self.entries.get(file) == Some(&Entry::Dir) {
                    return failed(alloc::format!("bstd fake fs: `{file}` is a directory"));
                }
                self.make_parents(file);
                let appends = matches!(effect, Effect::Append { .. });
                // clone: the name is the folder's key, a refcount.
                let held = self.entries.entry(file.clone()).or_insert(Entry::File(Vec::new()));
                if let Entry::File(held) = held {
                    if !appends {
                        held.clear();
                    }
                    held.extend_from_slice(bytes);
                }
                Some(if appends { Done::Appended } else { Done::Written })
            }
            Effect::Remove { file, .. } => match self.entries.get(file) {
                Some(Entry::File(_)) => {
                    self.entries.remove(file);
                    Some(Done::Removed)
                }
                Some(Entry::Dir) => failed(alloc::format!("bstd fake fs: `{file}` is a directory")),
                None => failed(alloc::format!("bstd fake fs: no file `{file}` to remove")),
            },
            Effect::Rename { from, to, .. } => match self.entries.get(from) {
                Some(Entry::File(_)) if self.parent_is_there(to) && self.entries.get(to) != Some(&Entry::Dir) => {
                    let moved = self.entries.remove(from);
                    if let Some(moved) = moved {
                        // clone: the name is the folder's key, a refcount.
                        self.entries.insert(to.clone(), moved);
                    }
                    Some(Done::Renamed)
                }
                Some(Entry::File(_)) => failed(alloc::format!("bstd fake fs: `{to}` cannot be renamed onto (its directory is not there, or it is one)")),
                Some(Entry::Dir) | None => failed(alloc::format!("bstd fake fs: no file `{from}` to rename")),
            },
            Effect::MakeDir { dir, .. } => {
                if self.entries.contains_key(dir) {
                    return failed(alloc::format!("bstd fake fs: `{dir}` is there already"));
                }
                if !self.parent_is_there(dir) {
                    return failed(alloc::format!("bstd fake fs: the directory above `{dir}` is not there"));
                }
                // clone: the name is the folder's key, a refcount.
                self.entries.insert(dir.clone(), Entry::Dir);
                Some(Done::MadeDir)
            }
            _ => None,
        }
    }
}

/// The escaped effects of one capability answered from a record: each such call is answered
/// with the record's next call of that capability whose bytes are the same, in order. A call
/// the record has no such answer for is refused, naming it in [`Recorded::unanswered`].
pub struct Recorded {
    capability: &'static str,
    calls: Vec<Called>,
    next: usize,
    unanswered: Vec<Vec<u8>>,
}

impl Recorded {
    /// `capability`'s calls of `record` (a capability [`capability`] names: `rng`, `clock`,
    /// `fs`; the network's when the wire carries it).
    pub fn new(capability: &'static str, record: crate::hosting::Record) -> Recorded {
        Recorded { capability, calls: record.calls, next: 0, unanswered: Vec::new() }
    }

    /// The capability it stands in for.
    pub fn capability(&self) -> &'static str {
        self.capability
    }

    /// The record's answer to the escaped effect `bytes`: its code, value and bytes.
    pub fn answer(&mut self, bytes: &[u8]) -> Option<(u32, u64, Vec<u8>)> {
        let mut at = self.next;
        while at < self.calls.len() {
            let called = &self.calls[at];
            if let crate::hosting::Call::Effect { bytes: recorded, .. } = &called.call
                && recorded == bytes
            {
                self.next = at + 1;
                // clone: the answer's bytes, put in the exchange.
                return Some((called.code, called.value, called.back.clone()));
            }
            at += 1;
        }
        self.unanswered.push(bytes.to_vec());
        None
    }

    /// The escaped effects the record had no answer for, in order.
    pub fn unanswered(&self) -> &[Vec<u8>] {
        &self.unanswered
    }
}
