//! The kernel: blaze v2's compute steps from MIR down (docs/v2/plan/design/kernel.md).
//!
//! A step is a registered, pure function of its input ([`Step`], [`Compute`]). It sees the
//! world only through [`Reads`], which records each read it makes: its name (a [`Read`], a kind
//! and the key's canonical bytes) and the hash the read saw ([`Seen`]). The value is kept in
//! blaze-cas as an [`Entry`] under the step's [`Identity`] (what it is of, the step's
//! [`CodeId`], its flags) and those reads. A later lookup takes it only when every recorded
//! read hashes the same now. That is a verifying trace (Mokhov, Mitchell and Peyton Jones,
//! "Build systems à la carte", 2018).
//!
//! - **Beside the front end, under codegen.** The front end keeps its own engine
//!   (`blaze_query::db`, derived from salsa): dense ids, in memory, one view at a time. A kernel
//!   entry is keyed by content, persisted, and shared by every process and checkout, so its
//!   reads are narrow facts named by stable keys (an item key's layout, not the item). A
//!   [`World`] that the wiring crate implements over the front end answers them. The kernel
//!   does not depend on `blaze-query`.
//! - **Identity first.** An entry with no reads is kept under [`whole::key`] alone, as its bare
//!   value: one probe, no header. A step that reads keeps the names of its reads (a [`Shape`])
//!   in a bounded chain of slots under its identity ([`MOST_SHAPES`]). Lookup hashes each
//!   shape's reads now and probes the entry those hashes name.
//! - **Refusals, not overwrites.** Two values under one identity and set of reads is a
//!   [`KernelError::Conflict`] (rule 21). A run whose read failed, or which saw two hashes for
//!   one read, keeps nothing.
//! - **Deterministic** (rule 25): an entry keeps its reads sorted by kind and key, each once.
//! - **Checked** (lane verify, `blaze_store_check`): a hit that `BLAZE_VERIFY_STORE` samples is
//!   computed again with fresh reads, and it must make the same reads and the same bytes.

#![feature(allocator_api, register_tool)]
#![register_tool(blaze)]

use std::alloc::Global;
use std::any::TypeId;
use std::fmt;

use blaze_cas::{Key, Kind};
use blaze_ids::{ContentId, Hasher};
use blaze_store_check::{Hit, Recompute, RecordedRead, Setting, VerifyError};

mod entry;
mod reads;
mod store;
#[cfg(test)]
mod tests;
pub mod why;

pub use crate::entry::{
    Entry, EntryKey, EntryReads, Shape, ShapeReads, entry_key, shape_slot, whole,
};
pub use crate::reads::{Reads, Trace};
pub use crate::store::{Get, Put, Reading, Writing};
pub use crate::why::{Miss, Missed, Part};

/// The most shapes one identity's chain holds. A step whose reads differ by shape more often
/// than this reads too much of what varies, so its keys should be split. Reaching the bound is
/// [`KernelError::TooManyShapes`], never an endless walk (rule 18).
pub const MOST_SHAPES: u32 = 1024;

/// Every step's number, in one table so that no two crates take the same one (registration
/// refuses a second step of one number or one name too).
pub mod steps {
    use crate::StepId;

    /// blaze-codegen's dev template of a function instance (`blaze_codegen::kernel::DevTemplate`).
    pub const DEV_TEMPLATE: StepId = StepId(1);
}

/// A compute step, by number: one number is one step for every blaze ([`steps`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StepId(pub u16);

/// The code identity of a step: its `compute` and everything that calls, as lane identity
/// hashes it. Until lane identity gives one, a wiring passes what it has (codegen passes
/// `Toolchain::id`: the pinned nightly and the blaze executable's hash).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodeId(pub ContentId);

/// The parts of an [`Identity`] that a step's input gives.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Subject {
    /// The stable identity of what is computed (lane identity's identity of a function,
    /// instance or constant). Never an interned id or a path in the checkout.
    pub of: ContentId,
    /// Everything else, outside the reads, that changes the value: target, settings.
    pub flags: ContentId,
}

/// What an entry is of: what lookup starts from, before any read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Identity {
    pub step: StepId,
    pub of: ContentId,
    pub code: CodeId,
    pub flags: ContentId,
}

const IDENTITY: &str = "blaze v2 kernel identity 1";

impl Identity {
    /// Its content ID: what its entries and shape slots are keyed by.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new(IDENTITY);
        hasher.update(&self.step.0.to_le_bytes()).id(&self.of).id(&self.code.0).id(&self.flags);
        hasher.finish()
    }
}

/// The identity of `S`'s value of `input` when its code is `code`. [`Kernel::identity`] gives
/// it with the code `S` was registered with.
pub fn identity<S: Step>(code: CodeId, input: &S::Input) -> Identity {
    let Subject { of, flags } = S::subject(input);
    Identity { step: S::ID, of, code, flags }
}

/// A kind of read, by number. Its [`World`] names it ([`World::describe`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ReadKind(pub u16);

/// One read: its kind and the canonical bytes of its key (content IDs and item keys; no dense
/// id, no absolute path). It is narrow: the layout of `Foo`, not `Foo`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Read<'a> {
    pub kind: ReadKind,
    pub key: &'a [u8],
}

/// What a read names now.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Seen {
    Is(ContentId),
    /// The key names nothing now (the item was deleted or renamed).
    Gone,
}

impl Seen {
    /// The hash a check names it by: all zeros when gone.
    pub fn hash(self) -> ContentId {
        match self {
            Seen::Is(hash) => hash,
            Seen::Gone => ContentId::default(),
        }
    }
}

/// The world could not answer a read. This is never a [`Seen::Gone`]: a read that fails keeps
/// nothing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorldError {
    pub kind: ReadKind,
    pub key: Vec<u8>,
    pub why: String,
}

impl WorldError {
    pub fn new(read: Read<'_>, why: impl Into<String>) -> WorldError {
        // alloc: an error
        WorldError { kind: read.kind, key: read.key.to_vec(), why: why.into() }
    }

    pub fn read(&self) -> Read<'_> {
        Read { kind: self.kind, key: &self.key }
    }
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "read {} of {} failed: {}", self.kind.0, hex(&self.key), self.why)
    }
}

/// The outside world, as hashes. The crate that wires a step implements it, over the front
/// end's facts, the file system, and the rest.
///
/// `hash` must give the [`Seen`] that [`Fact::get`] gives for the same read. Lookup trusts it,
/// and a sampled check (lane verify) finds out when the two disagree.
pub trait World {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError>;

    /// The read as an error or a check names it, e.g. "layout of `core::option::Option<u8>`".
    /// Rendered only for an error or a sampled check, never on a hit's path.
    fn describe(&self, read: Read<'_>) -> String {
        // alloc: an error's or a check's text
        format!("read {} of {}", read.kind.0, hex(read.key))
    }

    /// The item the read is of, by path (the type whose layout it is, the callee whose
    /// identity it is), for a reason's chain to follow ([`why::Missed::Read`]). Asked only on a
    /// miss.
    fn names(&self, _read: Read<'_>) -> Option<String> {
        None
    }
}

/// A typed read: what a step asks [`Reads::get`] for, and how its key is written.
pub trait Fact<W: ?Sized> {
    const KIND: ReadKind;
    type Key: ?Sized;
    type Value<'w>
    where
        W: 'w;

    /// Appends the key's canonical bytes.
    fn key(key: &Self::Key, out: &mut Vec<u8>);

    /// The value and the hash of what it is. A key that names nothing now is `Seen::Gone`,
    /// with a value saying so.
    fn get<'w>(world: &'w W, key: &Self::Key) -> Result<(Self::Value<'w>, Seen), WorldError>;
}

/// A registered step: what its identity is made of. Every method of every impl must be pure
/// (lane pure's `#[blaze::pure]` on a trait checks each one).
#[blaze::pure]
pub trait Step: 'static {
    const ID: StepId;
    /// Its name, as errors give it (`codegen dev template`).
    const NAME: &'static str;
    /// The store kind of its entries.
    const KIND: Kind;
    type Input: ?Sized;

    /// The input's stable identity and flags. The kernel adds the step and its code.
    fn subject(input: &Self::Input) -> Subject;

    /// What an error or a check calls the input (a function's path). Never on a hit's path.
    fn label(input: &Self::Input) -> String;
}

/// A step the kernel computes. A step whose value is computed elsewhere (codegen's batched
/// templates) is a [`Step`] only, kept through [`Kernel::find_whole`], [`Kernel::keep_whole`]
/// or [`whole::key`].
#[blaze::pure]
pub trait Compute: Step {
    type World: World + ?Sized;

    /// The value of `input`, from what `ctx` reads and nothing else.
    fn compute(ctx: &mut Reads<'_, Self::World>, input: &Self::Input)
    -> Result<Vec<u8>, StepError>;
}

/// Why a step's run made no value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StepError {
    /// A read failed (`?` on [`Reads::get`]).
    World(WorldError),
    /// The step refused its input, naming why.
    Refused(String),
}

impl From<WorldError> for StepError {
    fn from(error: WorldError) -> StepError {
        StepError::World(error)
    }
}

/// What [`Kernel::get`] gave.
#[derive(Debug)]
pub enum Got<'s> {
    /// The store's entry, read in place.
    Hit(Entry<'s>),
    /// Computed now and kept under `key`, with the reads its entry records (sorted by kind
    /// and key, each once): what the invalidation oracle's trace takes of a computed entry
    /// (`blaze_v2_proof::kernel`), as it takes [`Entry::reads`] of a hit. Why the lookup found
    /// nothing is [`Kernel::why`]'s, asked only by whoever wants it.
    Computed { key: Key, value: Vec<u8>, reads: Trace },
}

impl Got<'_> {
    pub fn value(&self) -> &[u8] {
        match self {
            Got::Hit(entry) => entry.value,
            Got::Computed { value, .. } => value,
        }
    }

    pub fn is_hit(&self) -> bool {
        matches!(self, Got::Hit(_))
    }
}

/// Which value an error is about: the step, the input as its step names it, and the identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Of {
    pub step: &'static str,
    pub label: String,
    pub identity: ContentId,
}

impl fmt::Display for Of {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} of `{}` (identity {:x})", self.step, self.label, self.identity)
    }
}

/// Why the kernel refused. Each variant names the step, the input and the identity; text
/// that needs the world is rendered while the world is at hand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KernelError {
    /// A step used before it was registered, or a type other than the one registered under
    /// its number.
    Unregistered { step: StepId, name: &'static str, holder: Option<&'static str> },
    /// Two steps of one number or one name.
    Twice { first: (StepId, &'static str), second: (StepId, &'static str) },
    /// The store could not read or keep a value.
    Store { of: Of, key: Key, why: String },
    /// A read failed: nothing was kept.
    World { of: Of, read: String, why: String },
    /// One read gave two hashes in one run: the world changed under the step, so nothing was kept.
    Unsteady { of: Of, read: String, first: Seen, then: Seen },
    /// The store has other bytes for this identity and set of reads (rule 21). Either the
    /// identity misses an input the value depends on, or the step is not deterministic.
    Conflict { of: Of, key: Key, reads: Vec<String>, kept: ContentId, given: ContentId },
    /// The identity's chain of shapes is full ([`MOST_SHAPES`]).
    TooManyShapes { of: Of },
    /// The step refused its input.
    Compute { of: Of, why: String },
    /// A sampled hit computed again to other reads or other bytes.
    Verify(VerifyError),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::Unregistered { step, name, holder: None } => {
                write!(f, "the kernel has no step `{name}` (step {}): register it first", step.0)
            }
            KernelError::Unregistered { step, name, holder: Some(holder) } => write!(
                f,
                "step {} is `{holder}` in the kernel, not `{name}`: one number is one step (blaze_kernel::steps)",
                step.0
            ),
            KernelError::Twice { first, second } => write!(
                f,
                "two kernel steps share a number or a name: `{}` (step {}) and `{}` (step {})",
                first.1, first.0.0, second.1, second.0.0
            ),
            KernelError::Store { of, key, why } => {
                write!(f, "{of}: the store at {:x}: {why}", key.0)
            }
            KernelError::World { of, read, why } => {
                write!(f, "{of}: the read {read} failed, so nothing was kept: {why}")
            }
            KernelError::Unsteady { of, read, first, then } => write!(
                f,
                "{of}: the read {read} saw {:x} and then {:x} in one run: the world changed under the step, so nothing was kept",
                first.hash(),
                then.hash()
            ),
            KernelError::Conflict { of, key, reads, kept, given } => write!(
                f,
                "{of}: the store holds other bytes ({kept:x}) than this run made ({given:x}) under {:x}, for the same identity and reads [{}]: the identity misses an input the value depends on, or the step is not deterministic",
                key.0,
                reads.join(", ")
            ),
            KernelError::TooManyShapes { of } => write!(
                f,
                "{of}: {MOST_SHAPES} shapes of reads are kept under this identity, the most one holds: the step reads too much of what varies"
            ),
            KernelError::Compute { of, why } => write!(f, "{of}: {why}"),
            KernelError::Verify(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for KernelError {}

#[derive(Clone, Copy, Debug)]
struct Registered {
    name: &'static str,
    code: CodeId,
    type_id: TypeId,
}

/// The registry of steps, each with its code. It is built at setup and shared (`&Kernel`)
/// after that.
pub struct Kernel {
    steps: blaze_hash::HashMap<StepId, Registered, Global>,
}

impl Default for Kernel {
    fn default() -> Kernel {
        Kernel::new()
    }
}

impl Kernel {
    pub fn new() -> Kernel {
        // alloc: the registry, once a process, a few entries
        Kernel { steps: blaze_hash::HashMap::new_in(Global) }
    }

    /// Registers `S` with its code. A second step of one number or one name is refused.
    pub fn register<S: Step>(&mut self, code: CodeId) -> Result<(), KernelError> {
        if let Some(had) = self.steps.get(&S::ID) {
            return Err(KernelError::Twice { first: (S::ID, had.name), second: (S::ID, S::NAME) });
        }
        if let Some((step, had)) = self.steps.iter().find(|(_, it)| it.name == S::NAME) {
            return Err(KernelError::Twice { first: (*step, had.name), second: (S::ID, S::NAME) });
        }
        // alloc: the registry, once a step
        self.steps.insert(S::ID, Registered { name: S::NAME, code, type_id: TypeId::of::<S>() });
        Ok(())
    }

    /// The code `S` was registered with.
    pub fn code<S: Step>(&self) -> Result<CodeId, KernelError> {
        match self.steps.get(&S::ID) {
            Some(it) if it.type_id == TypeId::of::<S>() => Ok(it.code),
            Some(it) => {
                Err(KernelError::Unregistered { step: S::ID, name: S::NAME, holder: Some(it.name) })
            }
            None => Err(KernelError::Unregistered { step: S::ID, name: S::NAME, holder: None }),
        }
    }

    pub fn identity<S: Step>(&self, input: &S::Input) -> Result<Identity, KernelError> {
        Ok(identity::<S>(self.code::<S>()?, input))
    }

    /// The entry of `input`, else `S` computed now and kept. A hit that `verify` samples is
    /// computed again with fresh reads, and its reads and bytes must equal the entry's.
    pub fn get<'s, S: Compute, P: Put + ?Sized>(
        &self,
        store: &'s mut P,
        world: &S::World,
        input: &S::Input,
        verify: &Setting,
    ) -> Result<Got<'s>, KernelError> {
        let identity = self.identity::<S>(input)?;
        if let Some(key) = find_key::<S, P>(&*store, world, input, &identity)? {
            let store: &'s P = store;
            let entry = read_entry::<S, P>(store, key, input, &identity)?;
            check::<S>(verify, world, input, &entry)?;
            return Ok(Got::Hit(entry));
        }
        let (value, trace) = run::<S>(world, input, &identity)?;
        let key = keep::<S, P>(store, world, input, &identity, &trace, &value)?;
        Ok(Got::Computed { key, value, reads: trace })
    }

    /// Why the store has no entry of `input` whose reads all hash the same now, against `last`:
    /// the key of the entry the caller saw kept last under its identity ([`Got::Computed`]'s,
    /// remembered as [`Miss::kept`]), none when it saw none. For a lookup that found nothing;
    /// worked out by reading the store and the world again, so only when it is wanted, and it
    /// writes nothing.
    pub fn why<S: Compute, G: Get + ?Sized>(
        &self,
        store: &G,
        world: &S::World,
        input: &S::Input,
        last: Option<Key>,
    ) -> Result<Missed, KernelError> {
        Ok(why::explain(store, world, &self.identity::<S>(input)?, last))
    }

    /// The entry of `input` whose reads all hash the same now, if the store has one.
    pub fn find<'s, S: Compute, G: Get + ?Sized>(
        &self,
        store: &'s G,
        world: &S::World,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        let identity = self.identity::<S>(input)?;
        match find_key::<S, G>(store, world, input, &identity)? {
            Some(key) => Ok(Some(read_entry::<S, G>(store, key, input, &identity)?)),
            None => Ok(None),
        }
    }

    /// The entry with no reads of a step computed elsewhere, if the store has one.
    pub fn find_whole<'s, S: Step, G: Get + ?Sized>(
        &self,
        store: &'s G,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        let identity = self.identity::<S>(input)?;
        let key = whole::key(&identity);
        match store.get(&key) {
            Ok(Some(bytes)) => Ok(Some(Entry::whole(bytes, identity))),
            Ok(None) => Ok(None),
            Err(why) => Err(KernelError::Store { of: of::<S>(input, &identity), key, why }),
        }
    }

    /// Keeps `value` as the entry with no reads of a step computed elsewhere. Other bytes under
    /// its key are a [`KernelError::Conflict`].
    pub fn keep_whole<S: Step, P: Put + ?Sized>(
        &self,
        store: &mut P,
        input: &S::Input,
        value: &[u8],
    ) -> Result<Key, KernelError> {
        let identity = self.identity::<S>(input)?;
        let key = whole::key(&identity);
        // alloc: the value, kept in the store
        store
            .put(key, S::KIND, value.to_vec())
            .map_err(|it| conflict::<S>(input, &identity, key, &[], &it))?;
        Ok(key)
    }
}

fn of<S: Step>(input: &S::Input, identity: &Identity) -> Of {
    Of { step: S::NAME, label: S::label(input), identity: identity.id() }
}

fn conflict<S: Step>(
    input: &S::Input,
    identity: &Identity,
    key: Key,
    reads: &[String],
    mismatch: &blaze_cas::Mismatch,
) -> KernelError {
    // alloc: an error
    let reads = reads.to_vec();
    KernelError::Conflict {
        of: of::<S>(input, identity),
        key,
        reads,
        kept: mismatch.kept,
        given: mismatch.given,
    }
}

/// The key of the entry of `identity` whose reads all hash the same now: the entry with no
/// reads, else the first shape in the chain whose reads' hashes now name an entry the store has.
fn find_key<S: Compute, G: Get + ?Sized>(
    store: &G,
    world: &S::World,
    input: &S::Input,
    identity: &Identity,
) -> Result<Option<Key>, KernelError> {
    let id = identity.id();
    let failed =
        |key: Key, why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    let whole = whole::key(identity);
    if store.has(&whole).map_err(|why| failed(whole, why))? {
        return Ok(Some(whole));
    }
    for n in 0..MOST_SHAPES {
        let slot = shape_slot(&id, n);
        let Some(bytes) = store.get(&slot).map_err(|why| failed(slot, why))? else {
            return Ok(None);
        };
        let shape = Shape::read(bytes, &id).map_err(|why| failed(slot, why))?;
        let mut key = EntryKey::new(&id, shape.len());
        for read in shape.reads() {
            let seen = world.hash(read).map_err(|error| KernelError::World {
                of: of::<S>(input, identity),
                read: world.describe(read),
                why: error.why,
            })?;
            key.read(read, seen);
        }
        let key = key.finish();
        if store.has(&key).map_err(|why| failed(key, why))? {
            return Ok(Some(key));
        }
    }
    Err(KernelError::TooManyShapes { of: of::<S>(input, identity) })
}

fn read_entry<'s, S: Step, G: Get + ?Sized>(
    store: &'s G,
    key: Key,
    input: &S::Input,
    identity: &Identity,
) -> Result<Entry<'s>, KernelError> {
    let failed = |why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    // alloc: an error
    let bytes = store
        .get(&key)
        .map_err(failed)?
        .ok_or_else(|| failed("its entry went missing after it was probed".to_owned()))?;
    Entry::read(bytes, key, identity).map_err(failed)
}

/// `S` of `input` with fresh reads: its value and its reads, sorted, each once.
fn run<S: Compute>(
    world: &S::World,
    input: &S::Input,
    identity: &Identity,
) -> Result<(Vec<u8>, Trace), KernelError> {
    let mut ctx = Reads::new(world);
    let computed = S::compute(&mut ctx, input);
    let trace = ctx.finish().map_err(|it| ended::<S>(world, input, identity, it))?;
    match computed {
        Ok(value) => Ok((value, trace)),
        Err(StepError::World(error)) => Err(KernelError::World {
            of: of::<S>(input, identity),
            read: world.describe(error.read()),
            why: error.why,
        }),
        Err(StepError::Refused(why)) => {
            Err(KernelError::Compute { of: of::<S>(input, identity), why })
        }
    }
}

fn ended<S: Compute>(
    world: &S::World,
    input: &S::Input,
    identity: &Identity,
    ended: reads::Unfinished,
) -> KernelError {
    match ended {
        reads::Unfinished::Failed(error) => KernelError::World {
            of: of::<S>(input, identity),
            read: world.describe(error.read()),
            why: error.why,
        },
        reads::Unfinished::Unsteady { kind, key, first, then } => KernelError::Unsteady {
            of: of::<S>(input, identity),
            read: world.describe(Read { kind, key: &key }),
            first,
            then,
        },
    }
}

/// Keeps `value` as the entry of `identity` and `trace`, then makes sure the trace's shape is in
/// the identity's chain. The entry goes first: a shape found always names reads that some
/// entry recorded.
///
/// `store` must read its own puts at once (blaze-cas does, through every writer): a slot taken
/// by another shape is found by its get, and a slot taken between the get and the put (another
/// writer) is a [`blaze_cas::Mismatch`] that moves on to the next slot.
fn keep<S: Compute, P: Put + ?Sized>(
    store: &mut P,
    world: &S::World,
    input: &S::Input,
    identity: &Identity,
    trace: &Trace,
    value: &[u8],
) -> Result<Key, KernelError> {
    let id = identity.id();
    let failed =
        |key: Key, why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    let key = entry_key(&id, trace.iter());
    let bytes = entry::encode(identity, trace, value);
    if let Err(mismatch) = store.put(key, S::KIND, bytes) {
        // alloc: an error
        let reads: Vec<String> =
            trace.iter().map(|(read, seen)| described(world, read, seen)).collect();
        return Err(conflict::<S>(input, identity, key, &reads, &mismatch));
    }
    if trace.is_empty() {
        return Ok(key);
    }
    let mut shape = entry::encode_shape(&id, trace);
    for n in 0..MOST_SHAPES {
        let slot = shape_slot(&id, n);
        match store.get(&slot).map_err(|why| failed(slot, why))? {
            Some(held) if held == shape.as_slice() => return Ok(key),
            Some(_) => continue,
            None => {}
        }
        match store.put(slot, Kind::Query, std::mem::take(&mut shape)) {
            Ok(()) => return Ok(key),
            // Another writer took the slot between the get and the put.
            Err(_) => shape = entry::encode_shape(&id, trace),
        }
    }
    Err(KernelError::TooManyShapes { of: of::<S>(input, identity) })
}

fn described<W: World + ?Sized>(world: &W, read: Read<'_>, seen: Seen) -> String {
    match seen {
        Seen::Is(hash) => format!("{} = {hash:x}", world.describe(read)), // alloc: an error's or a check's text
        Seen::Gone => format!("{} (gone)", world.describe(read)), // alloc: an error's or a check's text
    }
}

/// Checks a hit when `verify` samples it: `S` is run again with fresh reads, and it must make
/// the entry's reads and bytes.
fn check<S: Compute>(
    verify: &Setting,
    world: &S::World,
    input: &S::Input,
    entry: &Entry<'_>,
) -> Result<(), KernelError> {
    let identity = entry.identity.id();
    if !verify.sampled(&identity) {
        return Ok(());
    }
    let label = S::label(input);
    // alloc: a sampled check's text
    let names: Vec<String> =
        entry.reads().map(|(read, seen)| described(world, read, seen)).collect();
    // alloc: a sampled check's reads
    let reads: Vec<RecordedRead<'_>> = names
        .iter()
        .zip(entry.reads())
        .map(|(what, (_, seen))| RecordedRead { what, hash: seen.hash() })
        .collect();
    let hit = Hit { function: &label, identity, reads: &reads, stored: entry.value };
    blaze_store_check::check(verify, &hit, &Again::<S> { world, input, entry })
        .map_err(KernelError::Verify)?;
    Ok(())
}

/// A hit's value computed again, for lane verify's check.
struct Again<'a, S: Compute> {
    world: &'a S::World,
    input: &'a S::Input,
    entry: &'a Entry<'a>,
}

impl<S: Compute> Recompute for Again<'_, S> {
    fn recompute(&self, _: &Hit<'_>) -> Result<Vec<u8>, String> {
        let (value, trace) =
            // alloc: an error
            run::<S>(self.world, self.input, &self.entry.identity).map_err(|it| it.to_string())?;
        let mut stored = self.entry.reads();
        for (n, (read, seen)) in trace.iter().enumerate() {
            match stored.next() {
                Some(was) if was == (read, seen) => {}
                Some((was, was_seen)) => {
                    // alloc: an error
                    return Err(format!(
                        "computed again, read {n} is {}, where the entry recorded {}",
                        described(self.world, read, seen),
                        described(self.world, was, was_seen)
                    ));
                }
                None => {
                    // alloc: an error
                    return Err(format!(
                        "computed again, it made {} reads, where the entry recorded {}",
                        trace.len(),
                        self.entry.reads().len()
                    ));
                }
            }
        }
        if stored.next().is_some() {
            // alloc: an error
            return Err(format!(
                "computed again, it made {} reads, where the entry recorded {}",
                trace.len(),
                self.entry.reads().len()
            ));
        }
        Ok(value)
    }
}

/// `bytes` in hex, at most 32 of them.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // alloc: an error's or a check's text
    let mut out = String::with_capacity(2 * bytes.len().min(32) + 16);
    for byte in bytes.iter().take(32) {
        let _ = write!(out, "{byte:02x}");
    }
    if bytes.len() > 32 {
        let _ = write!(out, "... ({} bytes)", bytes.len());
    }
    out
}
