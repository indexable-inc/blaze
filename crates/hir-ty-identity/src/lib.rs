//! Code identity (docs/v2/plan/design/code-identity.md): the one definition in blaze of what a
//! piece of code is, as a hash that is the same in every process, on every machine and in every
//! checkout of the same source. Two notions, each a Merkle graph: an instance is hashed once,
//! by its own query, and shared by every root that reaches it. Behaviour identity hashes a
//! recursion as one strongly connected component ([`scc`]); compile identity has none for an
//! instance on a cycle other than its own body's ([`hir_ty_share::Unshared::Recursive`]).
//!
//! - **Compile identity**, [`instance_identity`] ([`instance`]): what one instance's generated
//!   code depends on beyond its unit's settings, by rule 21, read so that instances which compile
//!   to one body have one identity: this is the identity the code generator's ICF folds by and
//!   puts in place of an instance's arguments in its instance key. In: the body's item key and
//!   code hash ([`blaze_items`]: each item it refers to by interface, never a position), the
//!   code of every body MIR inlining built into it ([`hir_ty_share::code`]), and, where its
//!   generic body names a parameter, what the instance has there: a type by its fact
//!   ([`hir_ty_share::Cx::fact`]: its layout and shape, so `Vec<&A>` and `Vec<&B>` are one), a
//!   constant by its value, a callee by its own compile identity, a drop by its glue's type,
//!   `TypeId`, `type_name`, vtables and `dyn` calls by full identity
//!   ([`hir_ty_share::identity`]), each under every MIR options of [`Compiled`]. Out: the
//!   unit's settings (overflow checks, target, backend), the instances it builds in (both in
//!   the unit's template key), the body of a callee it only calls (a plain call is by symbol),
//!   the checkout's path (no key reads a path: crates by content, items by AST id within their
//!   file, files by nothing), comments and formatting, and every interned id of the database.
//!   An instance with no compile identity says why ([`hir_ty_share::Unshared`]); the code
//!   generator compiles one of a cycle, a shim or a per-instance intrinsic for its arguments
//!   alone, and stops at any other reason.
//! - **Behaviour identity**, [`code_id`]: what running an instance can depend on: its semantic
//!   part (its code, arguments, overflow setting and the facts of its monomorphized runtime
//!   body, each static by value) and the behaviour identity of every instance it can run,
//!   closed-world over [`hir_ty_mir::mir::dispatch`]. It stops at
//!   `#[blaze::pure(trusted = "..")]` functions and the methods of a trusted trait (the kernel's
//!   `ctx` door), which are leaves by key, interface and their own code: a change to what a step
//!   runs changes its CodeId, and a change to the kernel, the daemon, a lint or the CLI does not.
//!   The MIR options are not in it.
//!
//! A function whose constants read the identity of code that runs it depends on itself: a typed
//! error ([`SELF_READ`]), never a fixpoint. Every hash streams into a [`blaze_ids::Hasher`] in an
//! order fixed by the source and the graph: MIR order, field order, site order, a component's
//! canonical order. No hash map's order, address or interned id reaches one (rule 25).

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod facts;
#[doc(hidden)]
pub mod hashed;
pub mod instance;
mod scc;
mod walk;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::fmt;

use base_db::Db;
use blaze_ids::ContentId;
use blaze_items::{ItemId, key};
use blaze_query::db::{AsId, FromId, Id, NamesIds};
use hir_def::{DefWithBodyId, FunctionId};
use hir_ty::{
    InferBodyId,
    db::{InternedClosureId, InternedCoroutineId},
    mir::{MirOptLevel, MirOptions},
    next_solver::StoredGenericArgs,
};
use hir_ty_mir::mir::dispatch::Body;

pub use instance::instance_identity;
pub use walk::{code_id, fn_code_id};

/// The domain of a compile identity. 3: an inlined callee's promoted constants are read, so
/// no identity made when two instances that differ only there shared one is reused. 4: a
/// callee of the same body, a callee whose constant is computed from parameters, and a callee
/// that is recursive or nests too deep are named by their arguments instead of walked into. 5:
/// a callee of the same body is named by its arguments only where they nest as deep or deeper
/// (shallower ones are asked), and any callee whose arguments hold a constant neither the
/// instance's arguments nor the body's literals hold is; a zero-sized promoted constant is
/// recorded as none, and one whose MIR does not build is refused only where it is not
/// zero-sized. 6: a callee of any body whose arguments nest deeper than the instance's, or as
/// deep and made of anything but the instance's arguments' parts and the body's literals (a
/// projection's normalized value, a closure of the caller), is named by its arguments instead
/// of asked (`Walk::asks`). That bounded a chain of asks only by |parts|^arity keys: digits held
/// in separate parameters beside a deep argument holding every digit counted 2^k steps. 7: a
/// callee as deep as the instance is asked only when it is another body and its type and
/// constant arguments are an order-preserving sub-list of the instance's own, and the check for
/// a new constant is gone (a sub-list holds none, and a shallower callee is bounded by the
/// depth), so a chain of asks is at most (`SHARED_DEPTH` + 1) × arity × bodies long; and every
/// key, a root's and a callee's, has its free regions erased and its binders anonymized before
/// it is read (`instance::erased`, from `instance_identity` and `Walk::instance_callee`), so
/// calls equal but for their lifetimes, or for how a `for<'a>` names its lifetime, are one key
/// and one `CALLEE_SELF` (one bump with the rest of 7: nothing was persisted under 7 before
/// it). Which callees are named by their arguments also turns on `SHARED_DEPTH`
/// (`instance.rs`): changing it changes identities, so it bumps this too.
pub const COMPILE_DOMAIN: &str = "blaze v2 compile identity 7";
/// The domain of the records of one walk of a body.
pub const WALK_DOMAIN: &str = "blaze v2 compile identity walk 1";
/// The domain of what a call, or an argument list, is keyed by.
pub const CALLEE_DOMAIN: &str = "blaze v2 compile identity callee 1";
/// The domain of an evaluated constant's bytes. 2: each vtable in its table is hashed with the
/// `dyn` type const eval's id names for it (`MemoryMap::vtable_of`), not its concrete type alone,
/// as the code generator emits the vtable of that type for that `dyn`. 3: the map's content is
/// hashed by `ComplexMemoryMap::hash_content` (the one list of its content fields, which
/// `Facts::allocation` shares), each number at a fixed width, where it was the map's own `Hash`
/// with its types taken out.
pub const VALUE_DOMAIN: &str = "blaze v2 compile identity value 3";
/// The domain of an instance's own part of its behaviour identity.
pub const BEHAVIOUR_DOMAIN: &str = "blaze v2 behaviour identity 1";
/// The domain of a leaf of the behaviour walk.
pub const LEAF_DOMAIN: &str = "blaze v2 behaviour identity leaf 1";
/// The domain of a [`CodeId`]: an instance's component of what runs what. 3: a body's types are
/// normalized all through, their free regions erased, error regions included, and their binders
/// anonymized (`Facts::ty`, `instance::erased`); and each vtable a constant holds is hashed with
/// the `dyn` type at the place that holds it (`facts.rs`, `VtableWalk`), as the code generator
/// pairs them when it relocates the constant (a pointer's `dyn` is its pointee's struct tail,
/// so `&Wrap<dyn Handler>` pairs its vtable with `dyn Handler`), where a vtable used to be hashed with every `dyn`
/// type found anywhere in the constant's type, so a vtable of `Display` behind a `&dyn Debug`
/// was hashed as one of `Debug`. A function item's or closure's id is its pointer and its
/// vtable at once (`&index as &dyn Handler`), so it is hashed as what its pointer runs and then
/// for each `dyn` found for it, which keeps a blanket impl's methods over `Fn` in the id. The
/// `dyn` types are normalized all through, erased as above and deduped. An id no word of the
/// value holds (a temporary of the evaluation) is skipped, as nothing can emit it; a raw
/// pointer is followed only where its memory holds all it points at. A vtable a word of the
/// value holds at no place whose type names its trait (in a union, a closure's or coroutine's
/// capture, an unsafe binder, a `DynMetadata` of no `dyn`) is refused, naming the constant or
/// static: its trait, and so the code its slots run, is not known. A refusal is no code id,
/// never a different one. Nothing was persisted under 3 before these, so they are one bump. 4:
/// const eval's id of a vtable names its `dyn` type (rustc's `GlobalAlloc::VTable`), and each
/// vtable a pointer of the value holds is hashed with that `dyn` (`Facts::allocation`), as the
/// code generator relocates it, where it used to be paired with the `dyn` at the place a type
/// reaching it named, and refused where none did (`as_ptr`, a union, a slice tail, a pointer
/// cast to another type); a vtable id no pointer holds is skipped, a function's id is hashed as
/// what its pointer runs only; and a word holding an id without provenance that no type reaching
/// it says is a pointer is refused, as the code generator refuses it (`VtableWalk`, which
/// follows its relocation rules). 5: a static is named by its key every time a body meets it,
/// where a static met before hashed nothing, so a constant pointing at one of two statics the
/// body named before kept its code id when it came to point at the other; and a value's memory
/// map is hashed by `ComplexMemoryMap::hash_content`, which adds each piece's alignment and
/// which words of the value and its pieces have provenance, both of which the code generator
/// reads (a data object's alignment; which words it relocates).
pub const CODE_ID_DOMAIN: &str = "blaze v2 code id 5";
/// The domain of the bodies inlined into another.
pub const INLINED_DOMAIN: &str = "blaze v2 code identity inlined 2";
/// The domain of the stable name that orders the members of one component.
pub const NAME_DOMAIN: &str = "blaze v2 code identity name 1";

/// Why code that reads its own identity has none: the query cycle's every member says it.
pub const SELF_READ: &str = "its code reads its own code identity: a constant in it asks for the identity of code that runs it";

/// What an identity is of: a function, a closure, an `async` block (as its `poll`, or by move),
/// as [`dispatch`](hir_ty_mir::mir::dispatch) names them, or a shim the code generator builds
/// without MIR: what its `InstanceDef` is, one for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subject {
    Body(Body),
    Shim(Shim),
}

/// A shim the code generator builds from its type argument, with no MIR of its own: it has no
/// compile identity and is compiled per instance ([`hir_ty_share::Unshared::Shim`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shim {
    /// `drop_in_place` of the instance's only argument.
    DropGlue,
    CallOnce,
    Vtable(FunctionId),
    FnPointer,
    Reify {
        receiver: bool,
    },
}

impl Shim {
    /// What it is, as [`hir_ty_share::Unshared::Shim`] says it.
    pub const fn what(self) -> &'static str {
        match self {
            Shim::DropGlue => "drop glue",
            Shim::CallOnce => "`FnOnce::call_once` shim",
            Shim::Vtable(_) => "vtable shim",
            Shim::FnPointer => "function pointer shim",
            Shim::Reify { .. } => "reify shim",
        }
    }
}

/// The body it is of: whose a query keyed by it is. A shim runs none of its own but the
/// function a vtable shim adapts.
impl NamesIds for Subject {
    fn names_ids(&self, out: &mut Vec<Id>) {
        match *self {
            Subject::Body(Body::Fn(it)) | Subject::Shim(Shim::Vtable(it)) => out.push(it.as_id()),
            Subject::Body(Body::Closure(it) | Body::Coroutine(it) | Body::CoroutineByMove(it)) => {
                out.push(it)
            }
            Subject::Shim(
                Shim::DropGlue | Shim::CallOnce | Shim::FnPointer | Shim::Reify { .. },
            ) => {}
        }
    }
}

/// A subject with its generic arguments, none of them a parameter: a closure's or coroutine's
/// are its parent's, as dispatch and the code generator have them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Instance {
    pub subject: Subject,
    pub args: StoredGenericArgs,
}

/// Its subject and what its arguments name.
impl NamesIds for Instance {
    fn names_ids(&self, out: &mut Vec<Id>) {
        self.subject.names_ids(out);
        self.args.names_ids(out);
    }
}

/// A set of [`MirOptions`], one bit for each of the forty-eight (three levels, UB checks or
/// not, lifetime markers or not, inlined or not, allocations moved to the stack or not).
/// Options with a program's dyn facts ([`MirOptions::world`]) are in no set: [`OptionsSet::of`]
/// would fold them into theirs without, which the code generator's `options_set` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OptionsSet(u64);

const LEVELS: [MirOptLevel; 3] = [MirOptLevel::Off, MirOptLevel::Debug, MirOptLevel::Release];

impl OptionsSet {
    pub const EMPTY: OptionsSet = OptionsSet(0);

    pub const fn of(options: &[MirOptions]) -> OptionsSet {
        let mut bits = 0;
        let mut at = 0;
        while at < options.len() {
            bits |= 1 << OptionsSet::index(options[at]);
            at += 1;
        }
        OptionsSet(bits)
    }

    pub(crate) const fn index(options: MirOptions) -> u32 {
        let level = match options.level {
            MirOptLevel::Off => 0,
            MirOptLevel::Debug => 1,
            MirOptLevel::Release => 2,
        };
        level * 16
            + (options.ub_checks as u32) * 8
            + (options.lifetime_markers as u32) * 4
            + (options.inline as u32) * 2
            + options.heap_to_stack as u32
    }

    pub const fn contains(self, options: MirOptions) -> bool {
        self.0 & (1 << OptionsSet::index(options)) != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The options, in [`MirOptions`]' order.
    pub fn iter(self) -> impl Iterator<Item = MirOptions> {
        LEVELS
            .into_iter()
            .flat_map(|level| {
                [false, true].into_iter().flat_map(move |ub_checks| {
                    [false, true].into_iter().flat_map(move |lifetime_markers| {
                        [false, true].into_iter().flat_map(move |inline| {
                            [false, true].map(|heap_to_stack| MirOptions {
                                level,
                                ub_checks,
                                lifetime_markers,
                                inline,
                                heap_to_stack,
                                world: None,
                            })
                        })
                    })
                })
            })
            .filter(move |&options| self.contains(options))
    }
}

/// The MIR options the code generator may compile a body with (`hir-ty-codegen`'s
/// `Build::options` over the units of the build): `lang` for a body of the library's crates,
/// `other` for the rest. An instance's `Described` is one for every unit of the build, so its
/// compile identity is over the body with each options here: two instances with one identity
/// are one under each of them. An options says whether the body is inlined, so the identity
/// reads the inlined body where a unit compiles one, and never keys an inlined and an
/// uninlined body as one. A build whose units all compile alike gives one options each, and
/// the identity costs one walk. The rest of a unit's settings (overflow checks, the target,
/// the backend) are in its `Settings`, beside the identity in its instance key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Compiled {
    pub lang: OptionsSet,
    pub other: OptionsSet,
}

impl Compiled {
    /// Every body compiled with `options`, whichever crate it is of.
    pub const fn every(options: MirOptions) -> Compiled {
        let set = OptionsSet::of(&[options]);
        Compiled { lang: set, other: set }
    }
}

/// Compile identity: equal identities of two instances of one body mean the code generator
/// is given the same MIR up to the facts it reads, so its output for one serves the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CompileIdentity(pub ContentId);

/// Behaviour identity, of a compute step: its root's semantic part and the behaviour identity
/// of every instance it can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CodeId(pub ContentId);

/// Why an instance or a compute step has no identity: the instance, by path, what of it could
/// not be identified, and for a [`CodeId`] the instances from the step's root to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Unidentified {
    pub instance: Box<str>,
    pub what: Box<str>,
    pub through: Box<[Box<str>]>,
}

impl Unidentified {
    fn of(db: &Db, instance: &Instance, what: String) -> Unidentified {
        Unidentified::named(name_of(db, instance.subject), what)
    }

    fn named(instance: String, what: String) -> Unidentified {
        Unidentified {
            instance: instance.into_boxed_str(),
            what: what.into_boxed_str(),
            through: Box::default(),
        }
    }

    /// The error of an instance `instance` reaches: `instance` first on its path.
    fn through(mut self, instance: String) -> Unidentified {
        let mut through = Vec::with_capacity(self.through.len() + 1); // alloc: error path
        through.push(instance.into_boxed_str());
        through.extend(self.through.into_vec());
        self.through = through.into_boxed_slice();
        self
    }
}

impl fmt::Display for Unidentified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.through {
            write!(f, "{step} → ")?;
        }
        write!(f, "{} has no code identity: {}", self.instance, self.what)
    }
}

impl std::error::Error for Unidentified {}

/// A subject as an error names it: an item by its path, a closure or block by its owner's.
pub(crate) fn name_of(db: &Db, subject: Subject) -> String {
    let owner = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(def) => item_path(db, def),
        InferBodyId::AnonConstId(_) => "an anonymous constant".to_owned(), // alloc: an error's name
    };
    match subject {
        Subject::Body(Body::Fn(it)) => item_path(db, it.into()),
        Subject::Body(Body::Closure(it)) => {
            format!("a closure in {}", owner(InternedClosureId::from_id(it).loc(db).owner)) // alloc: an error's name
        }
        Subject::Body(Body::Coroutine(it) | Body::CoroutineByMove(it)) => {
            format!("an `async` block in {}", owner(InternedCoroutineId::from_id(it).loc(db).owner)) // alloc: an error's name
        }
        Subject::Shim(shim) => shim.what().to_owned(), // alloc: an error's name
    }
}

fn item_path(db: &Db, def: DefWithBodyId) -> String {
    format!("`{}`", key::path(db, ItemId::of_body(db, def))) // alloc: an error's name
}
