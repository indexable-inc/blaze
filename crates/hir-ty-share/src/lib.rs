//! Shared code across generic instances (idea 2 of `docs/v2/plan/design/compile-time.md`,
//! `docs/v2/plan/lanes/fastcompile/share.md`): two instances of one generic body compile to one
//! body when nothing their code does tells their arguments apart beyond the layouts of those
//! arguments. The decision is made here, before codegen, as a key: [`share_key`].
//!
//! - The key of an instance is a BLAKE3 hash of what its monomorphized MIR depends on, read
//!   at each place where the generic body names a generic parameter: the fact of each type
//!   there ([`fact`]: its layout, its fields' facts, a pointee's size and alignment), each
//!   constant's value, each callee by its own key ([`walk`]), each drop by its glue's key
//!   ([`drop`]), each intrinsic by what rustc's definition of it reads ([`intrinsics`]), and
//!   the full identity of everything else that tells types apart ([`identity`], the code
//!   generator's spelling of a type too): `TypeId`, `type_name`, vtables, `dyn` calls, closures,
//!   coroutines by their compiled code ([`code`]). A place of the generic body that names no
//!   parameter is the same in every instance and is not read.
//! - Equal keys mean equal code: the code generator puts the key in place of the arguments in
//!   the instance's code identity, so instances of one body with equal keys are one instance.
//!   An instance with no key ([`Unshared`], with why) keeps its arguments' hash, exactly as
//!   before: that is the per-instance path, not a guess.
//! - Nothing here reads a dense id into a hash: items by their [`blaze_items`] keys, types by
//!   content.
//!
//! Every query is the query database's, memoized by (instance, environment, options): a hit is one lookup.
//! A cycle of instances (polymorphic recursion, a recursive type's drop glue) gives every
//! instance in it [`Unshared::Recursive`], the same whichever is asked first. A chain whose
//! arguments grow with each call has no cycle and no end: past the `#![recursion_limit]` of
//! the crate it started in, it stops the build ([`Unshared::RecursionLimit`]).

#![forbid(unsafe_code)]

pub mod code;
mod drop;
mod fact;
pub mod identity;
mod intrinsics;
mod layout;
mod walk;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::fmt;

use base_db::Db;
use blaze_ids::{ContentId, Fingerprint};
use hir_def::FunctionId;
use hir_ty::{
    db::HirDatabase,
    mir::{MirOptLevel, MirOptions},
    next_solver::StoredGenericArgs,
    traits::StoredParamEnvAndCrate,
};

pub use fact::is_plain_data;
pub use intrinsics::{Class, classify};

/// The domain of a share key.
pub const SHARE_DOMAIN: &str = "blaze v2 share key 2";
/// The domain of a type's fact.
pub const FACT_DOMAIN: &str = "blaze v2 share fact 1";
/// The domain of the records of one walk of a body.
pub const WALK_DOMAIN: &str = "blaze v2 share walk 1";
/// The domain of what a call, or an argument list, is keyed by.
pub const CALLEE_DOMAIN: &str = "blaze v2 share callee 1";
/// The domain of an evaluated constant's bytes.
pub const VALUE_DOMAIN: &str = "blaze v2 share value 1";

/// What an instance runs: the code generator's `InstanceDef`, one for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareDef {
    Fn(FunctionId),
    /// An `InternedClosureId`, without its database lifetime.
    Closure(blaze_query::db::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block.
    Coroutine(blaze_query::db::Id),
    /// The same of the future an `async` closure gives when called by value.
    CoroutineByMove(blaze_query::db::Id),
    /// `drop_in_place` of the instance's only argument.
    DropGlue,
    CallOnceShim,
    VtableShim(FunctionId),
    FnPointerShim,
    ReifyShim { receiver: bool },
}

/// The body it runs: whose a query keyed by it is (`blaze_query::db::Storage::ids_of`, read by
/// `crates/hir-firewall`). A shim runs none of its own.
impl blaze_query::db::NamesIds for ShareDef {
    fn names_ids(&self, out: &mut Vec<blaze_query::db::Id>) {
        use blaze_query::db::AsId;
        match *self {
            ShareDef::Fn(it) | ShareDef::VtableShim(it) => out.push(it.as_id()),
            ShareDef::Closure(it) | ShareDef::Coroutine(it) | ShareDef::CoroutineByMove(it) => {
                out.push(it)
            }
            ShareDef::DropGlue
            | ShareDef::CallOnceShim
            | ShareDef::FnPointerShim
            | ShareDef::ReifyShim { .. } => {}
        }
    }
}

/// An instance: what it runs and its generic arguments (a closure's or coroutine's are its
/// parent's, as the code generator's are).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareInstance {
    pub def: ShareDef,
    pub args: StoredGenericArgs,
}

/// What it runs and the closures, coroutines and opaque types its arguments name.
impl blaze_query::db::NamesIds for ShareInstance {
    fn names_ids(&self, out: &mut Vec<blaze_query::db::Id>) {
        use blaze_query::db::NamesIds;
        self.def.names_ids(out);
        self.args.names_ids(out);
    }
}

/// A set of [`MirOptions`], one bit for each of the twenty-four (three levels, UB checks or
/// not, lifetime markers or not, inlined or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OptionsSet(u32);

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
        level * 8
            + (options.ub_checks as u32) * 4
            + (options.lifetime_markers as u32) * 2
            + options.inline as u32
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
                        [false, true].map(|inline| MirOptions {
                            level,
                            ub_checks,
                            lifetime_markers,
                            inline,
                        })
                    })
                })
            })
            .filter(move |&options| self.contains(options))
    }
}

/// The options the code generator may compile a body with (`hir-ty-codegen`'s
/// `Build::options` over the units of the build): `lang` for a body of the library's crates,
/// `other` for the rest. An instance's `Described` is one for every unit of the build, so the
/// key is over the body with each options here: two instances with one key are one under
/// each of them. An options says whether the body is inlined, so the key reads the inlined
/// body where a unit compiles one, and never keys an inlined and an uninlined body as one. A
/// build whose units all compile alike gives one options each, and the key costs one walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyOptions {
    pub lang: OptionsSet,
    pub other: OptionsSet,
}

/// What an instance's code depends on beyond the layouts of its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShareKey(pub ContentId);

impl ShareKey {
    pub fn fingerprint(&self) -> Fingerprint {
        self.0.fingerprint()
    }
}

/// Why an instance has no share key and is compiled for its arguments alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Unshared {
    /// The instance is in a cycle of instances or types, which the query engine gives this to all of.
    Recursive,
    /// A shim the code generator builds without MIR: kept per instance.
    Shim(&'static str),
    /// An intrinsic that is not in [`intrinsics`]' table, by its name.
    UnknownIntrinsic(String),
    /// An intrinsic whose code is made for the one instance ([`Class::PerInstance`]).
    PerInstance(String),
    /// No options to key the body with: [`BodyOptions`] is empty for its crate.
    NoOptions,
    /// The body has no MIR, or a part of it the code generator refuses too.
    NoMir(String),
    /// A type has no layout.
    NoLayout(String),
    /// A type or constant has no content: a parameter, an inference variable, an error, an
    /// alias that does not normalize, a constant made of other allocations.
    NoContent(String),
    /// A constant could not be evaluated.
    ConstEval(String),
    /// An item has no hash in [`blaze_items`].
    Unhashable(String),
    /// The body has a place whose type the key does not read.
    Shape(&'static str),
    /// A chain of calls instantiates one body more times than the `#![recursion_limit]` of
    /// the crate the chain started in (rustc's collector, `check_recursion_limit`): arguments
    /// that grow with each call (`fn f<T>(x: T) { f((x,)) }`) never end. The build stops; it
    /// is never keyed by the instance's arguments in place of a share key.
    RecursionLimit {
        /// The crate the chain started in, whose limit it is.
        krate: String,
        /// The body instantiated past the limit.
        body: String,
        /// How many times the body is on the chain, this instance with them.
        depth: u32,
        limit: u32,
        /// The instance past the limit.
        instance: String,
        /// The chain's first ten and last three instances, each its callee with its
        /// arguments, outermost first, and how many are between: the first ten name where the
        /// arguments begin to grow, which rustc's instances never do in a program it builds.
        chain: Vec<String>,
    },
}

impl fmt::Display for Unshared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unshared::Recursive => f.write_str("it is in a cycle of instances or types"),
            Unshared::Shim(what) => write!(f, "it is a {what}, which is kept per instance"),
            Unshared::UnknownIntrinsic(name) => write!(f, "it calls the unclassified intrinsic `{name}`"),
            Unshared::PerInstance(name) => write!(f, "it calls `{name}`, whose code is per instance"),
            Unshared::NoOptions => f.write_str("no MIR options were given for its crate"),
            Unshared::NoMir(what) => write!(f, "its MIR: {what}"),
            Unshared::NoLayout(what) => write!(f, "a layout: {what}"),
            Unshared::NoContent(what) => write!(f, "no content: {what}"),
            Unshared::ConstEval(what) => write!(f, "a constant: {what}"),
            Unshared::Unhashable(what) => f.write_str(what),
            Unshared::Shape(what) => write!(f, "its body has {what}, whose type the key does not read"),
            Unshared::RecursionLimit { krate, body, depth, limit, instance, chain } => write!(
                f,
                "reached the recursion limit while instantiating `{instance}`: `{body}` is instantiated \
                 {depth} times in one chain of calls, past crate `{krate}`'s recursion_limit of {limit} \
                 (`#![recursion_limit = \"N\"]` raises it); the chain: {}",
                chain.join(" -> ")
            ),
        }
    }
}

/// The share key of `instance` in `env`, with its body under each of `options`: equal keys of
/// two instances of one body mean their code is the same.
pub fn share_key<'db>(
    db: &'db Db,
    instance: &ShareInstance,
    env: &StoredParamEnvAndCrate,
    options: BodyOptions,
) -> &'db Result<ShareKey, Unshared> {
    hir_ty::attach_db(db, || walk::share(db, instance.clone(), env.clone(), options)) // clone: the query's keys, reference counts
}
