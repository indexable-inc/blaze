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
    CodegenError, CompiledFunction, InstanceKey, Owner, Static, Unit,
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
#[derive(Debug)]
pub struct Described<I, S> {
    pub item: ItemKey,
    pub code: CodeHash,
    /// The content ID of its type arguments' facts its code reads.
    pub args: ContentId,
    /// Whose code it is: the item's crate's, or for a generic, the crate that instantiates it.
    pub origin: Origin,
    /// `Some` when its MIR says (std's built without, `#[rustc_inherit_overflow_checks]`).
    pub overflow_checks: Option<bool>,
    pub address: Address,
    /// The symbol it is defined as, for what outside code names it by.
    pub symbol: Option<String>,
    /// Everything it refers to, each once, in the order of its MIR: its template's places.
    pub refs: Vec<FrontRef<I, S>>,
    /// The places among `refs` of the functions whose bodies its code builds in, as LLVM's
    /// always-inliner does at -O0: `#[inline(always)]` callees of crates no edit reaches.
    /// Their keys, and those of what they build in, are in its store key; its
    /// [`CompiledFunction::inlined`] may name only their code hashes.
    pub built_in: Vec<u32>,
    /// What a debugger and a backtrace call it: rustc's demangled name of the instance,
    /// with its generic arguments (`core::ptr::drop_in_place<alloc::string::String>`).
    pub name: String,
    /// Whose it is for a dependency image: [`Owner::Dependency`] when its item is a
    /// dependency's or the standard library's and none of its type arguments names a
    /// workspace crate's type, closure or item; an `asm!` function is its asker's.
    pub owner: Owner,
    /// What it is across versions, before its settings ([`crate::exec::fn_id`]): its item's
    /// key, which instance of the item it is (a closure by its place in its owner's body, a
    /// shim by its kind) and its type arguments. An edit of its body keeps it.
    pub identity: ContentId,
    /// Its code as it runs, test selection's `Bound`: its code hash, its item's
    /// `blaze_items::bound_hash` (which function each ref of its body binds to) and its type
    /// arguments. `Err` names why its item's could not be hashed; only a test binary's table
    /// needs it.
    pub bound: Result<ContentId, String>,
}

/// What the front end says of a static.
#[derive(Debug)]
pub struct DescribedStatic<I, S> {
    pub item: ItemKey,
    /// Which instance of the item: a generic's thread-local or a promoted constant's owner;
    /// [`ContentId::default`] for a plain static.
    pub instance: ContentId,
    /// The content ID of its evaluated value and its layout: the key of its bytes.
    pub value: ContentId,
    /// `static mut`, or a type with interior mutability.
    pub mutable: bool,
    pub thread_local: bool,
    /// `#[link_section]`, as written.
    pub section: Option<String>,
    pub origin: Origin,
    pub symbol: Option<String>,
    pub refs: Vec<FrontRef<I, S>>,
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
}

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
    let mut first: blaze_hash::HashMap<ContentId, u32> =
        blaze_hash::HashMap::with_capacity_and_hasher(total, Default::default());
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
    fn masked(&self, unit: &Unit, prepared: &Self::Prepared, refs: &Refs) -> Option<crate::fold::Masked>;

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
