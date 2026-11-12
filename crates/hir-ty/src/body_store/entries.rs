//! A body's inference, its built MIR and its borrow check's verdict as `blaze_kernel` step
//! entries (docs/v2/plan/design/end-state.md, "The front end as kernel steps", order row 3):
//! [`InferEntry`], [`MirEntry`] and [`BorrowckEntry`], each of one body by its syntax
//! ([`BodyKey`]: its file and its content-hashed `blaze_front_items` item id, never a dense id).
//!
//! An entry's reads, which are what a carry reaches it by:
//!
//! - **its own syntax record**: one part read of `items(file)` at
//!   [`blaze_front_items::entry::body_key`] of the item, so an edit of another item, or a
//!   comment between items, leaves it verified with no work;
//! - **its identity** ([`reads::IDENTITY`], valued by the item store's
//!   [`BodyFacts::identity`](super::BodyFacts::identity)): what the identity hashes beyond the
//!   body's tokens (its owner's signature, what each id it names is, what each path resolves
//!   to, its crate's inputs). [`Identities`] keeps item-hash's identity column from pin to pin
//!   and gives its delta exactly ([`BodyDelta::identities`]); a driver with no delta says it is
//!   always dirty and counts it by name ([`always_dirty`]);
//! - **its facts**: one read per fact the run's trace names ([`reads::fact`], the fact's kind
//!   as the read's kind, so a driver says each kind's difference apart), valued by the world
//!   ([`BodyWorld::fact`]); a fact that is the body's own earlier step
//!   ([`BodyFacts::own_step`](super::BodyFacts::own_step)) is a read of that step's entry.
//!
//! A step's run is the database's: [`BodyWorld::run`] asks it for the step, and the item store
//! kept what that run read and made ([`super::Store::traced`], written where the step offers
//! itself). A body with no trace, or whose run the store refused, is the entry's error naming
//! the body and why: no entry is kept for it (AGENTS.md rule 3).
//!
//! What is still trusted, and named: the identity covers every read the store's
//! classification calls covered (item-hash's section 6), as the item store's records already
//! trust; the world's map from a [`BodyKey`] to the database's body ([`BodyWorld::body`]) is
//! the one the syntax gives ([`bodies`], [`def_of`]).

use std::{fmt, marker::PhantomData};

use base_db::Db;
use blaze_cas::Kind as StoreKind;
use blaze_format::SchemaHash;
use blaze_front_items::{
    ITEMS, Table,
    entry::{Entry, ItemId, Kind, body_key},
    steps::{FileKey, Items, Source},
};
use blaze_ids::ContentId;
use blaze_kernel::{
    Ask, Changed, CodeId, Compute, Fact, Get, Kernel, KernelError, Read, ReadKind, Reads, Seen,
    Session, Step as KernelStep, StepError, StepId, Subject, WorldError, steps as ids,
};
use blaze_query::QueryKind;
use hir_def::{DefWithBodyId, ModuleDefId, ModuleId, nameres::DefMap, signatures::FunctionSignature};
use hir_expand::name::Name;

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

    /// Asks the database for `step` of `def` (its inference, its built MIR, its borrow
    /// check), so that the run that made its value, now or earlier, left its trace in the
    /// item store; gives the schema it was offered under ([`infer_schema`], [`mir_schema`],
    /// [`borrowck_schema`]). A borrow check that fails is an error naming the body.
    fn run(&self, def: DefWithBodyId, step: Step) -> Result<SchemaHash, String>;

    /// The value of `fact` as the crate named `krate` sees it now: gone where it names
    /// nothing; [`WorldError::not_now`] where it cannot be hashed now.
    fn fact(&self, krate: &str, fact: FactRef) -> Result<Seen, WorldError>;
}

const SUBJECT: &str = "blaze v2 body entry 1";

/// `key`'s stable bytes: the length of its file's key (two bytes, big-endian), the file's key
/// as `items(file)` is keyed, the item's id, then the crate's name.
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

fn subject<W: BodyWorld + ?Sized + 'static>(key: &BodyKey) -> Subject {
    // alloc: the input's bytes, once an identity
    let mut bytes = Vec::new();
    key_of::<W>(key, &mut bytes);
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

/// The schema [`BodyWorld::run`] gives for a body's built MIR; an error for a body with no
/// MIR of its own.
pub fn mir_schema(db: &Db, def: DefWithBodyId) -> Result<SchemaHash, String> {
    let owner = crate::mir::MirOwner::Body(crate::InferBodyId::DefWithBodyId(def));
    match super::mir::place(db, owner) {
        Some((_, schema)) => Ok(schema),
        // alloc: error path
        None => Err(format!("{def:?} has no MIR of its own")),
    }
}

/// Asks `db` for `def`'s built MIR: the run of [`BodyWorld::run`] for [`Step::Mir`].
pub fn run_mir(db: &Db, def: DefWithBodyId) -> Result<SchemaHash, String> {
    let schema = mir_schema(db, def)?;
    let owner = crate::mir::MirOwner::Body(crate::InferBodyId::DefWithBodyId(def));
    let _ = crate::providers::mir_at(db, owner, crate::mir::MirStage::Built);
    Ok(schema)
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

/// The run of `step` of the body `input`: its reads, then the answer its database's run made.
fn compute<W: BodyWorld + ?Sized + 'static>(
    ctx: &mut Reads<'_, W>,
    input: &BodyKey,
    step: Step,
) -> Result<Vec<u8>, StepError> {
    #[cfg(test)]
    RUNS.with(|it| {
        let mut runs = it.get();
        runs[step as usize] += 1;
        it.set(runs);
    });
    // (a) The body's own syntax record.
    if ctx.part::<Items<W>>(&input.file, &body_key(input.item))?.is_none() {
        return Err(refused(format!("{input:?}: `items` of its file has no body under its id")));
    }
    // Its identity, and through it the body in the world's database.
    let Body { world, def } = ctx.get::<IdentityOf<W>>(input)?;
    #[cfg(test)]
    if dropped(reads::IDENTITY) {
        ctx.forget::<IdentityOf<W>>(input);
    }
    let schema = world.run(def, step).map_err(|why| refused(format!("{input:?} ({def:?}): {why}")))?;
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
            Some(Step::Mir) if step > Step::Mir => {
                ctx.step::<MirEntry<W>>(input)?;
            }
            Some(earlier) => {
                return Err(refused(format!(
                    "{input:?}: its {} read its own {}, which is no earlier step with an entry",
                    super::what(step),
                    super::what(earlier)
                )));
            }
            None => {
                key.clear();
                let kind = fact_read(fact, &input.krate, &mut key);
                ctx.hash(Read { kind, key: &key })?;
            }
        }
    }
    Ok(answer.to_vec())
}

macro_rules! body_step {
    ($(#[$doc:meta])* $name:ident, $id:expr, $text:literal, $step:expr) => {
        $(#[$doc])*
        pub struct $name<W: ?Sized>(PhantomData<fn(&W)>);

        impl<W: BodyWorld + ?Sized + 'static> KernelStep for $name<W> {
            const ID: StepId = $id;
            const NAME: &'static str = $text;
            const KIND: StoreKind = StoreKind::Query;
            type Input = BodyKey;

            fn subject(input: &BodyKey) -> Subject {
                subject::<W>(input)
            }

            fn label(input: &BodyKey) -> String {
                // alloc: a label, for an error or a check
                format!("{} of {input:?}", $text)
            }
        }

        impl<W: BodyWorld + ?Sized + 'static> Compute for $name<W> {
            type World = W;

            fn compute(ctx: &mut Reads<'_, W>, input: &BodyKey) -> Result<Vec<u8>, StepError> {
                compute(ctx, input, $step)
            }
        }

        impl<W: BodyWorld + ?Sized + 'static> Ask for $name<W> {
            fn key(input: &BodyKey, out: &mut Vec<u8>) {
                key_of::<W>(input, out);
            }

            fn input(key: &[u8]) -> Result<BodyKey, String> {
                input_of::<W>(key)
            }
        }
    };
}

body_step!(
    /// `infer(body)`: the body's inference answer, as the item store encodes it.
    InferEntry, ids::BODY_INFER, "body inference", Step::Infer
);
body_step!(
    /// `mir(body)`: the body's built MIR, as the item store encodes it.
    MirEntry, ids::BODY_MIR, "body MIR", Step::Mir
);
body_step!(
    /// `borrowck(body)`: that the body's borrow check passed (no bytes; a failing check is no
    /// entry, but the error of whoever asks).
    BorrowckEntry, ids::BODY_BORROWCK, "body borrow check", Step::Borrowck
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

/// What differs, between two pins, of the reads body entries make, as the maintainers'
/// deltas say it (end-state.md review point 8). A kind said by none of these is left unsaid,
/// and a carry of a session with reads of it is refused naming it (`CarryError::Unsaid`):
/// never taken as unchanged.
#[derive(Default)]
pub struct BodyDelta {
    /// The fact kinds whose difference a maintainer's delta gives exactly (impls' last write,
    /// name resolution's bindings), each with the facts that differ and the crate each is
    /// seen from.
    pub exact: Vec<(ReadKind, Vec<(FactRef, String)>)>,
    /// The fact kinds no delta gives yet, each by name: every entry with a read of one is
    /// valued again at the carry ([`always_dirty`] counts them).
    pub unknown: Vec<(ReadKind, &'static str)>,
    /// The bodies whose identity differs, where a delta gives them; `None` while none does,
    /// and every identity read is always dirty.
    pub identities: Option<Vec<BodyKey>>,
}

/// The name [`always_dirty`] gives identity reads by.
pub const IDENTITY_NAME: &str = "body identity";

impl BodyDelta {
    /// Says on `changed` what this delta says.
    pub fn say<W: BodyWorld + ?Sized + 'static>(&self, changed: &mut Changed) {
        match &self.identities {
            None => changed.everything(reads::IDENTITY),
            Some(bodies) => {
                changed.exact(reads::IDENTITY);
                // alloc: one read's name at a time, reused
                let mut key = Vec::new();
                for body in bodies {
                    key.clear();
                    key_of::<W>(body, &mut key);
                    changed.differs(Read { kind: reads::IDENTITY, key: &key });
                }
            }
        }
        // alloc: one read's name at a time, reused
        let mut key = Vec::new();
        for (kind, facts) in &self.exact {
            changed.exact(*kind);
            for (fact, krate) in facts {
                key.clear();
                let read = fact_read(*fact, krate, &mut key);
                assert_eq!(read, *kind, "a fact listed under another kind than its own");
                changed.differs(Read { kind: read, key: &key });
            }
        }
        for (kind, _) in &self.unknown {
            changed.everything(*kind);
        }
    }

    /// The kinds it says are always dirty, each with its name.
    fn dirty(&self) -> impl Iterator<Item = (ReadKind, &'static str)> + '_ {
        let identity = self.identities.is_none().then_some((reads::IDENTITY, IDENTITY_NAME));
        identity.into_iter().chain(self.unknown.iter().copied())
    }
}

/// For each kind `delta` says is always dirty, by name: how many of `session`'s entries read
/// one, which a carry under `delta` values again whatever changed. Asked before the carry.
pub fn always_dirty<G: Get + ?Sized>(
    session: &mut Session,
    store: &G,
    delta: &BodyDelta,
) -> Result<Vec<(&'static str, usize)>, KernelError> {
    delta.dirty().map(|(kind, name)| Ok((name, session.readers_of(store, kind)?))).collect()
}

/// Item-hash's identity column of a pin's bodies, kept from one pin to the next: what gives
/// [`BodyDelta::identities`] exactly, so no identity read is always dirty. Each body's
/// identity is the item store's ([`BodyFacts::identity`](super::BodyFacts::identity), a
/// memoized query: after an edit recomputed only where one of its parts changed), and the
/// delta is every body whose identity is other bytes than at the last pin, is new, or is gone.
///
/// Still owed: a delta made from the parts' own deltas (the edited files' bodies, the
/// readers of an item whose signature changed) without asking each body's identity.
#[derive(Default)]
pub struct Identities {
    /// Each body by its key's bytes ([`key_of`]), with its identity at the last pin: none
    /// where it had none.
    kept: std::collections::BTreeMap<Vec<u8>, (BodyKey, Option<ContentId>)>,
}

impl Identities {
    /// Keeps the identities of `keys`, the bodies of `world`'s pin, and gives the bodies whose
    /// identity differs from the last pin's: other bytes, new, or gone (in key order). An
    /// error names the body whose identity cannot be had.
    pub fn delta<W: BodyWorld + ?Sized + 'static>(
        &mut self,
        world: &W,
        keys: &[BodyKey],
    ) -> Result<Vec<BodyKey>, String> {
        let db = world.db();
        // alloc: one pin's column, replacing the last
        let mut now = std::collections::BTreeMap::new();
        for key in keys {
            let def = world.body(key)?;
            let Some(identity) = with_store(db, |store| store.facts.identity(db, def)) else {
                // alloc: error path
                return Err(format!("{key:?}: the database has no item store installed"));
            };
            // alloc: one body's key
            let mut bytes = Vec::new();
            key_of::<W>(key, &mut bytes);
            // clone: the key, kept with its identity
            now.insert(bytes, (key.clone(), identity));
        }
        // alloc: the delta
        let mut differs = Vec::new();
        for (bytes, (key, identity)) in &now {
            if self.kept.get(bytes).is_none_or(|(_, kept)| kept != identity) {
                // clone: one body of the delta
                differs.push(key.clone());
            }
        }
        for (bytes, (key, _)) in &self.kept {
            if !now.contains_key(bytes) {
                // clone: one body of the delta
                differs.push(key.clone());
            }
        }
        self.kept = now;
        Ok(differs)
    }
}

/// A body the syntax of one file has: its item, what it is, the inline modules it is in from
/// the file's own module out, and its name.
#[derive(Clone, PartialEq, Eq)]
pub struct Named {
    pub item: ItemId,
    pub kind: Kind,
    pub modules: Vec<String>,
    pub name: String,
}

/// The bodies of `items`, the value of `items(file)`, in the order written: a function with a
/// body at the file's top or in an inline module of it. Not yet: the bodies of `impl` and
/// `trait` items, `const`s and `static`s; [`bodies`] gives how many of those it passed over,
/// so a driver says what it did not key.
pub fn bodies(items: &[u8]) -> Result<(Vec<Named>, usize), String> {
    // alloc: error path
    let table = Table::read(ITEMS, items).map_err(|why| format!("`items`' value: {why:?}"))?;
    // alloc: the file's bodies
    let mut out = Vec::new();
    let mut passed = 0;
    walk(&table, ItemId::ROOT, &mut Vec::new(), &mut out, &mut passed)?;
    Ok((out, passed))
}

fn walk(
    table: &Table<'_>,
    at: ItemId,
    modules: &mut Vec<String>,
    out: &mut Vec<Named>,
    passed: &mut usize,
) -> Result<(), String> {
    let entry = entry_at(table, at)?;
    for child in entry.children() {
        let it = entry_at(table, child)?;
        match it.kind {
            Kind::Fn if it.has_body() => {
                // alloc: one body's name and modules
                out.push(Named { item: child, kind: it.kind, modules: modules.clone(), name: it.name.to_owned() });
            }
            Kind::Mod => {
                // alloc: one module's name, while it is walked
                modules.push(it.name.to_owned());
                walk(table, child, modules, out, passed)?;
                modules.pop();
            }
            Kind::Impl | Kind::Trait => *passed += it.children().len(),
            Kind::Const | Kind::Static if it.has_body() => *passed += 1,
            _ => {}
        }
    }
    Ok(())
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

/// The body `named` is in `db`, where `file_module` is the module whose file `named` is of:
/// the module its inline modules lead to by name, and the one function of its name declared
/// there. An error names what is not there, or that two are.
pub fn def_of(
    db: &Db,
    def_map: &DefMap,
    file_module: ModuleId,
    named: &Named,
) -> Result<DefWithBodyId, String> {
    let mut module = file_module;
    for name in &named.modules {
        let data = def_map
            .modules(db)
            .find(|(id, _)| *id == module)
            .map(|(_, data)| data)
            // alloc: error path
            .ok_or_else(|| format!("no module {module:?} in its crate's def map"))?;
        module = *data
            .children(db)
            .get(&Name::new_root(name))
            // alloc: error path
            .ok_or_else(|| format!("no module `{name}` in {module:?}"))?;
    }
    let data = def_map
        .modules(db)
        .find(|(id, _)| *id == module)
        .map(|(_, data)| data)
        // alloc: error path
        .ok_or_else(|| format!("no module {module:?} in its crate's def map"))?;
    let name = Name::new_root(&named.name);
    let mut found = data.scope(db).declarations().filter_map(|def| match def {
        ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name == name => Some(it),
        _ => None,
    });
    match (found.next(), found.next()) {
        (Some(it), None) => Ok(DefWithBodyId::FunctionId(it)),
        // alloc: error path
        (None, _) => Err(format!("no function `{}` in {module:?}", named.name)),
        // alloc: error path
        (Some(_), Some(_)) => Err(format!("two functions `{}` in {module:?}", named.name)),
    }
}

#[cfg(test)]
mod tests;
