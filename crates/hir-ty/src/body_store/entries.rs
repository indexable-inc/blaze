//! A body's inference, its built MIR and its borrow check's verdict as `blaze_kernel` step
//! entries (docs/v2/plan/design/end-state.md, "The front end as kernel steps", order row 3):
//! [`InferEntry`] and [`BorrowckEntry`], each of one body by its syntax ([`BodyKey`]: its file
//! and its content-hashed `blaze_front_items` item id, never a dense id), and a [`MirEntry`] for
//! each MIR owner of the body ([`OwnerKey`]: the body and the owner's [`Place`] in it, the body
//! itself, each closure and each `async` coroutine, [`places`]).
//!
//! An entry's reads, which are what a carry reaches it by:
//!
//! - **its own syntax record**: one part read of `items(file)` at
//!   [`blaze_front_items::entry::body_key`] of the item, so an edit of another item, or a
//!   comment between items, leaves it verified with no work;
//! - **its identity** ([`reads::IDENTITY`], valued by the item store's
//!   [`BodyFacts::identity`](super::BodyFacts::identity)): what the identity hashes beyond the
//!   body's tokens (its owner's signature, what each id it names is, what each path resolves
//!   to, its crate's inputs). A carry says its difference as every other kind's
//!   ([`BodyDelta::say`]): valued, each distinct read once, by the kernel, unless a producer
//!   names the reads that differ ([`Exact`]); no kind is always dirty;
//! - **its facts**: one read per fact the run's trace names ([`reads::fact`], the fact's kind
//!   as the read's kind, so a driver says each kind's difference apart), valued by the world
//!   ([`BodyWorld::fact`]); a fact that is the body's own earlier step
//!   ([`BodyFacts::own_step`](super::BodyFacts::own_step)) is a read of that step's entry: of
//!   its inference, or, for the borrow check, of every one of its MIR entries (a borrow check
//!   reads each closure's MIR, all filed as the one fact).
//!
//! A step's run is the database's: [`BodyWorld::run`] asks it for the step, and the item store
//! kept what that run read and made ([`super::Store::traced`], written where the step offers
//! itself). A body with no trace, or whose run the store refused, is the entry's error naming
//! the body and why: no entry is kept for it (AGENTS.md rule 3).
//!
//! What is still trusted, and named: the identity covers every read the store's
//! classification calls covered (item-hash's section 6), as the item store's records already
//! trust; the world's map from a [`BodyKey`] to the database's body ([`BodyWorld::body`]) is
//! the one the syntax gives: each body part of a file's `items` by where it starts ([`keyed`]),
//! joined with where each body of the database is written ([`written`], [`join`]).

use std::{alloc::Global, fmt, marker::PhantomData, sync::Arc};

use base_db::Db;
use blaze_cas::Kind as StoreKind;
use blaze_format::SchemaHash;
use blaze_front_items::{
    ITEMS, Table,
    entry::{Entry, ItemId, Kind, body_key, key_item},
    lines::{ItemLines, LINES},
    steps::{FileKey, Items, Source},
};
use blaze_ids::ContentId;
use blaze_kernel::{
    Ask, Changed, CodeId, Compute, Fact, Got, Kernel, KernelError, Read, ReadKind, Reads, Seen,
    Session, Step as KernelStep, StepError, StepId, Subject, WorldError, steps as ids,
};
use blaze_query::{QueryKind, db::View};
use hir_def::{DefWithBodyId, HasModule as _, Lookup};
use syntax::view::AstNode as _;

pub use super::mir::Place;
use super::{Refusal, Step, with_store};
use crate::next_solver::goal_store::{FactObject, FactRef};

/// The kinds of read a body entry makes of its world.
pub mod reads {
    use blaze_kernel::ReadKind;
    use blaze_query::kinds::FACTS;

    /// A body's identity: its key is the body's [`super::BodyKey`] as its entries are keyed;
    /// it saw the identity, or gone where the body has none or is not there.
    pub const IDENTITY: ReadKind = ReadKind(0x0301);

    /// Whether reads of `kind` are of a fact: a fact's kind (`blaze_query::kinds::FACTS`) is
    /// its read's kind, so a change set says each fact kind's difference on its own.
    pub fn fact(kind: ReadKind) -> bool {
        FACTS.contains(&kind.0)
    }
}

/// What the item store kept of the last run of a body's step ([`super::Store::traced`]).
pub(super) enum Ran {
    /// The facts the run read, sorted, each once, and the answer's bytes: the entry's value.
    Kept { facts: Box<[FactRef]>, answer: Box<[u8]> },
    /// The store would not keep the run, and why.
    Refused(Refusal),
}

/// A body as its entries are keyed: its file, its item there and its crate (the crate a fact
/// read by it is seen from).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BodyKey {
    pub file: FileKey,
    pub item: ItemId,
    pub krate: String,
}

impl fmt::Debug for BodyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the body of item {} of `{}` in crate `{}`", hex(&self.item.0), self.file.name, self.krate)
    }
}

/// One MIR owner of a body as its MIR entry is keyed: the body, and the owner's place in it
/// (the body itself is [`Place::BODY`]).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OwnerKey {
    pub body: BodyKey,
    pub place: Place,
}

impl fmt::Debug for OwnerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the MIR at {:?} of {:?}", self.place, self.body)
    }
}

/// Which entry of a body: its inference, the MIR of one of its owners, its borrow check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Of {
    Infer,
    Mir(Place),
    Borrowck,
}

impl Of {
    /// The item store's step this entry is of.
    pub fn step(self) -> Step {
        match self {
            Of::Infer => Step::Infer,
            Of::Mir(_) => Step::Mir,
            Of::Borrowck => Step::Borrowck,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    // alloc: an error's or a label's text
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The world body entries read: the pinned files (`Source`), the database at that pin, and
/// the facts. The wiring implements it over the daemon's database and lane item-hash's facts;
/// its `World::hash` answers [`reads::IDENTITY`] and fact reads through [`hash`].
pub trait BodyWorld: Source {
    /// The database at this world's pin, with an item store installed (`super::install`).
    fn db(&self) -> &Db;

    /// The body `key` names in [`BodyWorld::db`]; an error naming why where there is none
    /// (the syntax has no such item, or no body the database has is it).
    fn body(&self, key: &BodyKey) -> Result<DefWithBodyId, String>;

    /// Asks the database for the step `of` of `def` (its inference, the built MIR of one of
    /// its owners, its borrow check), so that the run that made its value, now or earlier,
    /// left its trace in the item store; gives the schema it was offered under
    /// ([`infer_schema`], [`mir_schema`], [`borrowck_schema`]). A borrow check that fails is
    /// an error naming the body.
    fn run(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String>;

    /// The value of `fact` as the crate named `krate` sees it now: gone where it names
    /// nothing; [`WorldError::not_now`] where it cannot be hashed now.
    fn fact(&self, krate: &str, fact: FactRef) -> Result<Seen, WorldError>;

    /// `fact`, of a trace of `def`'s steps, as a read every body can share: a fact the item
    /// store lists relative to the body (lane item-hash names the body's own item by one tag,
    /// `values::OWN`, whatever the body) with the body's own item named in its place, so its
    /// value is one whoever asks ([`BodyWorld::fact`] is given no body). Any other fact as it is.
    fn absolute(&self, def: DefWithBodyId, fact: FactRef) -> FactRef;

    /// The schema the step `of` of `def` is offered under in [`BodyWorld::db`], as
    /// [`BodyWorld::run`] gives it, with nothing run: what [`serve`] finds a served step's row
    /// under, and an earlier step's it read.
    fn schema(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String>;
}

const SUBJECT: &str = "blaze v2 body entry 1";

/// `key`'s stable bytes: the length of its file's key (two bytes, big-endian), the file's key
/// as `items(file)` is keyed, the item's id, then the crate's name.
#[expect(
    blaze::narrowed_len,
    reason = "the entry key frames its file key's length in two bytes; a file key of 64 KiB or more (a very long path) needs a typed refusal or a wider frame"
)]
fn key_of<W: BodyWorld + ?Sized + 'static>(key: &BodyKey, out: &mut Vec<u8>) {
    let at = out.len();
    out.extend_from_slice(&[0, 0]);
    <Items<W> as Ask>::key(&key.file, out);
    let len = u16::try_from(out.len() - at - 2).expect("a file's key under 65536 bytes");
    out[at..at + 2].copy_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&key.item.0);
    out.extend_from_slice(key.krate.as_bytes());
}

fn input_of<W: BodyWorld + ?Sized + 'static>(bytes: &[u8]) -> Result<BodyKey, String> {
    let Some((len, rest)) = bytes.split_first_chunk::<2>() else {
        // alloc: error path
        return Err(format!("a body's key of {} bytes, under its two of length", bytes.len()));
    };
    let len = usize::from(u16::from_be_bytes(*len));
    if rest.len() < len + 16 {
        // alloc: error path
        return Err(format!("a body's key whose file's key is {len} bytes and {} follow", rest.len()));
    }
    let file = <Items<W> as Ask>::input(&rest[..len])?;
    let item = ItemId(rest[len..len + 16].try_into().expect("16 bytes"));
    let Ok(krate) = std::str::from_utf8(&rest[len + 16..]) else {
        // alloc: error path
        return Err(String::from("a body's key whose crate's name is not UTF-8"));
    };
    // alloc: the input a key names
    Ok(BodyKey { file, item, krate: krate.to_owned() })
}

/// An entry's input as its key's bytes: a body's ([`BodyKey`], [`key_of`]) or one MIR owner's
/// ([`OwnerKey`]: the body's, then its place's 25 bytes, [`Place::bytes`]).
trait EntryInput: Sized {
    fn write<W: BodyWorld + ?Sized + 'static>(&self, out: &mut Vec<u8>);
    fn read<W: BodyWorld + ?Sized + 'static>(bytes: &[u8]) -> Result<Self, String>;
}

impl EntryInput for BodyKey {
    fn write<W: BodyWorld + ?Sized + 'static>(&self, out: &mut Vec<u8>) {
        key_of::<W>(self, out);
    }

    fn read<W: BodyWorld + ?Sized + 'static>(bytes: &[u8]) -> Result<BodyKey, String> {
        input_of::<W>(bytes)
    }
}

impl EntryInput for OwnerKey {
    fn write<W: BodyWorld + ?Sized + 'static>(&self, out: &mut Vec<u8>) {
        key_of::<W>(&self.body, out);
        out.extend_from_slice(&self.place.bytes());
    }

    fn read<W: BodyWorld + ?Sized + 'static>(bytes: &[u8]) -> Result<OwnerKey, String> {
        let Some((body, place)) = bytes.split_last_chunk::<25>() else {
            // alloc: error path
            return Err(format!("a MIR owner's key of {} bytes, under its place's 25", bytes.len()));
        };
        Ok(OwnerKey { body: input_of::<W>(body)?, place: Place::read(place) })
    }
}

fn subject<W: BodyWorld + ?Sized + 'static, I: EntryInput>(input: &I) -> Subject {
    // alloc: the input's bytes, once an identity
    let mut bytes = Vec::new();
    input.write::<W>(&mut bytes);
    Subject { of: ContentId::of(SUBJECT, &bytes), flags: ContentId::default() }
}

/// The value of a body's identity read: the world and the body, for the run.
pub struct Body<'w, W: ?Sized> {
    world: &'w W,
    def: DefWithBodyId,
}

/// The read of a body's identity ([`reads::IDENTITY`]), and the one door from a body's run to
/// its database: what the run reads past its identity is its trace's facts.
pub struct IdentityOf<W: ?Sized>(PhantomData<fn(&W)>);

impl<W: BodyWorld + ?Sized + 'static> Fact<W> for IdentityOf<W> {
    const KIND: ReadKind = reads::IDENTITY;
    type Key = BodyKey;
    type Value<'w>
        = Body<'w, W>
    where
        W: 'w;

    fn key(key: &BodyKey, out: &mut Vec<u8>) {
        key_of::<W>(key, out);
    }

    fn get<'w>(world: &'w W, key: &BodyKey) -> Result<(Body<'w, W>, Seen), WorldError> {
        // alloc: the read's name, once a run
        let mut bytes = Vec::new();
        key_of::<W>(key, &mut bytes);
        let read = Read { kind: reads::IDENTITY, key: &bytes };
        let def = world.body(key).map_err(|why| WorldError::not_now(read, why))?;
        let seen = identity_of(world.db(), def, read)?;
        Ok((Body { world, def }, seen))
    }
}

fn identity_of(db: &Db, def: DefWithBodyId, read: Read<'_>) -> Result<Seen, WorldError> {
    match with_store(db, |store| store.facts.identity(db, def)) {
        Some(Some(identity)) => Ok(Seen::Is(identity)),
        Some(None) => Ok(Seen::Gone),
        None => Err(WorldError::new(read, "the database has no item store installed")),
    }
}

/// The read of `fact` as the crate `krate` sees it: its kind is the fact's, its key the
/// fact's subject, its object (a tag, then the field's number or the id) and the crate's name.
pub fn fact_read(fact: FactRef, krate: &str, out: &mut Vec<u8>) -> ReadKind {
    out.extend_from_slice(&fact.subject.0);
    match fact.object {
        FactObject::None => out.push(0),
        FactObject::Field(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        FactObject::Id(id) => {
            out.push(2);
            out.extend_from_slice(&id.0);
        }
    }
    out.extend_from_slice(krate.as_bytes());
    ReadKind(fact.kind.0)
}

/// The fact and the crate a fact read names ([`fact_read`]); none for bytes it did not write.
pub fn fact_of(read: Read<'_>) -> Option<(FactRef, &str)> {
    if !reads::fact(read.kind) {
        return None;
    }
    let (subject, rest) = read.key.split_first_chunk::<32>()?;
    let (tag, rest) = rest.split_first()?;
    let (object, rest) = match tag {
        0 => (FactObject::None, rest),
        1 => {
            let (n, rest) = rest.split_first_chunk::<4>()?;
            (FactObject::Field(u32::from_le_bytes(*n)), rest)
        }
        2 => {
            let (id, rest) = rest.split_first_chunk::<32>()?;
            (FactObject::Id(ContentId(*id)), rest)
        }
        _ => return None,
    };
    let krate = std::str::from_utf8(rest).ok()?;
    Some((FactRef { kind: QueryKind(read.kind.0), subject: ContentId(*subject), object }, krate))
}

/// What a [`BodyWorld`]'s `World::hash` gives for the reads body entries make: an identity, a
/// fact; `None` for a read of any other kind, which the world answers itself.
pub fn hash<W: BodyWorld + ?Sized + 'static>(
    world: &W,
    read: Read<'_>,
) -> Option<Result<Seen, WorldError>> {
    if read.kind == reads::IDENTITY {
        let hashed = input_of::<W>(read.key)
            .map_err(|why| WorldError::new(read, why))
            .and_then(|key| world.body(&key).map_err(|why| WorldError::not_now(read, why)))
            .and_then(|def| identity_of(world.db(), def, read));
        return Some(hashed);
    }
    if !reads::fact(read.kind) {
        return None;
    }
    Some(match fact_of(read) {
        Some((fact, krate)) => world.fact(krate, fact),
        None => Err(WorldError::new(read, "a fact read whose key no body entry wrote")),
    })
}

/// The schema [`BodyWorld::run`] gives for a body's inference.
pub fn infer_schema() -> SchemaHash {
    super::infer::SCHEMA
}

/// Asks `db` for `def`'s inference: the run of [`BodyWorld::run`] for [`Step::Infer`].
pub fn run_infer(db: &Db, def: DefWithBodyId) -> SchemaHash {
    let _ = crate::InferenceResult::of(db, def);
    infer_schema()
}

/// The places of `def`'s MIR owners an entry keys ([`super::mir::owners`]): [`Place::BODY`],
/// then each closure and `async` coroutine written in it, in the order of their expressions.
/// Not here, so no entry: an inline constant's MIR, a by-move coroutine's and a `gen`
/// coroutine's (a later step that reads one is refused by the item store already).
///
/// Read from the body's lowering ([`super::lowering`], [`super::Lowered::places`]), with no
/// inference: found by its key, nothing of the body is lowered either. Why the places are the
/// same as [`super::mir::owners`]' (which types each closure by the body's inference): the
/// inference of a closure expression interns its owner with the kind as written
/// (`infer::closure`, `InternedClosure { owner, expr, kind: closure_kind }`), an `async`
/// closure's closure is the same interned location (`as_closure`), and an inline `const { .. }`
/// block is inferred inside the body with the body as owner, while any other anonymous constant
/// is a body of its own whose closures the body's inference does not type. So each own closure
/// of a keyed kind is an owner at the place its expression and its written kind give. Checked
/// where both are had: [`run_mir`] refuses a body whose two lists differ, naming it.
pub fn places(db: &Db, def: DefWithBodyId) -> Vec<Place> {
    // alloc: the body's places, a handful
    super::lowering(db, def).places().collect()
}

/// The schema [`BodyWorld::run`] gives for the built MIR of `def`'s owner at `place`.
pub fn mir_schema(db: &Db, def: DefWithBodyId, place: Place) -> SchemaHash {
    super::mir::schema_of(db, def, place)
}

/// The place of [`places`] whose [`mir_schema`] is `schema`: which owner a MIR trace filed
/// under `schema` is of. None for a schema no keyed owner of `def` has.
pub fn place_of(db: &Db, def: DefWithBodyId, schema: SchemaHash) -> Option<Place> {
    places(db, def).into_iter().find(|&place| mir_schema(db, def, place) == schema)
}

/// Asks `db` for the built MIR of `def`'s owner at `place`: the run of [`BodyWorld::run`] for
/// [`Of::Mir`]. An error for a place no owner [`places`] gives is at.
pub fn run_mir(db: &Db, def: DefWithBodyId, place: Place) -> Result<SchemaHash, String> {
    let owners = super::mir::owners(db, def);
    // The proof [`places`] states, checked where the inference is had anyway.
    if !owners.iter().map(|(at, _)| *at).eq(super::lowering(db, def).places()) {
        // alloc: error path
        return Err(format!(
            "{def:?}: the MIR owners its inference gives ({:?}) are not the places its lowering gives ({:?})",
            owners.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            super::lowering(db, def).places().collect::<Vec<_>>(),
        ));
    }
    let Some((_, owner)) = owners.into_iter().find(|(at, _)| *at == place) else {
        // alloc: error path
        return Err(format!(
            "{def:?} has no MIR owner at {place:?} that an entry keys (not an inline const's, by-move or `gen` MIR)"
        ));
    };
    let _ = crate::providers::mir_at(db, owner, crate::mir::MirStage::Built);
    Ok(mir_schema(db, def, place))
}

/// The schema [`BodyWorld::run`] gives for a passing borrow check of `def` by the checker
/// named `checker` (what `hir_ty_borrowck::gate::checked` offers it under).
pub fn borrowck_schema(db: &Db, def: DefWithBodyId, checker: &'static str) -> SchemaHash {
    super::with_crate_attrs(db, def, super::passed_schema(checker))
}

fn refused(why: String) -> StepError {
    StepError::Refused(why)
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the oracle's control: a body entry's run leaves its read of this
    /// kind out of its trace, as a trace that missed a read would.
    pub(super) static DROPPED: std::cell::Cell<Option<ReadKind>> = const { std::cell::Cell::new(None) };
    /// How many runs of each step's entry this thread made, by [`Step`] as an index.
    pub(super) static RUNS: std::cell::Cell<[u32; Step::COUNT]> = const { std::cell::Cell::new([0; Step::COUNT]) };
}

#[cfg(test)]
fn dropped(kind: ReadKind) -> bool {
    DROPPED.with(|it| it.get() == Some(kind))
}

/// The run of the step `of` of the body `input`: its reads, then the answer its database's run
/// made.
fn compute<W: BodyWorld + ?Sized + 'static>(
    ctx: &mut Reads<'_, W>,
    input: &BodyKey,
    of: Of,
) -> Result<Vec<u8>, StepError> {
    let step = of.step();
    #[cfg(test)]
    RUNS.with(|it| {
        let mut runs = it.get();
        runs[step as usize] += 1;
        it.set(runs);
    });
    // (a) The body's own syntax record.
    if ctx.part::<Items<W>>(&input.file, &body_key(input.item))?.is_none() {
        // alloc: error path
        return Err(refused(format!("{input:?}: `items` of its file has no body under its id")));
    }
    // Its identity, and through it the body in the world's database.
    let Body { world, def } = ctx.get::<IdentityOf<W>>(input)?;
    #[cfg(test)]
    if dropped(reads::IDENTITY) {
        ctx.forget::<IdentityOf<W>>(input);
    }
    let schema = world.run(def, of).map_err(|why| refused(format!("{input:?} ({def:?}): {why}")))?;
    let db = world.db();
    let Some(traced) = with_store(db, |store| store.traced(def, step, schema)) else {
        return Err(refused(format!("{input:?}: the database has no item store installed")));
    };
    let Some(ran) = traced else {
        return Err(refused(format!(
            "{input:?} ({def:?}): its {} left no trace in the item store (a body with no identity, or a run made before the store was installed)",
            super::what(step)
        )));
    };
    let (facts, answer): (&[FactRef], &[u8]) = match &*ran {
        Ran::Kept { facts, answer } => (&facts[..], &answer[..]),
        Ran::Refused(refusal) => {
            return Err(refused(format!(
                "{input:?} ({def:?}): the item store refused its {}: {refusal:?}",
                super::what(step)
            )));
        }
    };
    // (b) Its facts, each a read the kernel posts.
    // alloc: one fact read's name at a time, reused
    let mut key = Vec::new();
    for &fact in facts {
        match with_store(db, |store| store.facts.own_step(fact)).flatten() {
            Some(Step::Infer) if step > Step::Infer => {
                ctx.step::<InferEntry<W>>(input)?;
            }
            // A borrow check reads the MIR of every owner of the body (each closure's is
            // checked with it), and the store files each such read as this one fact: the run
            // reads every MIR entry of the body, so a fact only a closure's MIR read reaches
            // the borrow check's entry.
            Some(Step::Mir) if of == Of::Borrowck => {
                for place in places(db, def) {
                    // clone: the body's key, one MIR owner's input each
                    ctx.step::<MirEntry<W>>(&OwnerKey { body: input.clone(), place })?;
                }
            }
            Some(earlier) => {
                return Err(refused(format!(
                    "{input:?}: its {} ({of:?}) read its own {}, which is no earlier step with an entry",
                    super::what(step),
                    super::what(earlier)
                )));
            }
            None => {
                // A trace names the body's own item by a tag every body shares
                // (`BodyWorld::absolute`): the read names the item itself, or every body of
                // a crate would read one fact here and each be given another's value.
                let fact = world.absolute(def, fact);
                key.clear();
                let kind = fact_read(fact, &input.krate, &mut key);
                // A test's planted bug: a fact kind left out of every trace.
                #[cfg(test)]
                if dropped(kind) {
                    continue;
                }
                ctx.hash(Read { kind, key: &key })?;
            }
        }
    }
    Ok(answer.to_vec())
}

macro_rules! body_step {
    ($(#[$doc:meta])* $name:ident, $id:expr, $text:literal, $input:ty, |$it:ident| $of:expr) => {
        $(#[$doc])*
        pub struct $name<W: ?Sized>(PhantomData<fn(&W)>);

        impl<W: BodyWorld + ?Sized + 'static> KernelStep for $name<W> {
            const ID: StepId = $id;
            const NAME: &'static str = $text;
            const KIND: StoreKind = StoreKind::Query;
            type Input = $input;

            fn subject(input: &$input) -> Subject {
                subject::<W, $input>(input)
            }

            fn label(input: &$input) -> String {
                // alloc: a label, for an error or a check
                format!("{} of {input:?}", $text)
            }
        }

        impl<W: BodyWorld + ?Sized + 'static> Compute for $name<W> {
            type World = W;

            fn compute(ctx: &mut Reads<'_, W>, input: &$input) -> Result<Vec<u8>, StepError> {
                let $it = input;
                let (body, of): (&BodyKey, Of) = $of;
                compute(ctx, body, of)
            }
        }

        impl<W: BodyWorld + ?Sized + 'static> Ask for $name<W> {
            fn key(input: &$input, out: &mut Vec<u8>) {
                input.write::<W>(out);
            }

            fn input(key: &[u8]) -> Result<$input, String> {
                <$input as EntryInput>::read::<W>(key)
            }
        }
    };
}

body_step!(
    /// `infer(body)`: the body's inference answer, as the item store encodes it.
    InferEntry, ids::BODY_INFER, "body inference", BodyKey, |it| (it, Of::Infer)
);
body_step!(
    /// `mir(owner)`: the built MIR of one MIR owner of a body (the body itself, a closure, an
    /// `async` coroutine), as the item store encodes it.
    MirEntry, ids::BODY_MIR, "body MIR", OwnerKey, |it| (&it.body, Of::Mir(it.place))
);
body_step!(
    /// `borrowck(body)`: that the body's borrow check passed (no bytes; a failing check is no
    /// entry, but the error of whoever asks).
    BorrowckEntry, ids::BODY_BORROWCK, "body borrow check", BodyKey, |it| (it, Of::Borrowck)
);

/// Registers the three body entries on `kernel` with `code` (the front end's code identity,
/// as `super::install`'s `build`). The kernel must have `blaze_front_items`' `parse` and
/// `items` of the same world registered too: an entry reads its body's part of `items`.
pub fn register<W: BodyWorld + ?Sized + 'static>(
    kernel: &mut Kernel<W>,
    code: CodeId,
) -> Result<(), KernelError> {
    kernel.register_ask::<InferEntry<W>>(code)?;
    kernel.register_ask::<MirEntry<W>>(code)?;
    kernel.register_ask::<BorrowckEntry<W>>(code)
}

/// A served step's answer as the kernel verified it ([`serve`]): what the step's query in the
/// database answers from (`super::Store::served_answer`), at `revision`, and at a later one
/// only where its crate is stamped there ([`stamp`]). With it, what the entry read, so the
/// query reads each again and its row depends on them: the body's identity, and each fact
/// with what it saw, of the step itself and of every earlier step of the body it read (theirs
/// taken from their own rows, so a fact only an earlier step read is here too), sorted, each
/// pair once.
pub struct Served {
    pub(super) revision: View,
    /// The crate of the body: whose stamp the row answers under.
    pub(super) krate: base_db::Crate,
    pub(super) identity: ContentId,
    pub(super) facts: Box<[(FactRef, Seen)]>,
    pub(super) answer: Arc<[u8]>,
}

/// The served answers of one database (`super::Store::served`), by body: each answers at the
/// revision it was posted at, or at one its crate was stamped at ([`stamp`]), never at any
/// other. A body's rows go when the driver unposts it ([`unpost`]).
///
/// With them, what the driver claimed of the database (`claim_serve`, `claim_keeps`), so a
/// body is served once a revision and kept once a run, however many units reach it.
#[derive(Default)]
pub(super) struct Posted {
    /// Each body's rows, one a step and schema.
    rows: blaze_hash::HashMap<DefWithBodyId, Vec<(Step, SchemaHash, Arc<Served>)>, Global>,
    /// Each crate's last stamp: the revision it is of, and the revision from which every row
    /// of the crate posted is one the stamping driver knows of (a row posted before that,
    /// by a driver whose rows it never saw, does not answer under the stamp).
    stamped: blaze_hash::HashMap<base_db::Crate, (View, View), Global>,
    /// The revision each crate's serve was last claimed at.
    claimed: blaze_hash::HashMap<base_db::Crate, View, Global>,
    /// Each trace a keep claimed, by its body, step and schema: the trace itself (a counted
    /// reference), so a run that replaced it is claimed again and nothing else is. Not cleared
    /// at a revision: a trace is the run's, whatever revision it is read at.
    kept: blaze_hash::HashMap<(DefWithBodyId, Step, SchemaHash), Arc<Ran>, Global>,
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the revision oracle's control: a served answer is given at any
    /// revision, not only the one it was posted at.
    pub(super) static ANY_REVISION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The planted bug of the stamp's oracle's control: [`unpost`] drops nothing, so a body
    /// whose entry moved keeps its old rows, and the crate's stamp makes them answer.
    pub(super) static KEEP_ALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn keep_all() -> bool {
    KEEP_ALL.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn keep_all() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the served row's oracle's control: [`serve`] keeps a row's own facts
    /// alone, not those of the earlier steps it read.
    pub(super) static OWN_FACTS_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The planted bug of the claims' oracle's control: a [`Claim`] dropped without
    /// [`Claim::kept`] keeps its marks, as a keep that failed and held its claims would.
    pub(super) static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The planted bug of the syntax join's oracle's control: [`join`] pairs a body with the
    /// first key at its start, whatever its kind.
    pub(super) static ANY_KIND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn own_facts_only() -> bool {
    OWN_FACTS_ONLY.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn own_facts_only() -> bool {
    false
}

#[cfg(test)]
fn held() -> bool {
    HELD.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn held() -> bool {
    false
}

#[cfg(test)]
fn any_kind() -> bool {
    ANY_KIND.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn any_kind() -> bool {
    false
}

#[cfg(test)]
fn any_revision() -> bool {
    ANY_REVISION.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn any_revision() -> bool {
    false
}

impl Posted {
    /// The answer of `step` of `def` under `schema` that answers at `now`: posted at `now`,
    /// or posted since its crate's stamp at `now` began ([`stamp`]); none otherwise.
    pub(super) fn get(
        &self,
        now: View,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
    ) -> Option<Arc<Served>> {
        let rows = self.rows.get(&def)?;
        let (_, _, served) = rows.iter().find(|(at, of, _)| (*at, *of) == (step, schema))?;
        let stamped = || {
            self.stamped.get(&served.krate).is_some_and(|&(at, since)| at == now && served.revision >= since)
        };
        if served.revision != now && !stamped() && !any_revision() {
            return None;
        }
        // clone: a counted reference
        Some(served.clone())
    }

    pub(super) fn post(&mut self, def: DefWithBodyId, step: Step, schema: SchemaHash, served: Served) {
        // alloc: a body's rows, a handful, kept until the driver unposts the body
        let rows = self.rows.entry(def).or_insert_with(Vec::new);
        // alloc: one answer a served step of a body, kept until its body is unposted
        let served = Arc::new(served);
        match rows.iter_mut().find(|(at, of, _)| (*at, *of) == (step, schema)) {
            Some(row) => row.2 = served,
            None => rows.push((step, schema, served)),
        }
    }
}

/// The identity the row of `step` of `def` under `schema` was verified with, if a row answers
/// in `db` at its revision now (posted now, or under its crate's stamp): what a test's oracle
/// compares with the body's identity now, with no query asked, so what answers is checked
/// whether or not the engine still holds a memo of the step.
#[cfg(test)]
pub(super) fn answering(db: &Db, def: DefWithBodyId, step: Step, schema: SchemaHash) -> Option<ContentId> {
    let (_, now) = db.nonce_and_revision();
    with_store(db, |store| store.served.lock().get(now, def, step, schema).map(|it| it.identity)).flatten()
}

/// Drops every row of `def` in `db`'s item store: the driver's word that the entries they
/// were posted from may not hold now. Nothing where `db` has no item store (none was posted).
pub fn unpost(db: &Db, def: DefWithBodyId) {
    with_store(db, |store| {
        if !keep_all() {
            store.served.lock().rows.remove(&def);
        }
    });
}

/// Stamps `krate` in `db` at its revision now: every row of the crate still posted since
/// `since` answers at this revision as if posted at it. The driver's word, said only once each
/// body whose entry moved since its rows were posted is unposted ([`unpost`]), and only for
/// rows it posted itself since `since` (a row posted before, by a driver whose rows it never
/// saw, answers under no stamp of it).
pub fn stamp(db: &Db, krate: base_db::Crate, since: View) {
    let (_, now) = db.nonce_and_revision();
    with_store(db, |store| {
        store.served.lock().stamped.insert(krate, (now, since));
    });
}

/// Posts what the kernel gave for the step `of` of the body `key` (`got`, from a get or after
/// a carry) to the item store of `world`'s database, at its revision now: the step's query
/// there answers from it until the database's next revision, checking again in the query what
/// the entry read (`super::Store::served_answer`). The row holds the facts the entry read and
/// those of each earlier step's row it read (an earlier step is a read of that step's entry,
/// whose own facts the kernel keeps in that entry, not in this one), so the query's row
/// depends on every fact the answer does. The driver serves every entry it gets, an earlier
/// step before a later one.
///
/// Refused, naming the body: an entry with no read of its identity (no entry of a body step
/// is made without one, so it is another step's or a corrupt one), a read of a step that is no
/// earlier body step, an earlier step not served at this revision, and a database with no item
/// store installed.
pub fn serve<W: BodyWorld + ?Sized + 'static>(
    world: &W,
    key: &BodyKey,
    of: Of,
    got: &Got<'_>,
) -> Result<(), String> {
    let step = of.step();
    let def = world.body(key)?;
    let schema = world.schema(def, of)?;
    let db = world.db();
    let (_, revision) = db.nonce_and_revision();
    let mut identity = None;
    // alloc: the entry's facts and its earlier steps', kept with its answer
    let mut facts = Vec::new();
    let mut note = |read: Read<'_>, seen: Seen| -> Result<(), String> {
        if read.kind == reads::IDENTITY {
            match seen {
                Seen::Is(it) => identity = Some(it),
                // alloc: error path
                Seen::Gone => return Err(format!("{key:?}: its {} saw no identity", super::what(step))),
            }
        } else if let Some((fact, _)) = fact_of(read) {
            facts.push((fact, seen));
        } else if read.kind == ReadKind::STEP {
            let Some((id, rest)) = read.key.split_first_chunk::<2>() else {
                // alloc: error path
                return Err(format!("{key:?}: its {} read a step under a key of no step", super::what(step)));
            };
            let earlier = match StepId(u16::from_be_bytes(*id)) {
                ids::BODY_INFER => Of::Infer,
                ids::BODY_MIR => match rest.split_last_chunk::<25>() {
                    Some((_, place)) => Of::Mir(Place::read(place)),
                    // alloc: error path
                    None => return Err(format!("{key:?}: its {} read a MIR entry with no place", super::what(step))),
                },
                // alloc: error path
                _ => return Err(format!("{key:?}: its {} read a step that is no earlier body step", super::what(step))),
            };
            if own_facts_only() {
                return Ok(());
            }
            let earlier_schema = world.schema(def, earlier)?;
            let posted = with_store(db, |store| store.served.lock().get(revision, def, earlier.step(), earlier_schema));
            match posted {
                Some(Some(row)) => facts.extend_from_slice(&row.facts),
                Some(None) => {
                    // alloc: error path
                    return Err(format!(
                        "{key:?}: its {} read its {} ({earlier:?}), which is not served",
                        super::what(step),
                        super::what(earlier.step())
                    ));
                }
                // alloc: error path
                None => return Err(format!("{key:?}: the database has no item store installed")),
            }
        }
        Ok(())
    };
    match got {
        Got::Hit(entry) => entry.reads().try_for_each(|(it, seen)| note(it, seen))?,
        Got::Computed { reads, .. } => reads.iter().try_for_each(|(it, seen)| note(it, seen))?,
    }
    let Some(identity) = identity else {
        // alloc: error path
        return Err(format!("{key:?}: its {} entry has no read of its identity", super::what(step)));
    };
    facts.sort_unstable();
    facts.dedup();
    let krate = def.krate(db);
    let served = Served { revision, krate, identity, facts: facts.into(), answer: got.value().into() };
    match with_store(db, |store| store.served.lock().post(def, step, schema, served)) {
        Some(()) => Ok(()),
        // alloc: error path
        None => Err(format!("{key:?}: the database has no item store installed")),
    }
}

/// Claims the serve of `krate`'s bodies in `db` at its revision now: true for the first claim
/// there, which serves them; false for every later one, which serves nothing (the bodies are
/// posted already, or being posted by the claim's holder). The lock is held for the claim
/// alone, never across a kernel call or a wait (rule 27). None where `db` has no item store
/// installed (`BLAZE_ITEM_STORE=off`): nothing is served or kept there.
pub fn claim_serve(db: &Db, krate: base_db::Crate) -> Option<bool> {
    let (_, now) = db.nonce_and_revision();
    with_store(db, |store| store.served.lock().claimed.insert(krate, now) != Some(now))
}

/// A run of a body's step the item store would not trace, named: its body, its step and
/// why (the store's refusal). No entry can be kept of it; the driver says each, never drops
/// one quietly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub def: DefWithBodyId,
    pub step: Step,
    pub why: String,
}

/// What [`claim_keeps`] handed over: each body's step whose run's trace no keep has claimed,
/// with the schema it ran under, and each such run the item store refused to trace.
///
/// The claim lets go unless it is kept: dropped without [`Claim::kept`] (the keep failed, or
/// returned early), each of its marks whose trace is still the one claimed is removed, so the
/// next [`claim_keeps`] claims those runs again and none is lost to a keep that did not reach
/// the store.
///
/// `#[blaze::linear]` (crates/hir-ty-linear): a claim is answered on every path. Its `Drop` is
/// its abandonment path, so an error exit (a `?` that propagates the keep's failure, a returned
/// `Err`) gives its runs back with no error; any other exit must call [`Claim::kept`] (the keep
/// reached the store) or [`Claim::give_back`] (it did not, said so).
#[blaze::linear]
pub struct Claim<'db> {
    pub steps: Vec<(DefWithBodyId, Step, SchemaHash)>,
    pub refused: Vec<Refused>,
    db: &'db Db,
    /// Each mark this claim made, with the trace it marked.
    marks: Vec<((DefWithBodyId, Step, SchemaHash), Arc<Ran>)>,
}

impl Claim<'_> {
    /// The keep reached the store: the marks stay, and no later claim takes these runs.
    #[blaze::consumes]
    pub fn kept(mut self) {
        self.marks.clear();
    }

    /// The keep did not reach the store: every run this claim took goes to the next
    /// [`claim_keeps`], as the claim's `Drop` gives them back on an error exit or an unwind.
    #[blaze::consumes]
    pub fn give_back(self) {}
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.marks.is_empty() || held() {
            return;
        }
        let marks = std::mem::take(&mut self.marks);
        // The posts' lock alone, for the marks, never across a wait (rule 27).
        with_store(self.db, |store| {
            let mut posted = store.served.lock();
            for (key, ran) in marks {
                // A mark another claim made since (a later run's trace) is that claim's.
                if posted.kept.get(&key).is_some_and(|it| Arc::ptr_eq(it, &ran)) {
                    posted.kept.remove(&key);
                }
            }
        });
    }
}

/// Claims every trace of a served step ([`Step::served`]) in `db` that no keep claimed
/// before: each run is kept by exactly one caller, however many units' keeps run at once, and
/// a claim dropped without [`Claim::kept`] gives its runs back ([`Claim`]). The traces are
/// taken under their lock, then the claims made under the posts' lock: never two at once, and
/// neither across a wait. None where `db` has no item store installed.
pub fn claim_keeps(db: &Db) -> Option<Claim<'_>> {
    let claimed = with_store(db, |store| {
        // alloc: one claim's snapshot of the database's traces, a counted reference each
        let traced: Vec<((DefWithBodyId, Step, SchemaHash), Arc<Ran>)> = store
            .traces
            .lock()
            .iter()
            .filter(|((_, step, _), _)| step.served())
            .map(|(key, ran)| (*key, Arc::clone(ran)))
            .collect();
        let mut posted = store.served.lock();
        let (mut steps, mut refused, mut marks) = (Vec::new(), Vec::new(), Vec::new());
        for (key, ran) in traced {
            if posted.kept.get(&key).is_some_and(|it| Arc::ptr_eq(it, &ran)) {
                continue;
            }
            let (def, step, _) = key;
            match &*ran {
                // alloc: the refusal's text, named to the driver
                Ran::Refused(refusal) => refused.push(Refused { def, step, why: format!("{refusal:?}") }),
                Ran::Kept { .. } => steps.push(key),
            }
            // clone: a counted reference, the mark and the claim's record of it
            posted.kept.insert(key, Arc::clone(&ran));
            marks.push((key, ran));
        }
        (steps, refused, marks)
    });
    // Made out here: a `Claim` is linear, and neither `with_store`'s result parameter nor a
    // closure given to `Option::map` may give one back (crates/hir-ty-linear, rule 4).
    let (steps, refused, marks) = claimed?;
    Some(Claim { steps, refused, db, marks })
}

/// One producer's word on one kind of read: the key ([`Read::key`], as an entry wrote it:
/// [`fact_read`]'s for a fact, the body's for [`reads::IDENTITY`]) of each read of `kind`
/// that differs between the carry's two pins. It may name more than differ, never fewer: a
/// read it leaves out is taken as unchanged (`MainLeftOut` in the tests is the control).
pub struct Exact {
    pub kind: ReadKind,
    /// Who says it, for the profile and a panic.
    pub by: &'static str,
    pub keys: Vec<Vec<u8>>,
}

/// What differs, between two pins, of the reads body entries make (end-state.md review point
/// 8): each kind a producer says exactly ([`Exact`], O(changed)), every other kind the
/// session reads valued by the kernel (`Changed::valued`: each distinct read once at the new
/// pin against what its entries saw). Total over the kinds the session reads, so no kind is
/// left unsaid and none is always dirty.
#[derive(Default)]
pub struct BodyDelta {
    pub exact: Vec<Exact>,
}

/// How [`BodyDelta::say`] said a kind: by a producer's [`Exact`], named, or valued.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    Exact(&'static str),
    Valued,
}

impl BodyDelta {
    /// Says on `changed`, for each kind `session` reads ([`Session::kinds`]), what differs:
    /// that kind's [`Exact`], else valued. Gives each kind with how it was said, in the
    /// session's order. Two [`Exact`]s of one kind are a driver's bug, and panic naming both.
    pub fn say(&self, session: &Session, changed: &mut Changed) -> Vec<(ReadKind, How)> {
        // alloc: one carry's kinds, a few dozen
        let mut said = Vec::new();
        for kind in session.kinds() {
            let mut of = self.exact.iter().filter(|it| it.kind == kind);
            let Some(exact) = of.next() else {
                changed.valued(kind);
                said.push((kind, How::Valued));
                continue;
            };
            if let Some(other) = of.next() {
                panic!(
                    "two producers say reads of kind {:#06x} exactly, `{}` and `{}`: one producer a kind",
                    kind.0, exact.by, other.by
                );
            }
            changed.exact(kind);
            for key in &exact.keys {
                changed.differs(Read { kind, key: key.as_slice() });
            }
            said.push((kind, How::Exact(exact.by)));
        }
        said
    }
}

/// A body part of one file's `items`: its item, what the item is (a function, a `const`, a
/// `static`, a variant's discriminant) and where its node starts in the file (its `lines`
/// row's start, attributes and doc comments included).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Keyed {
    pub item: ItemId,
    pub kind: Kind,
    pub start: u32,
}

/// The bodies of a file, from `items` and `lines`, the values of `items(file)` and its line
/// part: each key of `items` that is a body's part ([`key_item`]), with its entry's kind and
/// its row's start, sorted by `(start, kind)` as [`join`] takes them. Every body the syntax
/// has, at the file's top, in an inline module, an `impl` or a `trait`, or in another body. An
/// error names a table that does not read or a body with no entry or no row.
pub fn keyed(items: &[u8], lines: &[u8]) -> Result<Vec<Keyed>, String> {
    // alloc: error path
    let table = Table::read(ITEMS, items).map_err(|why| format!("`items`' value: {why:?}"))?;
    // alloc: error path
    let rows = Table::read(LINES, lines).map_err(|why| format!("`lines`' value: {why:?}"))?;
    // alloc: the file's bodies
    let mut out = Vec::new();
    for it in table.iter() {
        // alloc: error path
        let (key, _) = it.map_err(|why| format!("`items`' value: {why:?}"))?;
        let Some((item, true)) = key_item(key) else { continue };
        let kind = entry_at(&table, item)?.kind;
        let row = match rows.get(&item.0) {
            // alloc: error path
            Ok(Some(row)) => ItemLines::read(row).map_err(|why| format!("`lines` under {}: {why}", hex(&item.0)))?,
            // alloc: error path
            Ok(None) => return Err(format!("`lines` has no row of the body under {}", hex(&item.0))),
            // alloc: error path
            Err(why) => return Err(format!("`lines` under {}: {why:?}", hex(&item.0))),
        };
        out.push(Keyed { item, kind, start: row.start });
    }
    out.sort_unstable_by_key(|it| (it.start, it.kind));
    Ok(out)
}

fn entry_at<'a>(table: &Table<'a>, id: ItemId) -> Result<Entry<'a>, String> {
    match table.get(&id.0) {
        Ok(Some(bytes)) => Entry::read(bytes),
        // alloc: error path
        Ok(None) => Err(format!("`items` has no entry under {}", hex(&id.0))),
        // alloc: error path
        Err(why) => Err(format!("`items` under {}: {why:?}", hex(&id.0))),
    }
}

/// Where a body of the database is written: its file, where its item's node starts there and
/// what the item is, as [`Keyed`] says it of the syntax.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Written {
    pub file: base_db::FileId,
    pub start: u32,
    pub kind: Kind,
}

/// Where `def` is written; none where its item is in a macro's expansion (no file has its
/// node). A variant's body is its discriminant, whose node is where the syntax's row of it
/// starts (`blaze_front_items::items`, the variant's entry), and none where it has none.
pub fn written(db: &Db, def: DefWithBodyId) -> Option<Written> {
    let (id, kind) = match def {
        DefWithBodyId::FunctionId(it) => (it.lookup(db).id.erase(), Kind::Fn),
        DefWithBodyId::ConstId(it) => (it.lookup(db).id.erase(), Kind::Const),
        DefWithBodyId::StaticId(it) => (it.lookup(db).id.erase(), Kind::Static),
        DefWithBodyId::VariantId(it) => (it.lookup(db).id.erase(), Kind::Variant),
    };
    let file = id.file_id.file_id()?.file_id(db);
    let start = match def {
        DefWithBodyId::VariantId(it) => it.lookup(db).id.to_view(db).const_arg()?.syntax().text_range().start(),
        _ => id.to_range(db).start(),
    };
    Some(Written { file, start: u32::from(start), kind })
}

/// Each body of `written` (the bodies of the database written in the file `keyed` is of, as
/// [`written`] gives them) with the item of `keyed` (sorted, as [`keyed`] gives them) that
/// starts where it does and is of its kind: an item's start is its node's start on both sides.
/// A body with none is left out (the driver counts it).
pub fn join(keyed: &[Keyed], written: &[(DefWithBodyId, Written)]) -> Vec<(ItemId, DefWithBodyId)> {
    // alloc: the file's pairs
    let mut out = Vec::with_capacity(written.len());
    for &(def, at) in written {
        let found = match any_kind() {
            true => keyed.iter().find(|it| it.start == at.start),
            false => keyed
                .binary_search_by_key(&(at.start, at.kind), |it| (it.start, it.kind))
                .ok()
                .map(|found| &keyed[found]),
        };
        if let Some(it) = found {
            out.push((it.item, def));
        }
    }
    out
}

#[cfg(test)]
mod tests;
