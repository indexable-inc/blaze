//! blaze v2's codegen of the front end (docs/v2/plan/lanes/codegen.md): this crate's IR builder behind
//! [`blaze_codegen`]'s per-function interfaces.
//!
//! [`HirFrontend`] says what an instance is and what it refers to, without compiling it: its
//! item and code by content ([`Hashes`]), whose code it is, and its references in the order
//! of its MIR ([`refs`]). [`Backends`] builds the instance's IR against those references by
//! place ([`places`]), says what the IR is with nothing in it of the build or the run
//! ([`shape`]), and compiles it with Cranelift or LLVM ([`release`]), the code by place and
//! data by content ([`convert`]).

use base_db::{Db, Crate};
use blaze_codegen::{CodegenError, InstanceKey, Unit};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId, ItemKey};
use cranelift_codegen::ir;
use hir_def::{DefWithBodyId, FunctionId, StaticId};
use hir_ty::{
    db::HirDatabase,
    next_solver::{Const, Ty},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::code::TABLES;

use crate::{Instance, InstanceDef, Pending, fragment};

pub(crate) mod allocator;
pub(crate) mod backend;
pub(crate) mod build;
#[cfg(test)]
pub(crate) mod by_id;
pub(crate) mod convert;
pub(crate) mod fold;
pub(crate) mod frontend;
pub(crate) mod hashes;
pub(crate) mod linked;
mod lowered;
pub(crate) mod places;
pub(crate) mod refs;
pub(crate) mod release;
mod roots;
pub(crate) mod shape;
pub(crate) mod side;
pub(crate) mod statics;
#[cfg(test)]
mod tests;
pub(crate) mod toolchain;
pub(crate) mod unwind;
pub(crate) mod wasm;

pub use backend::Backends;
pub use frontend::{BodyChecks, Databases, HirFrontend};
pub use hashes::ItemHashes;
pub use linked::{LinkInputs, span_tables_of};
pub use lowered::{Lowering, lowered};
pub use roots::{RootKind, RootTest, Roots, harness::MODULE as HARNESS_MODULE, proc_macros as proc_macro_list};
pub use toolchain::{Runs, build, select, toolchain};
#[cfg(feature = "llvm")]
pub use crate::llvm::pgo::Profile;
#[cfg(feature = "llvm")]
pub use release::{give_profile, profile_id};

/// What the items lane says of the front end's items by content (docs/v2/plan/lanes/items.md): the
/// keys the code generator puts in place of the database's dense ids, which differ from one
/// run to the next.
pub trait Hashes: Sync {
    /// Which item `item` is, by its path and kind.
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String>;
    /// What a body is: its normalized MIR, each callee and type by content.
    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String>;
    /// A type by content: an ADT by its item and its layout's facts, never by its name.
    /// Regions are erased.
    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String>;
    /// A const generic argument by its value and type.
    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String>;
    /// Where a statement or expression of `owner`'s body is, in its item's span table.
    fn span(
        &self,
        db: &Db,
        owner: DefWithBodyId,
        span: MirSpan,
    ) -> Result<SpanRef, String>;
    /// The anchors of `owner`'s span tables in [`hir_ty_share::code::table_of`]'s order: what
    /// [`Hashes::span`] names a span of the body by, beside its index in the table.
    fn anchors(&self, db: &Db, owner: DefWithBodyId) -> Result<[ContentId; TABLES], String>;
    /// Where a function is declared: what a call through a pointer to a `#[track_caller]`
    /// function passes as its caller's location.
    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String>;
    /// The name a file is known by in the program (`file!()`, a panic location): the path
    /// relative to its package's root, as rustc prints it.
    fn file_name(&self, db: &Db, file: base_db::FileId) -> Result<String, String>;
}

/// The [`Hashes`] a build keys its code by: the items lane's, or a test's.
pub enum HashSource {
    Items(ItemHashes),
    #[cfg(test)]
    ById(by_id::ById),
}

impl Hashes for HashSource {
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String> {
        match self {
            HashSource::Items(it) => it.item(db, item),
            #[cfg(test)]
            HashSource::ById(it) => it.item(db, item),
        }
    }

    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        match self {
            HashSource::Items(it) => it.code(db, body),
            #[cfg(test)]
            HashSource::ById(it) => it.code(db, body),
        }
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        match self {
            HashSource::Items(it) => it.ty(db, ty),
            #[cfg(test)]
            HashSource::ById(it) => it.ty(db, ty),
        }
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        match self {
            HashSource::Items(it) => it.konst(db, konst),
            #[cfg(test)]
            HashSource::ById(it) => it.konst(db, konst),
        }
    }

    fn span(&self, db: &Db, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String> {
        match self {
            HashSource::Items(it) => it.span(db, owner, span),
            #[cfg(test)]
            HashSource::ById(it) => it.span(db, owner, span),
        }
    }

    fn anchors(&self, db: &Db, owner: DefWithBodyId) -> Result<[ContentId; TABLES], String> {
        match self {
            HashSource::Items(it) => it.anchors(db, owner),
            #[cfg(test)]
            HashSource::ById(it) => it.anchors(db, owner),
        }
    }

    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String> {
        match self {
            HashSource::Items(it) => it.declaration(db, func),
            #[cfg(test)]
            HashSource::ById(it) => it.declaration(db, func),
        }
    }

    fn file_name(&self, db: &Db, file: base_db::FileId) -> Result<String, String> {
        match self {
            HashSource::Items(it) => it.file_name(db, file),
            #[cfg(test)]
            HashSource::ById(it) => it.file_name(db, file),
        }
    }
}

/// A body whose code [`Hashes::code`] says. The ids are the query database's interned closures and
/// coroutines (`InternedClosureId`, `InternedCoroutineId`), without their database lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Body {
    Item(DefWithBodyId),
    Closure(blaze_query::db::Id),
    /// The `Future::poll` of an `async` block.
    Coroutine(blaze_query::db::Id),
    /// The future an `async` closure gives when called by value though it could be again.
    CoroutineByMove(blaze_query::db::Id),
}

/// A function instance as the front end holds it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct HirInstance(pub(crate) Kind);

impl HirInstance {
    /// The function item this is an instance of, with any generic arguments. `None` for a
    /// closure, a coroutine, a shim, an `asm!` function, the allocator shim's and the C `main`.
    pub fn function(&self) -> Option<FunctionId> {
        match &self.0 {
            Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) => Some(*func),
            Kind::Code(..) | Kind::Asm(..) | Kind::Start(_) | Kind::Allocator(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)] // clone: inside `Instance`, which the walk of what a function reaches queues once per callee
pub(crate) enum Kind {
    /// What the IR builder compiles: a function, closure, coroutine or shim with its generic
    /// arguments, regions erased, in the environment of the crate it is compiled as
    /// ([`crate::home::Homes`]). `checked` is always `false`: a unit's overflow checks are
    /// its [`blaze_codegen::settings::Spec`]'s.
    Code(Instance, StoredParamEnvAndCrate),
    /// The function a `global_asm!` or `naked_asm!` defines, by its text, and whether code of
    /// the dependency image asks for it: such an asker may be compiled as a workspace crate
    /// asks (core's `__isb::<SY>` for a build-time crate), and its `asm!` function is then
    /// the image's too. The walk and the IR builder name it with `false`; the front end
    /// sets it on the references it describes.
    Asm(String, bool),
    /// The C `main` that runs the program's `fn main`.
    Start(FunctionId),
    /// A function of the program's allocator shim.
    Allocator(allocator::Forward),
}

/// A static as the front end holds it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HirStatic(pub StaticId);

/// A function whose IR is built, waiting for its backend.
pub enum Prepared {
    Cranelift(Box<Function>),
    Llvm(Box<Function>),
    /// For copy-and-patch (`blaze_stencil`): prepared as for Cranelift, over the same IR.
    Stencil(Box<Function>),
}

impl Prepared {
    pub(crate) fn function(&self) -> &Function {
        match self {
            Prepared::Cranelift(it) | Prepared::Llvm(it) | Prepared::Stencil(it) => it,
        }
    }

    pub(crate) fn into_function(self) -> Box<Function> {
        match self {
            Prepared::Cranelift(it) | Prepared::Llvm(it) | Prepared::Stencil(it) => it,
        }
    }
}

/// One prepared function: its IR and what each of its requests is in the build.
pub(crate) struct Function {
    pub(crate) code: Built,
    pub(crate) places: Places,
    /// The positions of its source locations, by the number Cranelift's `SourceLoc` holds.
    pub(crate) spans: Vec<SpanRef>,
    /// Its named locals, for a debugger: empty unless the unit's debug info has them.
    pub(crate) variables: Vec<side::Variable>,
    /// The scopes `variables` are bound in.
    pub(crate) scopes: side::Scopes,
    /// The code hashes of what its IR builds in, at any depth
    /// ([`blaze_codegen::CompiledFunction::inlined`]).
    pub(crate) inlined: Vec<CodeHash>,
    /// What its `#[inline]` asks of LLVM; `None` for closures, glue and shims.
    pub(crate) hint: fragment::Hint,
    /// The sanitizers of the unit's set that are off for it: what its `#[sanitize(.. = "off")]`
    /// turns off, and SanitizerCoverage for a `__sanitizer_*` symbol. None without a set.
    pub(crate) unsanitized: blaze_codegen::settings::Sanitizers,
    /// The target features its code is compiled with: its unit's (`Toolchain::features`) and
    /// what its `#[target_feature(enable)]` turns on.
    pub(crate) features: crate::Features,
    /// Where [`release::summarize`] leaves what [`release::emit`] takes up.
    pub(crate) release: release::Kept,
    /// Whether a fold may give it another function's code: not where its address is
    /// significant (a symbol of its own) or it is C-variadic.
    pub(crate) foldable: bool,
    /// On wasm32, what its code's header says of its imports and export; empty elsewhere.
    pub(crate) wasm: wasm::Header,
}

pub(crate) enum Built {
    /// The IR, with the requests the IR builder made of it (`pending.wants`).
    Ir(Pending),
    /// An assembly function: its code, which no backend compiles.
    Assembled(fragment::Code, fragment::Requests),
}

impl Function {
    /// The IR, for what reads or rewrites it before the backend (fold, release).
    pub(crate) fn ir(&self) -> Option<&ir::Function> {
        match &self.code {
            Built::Ir(pending) => Some(&pending.func),
            Built::Assembled(..) => None,
        }
    }

    pub(crate) fn ir_mut(&mut self) -> Option<&mut ir::Function> {
        match &mut self.code {
            Built::Ir(pending) => Some(&mut pending.func),
            Built::Assembled(..) => None,
        }
    }

    pub(crate) fn requests(&self) -> &fragment::Requests {
        match &self.code {
            Built::Ir(pending) => &pending.wants.requests,
            Built::Assembled(_, requests) => requests,
        }
    }
}

/// What each request of a function's IR is in the build: its references by their place in
/// [`blaze_codegen::backend::Described::refs`], its data by content.
#[derive(Clone, PartialEq, Eq, Debug, Default)] // clone: a release build's export keeps the places of each function it builds
pub(crate) struct Places {
    /// By the function request's number (`FuncId`): its place.
    pub(crate) funcs: Vec<u32>,
    /// By the data request's number (`DataId`).
    pub(crate) datas: Vec<DataPlace>,
    /// How many references the function was prepared against.
    pub(crate) refs: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: inside `Places`
pub(crate) enum DataPlace {
    /// A static, or one of an `extern` block, by its place.
    Place(u32),
    /// Read-only data of the function's own, made of the request's blob.
    Own,
    /// A position the program observes: a `#[track_caller]` location, a panic's.
    Position(SpanRef),
    /// The bytes of a `&str` made at compile time: `file!()`, with what is `concat!`ed to it.
    Text(String),
    /// A `core::panic::Location` of no place the program has a span of.
    Nowhere,
}

/// A crate the build compiles for its build scripts and proc macros, not for the program.
#[derive(Default, Debug)]
pub struct BuildTime(pub blaze_hash::HashSet<Crate>);

/// The error of `unit`, saying `what`.
pub(crate) fn failed(unit: &Unit, what: impl std::fmt::Display) -> CodegenError {
    // alloc: the error's message
    CodegenError { unit: unit.instance.key(), item: unit.item, what: what.to_string() }
}

/// An instance as an error names it: its path with its arguments, and the crate it is
/// compiled in.
pub(crate) struct Named<'a>(pub(crate) &'a Db, pub(crate) &'a HirInstance);

impl std::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Named(db, instance) = *self;
        write!(f, "`{}`", linked::debug_name(db, &instance.0))?;
        if let Kind::Code(_, env) = &instance.0
            && let Some(name) = &env.krate.extra_data(db).display_name
        {
            write!(f, " (compiled in `{}`)", name.as_str())?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// The error of an instance that is not keyed yet, while it is described. Its `Debug` reads
/// types through the interner, so it is formatted with `db` attached: describing runs on
/// threads that have none.
pub(crate) fn undescribed(
    db: &Db,
    instance: &impl std::fmt::Debug,
    item: Option<ItemKey>,
    what: impl std::fmt::Display,
) -> CodegenError {
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: item.unwrap_or(ItemKey(ContentId::default())),
        what: hir_ty::attach_db(db, || format!("{instance:?}: {what}")), // alloc: the error's message
    }
}
