//! [`Program`]: what the daemon keeps of one program its launchers run, and what it tells
//! each of them.
//!
//! A sans-IO core. The daemon's driver (blaze-cargo) gives it each build of the program
//! ([`Program::built`]) and each message a launcher sent ([`Program::said`]); it hands back
//! frames for the driver to write to that launcher's socket, in the order given. It reads no
//! clock and owns no socket, so its protocol runs in a simulation (`tests`).
//!
//! # Slots
//!
//! A function's slot is its *identity*'s (`blaze_codegen::backend::Described::identity`:
//! the item, which instance of it, its type arguments), which an edit of its body keeps. A
//! slot is given once and never taken back: an identity new to a build gets the next one, and
//! a launcher that runs already is told to declare it (`Edit::declare`).
//!
//! # What a launcher has
//!
//! For each launcher: the record each of its slots is bound to. A slot is bound when its
//! stub fires ([`wire::Ask::Need`]) and bound again when a build changes the record of the
//! function it names. Everything said to a launcher is computed from the newest build at the
//! moment it is said and written in order on one socket, which the launcher reads in order:
//! so whatever was in flight when a build landed, the last thing a launcher is told of a
//! slot is the newest build's record of it.
//!
//! **The invariant** (`tests`): once a launcher has carried out everything it was sent, each
//! of its bound slots holds the record the newest build makes of that slot's function, or
//! the launcher was told to start again.
//!
//! # A build: live, or a restart
//!
//! [`Program::built`] classifies what changed since the build a launcher is on:
//!
//! | what changed | the launcher is told |
//! |---|---|
//! | one function's body (every instance of one generic counts as one) | an edit: the bound slots whose records changed, switched together |
//! | lines only (a comment above a function) | an edit of the bound functions whose positions moved, not gated: each means what it meant |
//! | a function or static was added | its slot declared, or its static defined; it is bound when first called |
//! | a function or static was removed | nothing: no code names it any more |
//! | a static's value or type | restart needed, naming the static |
//! | two or more functions' bodies at once | restart needed, naming them: a changed signature changes its callers' code with it and cannot yet be told from two body edits, and a frame of an old caller would call the new callee the old way |
//! | the entry | restart needed |
//!
//! "One body changed" is sound because a function's code hash takes its callees by their
//! interface: were the changed function's signature another, its callers' code would have
//! changed too.

use std::alloc::Global;
use std::sync::Arc;

use blaze_codegen::driver::Built;
use blaze_codegen::{Arch, CompiledFunction, InstanceKey, StaticId};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::CodeHash;
use blaze_launch::wire::{self, Ask, List, RecordId, Tell};
use blaze_launch::{CodeId, Slot, Tier, Txn};
use blaze_link::Position;

use crate::record::{self, Made, Named, Positions};

/// One launcher's connection, as the driver names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Conn(pub u64);

/// The span tables a build's code names, as the driver has them.
pub enum Tables<'a> {
    /// Every table, by anchor.
    Whole(&'a [(ContentId, Vec<u8>)]),
    /// What differs from the tables given last: those new or changed, and the anchors gone.
    Changed { tables: &'a [(ContentId, &'a [u8])], removed: &'a [ContentId] },
}

/// The program's span tables, read in place (`blaze_link::SpanTable`'s encoding).
#[derive(Default)]
struct Spans {
    by_anchor: Option<HashMap<ContentId, Vec<u8>, Global>>,
    /// The symbols the newest build's functions and statics define, by name.
    symbols: Vec<(String, Named)>,
}

const SPANS_MAGIC: &[u8; 8] = b"BZSPANS1";

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl Positions for Spans {
    fn of(&self, span: &blaze_format::SpanRef) -> Result<(&str, Position), String> {
        let table = self.by_anchor.as_ref().and_then(|it| it.get(&span.anchor));
        let Some(bytes) = table else {
            return Err("the build gave no span table of that anchor".to_owned()); // alloc: an error
        };
        let bad = || "its span table does not read".to_owned(); // alloc: an error
        if bytes.get(..8) != Some(SPANS_MAGIC) {
            return Err(bad());
        }
        let files = u32_at(bytes, 8).ok_or_else(bad)? as usize;
        // Past the file names, to the spans.
        let mut at = 12;
        for _ in 0..files {
            at += 4 + u32_at(bytes, at).ok_or_else(bad)? as usize;
        }
        let count = u32_at(bytes, at).ok_or_else(bad)?;
        if span.index >= count {
            // alloc: an error
            return Err(format!("its span table has {count} spans"));
        }
        let row = at + 4 + span.index as usize * 12;
        let position = Position {
            file: u32_at(bytes, row).ok_or_else(bad)?,
            line: u32_at(bytes, row + 4).ok_or_else(bad)?,
            column: u32_at(bytes, row + 8).ok_or_else(bad)?,
        };
        // The file's name: the `position.file`th.
        let mut at = 12;
        for _ in 0..position.file {
            at += 4 + u32_at(bytes, at).ok_or_else(bad)? as usize;
        }
        let len = u32_at(bytes, at).ok_or_else(bad)? as usize;
        let name = bytes.get(at + 4..at + 4 + len).ok_or_else(bad)?;
        let name = core::str::from_utf8(name).map_err(|_| bad())?;
        Ok((name, position))
    }

    fn defined(&self, name: &str) -> Option<Named> {
        let at = self.symbols.binary_search_by(|it| it.0.as_str().cmp(name)).ok()?;
        Some(self.symbols[at].1)
    }
}

/// What the program keeps of its newest build.
struct Image {
    /// By key.
    functions: Vec<Arc<CompiledFunction>>,
    /// Each function dedupe merged away, with the one kept for it, by key.
    merged: Vec<(InstanceKey, InstanceKey)>,
    /// Every instance's identity, by key.
    identities: Vec<(InstanceKey, ContentId)>,
    /// By identity: the instance of it in this build, and the code hash of the generic it
    /// is an instance of.
    by_identity: Vec<(ContentId, InstanceKey, CodeHash)>,
    /// What a backtrace calls each instance, by key.
    names: Vec<(InstanceKey, String)>,
    /// The statics, by id (their number is their place here), each with its record.
    statics: Vec<(StaticId, Made)>,
    /// The numbers of the statics in initializer sections, in the order they run.
    inits: Vec<u32>,
    entry: ContentId,
    arch: Arch,
}

/// One launcher.
struct Launcher {
    conn: Conn,
    /// By slot: the record the launcher was last told to bind there.
    bound: Vec<Option<RecordId>>,
    /// How many slots it was told to declare.
    declared: u32,
    /// How many of the image's statics (by their order when it connected or was last told)
    /// it has defined: their ids, to tell an added one from one it has.
    statics: Vec<StaticId>,
    /// Whether it said `Hello` and was answered.
    welcomed: bool,
    /// Told to start again: nothing more is said to it.
    restarting: bool,
    /// The edits sent to answer a stub and not yet answered: the transaction and its slot.
    needs: Vec<(u64, u32)>,
}

/// Why the program cannot serve a launcher.
pub type Refusal = String;

/// The daemon's state of one program. See the module's documentation.
pub struct Program {
    /// By identity: its slot.
    slots: HashMap<ContentId, u32, Global>,
    /// By slot: its identity.
    identities: Vec<ContentId>,
    image: Option<Image>,
    spans: Spans,
    /// The records made of the newest build's functions so far, by slot.
    made: HashMap<u32, Arc<Made>, Global>,
    launchers: Vec<Launcher>,
    /// The slots a launcher's stub ever asked for, in the order they were first asked: what
    /// the next launcher is bound to before it starts (launcher.md, step 4: the prefetch of
    /// the last run's set), so a second run asks for a function only where it goes somewhere
    /// the runs before it did not.
    needed: Vec<u32>,
    /// By slot: whether it is in `needed`.
    is_needed: Vec<bool>,
    /// The last transaction's number, over every launcher.
    txn: u64,
    /// The last message's id.
    said: u64,
    /// A safeguard taken out, for the tests' controls: always `Plant::None` outside them.
    pub(crate) plant: Plant,
}

/// A planted bug the simulation's oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
pub(crate) enum Plant {
    None,
    /// A build rebinds no slot: launchers run on the code they have.
    NoRebind,
    /// Two bodies changed at once are sent as an edit.
    NoRestart,
}

impl Default for Program {
    fn default() -> Program {
        Program::new()
    }
}

/// What became of a launcher at a build.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Told {
    /// Nothing it has bound changed.
    Nothing,
    /// An edit of this many slots, and whether they switch together.
    Edit { binds: usize, together: bool },
    /// It starts again, for this reason.
    Restart { why: String },
}

impl Program {
    pub fn new() -> Program {
        Program {
            slots: HashMap::new_in(Global),
            identities: Vec::new(), // alloc: the program's tables, for as long as it is held
            image: None,
            spans: Spans::default(),
            made: HashMap::new_in(Global),
            launchers: Vec::new(), // alloc: as above
            needed: Vec::new(),    // alloc: as above
            is_needed: Vec::new(), // alloc: as above
            txn: 0,
            said: 0,
            plant: Plant::None,
        }
    }

    /// How many launchers are connected.
    pub fn launchers(&self) -> usize {
        self.launchers.len()
    }

    /// A launcher connected. It is told nothing until it says `Hello`.
    pub fn connected(&mut self, conn: Conn) {
        self.launchers.push(Launcher {
            conn,
            bound: Vec::new(), // alloc: a launcher's table
            declared: 0,
            statics: Vec::new(), // alloc: as above
            welcomed: false,
            restarting: false,
            needs: Vec::new(), // alloc: as above
        });
    }

    /// A launcher's socket ended.
    pub fn disconnected(&mut self, conn: Conn) {
        self.launchers.retain(|it| it.conn != conn);
    }

    fn frame(&mut self, tell: &Tell<'_>, out: &mut Vec<u8>) -> Result<(), Refusal> {
        self.said += 1;
        tell.encode(self.said, out).map_err(|why| format!("a message to a launcher: {why}"))
    }

    /// The key the image's function of `key` is kept under: itself, or what dedupe kept.
    fn kept(image: &Image, key: InstanceKey) -> InstanceKey {
        match image.merged.binary_search_by_key(&key, |it| it.0) {
            Ok(at) => image.merged[at].1,
            Err(_) => key,
        }
    }

    /// The slot of the function `key` names in the image.
    fn slot_of(&self, image: &Image, key: InstanceKey) -> Result<u32, Refusal> {
        let identity = image
            .identities
            .binary_search_by_key(&key, |it| it.0)
            .map(|at| image.identities[at].1)
            .map_err(|_| format!("{key:?} is named by the program's code and is no instance of its build"))?;
        self.slots
            .get(&identity)
            .copied()
            .ok_or_else(|| format!("{key:?} has an identity with no slot"))
    }

    /// The record of `slot`'s function in the newest build, made once a build.
    fn record(&mut self, slot: u32) -> Result<Arc<Made>, Refusal> {
        if let Some(made) = self.made.get(&slot) {
            return Ok(Arc::clone(made));
        }
        let Some(image) = &self.image else {
            return Err("no build of the program is held".to_owned()); // alloc: an error
        };
        let Some(identity) = self.identities.get(slot as usize) else {
            return Err(format!("slot {slot} is past the program's {}", self.identities.len()));
        };
        let Ok(at) = image.by_identity.binary_search_by_key(identity, |it| it.0) else {
            // alloc: an error
            return Err(format!("slot {slot}'s function is not in the program's newest build"));
        };
        let key = Program::kept(image, image.by_identity[at].1);
        let Ok(function) = image.functions.binary_search_by_key(&key, |it| it.key) else {
            // alloc: an error
            return Err(format!("slot {slot}'s function {key:?} has no code in the newest build"));
        };
        let name = match image.names.binary_search_by_key(&image.by_identity[at].1, |it| it.0) {
            Ok(at) => &image.names[at].1[..],
            Err(_) => "<unnamed>",
        };
        let made = Arc::new(record::function(&image.functions[function], name, &self.spans)?);
        self.made.insert(slot, Arc::clone(&made));
        Ok(made)
    }

    /// The numbers a bind gives for `made`'s places: slots and statics' numbers.
    fn refs(&self, made: &Made) -> Result<Vec<u32>, Refusal> {
        let Some(image) = &self.image else {
            return Err("no build of the program is held".to_owned()); // alloc: an error
        };
        made.named
            .iter()
            .map(|named| match named {
                Named::Function(key) => self.slot_of(image, *key),
                Named::Static(id) => image
                    .statics
                    .binary_search_by_key(id, |it| it.0)
                    .map(|at| at as u32)
                    .map_err(|_| format!("the static {id:?} is named and is not in the build")),
            })
            .collect() // alloc: one bind's numbers
    }

    /// The frames that define the image's statics from `from` on (by their place in
    /// `image.statics`... given as a list of numbers).
    fn define(&mut self, numbers: &[u32], out: &mut Vec<u8>) -> Result<(), Refusal> {
        if numbers.is_empty() {
            return Ok(());
        }
        // alloc: the definitions of one message
        let mut refs: Vec<Vec<u32>> = Vec::with_capacity(numbers.len());
        let mut ids = Vec::with_capacity(numbers.len());
        for number in numbers {
            let (id, bytes, made_refs) = {
                let Some(image) = &self.image else {
                    return Err("no build of the program is held".to_owned()); // alloc: an error
                };
                let made = &image.statics[*number as usize].1;
                (made.id, made.bytes.clone(), self.refs(made)?) // clone: the record's bytes into the frame's builder
            };
            self.frame(&Tell::Record { id, bytes: &bytes }, out)?;
            refs.push(made_refs);
            ids.push(id);
        }
        let defs: Vec<wire::StaticDef<'_>> = numbers // alloc: as above
            .iter()
            .zip(&ids)
            .zip(&refs)
            .map(|((number, id), refs)| wire::StaticDef {
                number: *number,
                record: *id,
                refs: List::of(refs),
            })
            .collect();
        self.frame(&Tell::Statics { defs: List::of(&defs) }, out)
    }

    /// The frames that bind `slots` in one edit of `launcher`, after declaring up to
    /// `declare` slots.
    fn edit(
        &mut self,
        launcher: usize,
        slots: &[u32],
        together: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), Refusal> {
        let declared = self.launchers[launcher].declared;
        let declare = (self.identities.len() as u32).saturating_sub(declared);
        if slots.is_empty() && declare == 0 {
            return Ok(());
        }
        // alloc: the binds of one edit
        let mut mades = Vec::with_capacity(slots.len());
        let mut refs = Vec::with_capacity(slots.len());
        for slot in slots {
            let made = self.record(*slot)?;
            refs.push(self.refs(&made)?);
            self.frame(&Tell::Record { id: made.id, bytes: &made.bytes }, out)?;
            mades.push(made);
        }
        let binds: Vec<wire::Bind<'_>> = slots // alloc: as above
            .iter()
            .zip(&mades)
            .zip(&refs)
            .map(|((slot, made), refs)| wire::Bind {
                slot: Slot(*slot),
                record: made.id,
                code: CodeId(made.id.0),
                tier: Tier::Zero,
                len: made.len,
                align: made.align,
                refs: List::of(refs),
            })
            .collect();
        self.txn += 1;
        let edit = Tell::Edit { txn: Txn(self.txn), binds: List::of(&binds), declare, together };
        self.frame(&edit, out)?;
        let launcher = &mut self.launchers[launcher];
        launcher.declared += declare;
        if launcher.bound.len() < launcher.declared as usize {
            launcher.bound.resize(launcher.declared as usize, None);
        }
        for (slot, made) in slots.iter().zip(&mades) {
            launcher.bound[*slot as usize] = Some(made.id);
        }
        Ok(())
    }

    /// A launcher's message: `item` is one frame's item. Appends to `out` what the launcher
    /// is told for it.
    pub fn said(&mut self, conn: Conn, item: &[u8], out: &mut Vec<u8>) -> Result<(), Refusal> {
        let (_, ask) = Ask::decode(item).map_err(|why| format!("a launcher's message: {why}"))?;
        let Some(at) = self.launchers.iter().position(|it| it.conn == conn) else {
            return Err(format!("launcher {} said something and is not connected", conn.0));
        };
        if self.launchers[at].restarting {
            // Told to start again: what it still says is of the program it leaves.
            return Ok(());
        }
        match ask {
            Ask::Hello { arch, .. } => {
                let Some(image) = &self.image else {
                    return Err("a launcher said hello and no build of its program is held".to_owned()); // alloc: an error
                };
                let ours = match image.arch {
                    Arch::X86_64 => wire::Arch::X86_64,
                    _ => wire::Arch::Aarch64,
                };
                if arch != ours {
                    return Err(format!("a launcher of {arch:?} asked for a program built for {ours:?}"));
                }
                let slots = self.identities.len() as u32;
                let numbers: Vec<u32> = (0..image.statics.len() as u32).collect(); // alloc: once a launcher
                let statics: Vec<StaticId> = image.statics.iter().map(|it| it.0).collect(); // alloc: as above
                let inits = image.inits.clone(); // clone: a few numbers, once a launcher
                let entry = self
                    .slots
                    .get(&image.entry)
                    .copied()
                    .ok_or_else(|| "the program's entry has no slot".to_owned())?;
                self.frame(&Tell::Welcome { slots }, out)?;
                self.define(&numbers, out)?;
                {
                    let launcher = &mut self.launchers[at];
                    launcher.declared = slots;
                    launcher.bound = vec![None; slots as usize]; // alloc: a launcher's table
                    launcher.statics = statics;
                    launcher.welcomed = true;
                }
                // What the runs before this one called, bound before it starts: those of
                // them the newest build still has code for.
                let prefetch: Vec<u32> = // alloc: once a launcher
                    self.needed.clone().into_iter().filter(|slot| self.record(*slot).is_ok()).collect(); // clone: a list of numbers, once a launcher
                self.edit(at, &prefetch, false, out)?;
                self.frame(&Tell::Start { entry: Slot(entry), inits: List::of(&inits) }, out)?;
            }
            Ask::Need { slot, .. } => {
                // The newest build's record of the slot, whatever build the stub fired on.
                if (slot.0 as usize) >= self.identities.len() {
                    let diagnostic = format!("slot {} is past the program's {}", slot.0, self.identities.len());
                    return self.frame(&Tell::Refused { slot, diagnostic: &diagnostic }, out);
                }
                if self.is_needed.len() <= slot.0 as usize {
                    self.is_needed.resize(slot.0 as usize + 1, false);
                }
                if !self.is_needed[slot.0 as usize] {
                    self.is_needed[slot.0 as usize] = true;
                    self.needed.push(slot.0);
                }
                match self.record(slot.0) {
                    Ok(_) => {
                        self.edit(at, &[slot.0], false, out)?;
                        let txn = self.txn;
                        self.launchers[at].needs.push((txn, slot.0));
                    }
                    Err(diagnostic) => {
                        self.frame(&Tell::Refused { slot, diagnostic: &diagnostic }, out)?;
                    }
                }
            }
            Ask::Applied { txn, .. } => self.launchers[at].needs.retain(|it| it.0 != txn.0),
            Ask::Restarting { .. } => {}
            Ask::Failed { txn, why } => {
                let needed = self.launchers[at].needs.iter().position(|it| it.0 == txn.0);
                match needed {
                    // A function that could not be placed when it was first called: the
                    // program stops with why, as for one that does not compile. Starting
                    // again would only fail again.
                    Some(needed) => {
                        let (_, slot) = self.launchers[at].needs.swap_remove(needed);
                        self.launchers[at].bound[slot as usize] = None;
                        self.frame(&Tell::Refused { slot: Slot(slot), diagnostic: why }, out)?;
                    }
                    // An edit of what it runs: the launcher lacks what the edit bound and
                    // cannot go on as it is.
                    None => {
                        let why = format!("edit {} could not be applied by the launcher: {why}", txn.0);
                        self.frame(&Tell::Restart { txn, why: &why }, out)?;
                        self.launchers[at].restarting = true;
                    }
                }
            }
            Ask::Frames { .. } | Ask::Ended { .. } => {
                return Err(format!("a launcher said {ask:?}, which this daemon does not ask for"));
            }
        }
        Ok(())
    }

    /// A build of the program: `built` is everything it made, `entry` the symbol of its
    /// entry (`main`), `tables` its span tables. Gives, for each launcher that runs the
    /// program, the frames to write to it and what they say.
    pub fn built(
        &mut self,
        built: &Built,
        entry: &str,
        tables: Tables<'_>,
    ) -> Result<Vec<(Conn, Vec<u8>, Told)>, Refusal> {
        // The span tables first: records are made against them.
        let changed_anchors: Option<Vec<ContentId>> = match tables {
            Tables::Whole(all) => {
                let mut map = HashMap::new_in(Global);
                for (anchor, bytes) in all {
                    map.insert(*anchor, bytes.clone()); // clone: the program's own copy of a table
                }
                self.spans.by_anchor = Some(map);
                None
            }
            Tables::Changed { tables, removed } => {
                let map = self.spans.by_anchor.get_or_insert_with(|| HashMap::new_in(Global));
                let mut changed = Vec::with_capacity(tables.len() + removed.len()); // alloc: once a build
                for (anchor, bytes) in tables {
                    map.insert(*anchor, bytes.to_vec()); // alloc: the program's own copy of a table
                    changed.push(*anchor);
                }
                for anchor in removed {
                    map.remove(anchor);
                    changed.push(*anchor);
                }
                changed.sort_unstable();
                Some(changed)
            }
        };

        // The symbols the program itself defines: a record's relocation against one is
        // against the function or static, as a link resolves it.
        // alloc: once a build, the build's exported names
        let mut symbols: Vec<(String, Named)> = built
            .symbols
            .iter()
            .map(|(key, name, _)| (name.clone(), Named::Function(*key))) // clone: a name, once a build
            .chain(
                built
                    .static_symbols
                    .iter()
                    .map(|(item, instance, name, _)| (name.clone(), Named::Static((*item, *instance)))), // clone: as above
            )
            .collect();
        symbols.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        symbols.dedup_by(|a, b| a.0 == b.0);
        self.spans.symbols = symbols;

        let Some(first) = built.functions.first() else {
            return Err("a build of no function".to_owned()); // alloc: an error
        };
        let arch = first.arch;
        // Slots for the identities new to this build, in the order of their keys.
        for (_, identity) in &built.identities {
            if !self.slots.contains_key(identity) {
                self.slots.insert(*identity, self.identities.len() as u32);
                self.identities.push(*identity);
            }
        }
        // alloc: the image's lists, once a build (each a copy of a list of the build's)
        let mut by_identity: Vec<(ContentId, InstanceKey, CodeHash)> = Vec::with_capacity(built.identities.len());
        for (key, identity) in &built.identities {
            let code = match built.owners.binary_search_by_key(key, |it| it.0) {
                Ok(at) => built.owners[at].2,
                Err(_) => return Err(format!("{key:?} has an identity and no owner in its build")),
            };
            by_identity.push((*identity, *key, code));
        }
        by_identity.sort_unstable_by_key(|it| it.0);
        let Some(entry_key) = built.symbols.iter().find(|it| it.1 == entry).map(|it| it.0) else {
            return Err(format!("the build defines no `{entry}`: it is no program"));
        };
        let entry_identity = built
            .identities
            .binary_search_by_key(&entry_key, |it| it.0)
            .map(|at| built.identities[at].1)
            .map_err(|_| format!("the build's `{entry}` has no identity"))?;
        let mut statics = Vec::with_capacity(built.statics.len());
        let mut inits = Vec::new();
        for (number, of) in built.statics.iter().enumerate() {
            let name = format!("static {:?}", of.item); // alloc: a static's name for a refusal
            if let Some(section) = &of.section
                && blaze_codegen::driver::runs_at_start(section)
            {
                let starts = section.contains("init_array") || section.contains("__mod_init_func");
                if !starts {
                    return Err(format!(
                        "`{name}` is in section `{section}`, a finalizer array, which the launcher does not run yet"
                    ));
                }
                inits.push(number as u32);
            }
            statics.push(((of.item, of.instance), record::of_static(of, &name, arch, &self.spans)?));
        }
        let image = Image {
            functions: built.functions.clone(), // clone: a count a function
            merged: built.deduped.merged.iter().map(|(from, to)| (*from, *to)).collect(),
            identities: built.identities.clone(), // clone: the build's list, once a build
            by_identity,
            names: built.debug_names.clone(), // clone: the build's names, once a build
            statics,
            inits,
            entry: entry_identity,
            arch,
        };

        // What changed, against the build before.
        let before = self.image.replace(image);
        let image = self.image.as_ref().expect("just set");
        let mut restart: Option<String> = None;
        let mut bodies: Vec<(CodeHash, CodeHash, ContentId)> = Vec::new(); // alloc: once a build
        let mut changed_slots: Vec<u32> = Vec::new(); // alloc: once a build
        if let Some(before) = &before {
            if before.entry != image.entry {
                restart = Some("the program's entry is another function".to_owned()); // alloc: once
            }
            for (identity, key, code) in &image.by_identity {
                let Ok(at) = before.by_identity.binary_search_by_key(identity, |it| it.0) else {
                    continue;
                };
                let (_, old_key, old_code) = before.by_identity[at];
                if old_key != *key {
                    changed_slots.push(self.slots[identity]);
                }
                if old_code != *code && !bodies.iter().any(|it| it.0 == old_code && it.1 == *code) {
                    bodies.push((old_code, *code, *identity));
                }
            }
            for (id, made) in &image.statics {
                if let Ok(at) = before.statics.binary_search_by_key(id, |it| it.0)
                    && before.statics[at].1.id != made.id
                {
                    restart = Some(format!("the static {id:?} has another value or type")); // alloc: once
                }
            }
            if bodies.len() > 1 && self.plant != Plant::NoRestart {
                let name = |identity: &ContentId| {
                    let at = image.by_identity.binary_search_by_key(identity, |it| it.0).ok()?;
                    let key = image.by_identity[at].1;
                    let at = image.names.binary_search_by_key(&key, |it| it.0).ok()?;
                    Some(image.names[at].1.clone()) // clone: a name into the reason
                };
                let named: Vec<String> = bodies.iter().take(3).filter_map(|it| name(&it.2)).collect();
                restart = Some(format!(
                    "{} functions' bodies changed together ({}{}): a changed signature cannot yet be told from \
                     body edits, so only one changed body is applied to a running program",
                    bodies.len(),
                    named.join(", "),
                    if bodies.len() > named.len() { ", .." } else { "" },
                ));
            }
        }

        // The records of the build before are let go where they may differ: a function whose
        // key moved, or one that read a span table that changed. With every table given
        // anew, all of them.
        changed_slots.sort_unstable();
        match &changed_anchors {
            None => self.made.clear(),
            Some(changed) => {
                self.made.retain(|slot, made| {
                    changed_slots.binary_search(slot).is_err()
                        && !made.anchors.iter().any(|it| changed.binary_search(it).is_ok())
                });
            }
        }

        let mut told = Vec::with_capacity(self.launchers.len()); // alloc: once a build
        for at in 0..self.launchers.len() {
            if !self.launchers[at].welcomed || self.launchers[at].restarting {
                continue;
            }
            let conn = self.launchers[at].conn;
            let mut out = Vec::new(); // alloc: a launcher's frames
            if let Some(why) = &restart {
                self.txn += 1;
                let (txn, why) = (Txn(self.txn), why.clone()); // clone: the reason, a launcher
                self.frame(&Tell::Restart { txn, why: &why }, &mut out)?;
                self.launchers[at].restarting = true;
                told.push((conn, out, Told::Restart { why }));
                continue;
            }
            // The statics the launcher does not have yet.
            let image = self.image.as_ref().expect("set above");
            let added: Vec<u32> = image // alloc: once a launcher a build
                .statics
                .iter()
                .enumerate()
                .filter(|(_, it)| !self.launchers[at].statics.contains(&it.0))
                .map(|(number, _)| number as u32)
                .collect();
            // A static's number is its place in the build's list: one added before another
            // moves the numbers of those after it, which the launcher has under the old.
            let moved = self.launchers[at]
                .statics
                .iter()
                .any(|id| image.statics.binary_search_by_key(id, |it| it.0).is_ok_and(|now| {
                    before.as_ref().is_some_and(|before| {
                        before.statics.binary_search_by_key(id, |it| it.0).is_ok_and(|was| was != now)
                    })
                }));
            if moved {
                let why = "a static was added or removed before others: their numbers moved".to_owned(); // alloc: once
                self.txn += 1;
                self.frame(&Tell::Restart { txn: Txn(self.txn), why: &why }, &mut out)?;
                self.launchers[at].restarting = true;
                told.push((conn, out, Told::Restart { why }));
                continue;
            }
            let ids: Vec<StaticId> = added.iter().map(|it| image.statics[*it as usize].0).collect(); // alloc: as above
            self.define(&added, &mut out)?;
            self.launchers[at].statics.extend(ids);
            // The bound slots whose record is another now.
            let mut rebind = Vec::new(); // alloc: once a launcher a build
            if self.plant != Plant::NoRebind {
                for slot in 0..self.launchers[at].bound.len() as u32 {
                    let Some(bound) = self.launchers[at].bound[slot as usize] else { continue };
                    // A slot whose function left the build is named by no code any more.
                    let Ok(made) = self.record(slot) else { continue };
                    if made.id != bound {
                        rebind.push(slot);
                    }
                }
            }
            let together = !bodies.is_empty();
            let binds = rebind.len();
            self.edit(at, &rebind, together, &mut out)?;
            let what = match out.is_empty() {
                true => Told::Nothing,
                false => Told::Edit { binds, together },
            };
            told.push((conn, out, what));
        }
        Ok(told)
    }

    /// The record `slot` holds in the newest build, when it has one: for the tests' oracle.
    #[cfg(test)]
    pub(crate) fn newest(&mut self, slot: u32) -> Option<RecordId> {
        self.record(slot).ok().map(|it| it.id)
    }
}
