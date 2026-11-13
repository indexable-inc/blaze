//! [`Program`]: what the daemon keeps of one program its launchers run, and what it tells
//! each of them.
//!
//! A sans-IO core. The daemon's driver (blaze-cargo) gives it each build's start
//! ([`Program::started`]), each message a launcher sent ([`Program::said`]) and each answer
//! to a first call it was asked for ([`Program::took`], [`Program::refused`]); it hands back
//! frames for the driver to write to that launcher's socket, in the order given. It reads no
//! clock, owns no socket and holds no build: only the bound records ([`Bound`]) it was given,
//! one a slot and one a static. Its protocol runs in a simulation (`tests`).
//!
//! # Pieces, not a build
//!
//! A build gives the program its start: the entry, each function a start-section static
//! names, every root static and those they name, each a [`Bound`] (`Piece`), and the
//! program's own symbols. Every other function is made when a launcher first calls it: a
//! slot with no `Bound` is answered with [`Said::Want`], and the driver answers that want
//! with a piece read from the store under the slot's bind key ([`crate::bind`]) or made by
//! the front end ([`Program::took`]), or refuses it by name ([`Program::refused`]).
//!
//! What the driver keeps (its contract, which the simulation holds it to):
//! - a `took`'s piece is of the newest build: a bind record is by key, and the front end
//!   makes pieces only between builds;
//! - a bind record's statics are read by their bind keys with it and given in the same
//!   piece (a piece naming a static that it does not give and the program does not hold is
//!   refused by name);
//! - `started`'s piece holds the start and a new `Bound` for each identity of
//!   [`Program::awaited`] the build made again, and `changed` names every slot identity whose
//!   code this build made anew. A held function in `changed` the piece does not give is let
//!   go; a launcher that has it bound starts again, naming it;
//! - a build whose policy or record maker ([`crate::bind::Under`]) is another is another
//!   program: the driver gives it to a new `Program`. One whose own symbols changed, this
//!   program sees itself: every launcher starts again.
//!
//! # Slots
//!
//! A function's slot is its slot identity's ([`crate::bind::slot_identity`]: the item, which
//! instance, its type arguments, and its ABI), which an edit of its body keeps. A slot is
//! given once and never taken back, and is given when a record that is sent somewhere first
//! names the function (never to the whole program up front); a launcher that runs already is
//! told to declare the new ones (`Edit::declare`).
//!
//! # Statics
//!
//! A static's number is its place in the order the program was first given statics, so a
//! static added later never moves another. A launcher defines the statics in that order, up
//! to the newest, before any record it binds names one.
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
//! the launcher was told to start again; and each first call is answered exactly once.
//!
//! # A build: live, or a restart
//!
//! [`Program::started`] classifies what changed since the build a launcher is on:
//!
//! | what changed | the launcher is told |
//! |---|---|
//! | function bodies (every instance of one generic counts as one; the entry shim around `fn main` is not counted) | an edit: the bound slots whose records changed, switched together |
//! | lines only (a comment above a function) | an edit of the bound functions whose positions moved, not gated: each means what it meant |
//! | a function's signature | its callers' code names another slot: the callers are rebound, the old slot keeps the body its old frames call |
//! | a function or static was added | its slot declared, or its static defined; it is bound when first called |
//! | a function or static was removed | nothing: no code names it any more |
//! | a static's value or type, of one the launcher defined | restart needed, naming the static |
//! | the entry, or the program's own symbols | restart needed |
//!
//! Changed bodies are one edit because a slot is keyed by its function's ABI: a body whose
//! signature changed is another slot, and every caller of it changed with it.
//!
//! # After a start of the daemon
//!
//! [`Program::kept`] gives the program's whole state and its kept answer to a `Hello` as one
//! start image ([`crate::start`]), and [`Program::restored`] reads it back: a daemon that
//! starts again holds the program, its launchers' next start is the kept frame, and a first
//! call the runs before made is bound in it. Each function's portable ask
//! ([`Program::ask_of`]) is held with its slot, so a first call no bind record answers is
//! made by a front end loaded then, from the ask.

use std::alloc::Global;
use std::sync::Arc;

use blaze_cas::Key;
use blaze_codegen::StaticId;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::CodeHash;
use blaze_launch::image as placed;
use blaze_launch::record::{Place, Record, What};
use blaze_launch::wire::{self, Ask, List, RecordId, Tell};
use blaze_launch::{CodeId, Slot, Tier, Txn};
use blaze_report::say::{Code, Words};

use crate::bind::{Bound, Names, symbols_id};
use crate::start::{self, Of};

/// One launcher's connection, as the driver names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Conn(pub u64);

/// Functions and statics made, bound: a build's start, or the answer to a first call.
#[derive(Default)]
pub struct Piece {
    pub functions: Vec<Bound>,
    pub statics: Vec<(StaticId, Bound)>,
}

/// What became of a launcher's message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Said {
    /// What it is told was written (or nothing is).
    Told,
    /// Its stub fired for `slot`, whose function the program holds no record of: the driver
    /// answers with [`Program::took`] or [`Program::refused`], once. With `bind`, the store
    /// may hold its bind record under that key; without, the front end makes it.
    Want { slot: u32, identity: ContentId, bind: Option<Key> },
}

/// What the program holds of one function, by slot identity.
pub(crate) struct Function {
    pub(crate) bound: Option<Arc<Bound>>,
    /// The bind key the newest record naming it gave it.
    pub(crate) bind: Option<Key>,
    /// Its portable ask (`HirFrontend::ask_bytes`), the newest a record naming it gave that
    /// was not empty: what a front end loaded after a start reads back as the instance to
    /// make. Empty while none did.
    pub(crate) ask: Vec<u8>,
}

impl Function {
    const NONE: Function = Function { bound: None, bind: None, ask: Vec::new() };

    /// What a record's name of it says: its bind key, and its ask where it gives one (an
    /// empty one never clears it).
    fn named(&mut self, bind: Option<Key>, ask: &[u8]) {
        self.bind = bind;
        if !ask.is_empty() && self.ask != ask {
            // alloc: the ask, held as long as the program holds the slot
            self.ask = ask.to_vec();
        }
    }
}

/// One launcher.
struct Launcher {
    conn: Conn,
    /// By slot: the record the launcher was last told to bind there.
    bound: Vec<Option<RecordId>>,
    /// How many slots it was told to declare.
    declared: u32,
    /// How many of the program's statics, in their order, it has defined.
    statics: u32,
    /// Whether it said `Hello` and was answered.
    welcomed: bool,
    /// Told to start again: nothing more is said to it.
    restarting: bool,
    /// The edits sent to answer a stub and not yet answered: the transaction and its slot.
    needs: Vec<(u64, u32)>,
    /// The slots whose first call waits for the driver's answer, with the bind key told it.
    wants: Vec<(u32, Option<Key>)>,
}

/// The answer to a `Hello`, kept ([`Program::said`]): one message, an image of placed code
/// (`blaze_launch::image`), and what a launcher that takes it holds.
pub(crate) struct Welcome {
    /// `Program::version` and `Program::needed.len()` when it was made.
    pub(crate) of: (u64, usize),
    /// The system and the page size of the launcher it was made for.
    pub(crate) os: wire::Os,
    pub(crate) page: u32,
    /// The frame of the `Tell::Image`.
    pub(crate) frame: Vec<u8>,
    pub(crate) declared: u32,
    pub(crate) bound: Vec<Option<RecordId>>,
    pub(crate) statics: u32,
}

/// Why the program cannot serve a launcher.
pub type Refusal = String;

/// The daemon's state of one program. See the module's documentation.
pub struct Program {
    /// By slot identity: its slot.
    pub(crate) slots: HashMap<ContentId, u32, Global>,
    /// By slot: its identity.
    pub(crate) identities: Vec<ContentId>,
    /// By slot identity: its record, when held, and its bind key.
    pub(crate) functions: HashMap<ContentId, Function, Global>,
    /// The program's own symbols, sorted by name.
    pub(crate) symbols: Vec<(String, Names)>,
    /// By number: each static and its record, in the order first given.
    pub(crate) statics: Vec<(StaticId, Arc<Bound>)>,
    /// Each static's number, by id.
    pub(crate) numbers: Vec<(StaticId, u32)>,
    /// The numbers of the statics in initializer sections, in the order they run.
    pub(crate) inits: Vec<u32>,
    /// The newest build's entry, by slot identity.
    pub(crate) entry: Option<ContentId>,
    /// The functions of the newest build's start, by slot identity: bound in every image.
    pub(crate) start: Vec<ContentId>,
    pub(crate) arch: Option<wire::Arch>,
    launchers: Vec<Launcher>,
    /// The slots a launcher's stub ever asked for, in the order they were first asked: what
    /// the next launcher is bound to before it starts (launcher.md, step 4: the prefetch of
    /// the last run's set), so a second run asks for a function only where it goes somewhere
    /// the runs before it did not.
    pub(crate) needed: Vec<u32>,
    /// By slot: whether it is in `needed`.
    pub(crate) is_needed: Vec<bool>,
    /// How many times what it holds changed: which records a kept answer is of.
    pub(crate) version: u64,
    /// The answer to the last `Hello`, kept while it is still the answer. A second run of an
    /// unchanged program is then told a copy of bytes.
    pub(crate) welcome: Option<Welcome>,
    /// The last transaction's number, over every launcher.
    pub(crate) txn: u64,
    /// The last message's id.
    pub(crate) said: u64,
    /// The system and page size of the last launcher whose `Hello` was answered: what
    /// [`Program::kept`] lays the start image for.
    pub(crate) hello: Option<(wire::Os, u32)>,
    /// `version`, `needed.len()` and `said` when [`Program::kept`] last gave an image: it
    /// gives none again until one of them moves. `said` moves with every frame, and so with
    /// every slot given and every transaction: an image whose last message's id is behind
    /// would tell a launcher ids it was told already.
    pub(crate) kept_of: Option<(u64, usize, u64)>,
    /// The check word (bytes 8..40) of the image [`Program::kept`] last gave or
    /// [`Program::restored`] read: what a launcher that started from that image's start file
    /// says it holds ([`Program::check`]).
    pub(crate) kept_check: Option<[u8; 32]>,
    /// A safeguard taken out, for the tests' controls: always `Plant::None` outside them.
    pub(crate) plant: Plant,
}

/// A planted bug the simulation's oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum Plant {
    None,
    /// A build rebinds no slot: launchers run on the code they have.
    NoRebind,
    /// A build that changes what a running launcher cannot take live (a static it defined,
    /// the entry) is sent as an edit.
    NoRestart,
    /// An edit counts the slots it declares before its records name their callees.
    DeclareFirst,
    /// The tests' driver keys a slot by what its function is alone, not also by its ABI: a
    /// function whose signature changed keeps its slot.
    SlotAcrossAbi,
    /// A piece's record of a function or static the program holds a record of already is
    /// not taken: the program keeps the `Bound` it had.
    StaleSlot,
    /// A bind record is read without its static names (`bind::read`).
    DropsStaticNames,
    /// A first call of a slot the program holds a record of is answered and also wanted.
    WantTwice,
    /// A restored program (`Program::restored`) leaves out the slots the runs before it
    /// called (`needed`).
    DropsNeeded,
    /// A restored program reads no slot's ask.
    ForgetsAsks,
    /// A start image's key (`start::start_key`) leaves out the hash of its body: two images
    /// of one program and build, of other slots, have one key.
    KeyOfIdentityAlone,
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

/// What taking a piece changed.
struct Applied {
    /// Each function whose record is new or another: its slot identity, and the code hash
    /// of the record it had, when it had one.
    fresh: Vec<(ContentId, Option<CodeHash>, CodeHash)>,
    /// The numbers of the statics whose record is another.
    moved: Vec<u32>,
}

/// What a refusal calls `bound`: its record's name.
fn called(bound: &Bound) -> String {
    match Record::decode(&bound.made.bytes) {
        Ok(record) => blaze_report::say!(Code(&record.name)).flat(),
        Err(why) => format!("the record of slot identity {:?} (which does not read: {why})", bound.slot), // alloc: an error
    }
}

impl Program {
    pub fn new() -> Program {
        Program {
            slots: HashMap::new_in(Global),
            identities: Vec::new(), // alloc: the program's tables, for as long as it is held
            functions: HashMap::new_in(Global),
            symbols: Vec::new(), // alloc: as above
            statics: Vec::new(), // alloc: as above
            numbers: Vec::new(), // alloc: as above
            inits: Vec::new(),   // alloc: as above
            entry: None,
            start: Vec::new(),     // alloc: as above
            arch: None,
            launchers: Vec::new(), // alloc: as above
            needed: Vec::new(),    // alloc: as above
            is_needed: Vec::new(), // alloc: as above
            version: 0,
            welcome: None,
            txn: 0,
            said: 0,
            hello: None,
            kept_of: None,
            kept_check: None,
            plant: Plant::None,
        }
    }

    /// The number of the static `id`, when the program holds it.
    fn number(&self, id: &StaticId) -> Option<u32> {
        self.numbers.binary_search_by_key(id, |it| it.0).ok().map(|at| self.numbers[at].1)
    }

    /// The record the program holds of `slot`'s function.
    fn held(&self, slot: u32) -> Option<Arc<Bound>> {
        let identity = self.identities.get(slot as usize)?;
        self.functions.get(identity)?.bound.as_ref().map(Arc::clone) // clone: a count
    }

    /// The record of `slot`'s function, or why there is none.
    fn bound_of(&self, slot: u32) -> Result<Arc<Bound>, Refusal> {
        self.held(slot).ok_or_else(|| {
            format!("slot {slot}'s function has no record the program holds") // alloc: an error
        })
    }

    /// Whether every name of `piece`'s records can be given a number, against `symbols`, with
    /// what the program holds: nothing of the program changes, so a piece that does not fit
    /// is refused whole. With `took`, a static the program holds another record of is refused
    /// too: a first call's piece is of the newest build, which gave its statics.
    pub(crate) fn check(&self, piece: &Piece, symbols: &[(String, Names)], took: bool) -> Result<(), Refusal> {
        // alloc: the piece's statics by id, once a piece
        let mut given: Vec<&StaticId> = piece.statics.iter().map(|it| &it.0).collect();
        given.sort_unstable();
        let has = |id: &StaticId| given.binary_search(&id).is_ok() || self.number(id).is_some();
        let symbol = |name: &str| {
            symbols.binary_search_by(|it| it.0.as_str().cmp(name)).ok().map(|at| &symbols[at].1)
        };
        let records = piece.functions.iter().map(|it| (it, true)).chain(piece.statics.iter().map(|it| (&it.1, false)));
        for (bound, function) in records {
            let record = Record::decode(&bound.made.bytes)
                .map_err(|why| format!("a record of the program's does not read: {why}"))?;
            let kind = match (function, record.what) {
                (true, What::Code) if bound.slot != ContentId::default() => true,
                (false, What::Static { .. }) if bound.slot == ContentId::default() => true,
                _ => false,
            };
            if !kind {
                // alloc: an error
                return Err(blaze_report::say!(
                    Code(&record.name),
                    " is given as a ",
                    if function { "function" } else { "static" },
                    " and its record is ",
                    Words(&format!("{:?} of slot identity {:?}", record.what, bound.slot)), // alloc: an error
                )
                .flat());
            }
            let numbered = record.places.iter().filter(Place::numbered);
            if record.numbered() != bound.names.len() {
                // alloc: an error
                return Err(blaze_report::say!(
                    Code(&record.name),
                    "'s record numbers ",
                    Words(&record.numbered().to_string()), // alloc: an error
                    " places, and its bind names ",
                    Words(&bound.names.len().to_string()), // alloc: an error
                    ": a static or a function it names is left out",
                )
                .flat());
            }
            for (number, (place, names)) in numbered.zip(&bound.names).enumerate() {
                let resolved = match names {
                    Names::Symbol(name) => match symbol(name) {
                        Some(Names::Symbol(_)) | None => {
                            // alloc: an error
                            return Err(blaze_report::say!(
                                Code(&record.name),
                                " names ",
                                Code(name),
                                " as the program's own symbol, and the program defines no ",
                                Code(name),
                            )
                            .flat());
                        }
                        Some(it) => it,
                    },
                    other => other,
                };
                match (place, resolved) {
                    (Place::Function, Names::Function { .. }) => {}
                    (Place::Static, Names::Static { id, .. }) if has(id) => {}
                    (Place::Static, Names::Static { id, .. }) => {
                        // alloc: an error
                        return Err(blaze_report::say!(
                            Code(&record.name),
                            " names the static ",
                            Words(&format!("{id:?}")), // alloc: an error
                            ", which its piece does not give and the program does not hold",
                        )
                        .flat());
                    }
                    _ => {
                        // alloc: an error
                        return Err(blaze_report::say!(
                            "place ",
                            Words(&number.to_string()), // alloc: an error
                            " of ",
                            Code(&record.name),
                            " is a ",
                            Words(&format!("{place:?}, and it is named {names:?}")), // alloc: an error
                        )
                        .flat());
                    }
                }
            }
        }
        if took {
            for (id, bound) in &piece.statics {
                if let Some(number) = self.number(id)
                    && self.statics[number as usize].1.made.id != bound.made.id
                {
                    // alloc: an error
                    return Err(format!(
                        "a first call's piece gives the static {id:?} another record than the newest build gave: a \
                         piece of another build"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Takes `piece`'s records, `check`ed: each static not held gets the next number, each
    /// record another than the one held replaces it, and every function named gets its bind
    /// key.
    fn apply(&mut self, piece: Piece) -> Applied {
        // alloc (each below): once a piece
        let mut applied = Applied { fresh: Vec::new(), moved: Vec::new() };
        let names = |bound: &Bound, functions: &mut HashMap<ContentId, Function, Global>| {
            for names in &bound.names {
                if let Names::Function { slot, bind, ask, .. } = names {
                    functions.entry(*slot).or_insert(Function::NONE).named(*bind, ask);
                }
            }
        };
        for (id, bound) in piece.statics {
            names(&bound, &mut self.functions);
            match self.number(&id) {
                Some(number) => {
                    if self.statics[number as usize].1.made.id != bound.made.id {
                        if self.plant != Plant::StaleSlot {
                            self.statics[number as usize].1 = Arc::new(bound);
                        }
                        applied.moved.push(number);
                    }
                }
                None => {
                    let number = self.statics.len() as u32;
                    let at = self.numbers.binary_search_by_key(&id, |it| it.0).unwrap_err();
                    self.numbers.insert(at, (id, number));
                    self.statics.push((id, Arc::new(bound)));
                }
            }
        }
        for bound in piece.functions {
            names(&bound, &mut self.functions);
            let function = self.functions.entry(bound.slot).or_insert(Function::NONE);
            match function.bound.as_ref().map(|it| (it.made.id, it.code)) {
                Some((held, _)) if held == bound.made.id => {}
                Some((_, code)) => {
                    applied.fresh.push((bound.slot, Some(code), bound.code));
                    if self.plant != Plant::StaleSlot {
                        function.bound = Some(Arc::new(bound));
                    }
                }
                None => {
                    applied.fresh.push((bound.slot, None, bound.code));
                    function.bound = Some(Arc::new(bound));
                }
            }
        }
        self.version += 1;
        applied
    }

    /// A launcher's whole start as one message, a `Tell::Image` frame: the image of placed
    /// code (`blaze_launch::image`) of the program's statics and of `binds` (the start's
    /// functions, then the slots the runs before this one called), for a launcher of `arch`
    /// and `os` in pages of `page` bytes whose entry is slot `entry`. With how many slots it
    /// declares and the record it binds in each.
    ///
    /// This is the one way a launcher starts. Where the program has something an image
    /// cannot hold, the launch is refused, naming the case (`image::Why`), what of the
    /// program it is, and what to do: nothing else is tried (rule `no-fallback`).
    fn start_image(
        &mut self,
        arch: wire::Arch,
        os: wire::Os,
        page: u32,
        binds: &[u32],
        entry: u32,
    ) -> Result<(Vec<u8>, u32, Vec<Option<RecordId>>), Refusal> {
        // alloc (each list below): once an image
        let statics: Vec<Arc<Bound>> = self.statics.iter().map(|it| Arc::clone(&it.1)).collect(); // clone: counts
        let inits = self.inits.clone(); // clone: a few numbers, once an image
        // Naming a function gives it its slot: every slot the statics and these records name
        // is given here, and the image declares them all.
        let mut refs: Vec<Vec<u32>> = Vec::with_capacity(statics.len() + binds.len());
        for bound in &statics {
            refs.push(self.refs(bound)?);
        }
        let mut functions: Vec<Arc<Bound>> = Vec::with_capacity(binds.len());
        for slot in binds {
            let bound = self.bound_of(*slot)?;
            refs.push(self.refs(&bound)?);
            functions.push(bound);
        }
        let declared = self.identities.len() as u32;
        let defs: Vec<wire::StaticDef<'_>> = statics
            .iter()
            .enumerate()
            .zip(&refs)
            .map(|((number, bound), refs)| wire::StaticDef {
                number: number as u32,
                record: bound.made.id,
                refs: List::of(refs),
            })
            .collect();
        let wired: Vec<wire::Bind<'_>> = binds
            .iter()
            .zip(&functions)
            .zip(&refs[statics.len()..])
            .map(|((slot, bound), refs)| wire::Bind {
                slot: Slot(*slot),
                record: bound.made.id,
                code: CodeId(bound.made.id.0),
                tier: Tier::Zero,
                len: bound.made.len,
                align: bound.made.align,
                refs: List::of(refs),
            })
            .collect();
        let mut decoded: Vec<(RecordId, Record<'_>)> = Vec::with_capacity(statics.len() + functions.len());
        for bound in statics.iter().chain(&functions) {
            let record = Record::decode(&bound.made.bytes).map_err(|why| format!("a record of the program's: {why}"))?;
            decoded.push((bound.made.id, record));
        }
        decoded.sort_unstable_by_key(|it| it.0.0);
        let records =
            |id: &RecordId| decoded.binary_search_by_key(&id.0, |it| it.0.0).ok().map(|at| decoded[at].1);
        let input = placed::Input {
            arch,
            os,
            page: u64::from(page),
            slots: declared,
            statics: &defs,
            binds: &wired,
            entry: Slot(entry),
            inits: &inits,
        };
        let refused = |why: placed::Why, what: &str| {
            // alloc: an error
            format!(
                "this program cannot be started by blaze's launcher ({}): {what}. Run it without the launcher: \
                 `blaze build` it and run the file that makes",
                why.name()
            )
        };
        let image = placed::lay(&input, &records, &placed::Bases::NONE)
            .map_err(|no| refused(no.why, &no.what))?;
        let mut bytes = Vec::new();
        image.encode(&mut bytes);
        let mut frame = Vec::new();
        self.frame(&Tell::Image { bytes: &bytes }, &mut frame)
            .map_err(|why| refused(placed::Why::Long, &why))?;
        let mut bound = vec![None; declared as usize]; // alloc: a launcher's table
        for (slot, made) in binds.iter().zip(&functions) {
            bound[*slot as usize] = Some(made.made.id);
        }
        Ok((frame, declared, bound))
    }

    /// Makes the answer to a `Hello` of a launcher of `arch` and `os` in pages of `page`
    /// bytes, and keeps it as `welcome`: the start's functions, then what the runs before
    /// this one called, those of them the program holds a record of, bound in one image.
    fn welcome(&mut self, arch: wire::Arch, os: wire::Os, page: u32) -> Result<(), Refusal> {
        let Some(entry) = self.entry else {
            return Err("a launcher said hello and no build of its program is held".to_owned()); // alloc: an error
        };
        let of = (self.version, self.needed.len());
        let entry = Program::slot_for(&mut self.slots, &mut self.identities, entry);
        // alloc (each below): once an answer
        let mut binds: Vec<u32> = Vec::with_capacity(self.start.len() + self.needed.len());
        let mut taken: Vec<bool> = Vec::new();
        for identity in self.start.clone() { // clone: a list of ids, once an answer
            let slot = Program::slot_for(&mut self.slots, &mut self.identities, identity);
            if taken.len() <= slot as usize {
                taken.resize(slot as usize + 1, false);
            }
            if !taken[slot as usize] && self.held(slot).is_some() {
                taken[slot as usize] = true;
                binds.push(slot);
            }
        }
        for slot in self.needed.clone() { // clone: a list of numbers, once an answer
            if taken.len() <= slot as usize {
                taken.resize(slot as usize + 1, false);
            }
            if !taken[slot as usize] && self.held(slot).is_some() {
                taken[slot as usize] = true;
                binds.push(slot);
            }
        }
        let (frame, declared, bound) = self.start_image(arch, os, page, &binds, entry)?;
        let statics = self.statics.len() as u32;
        self.welcome = Some(Welcome { of, os, page, frame, declared, bound, statics });
        Ok(())
    }

    /// The ask the newest record naming the function of slot identity `identity` gave
    /// (`Names::Function::ask`); `None` when none gave one.
    pub fn ask_of(&self, identity: &ContentId) -> Option<&[u8]> {
        self.functions.get(identity).map(|it| it.ask.as_slice()).filter(|it| !it.is_empty())
    }

    /// The statics the program holds, by number.
    pub fn statics_held(&self) -> Vec<StaticId> {
        self.statics.iter().map(|it| it.0).collect() // alloc: the driver's list, once a restore
    }

    /// The program's start image ([`crate::start`]) under its key, when it differs from the
    /// one this last gave: its whole state with the answer to a `Hello` of the last launcher's
    /// system and page size, `of` the build and checkout it is of. `None` before a launcher
    /// was answered, or while nothing changed since the last image. A program an image cannot
    /// hold is refused, as its `Hello` would be.
    pub fn kept(&mut self, of: &Of) -> Result<Option<(Key, Vec<u8>)>, Refusal> {
        self.kept_planted(of, self.plant)
    }

    /// [`Program::kept`], with a planted bug of the tests' (`Plant::KeyOfIdentityAlone`) or
    /// none.
    pub(crate) fn kept_planted(&mut self, of: &Of, plant: Plant) -> Result<Option<(Key, Vec<u8>)>, Refusal> {
        let (Some((os, page)), Some(arch), Some(_)) = (self.hello, self.arch, self.entry) else {
            return Ok(None);
        };
        let now = (self.version, self.needed.len());
        if self.kept_of == Some((now.0, now.1, self.said)) {
            return Ok(None);
        }
        if !self.welcome.as_ref().is_some_and(|it| it.of == now && it.os == os && it.page == page) {
            self.welcome(arch, os, page)?;
        }
        let (bytes, check) = start::encode(self, of)?;
        self.kept_of = Some((now.0, now.1, self.said));
        self.kept_check = Some(check.0);
        Ok(Some((start::key_planted(of, &check, plant), bytes)))
    }

    /// The program a start image ([`Program::kept`]) holds, and what it is of: no launcher is
    /// connected, and its next `Hello` of the image's system and page size is told the kept
    /// frame. Bytes of another layout, cut short, of a body its check does not hash to, or
    /// whose slots, statics' numbers or records' names do not fit are refused naming the
    /// offset; an image of another program than `program` naming both ids.
    pub fn restored(bytes: &[u8], program: &[u8; 32]) -> Result<(Program, Of), String> {
        start::decode(bytes, program, Plant::None)
    }

    /// The check word of the program's newest image, while what it holds is still what that
    /// image holds (no build and no new slot asked since): a launcher that says it holds this
    /// word started from the image a launcher would be sent now.
    pub fn kept_check(&self) -> Option<[u8; 32]> {
        match self.kept_of {
            Some((version, needed, _)) if (version, needed) == (self.version, self.needed.len()) => self.kept_check,
            _ => None,
        }
    }

    /// The launcher `conn`, which started from a start image whose [`start::Table`] is
    /// `table` while it was running (`Hello { running: true }`) and holds an image older than
    /// the program's newest, is bound as one welcomed with that image: its declared slots,
    /// binds and statics are the table's, it is told `Tell::Welcome` (appended to `out`), and
    /// later needs and builds are answered as for any welcomed launcher. Refused where the
    /// image is of another build than `record`, the program's, naming both.
    pub fn join(&mut self, conn: Conn, table: &start::Table, record: &ContentId, out: &mut Vec<u8>) -> Result<(), String> {
        if table.record != *record {
            return Err(format!(
                // alloc: an error
                "the launcher started from an image of build {:?}, and the program is build {:?} now: run it again",
                table.record, record
            ));
        }
        let Some(at) = self.launchers.iter().position(|it| it.conn == conn) else {
            return Err(format!("launcher {} joined and is not connected", conn.0)); // alloc: an error
        };
        if table.declared as usize > self.identities.len() || table.statics as usize > self.statics.len() {
            return Err(format!(
                // alloc: an error
                "the launcher's image declares {} slots and defines {} statics, and its build {:?} holds {} and {}",
                table.declared,
                table.statics,
                record,
                self.identities.len(),
                self.statics.len()
            ));
        }
        self.frame(&Tell::Welcome { slots: table.declared }, out)?;
        let launcher = &mut self.launchers[at];
        launcher.declared = table.declared;
        launcher.bound = table.bound.clone(); // clone: a launcher's own table
        launcher.statics = table.statics;
        launcher.welcomed = true;
        Ok(())
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
            statics: 0,
            welcomed: false,
            restarting: false,
            needs: Vec::new(), // alloc: as above
            wants: Vec::new(), // alloc: as above
        });
    }

    /// A launcher's socket ended. A want of its still in flight is answered to no one.
    pub fn disconnected(&mut self, conn: Conn) {
        self.launchers.retain(|it| it.conn != conn);
    }

    fn frame(&mut self, tell: &Tell<'_>, out: &mut Vec<u8>) -> Result<(), Refusal> {
        self.said += 1;
        tell.encode(self.said, out).map_err(|why| format!("a message to a launcher: {why}"))
    }

    /// The slot of `identity`: the one it has, or the next, given now.
    fn slot_for(
        slots: &mut HashMap<ContentId, u32, Global>,
        identities: &mut Vec<ContentId>,
        identity: ContentId,
    ) -> u32 {
        *slots.entry(identity).or_insert_with(|| {
            identities.push(identity);
            (identities.len() - 1) as u32
        })
    }

    /// The numbers a bind gives for `bound`'s places: slots and statics' numbers, each
    /// `Names::Symbol` resolved in the program's symbols.
    ///
    /// A function gets its slot here, the first time a record that is sent somewhere names
    /// it: a slot is given to what code that is sent names, never to the whole program, so
    /// what a launcher declares at its start follows what it runs and not how much there is
    /// (hello world names some 14,000 functions and a run calls some 200).
    fn refs(&mut self, bound: &Bound) -> Result<Vec<u32>, Refusal> {
        let Program { symbols, slots, identities, numbers, .. } = self;
        let mut refs = Vec::with_capacity(bound.names.len()); // alloc: one bind's numbers
        for names in &bound.names {
            let names = match names {
                Names::Symbol(name) => match symbols.binary_search_by(|it| it.0.as_str().cmp(name)) {
                    Ok(at) => &symbols[at].1,
                    Err(_) => {
                        // alloc: an error
                        return Err(blaze_report::say!(
                            Words(&called(bound)),
                            " names ",
                            Code(name),
                            " as the program's own symbol, and the program defines no ",
                            Code(name),
                        )
                        .flat());
                    }
                },
                other => other,
            };
            refs.push(match names {
                Names::Function { slot, .. } => Program::slot_for(slots, identities, *slot),
                Names::Static { id, .. } => match numbers.binary_search_by_key(id, |it| it.0) {
                    Ok(at) => numbers[at].1,
                    Err(_) => {
                        // alloc: an error
                        return Err(format!("{} names the static {id:?}, which the program does not hold", called(bound)));
                    }
                },
                Names::Symbol(name) => {
                    return Err(blaze_report::say!("the program's symbol ", Code(name), " is named as another symbol").flat());
                }
            });
        }
        Ok(refs)
    }

    /// The frames that bind `slots` in one edit of launcher `at`: first the statics the
    /// program holds and it has not defined (after declaring the slots they name), then the
    /// records, then the edit, declaring every slot given since it last declared.
    fn edit(&mut self, at: usize, slots: &[u32], together: bool, out: &mut Vec<u8>) -> Result<(), Refusal> {
        let early = (self.identities.len() as u32).saturating_sub(self.launchers[at].declared);
        // alloc (each below): the definitions and binds of one edit
        let numbers: Vec<u32> = (self.launchers[at].statics..self.statics.len() as u32).collect();
        let mut static_refs = Vec::with_capacity(numbers.len());
        let mut static_bounds = Vec::with_capacity(numbers.len());
        for number in &numbers {
            let bound = Arc::clone(&self.statics[*number as usize].1); // clone: a count
            static_refs.push(self.refs(&bound)?);
            static_bounds.push(bound);
        }
        let mut bounds = Vec::with_capacity(slots.len());
        let mut refs = Vec::with_capacity(slots.len());
        for slot in slots {
            let bound = self.bound_of(*slot)?;
            refs.push(self.refs(&bound)?);
            bounds.push(bound);
        }
        // After the records' numbers were given: naming a function gives it its slot, and
        // the edit declares every slot given since this launcher last declared.
        let declared = self.launchers[at].declared;
        let declare = match self.plant {
            Plant::DeclareFirst => early,
            _ => (self.identities.len() as u32).saturating_sub(declared),
        };
        if slots.is_empty() && declare == 0 && numbers.is_empty() {
            return Ok(());
        }
        let mut left = declare;
        if !numbers.is_empty() {
            // The slots the statics name are declared before the statics are defined.
            if left > 0 {
                self.txn += 1;
                let edit = Tell::Edit { txn: Txn(self.txn), binds: List::of(&[]), declare: left, together: false };
                self.frame(&edit, out)?;
                left = 0;
            }
            for bound in &static_bounds {
                self.frame(&Tell::Record { id: bound.made.id, bytes: &bound.made.bytes }, out)?;
            }
            let defs: Vec<wire::StaticDef<'_>> = numbers
                .iter()
                .zip(&static_bounds)
                .zip(&static_refs)
                .map(|((number, bound), refs)| wire::StaticDef {
                    number: *number,
                    record: bound.made.id,
                    refs: List::of(refs),
                })
                .collect();
            self.frame(&Tell::Statics { defs: List::of(&defs) }, out)?;
        }
        if !slots.is_empty() || left > 0 {
            for bound in &bounds {
                self.frame(&Tell::Record { id: bound.made.id, bytes: &bound.made.bytes }, out)?;
            }
            let binds: Vec<wire::Bind<'_>> = slots
                .iter()
                .zip(&bounds)
                .zip(&refs)
                .map(|((slot, bound), refs)| wire::Bind {
                    slot: Slot(*slot),
                    record: bound.made.id,
                    code: CodeId(bound.made.id.0),
                    tier: Tier::Zero,
                    len: bound.made.len,
                    align: bound.made.align,
                    refs: List::of(refs),
                })
                .collect();
            self.txn += 1;
            let edit = Tell::Edit { txn: Txn(self.txn), binds: List::of(&binds), declare: left, together };
            self.frame(&edit, out)?;
        }
        let statics = self.statics.len() as u32;
        let launcher = &mut self.launchers[at];
        launcher.declared += declare;
        launcher.statics = statics;
        if launcher.bound.len() < launcher.declared as usize {
            launcher.bound.resize(launcher.declared as usize, None);
        }
        for (slot, bound) in slots.iter().zip(&bounds) {
            launcher.bound[*slot as usize] = Some(bound.made.id);
        }
        Ok(())
    }

    /// A launcher's message: `item` is one frame's item. Appends to `out` what the launcher
    /// is told for it, and says whether a first call waits for the driver.
    pub fn said(&mut self, conn: Conn, item: &[u8], out: &mut Vec<u8>) -> Result<Said, Refusal> {
        let (_, ask) = Ask::decode(item).map_err(|why| format!("a launcher's message: {why}"))?;
        let Some(at) = self.launchers.iter().position(|it| it.conn == conn) else {
            return Err(format!("launcher {} said something and is not connected", conn.0));
        };
        if self.launchers[at].restarting {
            // Told to start again: what it still says is of the program it leaves.
            return Ok(Said::Told);
        }
        match ask {
            Ask::Hello { arch, os, page, holds, running, .. } => {
                if let Some(held) = holds
                    && self.kept_check() != Some(held)
                    && running
                {
                    // A launcher that ran from an older start file: its driver joins it
                    // ([`Program::join`]) before this `Hello` is read, or it is refused.
                    if self.launchers[at].welcomed {
                        return Ok(Said::Told);
                    }
                    return Err(format!(
                        // alloc: an error
                        "a running launcher holds the start image {:?}, which is not the program's newest, and was not joined",
                        ContentId(held)
                    ));
                }
                let (Some(ours), Some(_)) = (self.arch, self.entry) else {
                    return Err("a launcher said hello and no build of its program is held".to_owned()); // alloc: an error
                };
                if arch != ours {
                    return Err(format!("a launcher of {arch:?} asked for a program built for {ours:?}"));
                }
                let of = (self.version, self.needed.len());
                if !self.welcome.as_ref().is_some_and(|it| it.of == of && it.os == os && it.page == page) {
                    // A program an image cannot hold is refused here, by name, and the
                    // launcher stops.
                    self.welcome(arch, os, page)?;
                }
                self.hello = Some((os, page));
                let current = holds.is_some() && holds == self.kept_check();
                let kept = self.welcome.as_ref().expect("the answer was made above");
                let (declared, bound, statics) = (kept.declared, kept.bound.clone(), kept.statics); // clone: a launcher's own table
                if current {
                    // The launcher started from the image it would be sent: no frame.
                    self.frame(&Tell::Welcome { slots: declared }, out)?;
                } else {
                    // The whole start as one message: what a launcher copies in.
                    out.extend_from_slice(&kept.frame);
                }
                let launcher = &mut self.launchers[at];
                launcher.declared = declared;
                launcher.bound = bound;
                launcher.statics = statics;
                launcher.welcomed = true;
            }
            Ask::Need { slot, .. } => {
                if (slot.0 as usize) >= self.identities.len() {
                    let diagnostic = format!("slot {} is past the program's {}", slot.0, self.identities.len());
                    self.frame(&Tell::Refused { slot, diagnostic: &diagnostic }, out)?;
                    return Ok(Said::Told);
                }
                if self.is_needed.len() <= slot.0 as usize {
                    self.is_needed.resize(slot.0 as usize + 1, false);
                }
                if !self.is_needed[slot.0 as usize] {
                    self.is_needed[slot.0 as usize] = true;
                    self.needed.push(slot.0);
                }
                let identity = self.identities[slot.0 as usize];
                let bind = self.functions.get(&identity).and_then(|it| it.bind);
                if self.held(slot.0).is_some() {
                    // The newest build's record of the slot, whatever build the stub fired on.
                    self.edit(at, &[slot.0], false, out)?;
                    let txn = self.txn;
                    self.launchers[at].needs.push((txn, slot.0));
                    if self.plant == Plant::WantTwice {
                        self.launchers[at].wants.push((slot.0, bind));
                        return Ok(Said::Want { slot: slot.0, identity, bind });
                    }
                    return Ok(Said::Told);
                }
                // Another thread of it called the slot first: the one answer binds it for both.
                if self.launchers[at].wants.iter().any(|it| it.0 == slot.0) {
                    return Ok(Said::Told);
                }
                self.launchers[at].wants.push((slot.0, bind));
                return Ok(Said::Want { slot: slot.0, identity, bind });
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
        Ok(Said::Told)
    }

    /// The answer to the want of `slot` of launcher `conn` ([`Said::Want`]): `piece` holds its
    /// record (a bind record decoded, or what the front end made of it and its callees).
    /// Appends to `out` the edit that binds it; a launcher that has gone or starts again is
    /// told nothing, and the piece is held all the same.
    pub fn took(&mut self, conn: Conn, slot: u32, piece: Piece, out: &mut Vec<u8>) -> Result<(), Refusal> {
        self.check(&piece, &self.symbols, true)?;
        self.apply(piece);
        let Some(at) = self.launchers.iter().position(|it| it.conn == conn) else { return Ok(()) };
        if self.launchers[at].restarting {
            return Ok(());
        }
        let Some(want) = self.launchers[at].wants.iter().position(|it| it.0 == slot) else { return Ok(()) };
        self.launchers[at].wants.remove(want);
        if self.held(slot).is_none() {
            // alloc: an error
            let diagnostic = format!(
                "the daemon answered the first call of slot {slot} with a piece that holds no code of its function"
            );
            return self.frame(&Tell::Refused { slot: Slot(slot), diagnostic: &diagnostic }, out);
        }
        self.edit(at, &[slot], false, out)?;
        let txn = self.txn;
        self.launchers[at].needs.push((txn, slot));
        Ok(())
    }

    /// The answer to the want of `slot` of launcher `conn` when its function cannot be made:
    /// the launcher is told `why` and the program stops there.
    pub fn refused(&mut self, conn: Conn, slot: u32, why: &str, out: &mut Vec<u8>) -> Result<(), Refusal> {
        let Some(at) = self.launchers.iter().position(|it| it.conn == conn) else { return Ok(()) };
        if self.launchers[at].restarting {
            return Ok(());
        }
        let Some(want) = self.launchers[at].wants.iter().position(|it| it.0 == slot) else { return Ok(()) };
        self.launchers[at].wants.remove(want);
        self.frame(&Tell::Refused { slot: Slot(slot), diagnostic: why }, out)
    }

    /// The slot identities with no bind key that the program holds a record of or a
    /// launcher waits for, sorted: what a build makes again when the front end says it
    /// changed, and gives in [`Program::started`]'s piece.
    pub fn awaited(&self) -> Vec<ContentId> {
        // alloc: once a build
        let mut awaited: Vec<ContentId> = self
            .functions
            .iter()
            .filter(|(_, it)| it.bind.is_none() && it.bound.is_some())
            .map(|(identity, _)| *identity)
            .collect();
        for launcher in &self.launchers {
            for (slot, bind) in &launcher.wants {
                if bind.is_none() {
                    awaited.push(self.identities[*slot as usize]);
                }
            }
        }
        awaited.sort_unstable();
        awaited.dedup();
        awaited
    }

    /// A build of the program: `entry` is the slot identity of its entry (`main`), `symbols`
    /// the program's own symbols (each name and what it is), `inits` the statics of its
    /// initializer sections in the order they run, `piece` its start and what it made again
    /// of [`Program::awaited`], and `changed` the slot identities whose code it made anew.
    /// Gives, for each launcher that runs the program, the frames to write to it and what
    /// they say.
    pub fn started(
        &mut self,
        entry: ContentId,
        symbols: Vec<(String, Names)>,
        inits: Vec<StaticId>,
        piece: Piece,
        changed: &[ContentId],
    ) -> Result<Vec<(Conn, Vec<u8>, Told)>, Refusal> {
        let mut symbols = symbols;
        symbols.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if let Some(twice) = symbols.windows(2).find(|it| it[0].0 == it[1].0) {
            return Err(blaze_report::say!("the program defines the symbol ", Code(&twice[0].0), " twice").flat());
        }
        self.check(&piece, &symbols, false)?;
        let given_entry = piece.functions.iter().find(|it| it.slot == entry);
        let held_entry = self.functions.get(&entry).and_then(|it| it.bound.as_ref());
        let Some(entry_bound) = given_entry.or(held_entry.map(|it| &**it)) else {
            return Err(format!("the build's start gives no code of its entry {entry:?}, and the program holds none"));
        };
        let arch = Record::decode(&entry_bound.made.bytes)
            .map_err(|why| format!("the record of the program's entry does not read: {why}"))?
            .arch;
        for id in &inits {
            if self.number(id).is_none() && !piece.statics.iter().any(|it| it.0 == *id) {
                return Err(format!("the initializer static {id:?} is in neither the build's start nor the program"));
            }
        }

        // Nothing below refuses: the build is the program's before this returns.
        let other = self.entry.is_some() && symbols_id(&symbols) != symbols_id(&self.symbols);
        if other {
            // Every record of the build before may name a place another symbol fills.
            for function in self.functions.values_mut() {
                function.bound = None;
            }
        }
        // alloc (each below): once a build
        let mut given: Vec<ContentId> = piece.functions.iter().map(|it| it.slot).collect();
        let start = given.clone(); // clone: a list of ids, the start's
        given.sort_unstable();
        for identity in changed {
            if given.binary_search(identity).is_err()
                && let Some(function) = self.functions.get_mut(identity)
            {
                function.bound = None;
            }
        }
        let applied = self.apply(piece);
        for (_, names) in &symbols {
            if let Names::Function { slot, bind, ask, .. } = names {
                self.functions.entry(*slot).or_insert(Function::NONE).named(*bind, ask);
            }
        }
        self.symbols = symbols;
        self.inits = inits.iter().map(|id| self.number(id).expect("checked above")).collect();
        let before = self.entry.replace(entry);
        self.arch = Some(arch);
        self.start = start;
        self.welcome = None;

        let mut restart: Option<String> = None;
        if self.plant != Plant::NoRestart {
            if other {
                restart = Some("the program's own symbols changed: every record may name another".to_owned()); // alloc: once
            } else if before.is_some_and(|it| it != entry) {
                restart = Some("the program's entry is another function".to_owned()); // alloc: once
            }
        }
        // The bodies that changed, each generic's instances once. The entry (the C `main`
        // the front end makes around the program's `fn main`) follows `fn main`'s code and
        // has no body of its own to edit, and its one frame is the process's first: it is
        // rebound with `fn main` and not counted.
        let mut bodies: Vec<(CodeHash, CodeHash)> = Vec::new();
        for (identity, old, new) in &applied.fresh {
            if let Some(old) = old
                && old != new
                && *identity != entry
                && !bodies.contains(&(*old, *new))
            {
                bodies.push((*old, *new));
            }
        }
        let together = !bodies.is_empty();

        let mut told = Vec::with_capacity(self.launchers.len());
        for at in 0..self.launchers.len() {
            if !self.launchers[at].welcomed || self.launchers[at].restarting {
                continue;
            }
            let conn = self.launchers[at].conn;
            let mut out = Vec::new(); // alloc: a launcher's frames
            let mut why = restart.clone(); // clone: the reason, a launcher
            if why.is_none() && self.plant != Plant::NoRestart {
                let defined = self.launchers[at].statics;
                if let Some(number) = applied.moved.iter().find(|it| **it < defined) {
                    let id = self.statics[*number as usize].0;
                    why = Some(format!("the static {id:?} has another value or type")); // alloc: once
                }
            }
            if why.is_none() {
                let bound = &self.launchers[at].bound;
                if let Some(slot) = (0..bound.len() as u32).find(|it| bound[*it as usize].is_some() && self.held(*it).is_none()) {
                    // alloc: once
                    why = Some(format!(
                        "the code of slot {slot} ({:?}) changed and the build did not make it again",
                        self.identities[slot as usize]
                    ));
                }
            }
            if let Some(why) = why {
                self.txn += 1;
                self.frame(&Tell::Restart { txn: Txn(self.txn), why: &why }, &mut out)?;
                self.launchers[at].restarting = true;
                told.push((conn, out, Told::Restart { why }));
                continue;
            }
            // The bound slots whose record is another now.
            let mut rebind = Vec::new(); // alloc: once a launcher a build
            if self.plant != Plant::NoRebind {
                for slot in 0..self.launchers[at].bound.len() as u32 {
                    let Some(bound) = self.launchers[at].bound[slot as usize] else { continue };
                    if self.held(slot).is_some_and(|it| it.made.id != bound) {
                        rebind.push(slot);
                    }
                }
            }
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

    /// The slot identity of `slot`: for the tests' oracle.
    #[cfg(test)]
    pub(crate) fn identity(&self, slot: u32) -> Option<ContentId> {
        self.identities.get(slot as usize).copied()
    }
}
