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
//! slot is given once and never taken back, and is given when a record that is sent somewhere
//! first names the function (never to the whole program up front); a launcher that runs
//! already is told to declare the new ones (`Edit::declare`).
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
//! | one function's body (every instance of one generic counts as one; the entry shim around `fn main` is not counted) | an edit: the bound slots whose records changed, switched together |
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
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use blaze_launch::image as placed;
use blaze_launch::record::Record;
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
    /// How the last build gave its tables, for a refusal.
    given: String,
}

const SPANS_MAGIC: &[u8; 8] = b"BZSPANS1";

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl Positions for Spans {
    fn of(&self, span: &blaze_format::SpanRef) -> Result<(&str, Position), String> {
        let table = self.by_anchor.as_ref().and_then(|it| it.get(&span.anchor));
        let Some(bytes) = table else {
            // alloc: an error
            return Err(format!(
                "the build gave no span table of that anchor ({} tables are held; the last build gave {})",
                self.by_anchor.as_ref().map_or(0, |it| it.len()),
                self.given
            ));
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

/// The build's instances by identity, each with its key and its code: `identities` (by key)
/// and `codes` beside it, sorted by identity. An identity is a BLAKE3 hash, so the list is
/// first dealt into 4096 runs by its leading twelve bits (in the order `ContentId` compares:
/// its bytes) and each run of a few is sorted: a pass and a half over the list where a sort
/// of twelve thousand 96-byte rows was a millisecond of every build.
fn by_identity(identities: &[(InstanceKey, ContentId)], codes: &[CodeHash]) -> Vec<(ContentId, InstanceKey, CodeHash)> {
    const RUNS: usize = 1 << 12;
    let run = |identity: &ContentId| (usize::from(identity.0[0]) << 4) | usize::from(identity.0[1] >> 4);
    // alloc (each below): once a build, let go at its end
    let mut ends = vec![0u32; RUNS + 1];
    for (_, identity) in identities {
        ends[run(identity) + 1] += 1;
    }
    for at in 1..ends.len() {
        ends[at] += ends[at - 1];
    }
    // Each run's next free place, then its end once every row is dealt.
    let mut order = vec![0u32; identities.len()];
    let mut next = ends[..RUNS].to_vec();
    for (at, (_, identity)) in identities.iter().enumerate() {
        let run = run(identity);
        order[next[run] as usize] = at as u32;
        next[run] += 1;
    }
    for run in 0..RUNS {
        order[ends[run] as usize..ends[run + 1] as usize].sort_unstable_by_key(|at| identities[*at as usize].1);
    }
    order
        .iter()
        .map(|at| {
            let (key, identity) = identities[*at as usize];
            (identity, key, codes[*at as usize])
        })
        .collect()
}

/// What a slot is keyed by, for each instance of a build: what the function is across edits
/// of its body (`identities`, by key) together with what a caller's code assumes of it
/// beyond its address (`abis`, by key: `blaze_codegen::backend::Described::abi`). A slot is
/// a path and an ABI (launcher.md, section 1): a function whose signature changes is another
/// slot, so the body of the old signature stays where the frames of its old callers call
/// it, and every caller, whose code changed with the signature, names the new one. An
/// instance the build gives no ABI of (a folded group's entry; a front end that tells none)
/// is keyed by what it is alone.
fn slot_identities(
    identities: &[(InstanceKey, ContentId)],
    abis: &[(InstanceKey, ContentId)],
    plant: Plant,
) -> Vec<(InstanceKey, ContentId)> {
    let mut at = 0;
    identities
        .iter()
        .map(|(key, identity)| {
            // Both by key: one pass over the two.
            while abis.get(at).is_some_and(|it| it.0 < *key) {
                at += 1;
            }
            match abis.get(at) {
                Some((of, abi)) if of == key && plant != Plant::SlotAcrossAbi => {
                    let mut hasher = Hasher::new("blaze v2 launch slot 1");
                    hasher.id(identity).id(abi);
                    (*key, hasher.finish())
                }
                _ => (*key, *identity),
            }
        })
        .collect() // alloc: the image's list, once a build
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

/// The answer to a `Hello`, kept ([`Program::welcome`]).
struct Welcome {
    /// `Program::builds` and `Program::needed.len()` when it was made.
    of: (u64, usize),
    /// The system and the page size of the launcher it was made for.
    os: wire::Os,
    page: u32,
    frames: Vec<u8>,
    declared: u32,
    bound: Vec<Option<RecordId>>,
    statics: Vec<StaticId>,
    /// The slots the frames bind before the program starts, in their order, and the entry's.
    prefetch: Vec<u32>,
    entry: u32,
    /// The same start as one message.
    start: Start,
}

/// A kept answer as one message, an image of placed code (`blaze_launch::image`): what the
/// frames leave in a launcher's memory, made once and copied in by every launcher after the
/// first. The first launcher of a build is sent the frames, so an edit's first run waits
/// for no image; the second asks for it.
enum Start {
    /// No second launcher asked yet.
    Unasked,
    /// The frame of the `Tell::Image`.
    Image(Vec<u8>),
    /// There is none, and why (the case's name, then what of the program): every launcher
    /// of this answer is sent the frames.
    Frames(String),
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
    /// The records made so far, by slot, each with the compiled function it was made of: a
    /// record is that function's for as long as the build holds that very function (a build
    /// shares with the one before it every function nothing changed, and makes another of
    /// one whose callee has another key) and no span table it read changed.
    made: HashMap<u32, (Arc<CompiledFunction>, Arc<Made>), Global>,
    launchers: Vec<Launcher>,
    /// The slots a launcher's stub ever asked for, in the order they were first asked: what
    /// the next launcher is bound to before it starts (launcher.md, step 4: the prefetch of
    /// the last run's set), so a second run asks for a function only where it goes somewhere
    /// the runs before it did not.
    needed: Vec<u32>,
    /// By slot: whether it is in `needed`.
    is_needed: Vec<bool>,
    /// How many builds the program was given: which image a kept answer is of.
    builds: u64,
    /// The answer to the last `Hello`, kept while it is still the answer: the image and the
    /// needed set it was made of, its frames, and what a launcher that carries them out has
    /// bound. A second run of an unchanged program is then told a copy of bytes (the
    /// answer was 0.8 to 1.0 ms of a launcher's 3.2 ms start, measured 2026-11-12).
    welcome: Option<Welcome>,
    /// Why the last answer has no image of placed code, until the driver takes it to say
    /// ([`Program::unimaged`]).
    unimaged: Option<String>,
    /// How many launchers were started from an image, and how many from records where an
    /// answer was kept and had no image: a program that never gets one is seen by the second.
    image_starts: u64,
    record_starts: u64,
    /// Why launchers of the build before were started from records, until the driver takes
    /// it for the next build's note ([`Program::from_records`]).
    from_records: Option<String>,
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
    /// An edit counts the slots it declares before its records name their callees.
    DeclareFirst,
    /// An image of a start leaves out the last body its frames bind.
    ShortImage,
    /// A slot is keyed by what its function is alone, not also by its ABI: a function whose
    /// signature changed keeps its slot.
    SlotAcrossAbi,
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
            builds: 0,
            welcome: None,
            unimaged: None,
            image_starts: 0,
            record_starts: 0,
            from_records: None,
            txn: 0,
            said: 0,
            plant: Plant::None,
        }
    }

    /// Why the answer to the last `Hello` could not be made one message (an image of placed
    /// code), once: the launchers of that answer are sent its frames, which is the same
    /// start and slower, so the driver says it where a person reads what was slow.
    pub fn unimaged(&mut self) -> Option<String> {
        self.unimaged.take()
    }

    /// How many launchers this program started from an image of placed code, and how many
    /// from records where it could have been an image and was not.
    pub fn starts(&self) -> (u64, u64) {
        (self.image_starts, self.record_starts)
    }

    /// Why this program's launchers were last started from records, not an image, taken: for
    /// the note of its next build, which a person reads.
    pub fn from_records(&mut self) -> Option<String> {
        self.from_records.take()
    }

    /// `kept`'s start as one message: the image of what its frames place, for a launcher of
    /// `arch`, as a `Tell::Image` frame. A refusal says what of the program an image cannot
    /// hold (`blaze_launch::image::lay`).
    fn start_image(&mut self, arch: wire::Arch, kept: &Welcome) -> Result<Vec<u8>, Refusal> {
        let (statics, inits): (Vec<Made>, Vec<u32>) = match &self.image {
            // clone: each static's record and a few numbers, once an image
            Some(image) => (image.statics.iter().map(|it| it.1.clone()).collect(), image.inits.clone()),
            None => return Err("no build of the program is held".to_owned()), // alloc: an error
        };
        // alloc (each list below): once an image
        let mut refs: Vec<Vec<u32>> = Vec::with_capacity(statics.len() + kept.prefetch.len());
        for made in &statics {
            refs.push(self.refs(made)?);
        }
        let prefetch = match self.plant {
            Plant::ShortImage => &kept.prefetch[..kept.prefetch.len().saturating_sub(1)],
            _ => &kept.prefetch[..],
        };
        let mut functions: Vec<Arc<Made>> = Vec::with_capacity(prefetch.len());
        for slot in prefetch {
            let made = self.record(*slot)?;
            refs.push(self.refs(&made)?);
            functions.push(made);
        }
        // The frames gave every function these records name its slot: none is given here.
        if self.identities.len() as u32 != kept.declared {
            return Err(format!(
                "the kept answer declares {} slots and its records now name {}",
                kept.declared,
                self.identities.len()
            ));
        }
        let defs: Vec<wire::StaticDef<'_>> = statics
            .iter()
            .enumerate()
            .zip(&refs)
            .map(|((number, made), refs)| wire::StaticDef {
                number: number as u32,
                record: made.id,
                refs: List::of(refs),
            })
            .collect();
        let binds: Vec<wire::Bind<'_>> = prefetch
            .iter()
            .zip(&functions)
            .zip(&refs[statics.len()..])
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
        let mut decoded: Vec<(RecordId, Record<'_>)> = Vec::with_capacity(statics.len() + functions.len());
        for made in statics.iter().chain(functions.iter().map(|it| &**it)) {
            let record = Record::decode(&made.bytes).map_err(|why| format!("a record of the program's: {why}"))?;
            decoded.push((made.id, record));
        }
        decoded.sort_unstable_by_key(|it| it.0.0);
        let records =
            |id: &RecordId| decoded.binary_search_by_key(&id.0, |it| it.0.0).ok().map(|at| decoded[at].1);
        let input = placed::Input {
            arch,
            os: kept.os,
            page: u64::from(kept.page),
            slots: kept.declared,
            statics: &defs,
            binds: &binds,
            entry: Slot(kept.entry),
            inits: &inits,
        };
        // The case's name first: what a count and a person tell the cases apart by.
        let image = placed::lay(&input, &records, &placed::Bases::NONE)
            .map_err(|refused| format!("{}: {}", refused.why.name(), refused.what))?;
        let mut bytes = Vec::new();
        image.encode(&mut bytes);
        let mut frame = Vec::new();
        self.frame(&Tell::Image { bytes: &bytes }, &mut frame)
            .map_err(|why| format!("{}: {why}", placed::Why::Long.name()))?;
        Ok(frame)
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

    /// The record of `slot`'s function in the newest build, made once a build.
    fn record(&mut self, slot: u32) -> Result<Arc<Made>, Refusal> {
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
        let function = &image.functions[function];
        if let Some((of, made)) = self.made.get(&slot)
            && Arc::ptr_eq(of, function)
        {
            return Ok(Arc::clone(made));
        }
        let name = match image.names.binary_search_by_key(&image.by_identity[at].1, |it| it.0) {
            Ok(at) => &image.names[at].1[..],
            Err(_) => "<unnamed>",
        };
        let made = Arc::new(record::function(function, name, &self.spans)?);
        self.made.insert(slot, (Arc::clone(function), Arc::clone(&made)));
        Ok(made)
    }

    /// The numbers a bind gives for `made`'s places: slots and statics' numbers.
    ///
    /// A function gets its slot here, the first time a record names it: a slot is given to
    /// what code that is sent somewhere names, never to the whole program, so what a launcher
    /// declares at its start follows what it runs and not how much there is (hello world
    /// names some 14,000 functions and a run calls some 200; declaring every one was a tenth
    /// of a launcher's start).
    fn refs(&mut self, made: &Made) -> Result<Vec<u32>, Refusal> {
        let Program { image, slots, identities, .. } = self;
        let Some(image) = image else {
            return Err("no build of the program is held".to_owned()); // alloc: an error
        };
        made.named
            .iter()
            .map(|named| match named {
                Named::Function(key) => image
                    .identities
                    .binary_search_by_key(key, |it| it.0)
                    .map(|at| Program::slot_for(slots, identities, image.identities[at].1))
                    .map_err(|_| {
                        format!("{key:?} is named by the program's code and is no instance of its build")
                    }),
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
            let made = {
                let Some(image) = &self.image else {
                    return Err("no build of the program is held".to_owned()); // alloc: an error
                };
                image.statics[*number as usize].1.clone() // clone: a static's record, into the frame's builder
            };
            let (id, bytes, made_refs) = (made.id, made.bytes.clone(), self.refs(&made)?); // clone: as above
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
        let early = (self.identities.len() as u32).saturating_sub(self.launchers[launcher].declared);
        // alloc: the binds of one edit
        let mut mades = Vec::with_capacity(slots.len());
        let mut refs = Vec::with_capacity(slots.len());
        for slot in slots {
            let made = self.record(*slot)?;
            refs.push(self.refs(&made)?);
            self.frame(&Tell::Record { id: made.id, bytes: &made.bytes }, out)?;
            mades.push(made);
        }
        // After the records' numbers were given: naming a function gives it its slot, and
        // the edit declares every slot given since this launcher last declared.
        let declared = self.launchers[launcher].declared;
        let declare = match self.plant {
            Plant::DeclareFirst => early,
            _ => (self.identities.len() as u32).saturating_sub(declared),
        };
        if slots.is_empty() && declare == 0 {
            return Ok(());
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
            Ask::Hello { arch, os, page, .. } => {
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
                let of = (self.builds, self.needed.len());
                if self.welcome.as_ref().is_some_and(|it| it.of == of && it.os == os && it.page == page) {
                    let Some(mut kept) = self.welcome.take() else { return Ok(()) };
                    // A second launcher of this answer: its start as one message, made now
                    // and kept, or why there is none.
                    if let Start::Unasked = kept.start {
                        kept.start = match self.start_image(arch, &kept) {
                            Ok(frame) => Start::Image(frame),
                            Err(why) => Start::Frames(why),
                        };
                    }
                    match &kept.start {
                        Start::Image(frame) => {
                            out.extend_from_slice(frame);
                            self.image_starts += 1;
                        }
                        // Said each time: a program that is never started from an image is
                        // slow at every start, not once.
                        Start::Frames(why) => {
                            out.extend_from_slice(&kept.frames);
                            self.record_starts += 1;
                            self.unimaged = Some(why.clone()); // clone: the reason, a start
                            self.from_records = Some(why.clone()); // clone: as above
                        }
                        Start::Unasked => out.extend_from_slice(&kept.frames),
                    }
                    let launcher = &mut self.launchers[at];
                    launcher.declared = kept.declared;
                    launcher.bound = kept.bound.clone(); // clone: a launcher's own table
                    launcher.statics = kept.statics.clone(); // clone: as above
                    launcher.welcomed = true;
                    self.welcome = Some(kept);
                    return Ok(());
                }
                let began = out.len();
                let numbers: Vec<u32> = (0..image.statics.len() as u32).collect(); // alloc: once a launcher
                let statics: Vec<StaticId> = image.statics.iter().map(|it| it.0).collect(); // alloc: as above
                let inits = image.inits.clone(); // clone: a few numbers, once a launcher
                let entry_identity = image.entry;
                let entry = Program::slot_for(&mut self.slots, &mut self.identities, entry_identity);
                // What the runs before this one called, bound before it starts: those of
                // them the newest build still has code for.
                let prefetch: Vec<u32> = // alloc: once a launcher
                    self.needed.clone().into_iter().filter(|slot| self.record(*slot).is_ok()).collect(); // clone: a list of numbers, once a launcher
                // Every slot the statics and those records name is given first, so that the
                // welcome declares them all and nothing below names one not declared.
                let mut body = Vec::new(); // alloc: once a launcher, the frames after the welcome
                self.define(&numbers, &mut body)?;
                for slot in &prefetch {
                    let made = self.record(*slot)?;
                    self.refs(&made)?;
                }
                let slots = self.identities.len() as u32;
                self.frame(&Tell::Welcome { slots }, out)?;
                out.extend_from_slice(&body);
                {
                    let launcher = &mut self.launchers[at];
                    launcher.declared = slots;
                    launcher.bound = vec![None; slots as usize]; // alloc: a launcher's table
                    launcher.statics = statics;
                    launcher.welcomed = true;
                }
                self.edit(at, &prefetch, false, out)?;
                self.frame(&Tell::Start { entry: Slot(entry), inits: List::of(&inits) }, out)?;
                let launcher = &self.launchers[at];
                self.welcome = Some(Welcome {
                    of,
                    os,
                    page,
                    prefetch,
                    entry,
                    start: Start::Unasked,
                    frames: out[began..].to_vec(), // alloc: the kept answer, once an image
                    declared: launcher.declared,
                    bound: launcher.bound.clone(),     // clone: the kept answer's table
                    statics: launcher.statics.clone(), // clone: as above
                });
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
                self.spans.given = format!("every table, {} of them", all.len()); // alloc: once a build
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
                // alloc: once a build
                self.spans.given = format!("{} changed tables and {} removed", tables.len(), removed.len());
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
        // alloc: the image's lists, once a build (each a copy of a list of the build's)
        // What each instance's slot is keyed by: what it is, and its ABI.
        let slotted = slot_identities(&built.identities, &built.abis, self.plant);
        // Each identity's code, in the identities' order: both lists are by key, so one pass
        // over the two.
        let mut codes: Vec<CodeHash> = Vec::with_capacity(slotted.len());
        let mut owner = 0;
        for (key, _) in &slotted {
            while built.owners.get(owner).is_some_and(|it| it.0 < *key) {
                owner += 1;
            }
            match built.owners.get(owner) {
                Some(it) if it.0 == *key => codes.push(it.2),
                _ => return Err(format!("{key:?} has an identity and no owner in its build")),
            }
        }
        let by_identity = by_identity(&slotted, &codes);
        let Some(entry_key) = built.symbols.iter().find(|it| it.1 == entry).map(|it| it.0) else {
            return Err(format!("the build defines no `{entry}`: it is no program"));
        };
        let entry_identity = built
            .identities
            .binary_search_by_key(&entry_key, |it| it.0)
            .map(|at| slotted[at].1)
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
        // Nothing below refuses: the build before is taken, and the new one is the program's
        // before this returns. A name the build before had under the same key is moved, not
        // copied: a body edit changes one key of some twelve thousand.
        let mut before = self.image.take();
        let mut was = before.as_mut().map(|it| std::mem::take(&mut it.names)).unwrap_or_default().into_iter().peekable();
        let mut names: Vec<(InstanceKey, String)> = Vec::with_capacity(built.debug_names.len());
        for (key, name) in &built.debug_names {
            while was.next_if(|it| it.0 < *key).is_some() {}
            match was.next_if(|it| it.0 == *key && it.1 == *name) {
                Some(kept) => names.push(kept),
                None => names.push((*key, name.clone())), // clone: a name the build before had not
            }
        }
        drop(was);
        let image = Image {
            functions: built.functions.clone(), // clone: a count a function
            merged: built.deduped.merged.iter().map(|(from, to)| (*from, *to)).collect(),
            identities: slotted,
            by_identity,
            names,
            statics,
            inits,
            entry: entry_identity,
            arch,
        };

        // What changed, against the build before.
        self.builds += 1;
        self.welcome = None;
        self.image = Some(image);
        let image = self.image.as_ref().expect("just set");
        let mut restart: Option<String> = None;
        let mut bodies: Vec<(CodeHash, CodeHash, ContentId)> = Vec::new(); // alloc: once a build
        if let Some(before) = &before {
            if before.entry != image.entry {
                restart = Some("the program's entry is another function".to_owned()); // alloc: once
            }
            // Both by identity: one pass over the two.
            let mut was = 0;
            for (identity, _, code) in &image.by_identity {
                while before.by_identity.get(was).is_some_and(|it| it.0 < *identity) {
                    was += 1;
                }
                let Some((_, _, old_code)) = before.by_identity.get(was).copied().filter(|it| it.0 == *identity)
                else {
                    continue;
                };
                // The entry (the C `main` the front end makes around the program's `fn
                // main`) follows `fn main`'s code and has no body of its own to edit, and its
                // one frame is the process's first: it is rebound with `fn main` and not
                // counted as a second changed body.
                let counted = *identity != image.entry
                    && old_code != *code
                    && !bodies.iter().any(|it| it.0 == old_code && it.1 == *code);
                if counted {
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
            // With the front end's ABI of each function a slot is keyed by it, so each of
            // these bodies has the signature it had and all are rebound together below. A
            // build that tells no ABI cannot say so, and more than one body starts again.
            if bodies.len() > 1 && built.abis.is_empty() && self.plant != Plant::NoRestart {
                let name = |identity: &ContentId| {
                    let at = image.by_identity.binary_search_by_key(identity, |it| it.0).ok()?;
                    let key = image.by_identity[at].1;
                    let at = image.names.binary_search_by_key(&key, |it| it.0).ok()?;
                    Some(image.names[at].1.clone()) // clone: a name into the reason
                };
                let named: Vec<String> = bodies.iter().take(3).filter_map(|it| name(&it.2)).collect();
                restart = Some(format!(
                    "{} functions' bodies changed together ({}{}) in a build that tells no function's ABI: a \
                     changed signature cannot be told from body edits, so only one changed body is applied to a \
                     running program",
                    bodies.len(),
                    named.join(", "),
                    if bodies.len() > named.len() { ", .." } else { "" },
                ));
            }
        }

        // The records that read a span table that changed are let go (with every table given
        // anew, all of them); one whose function is another now is made again when asked
        // for (`record`).
        match &changed_anchors {
            None => self.made.clear(),
            Some(changed) => {
                self.made.retain(|_, (_, made)| {
                    !made.anchors.iter().any(|it| changed.binary_search(it).is_ok())
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
            // The slots the new statics name are declared before the statics are defined.
            let mut defined = Vec::new(); // alloc: once a launcher a build
            self.define(&added, &mut defined)?;
            self.edit(at, &[], false, &mut out)?;
            out.extend_from_slice(&defined);
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
