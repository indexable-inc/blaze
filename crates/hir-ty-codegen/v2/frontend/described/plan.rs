//! An instance's plan as a record (docs/v2/plan/design/command.md, "Code"; the no-daemon
//! command's decision 1): its describe, keyed by what the instance is and filed by the driver
//! at the address of what it read, with its edges named so a driver with no database keys the
//! plans of what it refers to.
//!
//! **The name** ([`HirFrontend::plan_ask`]) is what the plan is keyed by. It is
//! `Hasher("blaze v2 instance plan name 2")` over, in order:
//!
//! 1. the code generator's code identity (the policy's toolchain id), as a describe record's
//!    producer;
//! 2. [`HirFrontend::record_stage`]'s digest: the toolchain whole (target, CPU, features), how
//!    each origin's units build bodies, by content, whether the build links an image and
//!    whether it is build-time code;
//! 3. the ask ([`ask_written`], "bzask001"): the instance ported (its body by its item's key,
//!    its arguments as types by content, its home by its crate key) and the origin it is
//!    compiled for ([`HirFrontend::compiled_for`]: its crate's own unless its users compile it,
//!    else the requester's), by kind and package name.
//!
//! Why that is every input of the plan that is not a read (rule 21): a describe reads the
//! database and, of the front end, what `Facing` holds (units, build time, image, the hashes'
//! kind, the program's world); the units, build time and image are (2), the hashes are blaze
//! items' (a front end keyed by the tests' ids or walked against a program's world has no
//! stage and so no plan), the requester reaches the describe only through the origin it is
//! compiled for (3). Everything else the describe reads it reads of the database, which the
//! plan task records as its read list (blaze-command's `code::plan`).
//!
//! What it holds of the machine (rule 25): nothing. No describe reads a file's name, text or
//! root: a `file!()` alone, every panic's and `#[track_caller]` location, and a `concat!` with
//! `file!()` are spans, item-relative, which the link names from the span side table (a
//! `Location` record, blaze-link's `position`; a text, `blaze_codegen::text`, rule 71).
//!
//! **The record** ([`HirFrontend::plan`]), little-endian:
//!
//! `"bzplan02"` | u32 length + the ask | u32 length + the describe ([`encode_described`],
//! its origin always written, never "the requester's") | a flag and, when set, the key of the
//! body its own span tables are of (32 bytes: [`plan_body`], set exactly when the head has its
//! own anchors, so a driver with no database knows which owner's span task makes each) | u32
//! count + one edge per reference of the describe, in its order:
//!
//! - `0` a function: its plan's name (32 bytes), then u32 length + its ask, asked for by this
//!   instance's origin, as the driver's walk asks a callee; `4` the same with the key of the
//!   body its plan reads ([`plan_body`]) between the name and the ask;
//! - `1` a static: its static plan's name, its item key and instance (32 bytes each), then a
//!   flag and, when set, the symbol it defines weakly (u32 length + text): a reference to it is
//!   by that name. The name is there so a driver with no database keys the static's plan task
//!   (its ask is [`static_ask`] of the item key), as a function edge's is;
//! - `2` an outside symbol, `3` an outside symbol linked weakly: u32 length + its name.
//!
//! Every byte of it is a function of the name and the reads: two builders of one name over one
//! universe write equal bytes.
//!
//! **The reads** (blaze-command's `code::plan`) are what the describe read of the database,
//! each row classified by `blaze_item_hash::reads::fact_of_in` asked by the instance's home
//! ([`HirFrontend::plan_home`]) and covering no body: a plan's key holds no body's text, so
//! nothing is covered and every row is a fact by key (a body's MIR is its `StepMir` fact, the
//! body record a body task makes; decision 3).
//!
//! **An instance with no body** (drop glue, the call-once, fn-pointer, reify and vtable shims,
//! the C `main`) is planned as any other, by the same two parts:
//!
//! - its key is its name: the stage and its ask, which is the shim's kind and its one type
//!   argument by content (drop glue of `T`: `T` ported, each ADT by its item key and each
//!   argument of it by content), the function a vtable shim wraps by its item key, and its home;
//! - its reads are the facts of the types and items it is made from, as its describe read
//!   them: for drop glue of `T`, `T`'s fields and layout, whether each field's type has a
//!   `Drop` impl (its `DropCheck` fact, and the `Implements` facts of `Drop` the solver read)
//!   and those types' own fields, down to what needs no drop; for a shim, the signature of
//!   the function it calls and what its type argument resolves to.
//!
//! Why a change to the type moves it (rule 21): the name holds `T` by content, so another `T`
//! is another plan; a change to `T`'s definition (a field added, a field's type gaining a
//! `Drop` impl, a layout moved) changes the answer of a fact in its read list (the field's type
//! `Fields`, its `DropCheck` or `Implements` fact), so the driver finds no record at the new
//! address and plans it again. Nothing else reaches a describe: its home is in the ask, and the
//! drop impls its glue calls are edges, each a plan of its own. The test: blaze-command's
//! `code::tests`, whose planted control keeps only the type-shape facts (signatures, layouts,
//! fields) of the read list and is caught missing the move.
//!
//! **A static's plan** ([`HirFrontend::static_plan_ask`], [`HirFrontend::static_plan`]) is the
//! same three parts for a static item, which is no generic:
//!
//! - its name: `Hasher("blaze v2 static plan name 2")` over the producer and stage (1, 2
//!   above) and its ask:
//!   `"bzsask01"` and the static's item key (40 bytes; found again by
//!   `blaze_items::key::resolve`);
//! - its reads: what describing it read (its value evaluated and shaped), classified as a
//!   function plan's, asked by the static's crate ([`HirFrontend::static_plan_home`]);
//! - its record, little-endian: `"bzsplan1"` | u32 length + the ask | the head: item key,
//!   instance, value (32 bytes each), the origin (tag 1 std, 2 dependency, 3 build time,
//!   4 member, then u32 length + package), flags (1 mutable, 2 thread-local, 4 defined weakly,
//!   8 the workspace's), section and symbol (a flag, then u32 length + text), u32 length + its
//!   path | u32 count + one edge per reference of its value, as a function plan's.
//!
//! A static's code is its template ([`blaze_codegen::template::StaticTemplate`]) under
//! `blaze_codegen::driver::static_shape_key`: the value's content (which covers its bytes,
//! alignment, relocations by place and imports), its flags, section and its references by
//! kind, never which static it is; the driver's build keys it by the same function.
//!
//! Floor: one describe of the instance and one ask per callee (a port of its few words); the
//! record is the describe's bytes and the edges, copied once into the caller's allocator.

use std::alloc::Allocator;

use super::*;

/// The first bytes of a plan record: a plan of another layout is refused by name.
pub const PLAN_MAGIC: &[u8; 8] = b"bzplan02";

/// The domain of a plan's name.
const NAME_DOMAIN: &str = "blaze v2 instance plan name 2";

/// The tags of an edge.
const EDGE_FUNCTION: u8 = 0;
const EDGE_STATIC: u8 = 1;
const EDGE_EXTERN: u8 = 2;
const EDGE_WEAK: u8 = 3;
/// A function whose plan reads a body's MIR: [`EDGE_FUNCTION`]'s with the body's key between
/// the name and the ask. A tag of its own, so a record written before it reads as it was.
const EDGE_FUNCTION_BODY: u8 = 4;

/// What a plan is refused as, by the rule.
pub const NO_STAGE: &str = "a plan: the front end has no record stage (made with no policy, keyed by a database's ids, or walked against a program's world)";
pub const NO_PORTABLE_ASK: &str = "a plan: the instance has no portable form (one worked out in an environment that is not empty, or one naming an item of a crate its home does not load)";
pub const NO_PORTABLE_DESCRIBE: &str = "a plan: its describe refers to something no item key of its home's closure names";
pub const NAME_MOVED: &str = "a plan: the ask the driver gave names another plan in this front end than the name it gave (another stage, or another origin)";
pub const NO_STATIC_KEY: &str = "a plan: a static its describe refers to has no item key";
pub const NO_STATIC_LINKAGE: &str = "a plan: a static its describe refers to has a `#[linkage]` that does not read";
pub const NOT_A_PLAN: &str = "a plan record: its first bytes are not `bzplan02`";
pub const NO_SPAN_OWNER: &str = "a plan: its head has its own span anchors and the instance names no body they are of (`plan_body`); or a record whose owner flag disagrees with its head's anchors";
pub const BAD_EDGE: &str = "a plan record: an edge of no kind";
pub const EDGES_DIFFER: &str = "a plan record: its edges are not one per reference of its describe";
pub const ORIGIN_MOVED: &str = "a plan: the describe is compiled for another origin than its ask names";

/// Why a plan could not be made or read, its words in the caller's allocator.
pub enum PlanFailed<A: Allocator> {
    /// The describe failed, naming the instance (rule 19).
    Describe(CodegenError),
    /// Refused by the rule named.
    Refused(&'static str),
    /// Bytes that do not read in this database, and why.
    Unread(Box<str, A>),
}

impl<A: Allocator> From<&'static str> for PlanFailed<A> {
    fn from(what: &'static str) -> PlanFailed<A> {
        PlanFailed::Refused(what)
    }
}

impl<A: Allocator> std::fmt::Debug for PlanFailed<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanFailed::Describe(error) => f.debug_tuple("Describe").field(error).finish(),
            PlanFailed::Refused(what) => f.debug_tuple("Refused").field(what).finish(),
            PlanFailed::Unread(why) => f.debug_tuple("Unread").field(&&**why).finish(),
        }
    }
}

/// Why bytes did not read (a decoder's words), copied into `alloc`.
fn unread<A: Allocator>(why: &str, alloc: A) -> PlanFailed<A> {
    PlanFailed::Unread(blaze_mem::String::from_str_in(why, alloc).into_boxed_str())
}

/// An instance as a plan task is asked for it: the plan's name and the ask its task reads back
/// ([`HirFrontend::plan`]).
pub struct PlanAsk<A: Allocator> {
    pub name: ContentId,
    pub ask: Vec<u8, A>,
    /// The body whose MIR the plan reads ([`plan_body`]), as [`Edge::Function`] carries it;
    /// `None` for a static's ask (see [`Edge::Static`]'s plan) and an instance with none.
    pub body: Option<ContentId>,
}

/// A plan made: its name and its record's bytes (the payload the driver files at the address
/// of what the task read).
pub struct Planned<A: Allocator> {
    pub name: ContentId,
    pub bytes: Vec<u8, A>,
}

/// A part of a plan's making, as [`HirFrontend::plan_reads_by_part`] names it: reading the ask
/// back, naming the plan, the describe, the closure, encoding the describe, the span owner, and
/// each edge by its index in the describe's references.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanPart {
    Ask,
    Name,
    Describe,
    Closure,
    Encode,
    Owner,
    Edge(u32),
}

/// Each part of a plan's making with the rows it read, as
/// [`HirFrontend::plan_reads_by_part`] gives them.
#[doc(hidden)]
pub type PartReads = Vec<(PlanPart, Vec<blaze_memo::Read, Global>), Global>;

/// One edge of a plan, read with no database.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge<'r> {
    /// A function: its plan's name, the ask its plan task is given, and the key of the body
    /// whose MIR its describe reads ([`plan_body`]), which the driver gives its plan task on the
    /// first run (the driver's `StableId` of a body record); `None` for an instance with none.
    Function { name: ContentId, ask: &'r [u8], body: Option<ContentId> },
    /// A static: its static plan's name, its item and instance, and the symbol it defines
    /// weakly, if it does. Its plan task's ask is [`static_ask`] of `item`.
    Static { name: ContentId, item: ItemKey, instance: ContentId, weak: Option<&'r str> },
    Extern(&'r str),
    Weak(&'r str),
}

/// A plan record read in place, with no database.
#[derive(Clone, Copy, Debug)]
pub struct PlanView<'r> {
    /// What its task was asked ([`ask_written`]'s bytes).
    pub ask: &'r [u8],
    /// The describe record's bytes.
    pub describe: &'r [u8],
    /// What the describe says that names no item.
    pub head: Head<'r>,
    /// The key of the body `head.anchors` are the span tables of: `Some` exactly when they are.
    pub span_owner: Option<ContentId>,
    edges: &'r [u8],
    count: usize,
}

/// The edges of a plan, in the order of its describe's references.
pub struct Edges<'r> {
    bytes: Reader<'r>,
    left: usize,
}

fn put_u32_in<A: Allocator>(out: &mut Vec<u8, A>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes_in<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) -> Result<(), &'static str> {
    let len = u32::try_from(bytes.len()).map_err(|_| "a plan record: a part past 4 GiB")?;
    put_u32_in(out, len);
    out.extend_from_slice(bytes);
    Ok(())
}

impl<'r> PlanView<'r> {
    /// The plan record `bytes`, read in place: its ask, its describe's head and where its
    /// edges are. One pass over the head; the edges are read by [`Self::edges`].
    pub fn read(bytes: &'r [u8]) -> Result<PlanView<'r>, &'static str> {
        let mut reader = Reader::new(bytes);
        if reader.array::<8>().map_err(cursor)? != PLAN_MAGIC {
            return Err(NOT_A_PLAN);
        }
        let ask = reader.bytes_u32().map_err(cursor)?;
        let describe = reader.bytes_u32().map_err(cursor)?;
        let (head, mut refs) = Head::of(describe)?;
        let references = refs.u32().map_err(cursor)? as usize;
        let span_owner = match flag_of(reader.u8().map_err(cursor)?)? {
            true => Some(id_of(&mut reader)?),
            false => None,
        };
        if span_owner.is_some() != head.anchors.is_some() {
            return Err(NO_SPAN_OWNER);
        }
        let count = reader.u32().map_err(cursor)? as usize;
        if count != references {
            return Err(EDGES_DIFFER);
        }
        let edges = reader.take_rest();
        Ok(PlanView { ask, describe, head, span_owner, edges, count })
    }

    /// Its edges, in the order of its describe's references: the places its code is made
    /// against.
    pub fn edges(&self) -> Edges<'r> {
        Edges { bytes: Reader::new(self.edges), left: self.count }
    }

    /// How many edges it has.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl<'r> Iterator for Edges<'r> {
    type Item = Result<Edge<'r>, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return match self.bytes.finish() {
                Ok(()) => None,
                Err(error) => {
                    // Bytes past the last edge: once, then nothing.
                    self.bytes = Reader::new(&[]);
                    Some(Err(cursor(error)))
                }
            };
        }
        self.left -= 1;
        Some(self.edge())
    }

    /// One item an edge left, and one more where bytes past the last edge are an error.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, self.left.checked_add(1))
    }
}

impl<'r> Edges<'r> {
    fn edge(&mut self) -> Result<Edge<'r>, &'static str> {
        let bytes = &mut self.bytes;
        Ok(match bytes.u8().map_err(cursor)? {
            EDGE_FUNCTION => {
                let name = id_of(bytes)?;
                Edge::Function { name, ask: bytes.bytes_u32().map_err(cursor)?, body: None }
            }
            EDGE_FUNCTION_BODY => {
                let name = id_of(bytes)?;
                let body = Some(id_of(bytes)?);
                Edge::Function { name, ask: bytes.bytes_u32().map_err(cursor)?, body }
            }
            EDGE_STATIC => {
                let name = id_of(bytes)?;
                let item = ItemKey(id_of(bytes)?);
                let instance = id_of(bytes)?;
                let weak = match flag_of(bytes.u8().map_err(cursor)?)? {
                    true => Some(bytes.str_u32().map_err(cursor)?),
                    false => None,
                };
                Edge::Static { name, item, instance, weak }
            }
            EDGE_EXTERN => Edge::Extern(bytes.str_u32().map_err(cursor)?),
            EDGE_WEAK => Edge::Weak(bytes.str_u32().map_err(cursor)?),
            _ => return Err(BAD_EDGE),
        })
    }
}

/// The key of the body whose MIR the describe of `instance` reads, as the driver names a body
/// record (`blaze_item_hash::values::body_key`, the key the given store looks a body up by):
/// a function's own, where it has one; for a closure or coroutine, the item it is written in,
/// whose body record holds its MIR. `None` for every other: a shim, drop glue, the C `main`
/// (its describe calls `main`, reading its signature, never its MIR), `asm!`, the allocator's,
/// a declared function and a closure in an anonymous constant (no body record of its own).
/// Unlike [`body_of`] (the body whose check an instance waits on), which counts the C `main`
/// and a vtable shim: a plan given a body it does not read would hold its MIR fact in its read
/// list and be planned again on every edit of it.
///
/// Floor: a few lookups; no MIR is built.
fn plan_body(db: &Db, instance: &HirInstance) -> Option<ContentId> {
    let Kind::Code(code, _) = &instance.0 else {
        return None;
    };
    let owner: DefWithBodyId = match code.def {
        InstanceDef::Fn(func) if FunctionSignature::of(db, func).has_body() => func.into(),
        def @ (InstanceDef::Closure(_) | InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_)) => {
            match closure_loc(db, def)?.owner {
                InferBodyId::DefWithBodyId(owner) => owner,
                InferBodyId::AnonConstId(_) => return None,
            }
        }
        _ => return None,
    };
    Some(blaze_item_hash::values::body_key(db, owner))
}

/// The crates `instance` may name ([`Closure`]): its home and what it loads; for the C `main`,
/// `main`'s crate and what it loads; for an allocator shim entry, its program's crate and what
/// it loads; none for an `asm!` function.
fn closure_of<'db>(db: &'db Db, instance: &HirInstance) -> Option<Closure<'db>> {
    match &instance.0 {
        Kind::Code(_, env) => Some(Closure::of(db, env.krate)),
        Kind::Start(main) => Some(Closure::of(db, main.krate(db))),
        Kind::Allocator(forward) => Some(Closure::of(db, forward.krate)),
        Kind::Asm(..) => Some(Closure { db, crates: &[] }),
    }
}

impl<'db> HirFrontend<'db> {
    /// What a plan task is asked for `instance` when `requester` asks for it: its name (this
    /// module's doc, "The name") and its ask. Read only of the database and this front end's
    /// stage; a plan task asked it reads it back in another database of the same sources.
    pub fn plan_ask<A: Allocator + Clone>(
        &self,
        instance: &HirInstance,
        requester: &Origin,
        alloc: A,
    ) -> Result<PlanAsk<A>, PlanFailed<A>> {
        self.with(|db| {
            let origin = self.compiled_for(db, instance, requester);
            let (name, ask) = self.plan_name_in(db, instance, &origin)?;
            let mut out = Vec::with_capacity_in(ask.len(), alloc);
            out.extend_from_slice(&ask);
            Ok(PlanAsk { name, ask: out, body: plan_body(db, instance) })
        })
    }

    /// The name of the plan of `instance` compiled for `origin`, with its ask.
    fn plan_name_in(
        &self,
        db: &Db,
        instance: &HirInstance,
        origin: &Origin,
    ) -> Result<(ContentId, Vec<u8>), &'static str> {
        let (producer, stage) = self.record_stage().ok_or(NO_STAGE)?;
        // clone: the ask's pair, a few ids, once per name
        let ask = ask_written(db, &(instance.clone(), *origin)).ok_or(NO_PORTABLE_ASK)?;
        let mut hasher = Hasher::new(NAME_DOMAIN);
        hasher.id(&producer).id(&stage).part(&ask);
        Ok((hasher.finish(), ask))
    }

    /// The plan of the instance `ask` names ([`Self::plan_ask`]), whose name the driver gave as
    /// `name`: the instance read back in this database, described, and written as this
    /// module's doc says ("The record"), each callee asked for by the origin the instance is
    /// compiled for. What it reads of the database is the caller's to record (it runs inside
    /// `hir_ty::body_store::capture`). A name this front end does not give the ask is refused:
    /// the driver keyed the plan by another stage than this one's (rule 21).
    pub fn plan<A: Allocator + Clone>(
        &self,
        ask: &[u8],
        name: &ContentId,
        alloc: A,
    ) -> Result<Planned<A>, PlanFailed<A>> {
        self.with(|db| {
            let (instance, origin) = ask_read(db, ask).map_err(|why| unread(&why, alloc.clone()))?;
            let (again, written) = self.plan_name_in(db, &instance, &origin)?;
            if again != *name || written != ask {
                return Err(PlanFailed::Refused(NAME_MOVED));
            }
            let described =
                self.described(db, &instance, &origin, None).map_err(PlanFailed::Describe)?;
            if described.origin != origin {
                return Err(PlanFailed::Refused(ORIGIN_MOVED));
            }
            let closure = closure_of(db, &instance).ok_or(NO_PORTABLE_ASK)?;
            let record = encode_described(db, &closure, Some(&described.origin), &described)
                .ok_or(NO_PORTABLE_DESCRIBE)?;
            let mut out = Vec::with_capacity_in(
                PLAN_MAGIC.len() + 8 + ask.len() + record.len() + 4 + 40 * described.refs.len(),
                alloc,
            );
            out.extend_from_slice(PLAN_MAGIC);
            put_bytes_in(&mut out, ask)?;
            put_bytes_in(&mut out, &record)?;
            match described.anchors {
                Some(_) => {
                    let owner = plan_body(db, &instance).ok_or(NO_SPAN_OWNER)?;
                    out.push(1);
                    out.extend_from_slice(&owner.0);
                }
                None => out.push(0),
            }
            let count = u32::try_from(described.refs.len()).map_err(|_| EDGES_DIFFER)?;
            put_u32_in(&mut out, count);
            for reference in &described.refs {
                self.put_edge(db, &described.origin, reference, &mut out)?;
            }
            Ok(Planned { name: *name, bytes: out })
        })
    }

    /// What each part of [`Self::plan`] reads, for a test that must name which part made a
    /// read: each part run on its own under `blaze_memo::record` in this database, in
    /// [`Self::plan`]'s order. Each list is whole (the recorder lists the rows of every query a
    /// part asks, hit or run), so the order of the parts changes no list. A part that fails gives
    /// its reads, and the parts that need its value are left out. Not for the build: it runs the
    /// plan's work again per part.
    ///
    /// # Errors
    ///
    /// [`blaze_memo::RecorderOff`] when this database's memo records no reads.
    ///
    /// Why not one reader per row: `blaze_memo::record` flattens the rows of every query asked
    /// into one list (`flatten_in`), dropping which entry read each. Naming the innermost
    /// tracked query per row needs the recorder to keep, beside each row of `taken.reads` and of
    /// each callee's list, the `KindId` of the entry whose run read it, and a `record_readers`
    /// that gives `(Read, KindId)` pairs before the flatten; blaze-memo is not this lane's.
    #[doc(hidden)]
    pub fn plan_reads_by_part(&self, ask: &[u8]) -> Result<PartReads, blaze_memo::RecorderOff> {
        /// One part run under the recorder, its reads pushed under `which`.
        fn part<T>(
            memo: &blaze_memo::Memo,
            which: PlanPart,
            out: &mut PartReads,
            work: impl FnOnce() -> Option<T>,
        ) -> Result<Option<T>, blaze_memo::RecorderOff> {
            let (value, reads) = blaze_memo::record(memo, work)?;
            out.push((which, reads));
            Ok(value)
        }
        self.with(|db| {
            let memo = blaze_memo::Database::memo(db);
            let mut out = Vec::new_in(Global);
            let Some((instance, origin)) =
                part(memo, PlanPart::Ask, &mut out, || ask_read(db, ask).ok())?
            else {
                return Ok(out);
            };
            part(memo, PlanPart::Name, &mut out, || self.plan_name_in(db, &instance, &origin).ok())?;
            let Some(described) = part(memo, PlanPart::Describe, &mut out, || {
                self.described(db, &instance, &origin, None).ok()
            })?
            else {
                return Ok(out);
            };
            if let Some(closure) =
                part(memo, PlanPart::Closure, &mut out, || closure_of(db, &instance))?
            {
                part(memo, PlanPart::Encode, &mut out, || {
                    encode_described(db, &closure, Some(&described.origin), &described)
                })?;
            }
            part(memo, PlanPart::Owner, &mut out, || plan_body(db, &instance))?;
            for (at, reference) in (0u32..).zip(&described.refs) {
                part(memo, PlanPart::Edge(at), &mut out, || {
                    let mut sink = Vec::new_in(Global);
                    self.put_edge(db, &described.origin, reference, &mut sink).ok()
                })?;
            }
            Ok(out)
        })
    }

    /// One edge of a plan, for `reference` of a describe compiled for `origin`.
    fn put_edge<A: Allocator>(
        &self,
        db: &Db,
        origin: &Origin,
        reference: &FrontRef<HirInstance, HirStatic>,
        out: &mut Vec<u8, A>,
    ) -> Result<(), &'static str> {
        match reference {
            FrontRef::Function(callee) => {
                // The walk asks a callee by the origin its caller is compiled for.
                let callee_origin = self.compiled_for(db, callee, origin);
                let (name, ask) = self.plan_name_in(db, callee, &callee_origin)?;
                match plan_body(db, callee) {
                    Some(body) => {
                        out.push(EDGE_FUNCTION_BODY);
                        out.extend_from_slice(&name.0);
                        out.extend_from_slice(&body.0);
                    }
                    None => {
                        out.push(EDGE_FUNCTION);
                        out.extend_from_slice(&name.0);
                    }
                }
                put_bytes_in(out, &ask)?;
            }
            FrontRef::Static(value) => {
                // The static's plan name and its item key, from the one place both are made.
                let (name, ask) = self.static_plan_name_in(db, value)?;
                out.push(EDGE_STATIC);
                out.extend_from_slice(&name.0);
                out.extend_from_slice(&ask[STATIC_ASK_MAGIC.len()..]);
                // A static is no generic: one instance of its item ([`Self::described_static`]).
                out.extend_from_slice(&ContentId::default().0);
                let weak = match crate::exported_static(db, value.0) {
                    Some(symbol) => match crate::exported_linkage(db, value.0.into())
                        .map_err(|_| NO_STATIC_LINKAGE)?
                    {
                        blaze_codegen::Linkage::Weak => Some(symbol),
                        blaze_codegen::Linkage::Strong => None,
                    },
                    None => None,
                };
                match weak {
                    Some(symbol) => {
                        out.push(1);
                        put_bytes_in(out, symbol.as_str().as_bytes())?;
                    }
                    None => out.push(0),
                }
            }
            FrontRef::Extern(name) => {
                out.push(EDGE_EXTERN);
                put_bytes_in(out, name.as_bytes())?;
            }
            FrontRef::Weak(name) => {
                out.push(EDGE_WEAK);
                put_bytes_in(out, name.as_bytes())?;
            }
        }
        Ok(())
    }

    /// The crate the instance of `ask` is worked out in, made in this database: its home (the
    /// crate whose environment its code is monomorphized in, the closure's root, [`Closure`]),
    /// the C `main`'s `main`'s crate, an allocator shim entry's program crate; `None` for an
    /// `asm!` function, which reads nothing of the database. Whose view the facts that depend on the asker are of when a plan task
    /// classifies its reads (`blaze_item_hash::reads::fact_of_in`): drop glue and shims have
    /// no body, and a plan covers none, so the asker is a crate and never a body.
    pub fn plan_home<A: Allocator + Clone>(
        &self,
        ask: &[u8],
        alloc: A,
    ) -> Result<Option<base_db::Crate>, PlanFailed<A>> {
        self.with(|db| {
            let (instance, _) = ask_read(db, ask).map_err(|why| unread(&why, alloc))?;
            Ok(match &instance.0 {
                Kind::Code(_, env) => Some(env.krate),
                Kind::Start(main) => Some(main.krate(db)),
                Kind::Allocator(forward) => Some(forward.krate),
                Kind::Asm(..) => None,
            })
        })
    }

    /// The instance, its origin and its describe of the plan `view`, made in this database:
    /// what a code task compiles. Its describe's origin is the record's own (a plan always
    /// writes it).
    pub fn planned<A: Allocator + Clone>(
        &self,
        view: &PlanView<'_>,
        alloc: A,
    ) -> Result<(HirInstance, Origin, Described<HirInstance, HirStatic>), PlanFailed<A>> {
        self.with(|db| {
            let (instance, origin) = ask_read(db, view.ask).map_err(|why| unread(&why, alloc.clone()))?;
            let described = decode_described(db, &origin, view.describe, |_, _| {})
                .map_err(|why| unread(&why, alloc.clone()))?;
            if described.origin != origin {
                return Err(PlanFailed::Refused(ORIGIN_MOVED));
            }
            Ok((instance, origin, described))
        })
    }

    /// What a static plan task is asked for `value`: its name and its ask (this module's doc,
    /// "A static's plan"). Read only of the database and this front end's stage.
    pub fn static_plan_ask<A: Allocator + Clone>(
        &self,
        value: &HirStatic,
        alloc: A,
    ) -> Result<PlanAsk<A>, PlanFailed<A>> {
        self.with(|db| {
            let (name, ask) = self.static_plan_name_in(db, value)?;
            let mut out = Vec::with_capacity_in(ask.len(), alloc);
            out.extend_from_slice(&ask);
            Ok(PlanAsk { name, ask: out, body: None })
        })
    }

    /// The name of the plan of the static `value`, with its ask.
    fn static_plan_name_in(
        &self,
        db: &Db,
        value: &HirStatic,
    ) -> Result<(ContentId, [u8; STATIC_ASK_LEN]), &'static str> {
        let (producer, stage) = self.record_stage().ok_or(NO_STAGE)?;
        let item = self.hashes.item(db, ItemId::Static(value.0)).map_err(|_| NO_STATIC_KEY)?;
        let ask = static_ask(&item);
        let mut hasher = Hasher::new(STATIC_NAME_DOMAIN);
        hasher.id(&producer).id(&stage).part(&ask);
        Ok((hasher.finish(), ask))
    }

    /// The plan of the static `ask` names ([`Self::static_plan_ask`]), whose name the driver
    /// gave as `name`: the static found by its item key in this database, described and
    /// written as this module's doc says ("A static's plan"). Its reads are the caller's to
    /// record, as [`Self::plan`]'s.
    pub fn static_plan<A: Allocator + Clone>(
        &self,
        ask: &[u8],
        name: &ContentId,
        alloc: A,
    ) -> Result<Planned<A>, PlanFailed<A>> {
        self.with(|db| {
            let value = static_ask_read(db, ask)?;
            let (again, written) = self.static_plan_name_in(db, &value)?;
            if again != *name || written != ask {
                return Err(PlanFailed::Refused(NAME_MOVED));
            }
            let described = self.described_static(db, &value).map_err(PlanFailed::Describe)?;
            let text = |it: Option<&str>| 1 + it.map_or(0, |it| 4 + it.len());
            let mut out = Vec::with_capacity_in(
                STATIC_PLAN_MAGIC.len()
                    + 4
                    + ask.len()
                    + 3 * 32
                    + 5
                    + origin_tag(&described.origin).1.len()
                    + 1
                    + text(described.section.as_deref())
                    + text(described.symbol.as_deref())
                    + 4
                    + described.name.len()
                    + 4
                    + 40 * described.refs.len(),
                alloc,
            );
            out.extend_from_slice(STATIC_PLAN_MAGIC);
            put_bytes_in(&mut out, ask)?;
            put_static_head(&mut out, &described)?;
            let count = u32::try_from(described.refs.len()).map_err(|_| EDGES_DIFFER)?;
            put_u32_in(&mut out, count);
            for reference in &described.refs {
                self.put_edge(db, &described.origin, reference, &mut out)?;
            }
            Ok(Planned { name: *name, bytes: out })
        })
    }

    /// The instance or static an ask of either layout names, as a person reads it: a
    /// function's path with its type arguments (its backtrace name, as `blaze run` names a
    /// function), a static's path. For a failed plan or code task's message (rule 19), never
    /// for a key; `None` where the ask does not read in this database.
    pub fn printed_ask(&self, ask: &[u8]) -> Option<String> {
        self.with(|db| match ask.get(..STATIC_ASK_MAGIC.len()) {
            Some(magic) if magic == STATIC_ASK_MAGIC => {
                static_ask_read(db, ask).ok().map(|it| crate::fncache::static_name(db, it.0))
            }
            _ => ask_read(db, ask).ok().map(|(instance, _)| super::super::super::linked::debug_name(db, &instance.0)),
        })
    }

    /// The crate the static of `ask` is defined in, made in this database: whose view the
    /// facts that depend on the asker are of when its plan task classifies its reads, as
    /// [`Self::plan_home`]. A static plan covers no body either: its key holds no text.
    pub fn static_plan_home(&self, ask: &[u8]) -> Result<base_db::Crate, &'static str> {
        self.with(|db| Ok(static_ask_read(db, ask)?.0.krate(db)))
    }

    /// The static of the plan `view`, found in this database: what a static code task
    /// compiles (`Backend::compile_static`, which reads its value again from the database).
    pub fn static_planned(&self, view: &StaticPlanView<'_>) -> Result<HirStatic, &'static str> {
        self.with(|db| static_ask_read(db, view.ask))
    }
}

/// The first bytes of a static plan record.
pub const STATIC_PLAN_MAGIC: &[u8; 8] = b"bzsplan1";

/// The first bytes of a static plan's ask, which is these and the static's item key.
pub const STATIC_ASK_MAGIC: &[u8; 8] = b"bzsask01";
pub const STATIC_ASK_LEN: usize = 8 + 32;

/// The ask of the plan of the static whose item key is `item`: [`STATIC_ASK_MAGIC`] and the
/// key. The one place a static's ask is made: its plan's name ([`HirFrontend::static_plan_ask`])
/// and a driver with no database reading a static edge (`Edge::Static`) both make it here.
pub fn static_ask(item: &ItemKey) -> [u8; STATIC_ASK_LEN] {
    let mut ask = [0u8; STATIC_ASK_LEN];
    ask[..STATIC_ASK_MAGIC.len()].copy_from_slice(STATIC_ASK_MAGIC);
    ask[STATIC_ASK_MAGIC.len()..].copy_from_slice(&item.0.0);
    ask
}

/// The domain of a static plan's name.
const STATIC_NAME_DOMAIN: &str = "blaze v2 static plan name 2";

/// What a static plan is refused as, by the rule.
pub const NOT_A_STATIC_ASK: &str =
    "a static plan: its ask is not `bzsask01` and an item key (40 bytes)";
pub const NO_STATIC_ITEM: &str =
    "a static plan: its ask's item key names no static in this database";
pub const NOT_A_STATIC_PLAN: &str = "a static plan record: its first bytes are not `bzsplan1`";
pub const BAD_STATIC_FLAGS: &str = "a static plan record: flags past the four it has";
pub const BAD_STATIC_ORIGIN: &str = "a static plan record: an origin of no kind";

/// The static a static plan's ask names, in `db`.
fn static_ask_read(db: &Db, ask: &[u8]) -> Result<HirStatic, &'static str> {
    use blaze_items::key::Resolved;
    let ask: &[u8; STATIC_ASK_LEN] = ask.try_into().map_err(|_| NOT_A_STATIC_ASK)?;
    if &ask[..8] != STATIC_ASK_MAGIC {
        return Err(NOT_A_STATIC_ASK);
    }
    let mut item = [0u8; 32];
    item.copy_from_slice(&ask[8..]);
    match blaze_items::key::resolve(db, ItemKey(ContentId(item))) {
        Some(Resolved::Item(ItemId::Static(it))) => Ok(HirStatic(it)),
        _ => Err(NO_STATIC_ITEM),
    }
}

fn put_text_in<A: Allocator>(out: &mut Vec<u8, A>, text: Option<&str>) -> Result<(), &'static str> {
    match text {
        Some(text) => {
            out.push(1);
            put_bytes_in(out, text.as_bytes())
        }
        None => {
            out.push(0);
            Ok(())
        }
    }
}

/// A static plan's head: its describe but its references.
fn put_static_head<A: Allocator>(
    out: &mut Vec<u8, A>,
    described: &DescribedStatic<HirInstance, HirStatic>,
) -> Result<(), &'static str> {
    out.extend_from_slice(&described.item.0.0);
    out.extend_from_slice(&described.instance.0);
    out.extend_from_slice(&described.value.0);
    let (tag, package) = origin_tag(&described.origin);
    out.push(tag);
    put_bytes_in(out, package.as_bytes())?;
    out.push(
        u8::from(described.mutable)
            | u8::from(described.thread_local) << 1
            | u8::from(matches!(described.linkage, blaze_codegen::Linkage::Weak)) << 2
            | u8::from(matches!(described.owner, Owner::Workspace)) << 3,
    );
    put_text_in(out, described.section.as_deref())?;
    put_text_in(out, described.symbol.as_deref())?;
    put_bytes_in(out, described.name.as_bytes())
}

/// What a static plan's describe says, read in place with no database: every field of
/// [`DescribedStatic`] but its references, which are the plan's edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StaticHead<'r> {
    pub item: ItemKey,
    pub instance: ContentId,
    pub value: ContentId,
    pub origin: Origin,
    pub mutable: bool,
    pub thread_local: bool,
    pub linkage: blaze_codegen::Linkage,
    pub owner: Owner,
    pub section: Option<&'r str>,
    pub symbol: Option<&'r str>,
    pub name: &'r str,
}

impl StaticHead<'_> {
    /// What its template's key holds of it besides its references
    /// (`blaze_codegen::driver::static_shape_key`).
    pub fn shape(&self) -> blaze_codegen::driver::StaticShape<'_> {
        blaze_codegen::driver::StaticShape {
            value: self.value,
            mutable: self.mutable,
            thread_local: self.thread_local,
            section: self.section,
        }
    }
}

/// A static plan record read in place, with no database.
#[derive(Clone, Copy, Debug)]
pub struct StaticPlanView<'r> {
    /// What its task was asked: `bzsask01` and the static's item key.
    pub ask: &'r [u8],
    pub head: StaticHead<'r>,
    edges: &'r [u8],
    count: usize,
}

impl<'r> StaticPlanView<'r> {
    /// The static plan record `bytes`, read in place.
    pub fn read(bytes: &'r [u8]) -> Result<StaticPlanView<'r>, &'static str> {
        let mut reader = Reader::new(bytes);
        if reader.array::<8>().map_err(cursor)? != STATIC_PLAN_MAGIC {
            return Err(NOT_A_STATIC_PLAN);
        }
        let ask = reader.bytes_u32().map_err(cursor)?;
        let bytes = &mut reader;
        let item = ItemKey(id_of(bytes)?);
        let instance = id_of(bytes)?;
        let value = id_of(bytes)?;
        let origin = match bytes.u8().map_err(cursor)? {
            0 => return Err(BAD_STATIC_ORIGIN),
            tag => origin_tagged(bytes, tag).map_err(|_| BAD_STATIC_ORIGIN)?,
        };
        let flags = bytes.u8().map_err(cursor)?;
        if flags >= 1 << 4 {
            return Err(BAD_STATIC_FLAGS);
        }
        let text = |bytes: &mut Reader<'r>| -> Result<Option<&'r str>, &'static str> {
            Ok(match flag_of(bytes.u8().map_err(cursor)?)? {
                true => Some(bytes.str_u32().map_err(cursor)?),
                false => None,
            })
        };
        let section = text(bytes)?;
        let symbol = text(bytes)?;
        let name = bytes.str_u32().map_err(cursor)?;
        let count = bytes.u32().map_err(cursor)? as usize;
        let head = StaticHead {
            item,
            instance,
            value,
            origin,
            mutable: flags & 1 != 0,
            thread_local: flags & 2 != 0,
            linkage: match flags & 4 != 0 {
                true => blaze_codegen::Linkage::Weak,
                false => blaze_codegen::Linkage::Strong,
            },
            owner: if flags & 8 != 0 { Owner::Workspace } else { Owner::Dependency },
            section,
            symbol,
            name,
        };
        let edges = reader.take_rest();
        Ok(StaticPlanView { ask, head, edges, count })
    }

    /// Its edges, in the order of its value's references.
    pub fn edges(&self) -> Edges<'r> {
        Edges { bytes: Reader::new(self.edges), left: self.count }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests;
