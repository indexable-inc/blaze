//! What the build ([`crate::driver`]) asks of the front end and of a backend.
//!
//! The front end ([`Frontend`]) says what an instance is without compiling it: its item, its
//! code hash and type-argument facts (its [`crate::Instance`]), whose code it is (for its
//! settings) and what it refers to, in order ([`FrontRef`]). A backend ([`Prepare`]) builds an
//! instance's IR against those references by place, says what the IR is ([`Shape`]: equal
//! shapes are one compile, #1291), and compiles it.

use std::{fmt::Debug, hash::Hash};

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use crate::{
    CodegenError, CompiledFunction, InstanceKey, Linkage, Owner, Static, Unit,
    plan::Origin,
    settings::Spec,
    summary::{Imports, Summary},
    template::Refs,
};

/// Whether a function's address is something the program can tell apart from another's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Address {
    /// Rust functions: their addresses are not unique (rustc's `unnamed_addr`), so equal
    /// code is one function.
    Free,
    /// A symbol outside code can name and compare: `#[no_mangle]`, `#[export_name]`, an
    /// `extern` definition. Never merged.
    Significant,
}

/// What an instance refers to, as its front end names it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)] // clone: `Described::refs` is copied out of the front end's memo, once per function described
pub enum FrontRef<I, S> {
    Function(I),
    Static(S),
    /// A symbol outside blaze's code, by its link name.
    Extern(String),
    /// The same, linked weakly (`#[linkage = "extern_weak"]`).
    Weak(String),
}

/// What the front end says of a function instance, without compiling it.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: copied out of the front end's memo, once per function described
pub struct Described<I, S> {
    pub item: ItemKey,
    /// Its code, with the key of the store its body is written in: what its instance key
    /// ([`crate::Instance::key`]) and so the link name it by, so two bodies written alike in
    /// two places are two functions, each reporting its own positions.
    pub code: CodeHash,
    /// What its template is kept under in place of `code` ([`crate::driver::dev_template_key`],
    /// [`crate::driver::release_template_key`]): with `anchors`, its code without its own item's
    /// or store's key, its callees' names or where it is written, so renaming or moving it,
    /// or renaming a local, keeps its template (docs/v2/plan/design/name-free-identity.md).
    /// `code` itself where `anchors` is `None`.
    pub template_code: CodeHash,
    /// The anchors of its own span tables, which its template holds by table
    /// ([`crate::template::own_anchor`]) and its instance is given back, when its template is
    /// kept under `template_code`; `None` when every position of its template is as compiled.
    pub anchors: Option<[ContentId; crate::template::OWN_TABLES]>,
    /// The code a caller that builds its body in has of it, which a
    /// [`CompiledFunction::inlined`] names it by: a copy built in is built under the copy's
    /// options (`MirOptions::built_in`), which read no dyn facts, so for one whose own unit is
    /// built against them (`code` then hashes this code with the facts it read) the two
    /// differ; `code` otherwise. The caller's own `Devirtualize` may still make a `dyn` call
    /// in the copy direct; what it read keys the caller's `code`, not this.
    pub built_in_code: CodeHash,
    /// The content ID of its type arguments' facts its code reads.
    pub args: ContentId,
    /// Whose code it is: the item's crate's, or for a generic, the crate that instantiates it.
    pub origin: Origin,
    /// Whether its body is written in a workspace crate, which an edit reaches: of a member's
    /// instances, only these are patchable in a live build ([`crate::plan::Choice::patchable`]).
    /// A generic of another crate that a member instantiates is the member's by `origin` but
    /// not editable: no edit changes its body (an edit of its arguments' types gives another
    /// instance), and a caller may build it in (`#[inline(always)]`, [`Self::built_in`]).
    pub editable: bool,
    /// Whether one of its type arguments names a workspace crate's type, closure, coroutine or
    /// function item, when its own body is not editable: no edit changes its body, but one can
    /// change which keys its references resolve to while its own key stays (a closure is a
    /// type argument by which one it is, never by its body), so its fill changes under its
    /// key. A live build reaches it through its slot, as it does an edited function
    /// (`driver`'s `rebind`; docs/v2/plan/design/codegen.md, "Slots and a splice's reach").
    pub over_editable: bool,
    /// `Some` when its MIR says (std's built without, `#[rustc_inherit_overflow_checks]`).
    pub overflow_checks: Option<bool>,
    pub address: Address,
    /// The symbol it is defined as, for what outside code names it by.
    pub symbol: Option<String>,
    /// How `symbol` is defined: [`Linkage::Weak`] makes every reference to it, its build's
    /// own included, one by name ([`crate::Target::Extern`]); [`Linkage::Strong`] without one.
    pub linkage: Linkage,
    /// Everything it refers to, each once, in the order of its MIR: its template's places.
    pub refs: Vec<FrontRef<I, S>>,
    /// The places among `refs` of the functions whose bodies its code builds in, as LLVM's
    /// always-inliner does at -O0: `#[inline(always)]` callees of crates no edit reaches.
    /// Their keys, and those of what they build in, are in its store key; its
    /// [`CompiledFunction::inlined`] may name only their [`Self::built_in_code`]s.
    pub built_in: Vec<u32>,
    /// What a debugger and a backtrace call it: rustc's demangled name of the instance,
    /// with its generic arguments (`core::ptr::drop_in_place<alloc::string::String>`).
    pub name: String,
    /// Whose it is for a dependency image, as the front end decides for every function and
    /// static of the build by one rule: [`Owner::Dependency`] for the standard library's code
    /// in every build, and for a dependency's when the build links an image, no edit reaches
    /// its crate and none of its type arguments names a workspace crate's type, closure or
    /// item; [`Owner::Workspace`] otherwise. An `asm!` function is its asker's.
    pub owner: Owner,
    /// What it is across versions, before whose code it is ([`crate::driver::subject`]): its item's
    /// key, which instance of the item it is (a closure by its place in its owner's body, a
    /// shim by its kind) and its type arguments. An edit of its body keeps it.
    pub identity: ContentId,
    /// Its code as it runs, test selection's `Bound`: its code hash, its item's
    /// `blaze_items::bound_hash` (which function each ref of its body binds to) and its type
    /// arguments. `Err` names why its item's could not be hashed; only a test binary's table
    /// needs it.
    pub bound: Result<ContentId, String>,
    /// Whether it runs only when something has gone wrong, so a full link places it after
    /// every other function ([`Built::cold`](crate::driver::Built::cold)): `#[cold]`, the
    /// panic machinery, a diverging `#[track_caller]` function, a closure of one of those.
    /// A profile that saw it run wins (docs/v2/plan/design/code-layout.md).
    pub cold: bool,
}

impl<I, S> Described<I, S> {
    /// The codes a store key that holds this body holds: its own `code`, which an import of
    /// its compiled body is, and its [`Self::built_in_code`], which `code` hashes, which a
    /// copy of its body built into a caller is. What [`CompiledFunction::inlined`] may name
    /// of it (`driver`'s `unkept_inline`).
    pub fn held_codes(&self) -> [CodeHash; 2] {
        [self.code, self.built_in_code]
    }
}

/// What the front end says of a static.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: copied out of the front end's memo, once per static described
pub struct DescribedStatic<I, S> {
    pub item: ItemKey,
    /// Which instance of the item: a generic's thread-local or a promoted constant's owner;
    /// [`ContentId::default`] for a plain static.
    pub instance: ContentId,
    /// The content ID of its evaluated value and its layout: the key of its bytes. Its
    /// definition's store key is made of it ([`crate::driver`]'s static definition), so it must
    /// cover everything the compiled static is made of but its flags, section and how many
    /// references of which kind: its alignment, how it is initialized and its data, its
    /// relocations by place (each one's offset, addend and the place it resolves), and
    /// what its externs are imported as. A value that leaves one out lets two statics of other
    /// bytes share one key (rule 21).
    pub value: ContentId,
    /// `static mut`, or a type with interior mutability.
    pub mutable: bool,
    pub thread_local: bool,
    /// `#[link_section]`, as written.
    pub section: Option<String>,
    pub origin: Origin,
    /// Whose it is for a dependency image: its crate's, by the rule that decides a function's
    /// ([`Described::owner`]), so a static of a crate whose functions are the workspace's is
    /// the workspace's too. Never read off `origin`, which is the settings' concept.
    pub owner: Owner,
    pub symbol: Option<String>,
    /// How `symbol` is defined, as [`Described::linkage`].
    pub linkage: Linkage,
    /// Its path, as errors name it (`krate::module::NAME`).
    pub name: String,
    pub refs: Vec<FrontRef<I, S>>,
}

/// Which body a function instance is of, as the build's walk counts instances against the
/// recursion limit ([`Frontend::counted`]): rustc's collector counts per `DefId`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Instantiates {
    /// The body: equal for two instances of one body (a function and its vtable shim are one,
    /// as they are one `DefId`; all drop glue is one), and only then.
    pub body: ContentId,
    /// Drop glue, which rustc counts at a quarter (its `LangItem::DropGlue` margin): a nested
    /// type gives it a chain of instances as deep as the type.
    pub drop_glue: bool,
}

/// A crate's `#![recursion_limit]`, and which crate's it is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Limit {
    /// The crate, by an identity that is equal for one crate and only then (never persisted).
    pub krate: ContentId,
    pub value: u32,
}

/// What the build's walk reads of a function instance for rustc's recursion limit
/// (`driver/recursion.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Counted {
    /// What it is an instance of: `None` for one the limit does not count (an `asm!` function,
    /// one of the allocator shim's, the C `main` that runs the program's `main`), as rustc's
    /// collector counts no such item.
    pub body: Option<Instantiates>,
    /// The limit of the crate it is built for, which holds a path it heads: `None` for one of
    /// no crate, past which each function heads a path of its own.
    pub limit: Option<Limit>,
    /// It is collected in its own crate's build, never in a crate that uses it: a function
    /// with no type or const arguments that is not `#[inline]` (rustc's
    /// `should_codegen_locally` is false for it upstream). Reached from another crate's path,
    /// it heads a path of its own, held to the larger of the two limits.
    pub heads_own_path: bool,
}

/// The front end of a build: which instances there are and what they refer to.
pub trait Frontend: Sync {
    /// A function instance, as the front end holds it. Never hashed: its content is
    /// [`Described::code`] and [`Described::args`].
    type Instance: Clone + Eq + Hash + Send + Sync + Debug;
    type Static: Clone + Eq + Hash + Send + Sync + Debug;

    /// `requester` is whose code asks for it: a generic is compiled as that crate asks.
    fn describe(
        &self,
        instance: &Self::Instance,
        requester: &Origin,
    ) -> Result<Described<Self::Instance, Self::Static>, CodegenError>;

    fn describe_static(
        &self,
        value: &Self::Static,
    ) -> Result<DescribedStatic<Self::Instance, Self::Static>, CodegenError>;

    /// The instances and statics the walk reached next, before any of them is described:
    /// what a front end checking bodies beside the build checks first. Once a level.
    fn reached(&self, functions: &[(Self::Instance, Origin)], statics: &[Self::Static]);

    /// Whether the build's outcome is decided without its code (a body it compiles failed
    /// its check, or its job's stop flag is set): the build begins no further function and
    /// gives [`stopped`](crate::driver). Asked once per function, so it must be a load or two.
    fn stopped(&self) -> bool;

    /// What the recursion limit reads of each of `functions`, in order ([`Counted`]): one
    /// entry a function. Another count is the build's error, naming the functions left
    /// without one.
    fn counted(&self, functions: &[(Self::Instance, Origin)]) -> Vec<Counted>;

    /// `instance` as an error names it: its path and its arguments, cut short.
    fn spell(&self, instance: &Self::Instance) -> String;

    /// The error of `instance`, the `depth`th repeat of its body along the walk's path from
    /// `head`, past `limit`, the `#![recursion_limit]` of the crate `limit_of` is built for:
    /// naming the function, the instance and that crate.
    fn past_recursion_limit(
        &self,
        instance: &Self::Instance,
        depth: u32,
        limit: u32,
        head: &Self::Instance,
        limit_of: &Self::Instance,
    ) -> CodegenError;

    /// What describing `instance` for `requester` gives now, where it is other than `was`:
    /// what this front end's database gave an earlier walk of it, which the driver kept
    /// (`driver::Kept`). `Ok(None)` says a describe now would give `was`, decided without
    /// the describe wherever the front end can (a memo its query engine verifies); the driver
    /// then takes `was` and asks nothing more. The answer is exact: a front end that cannot
    /// tell gives the new describe. The default describes again, which is the whole
    /// computation, so a walk over it is the walk with nothing kept.
    fn redescribe(
        &self,
        instance: &Self::Instance,
        requester: &Origin,
        was: &Described<Self::Instance, Self::Static>,
    ) -> Result<Option<Described<Self::Instance, Self::Static>>, CodegenError> {
        let _ = was;
        self.describe(instance, requester).map(Some)
    }

    /// Whether what this front end says of `instance` for `requester` ([`Self::describe`],
    /// [`Self::counted`]) was made of durable inputs alone: ones whose every write moves
    /// [`Self::durable_view`]. Asked after a describe of it in this build. `true` promises
    /// that for as long as the durable view is the same, a describe and a count give what
    /// they gave, so the driver asks nothing of it. The default is `false`: asked each build.
    fn durable(&self, instance: &Self::Instance, requester: &Origin) -> bool {
        let _ = (instance, requester);
        false
    }

    /// A number that is another after any write of an input a durable describe may read
    /// (a library's source, a frozen table); `None` for a front end that tells none, whose
    /// walk is then never patched.
    fn durable_view(&self) -> Option<u64> {
        None
    }

    /// Whether a walk of this front end may close facts of the whole program
    /// ([`Self::closed`] may say `true`). The facts are made from everything the walk reached
    /// ([`Self::reached`]), so a walk patched from a kept one, which tells only what changed,
    /// is for a front end that says `false`. The default is `true`.
    fn closes(&self) -> bool {
        true
    }

    /// Told that the walk has reached everything it was told of ([`Self::reached`]), with
    /// no error: `true` when what it reached closes facts of the whole program its bodies are
    /// to be described and built against from now on (a final binary's trait objects, which
    /// `hir-ty-codegen` devirtualizes calls by), so the walk runs once more and describes
    /// every instance again; `false` when there are none, or they are those the walk was made
    /// against. Facts the second walk finds other than the first are the front end's error.
    fn closed(&self) -> Result<bool, CodegenError> {
        Ok(false)
    }
}

/// What a prepared function compiles to, up to what fills its places: the content ID of its
/// canonical bytes (its IR with every reference by its place in [`Described::refs`] and every
/// setting that changes its code), its unit's settings and architecture. Two with one compile
/// to one [`crate::template::Template`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Shape(ContentId);

pub const SHAPE_DOMAIN: &str = "blaze v2 shape 1";

impl Shape {
    /// The shape of `bytes`, the canonical bytes of a prepared function of `unit`: hashed as
    /// given, kept nowhere.
    pub fn of(unit: &Unit, bytes: &[u8]) -> Shape {
        let mut hasher = blaze_ids::Hasher::new(SHAPE_DOMAIN);
        hasher.id(&unit.instance.settings.0);
        hasher.update(&[crate::content::arch_tag(unit.arch)]).part(bytes);
        Shape(hasher.finish())
    }

    pub fn key(&self) -> ContentId {
        self.0
    }

    /// This shape as compiled to a template relative to its function's own anchors
    /// ([`Described::anchors`]) when `relative`, else itself. A relative and an absolute
    /// template of one IR are two memo entries: a template's places at its own code hold
    /// anchors or store keys, and the memo is kept across builds, so a function whose
    /// template key turns name-free (or back, when a report or profiling starts or stops
    /// naming it) never takes the other form.
    pub fn relative(self, relative: bool) -> Shape {
        if !relative {
            return self;
        }
        let mut hasher = blaze_ids::Hasher::new(RELATIVE_SHAPE_DOMAIN);
        hasher.id(&self.0);
        Shape(hasher.finish())
    }

    /// This shape as compiled to a template of a function whose place `at` is itself
    /// ([`crate::template::own_place`]), else itself. The IR takes that place like any other,
    /// but its template holds a [`crate::template::Ref::Own`] there, so a function that calls
    /// itself and one with the same IR that calls another instance at that place compile to two
    /// templates: one memo entry would give one of them the other's call (rule 21).
    pub fn own_place(self, at: Option<u32>) -> Shape {
        let Some(at) = at else { return self };
        let mut hasher = blaze_ids::Hasher::new(OWN_PLACE_SHAPE_DOMAIN);
        hasher.id(&self.0).update(&at.to_le_bytes());
        Shape(hasher.finish())
    }
}

pub const RELATIVE_SHAPE_DOMAIN: &str = "blaze v2 relative shape 1";
pub const OWN_PLACE_SHAPE_DOMAIN: &str = "blaze v2 own place shape 1";

/// What a compile gives.
#[derive(PartialEq, Eq, Debug)]
pub struct Emitted {
    pub function: CompiledFunction,
    /// Its debug info and panic locations, kept in the store under
    /// [`CompiledFunction::side`].
    pub side: Option<Vec<u8>>,
    /// Outside symbols the backend calls of its own accord, which no reference names: LLVM's
    /// `memcpy` from a loop idiom, `bcmp`, libm, compiler-rt, stack probes. Each is a
    /// [`crate::Target::Extern`] of the function's relocations, by name; the template keeps
    /// them ([`crate::template::Template::externs`]). Empty for what lists every symbol it
    /// calls among its references.
    pub extra: Vec<String>,
}

/// A function a release compile imports: its export (its IR as the backend shows it to an
/// importer) and what its places are in this build.
#[derive(Debug)]
pub struct Import<'a, E> {
    pub key: InstanceKey,
    pub export: &'a E,
    pub refs: &'a Refs,
}

impl<E> Clone for Import<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Import<'_, E> {}

/// The references of a release compile: its own, then each imported function's, in
/// [`Imports::imported`]'s order, each target once. The places of its template. With them,
/// the first place of each of its own references and then each import's, in that order:
/// which of them are one target, which no key of the compile says (a key takes callees by
/// their interface), and which decides how many places its template has and where each
/// relocation points. [`laid_out`] keys the compile on it.
pub fn release_refs(own: &Refs, imported: &[&Refs]) -> (Vec<crate::Target>, Vec<u32>) {
    let total = own.len() + imported.iter().map(|it| it.len()).sum::<usize>();
    let mut first: blaze_hash::HashMap<ContentId, u32, std::alloc::Global> =
        blaze_hash::HashMap::with_capacity_in(total, std::alloc::Global);
    // alloc: the release unit's references, kept by its job
    let mut refs = own.to_vec();
    // alloc: the release unit's layout, hashed into its key and dropped
    let mut places = Vec::with_capacity(total);
    for (at, target) in own.iter().enumerate() {
        places.push(*first.entry(crate::content::target_id(target)).or_insert(place(at)));
    }
    for import in imported {
        for target in import.iter() {
            let at = *first.entry(crate::content::target_id(target)).or_insert_with(|| {
                // clone: as `refs`, once per target an import adds
                refs.push(target.clone());
                place(refs.len() - 1)
            });
            places.push(at);
        }
    }
    (refs, places)
}

fn place(at: usize) -> u32 {
    u32::try_from(at).expect("a compile refers to fewer than 2^32 things")
}

pub const LAID_OUT_DOMAIN: &str = "blaze v2 release laid out 1";

/// What a release compile of `stored` is kept under, before its imports
/// ([`Imports::blob_key`]): `stored` and the places of [`release_refs`].
pub fn laid_out(stored: InstanceKey, places: &[u32]) -> InstanceKey {
    let mut hasher = blaze_ids::Hasher::new(LAID_OUT_DOMAIN);
    hasher.id(&stored.0).update(&(places.len() as u64).to_le_bytes());
    for at in places {
        hasher.update(&at.to_le_bytes());
    }
    InstanceKey(hasher.finish())
}

/// A backend, as a build runs it per function.
pub trait Prepare: Sync {
    type Frontend: Frontend;
    /// A function whose IR is built, waiting to be compiled.
    type Prepared: Send;
    /// A release function's IR as its importers link it; held for one build, never stored.
    type Export: Send + Sync;

    /// Builds the IR of `instance` against `refs`, which are [`Described::refs`] as this
    /// build resolved them. A reference the IR needs that `refs` lacks is an error, never
    /// added: the front end's list is the template's places.
    fn prepare(
        &self,
        frontend: &Self::Frontend,
        unit: &Unit,
        spec: &Spec,
        instance: &<Self::Frontend as Frontend>::Instance,
        refs: &Refs,
    ) -> Result<(Shape, Self::Prepared), CodegenError>;

    /// Release: what cross-function passes need of the function, and its export, from its
    /// prepared IR. Only asked of units whose backend is LLVM. What it makes that
    /// [`Prepare::emit`] takes up (a pre-linked module) stays in `prepared`, which the build
    /// hands to `emit`: never in state shared between units.
    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Self::Prepared,
        refs: &Refs,
    ) -> Result<(Summary, Self::Export), CodegenError>;

    /// Compiles a prepared function. `imports` is what a release unit may build in, decided
    /// from the summaries of the whole build, with [`release_refs`] as `refs`; `None` for
    /// every other, with its own.
    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Self::Prepared,
        refs: &Refs,
        imports: Option<(&Imports, &[Import<'_, Self::Export>])>,
    ) -> Result<Emitted, CodegenError>;

    /// #1292: the prepared IR with its constants masked, for [`crate::fold::groups`]; `None`
    /// for one that can't be folded (a backend or a function that has no thunk).
    fn masked(
        &self,
        unit: &Unit,
        prepared: &Self::Prepared,
        refs: &Refs,
    ) -> Option<crate::fold::Masked>;

    /// Compiles a group's one body, taking the constants that differ as parameters, and a
    /// thunk for each member, which calls it with the member's constants. `members` are in
    /// the group's order, each with its own references.
    fn merge(
        &self,
        group: &crate::fold::Group,
        members: Vec<(&Unit, &Spec, Self::Prepared, &Refs)>,
    ) -> Result<crate::fold::Merged, CodegenError>;

    /// A static's value, its references by place in `refs`.
    fn compile_static(
        &self,
        frontend: &Self::Frontend,
        spec: &Spec,
        value: &<Self::Frontend as Frontend>::Static,
        refs: &Refs,
    ) -> Result<Static, CodegenError>;
}
