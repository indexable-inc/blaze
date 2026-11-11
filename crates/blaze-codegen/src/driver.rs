//! A build's codegen, one function instance at a time (docs/v2/plan/design/codegen.md).
//!
//! 1. **Walk**: from the roots, the front end describes each instance and what it refers to,
//!    a level at a time on all cores; the policy gives each its settings and so its key.
//! 2. **Places**: each instance's references resolved to this build's targets (a callee
//!    reached through its slot or directly).
//! 3. **Dev** (Cranelift, stencils): a template from the store by instance key, else from the memo by
//!    shape (#1291), else compiled; folded groups first where the settings ask (#1292).
//! 4. **Release** (LLVM): every summary (stored, else from prepared IR), the imports of the
//!    whole build decided from them, then each compile by its imports' blob key.
//! 5. **Statics**, by their value's content and settings, never merged.
//! 6. Every template instantiated with this build's places, then code dedupe.
//!
//! The store is read through [`Lookup`] on the calling thread and written by the caller from
//! [`Built::puts`]: what the build compiled, its keys. A store entry that fails to read is an
//! error naming the unit, never a reason to compile again.

use std::alloc::Global;
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use blaze_cas::{Key, Kind};
use blaze_graph::par::{self, IntoSource};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use blaze_mem::{Tag, frame};

use crate::{
    CodegenError, CompiledFunction, Instance, InstanceKey, Linkage, Static, Target, Unit,
    backend::{
        Address, Described, DescribedStatic, Emitted, FrontRef, Frontend, Import, Prepare, Shape,
        laid_out, release_refs,
    },
    blob,
    dedupe::{Deduped, Originals, dedupe_keeping},
    flight::{Asked, Decision, Found, Plan},
    fold::{self, Group, Masked, Masking},
    imports, kernel,
    memo::Memo,
    plan::{Origin, Policy},
    query::keys,
    settings::Spec,
    summary::{Imports, Summary},
    template::{OwnAnchors, StaticTemplate, Template, TemplateError, own_place},
};

/// The store as a build reads it.
pub trait Lookup {
    /// The verified bytes under `key`, borrowed from the store, read in place; `Err` names
    /// why what is there can't be read.
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String>;
}

impl Lookup for blaze_cas::Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        match blaze_cas::Store::get(self, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// A value for the store.
#[derive(PartialEq, Eq, Debug)]
pub struct Put {
    pub key: Key,
    pub kind: Kind,
    pub bytes: Vec<u8>,
}

/// One dev template a build took from the store or made, as the kernel keeps it: the entry of
/// [`kernel::DevTemplate`] under [`blaze_kernel::whole::key`]. What the invalidation oracle's
/// trace holds of the kernel (`blaze_v2_proof::kernel`), as [`Built::inputs`] is what its codegen
/// inputs are.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct KernelEntry {
    /// The function of the walk it is the template of, by key.
    pub instance: InstanceKey,
    pub step: blaze_kernel::StepId,
    /// The store key it is kept under.
    pub key: Key,
    /// Taken from the store, rather than compiled or found in the memo or a flight.
    pub hit: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    pub instances: usize,
    pub statics: usize,
    /// Templates read from the store.
    pub stored: usize,
    /// Templates the memo had: compiles saved up to callee renaming.
    pub memo: usize,
    /// Templates another build of the memo compiled, before this one reached them or while it
    /// waited ([`crate::flight`]): neither prepared nor compiled here.
    pub flown: usize,
    pub compiled: usize,
    /// Functions that became thunks of a folded body.
    pub folded: usize,
    /// Imports of the release compiles that ran.
    pub imported: usize,
    /// What each profile the build used (`Pgo::Use`, by its content ID) did to its functions,
    /// compiled or found in the store. Sorted, so a caller that sums the builds of a job and
    /// names the profiles that applied to nothing names them in one order. A build decides
    /// nothing from it: a target the profiled program never ran applies none of its records,
    /// which only the whole job, every target summed, can tell from a profile that names
    /// nothing at all (blaze-cargo's job does).
    pub profiles: std::collections::BTreeMap<ContentId, ProfileCounts>,
    /// Store hits the store check ([`Build::verify_store`]) compiled again and compared.
    pub verified: usize,
}

impl Stats {
    /// `other`'s counts added to these: what one item of a loop on all cores counted, into its
    /// build's ([`Out::absorb`]).
    fn add(&mut self, other: Stats) {
        let Stats {
            instances,
            statics,
            stored,
            memo,
            flown,
            compiled,
            folded,
            imported,
            profiles,
            verified,
        } = other;
        self.instances += instances;
        self.statics += statics;
        self.stored += stored;
        self.memo += memo;
        self.flown += flown;
        self.compiled += compiled;
        self.folded += folded;
        self.imported += imported;
        self.verified += verified;
        for (id, counts) in profiles {
            // alloc: once per profile a build uses
            let at = self.profiles.entry(id).or_default();
            *at = at.plus(counts);
        }
    }
}

/// What one profile did to the functions built with it ([`crate::Profiled::Used`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ProfileCounts {
    /// Functions built with it.
    pub used: u64,
    /// Of those, the ones it has a record of under their name with another control flow hash:
    /// edited since the profile was made ([`crate::Profiled::mismatched`]).
    pub mismatched: u64,
    /// Of those, the ones it gave counts ([`crate::Profiled::applied`]).
    pub applied: u64,
}

impl ProfileCounts {
    /// `self` and `other`, summed.
    pub fn plus(self, other: ProfileCounts) -> ProfileCounts {
        ProfileCounts {
            used: self.used + other.used,
            mismatched: self.mismatched + other.mismatched,
            applied: self.applied + other.applied,
        }
    }
}

/// What a build gives the linker.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Built {
    /// By key, after dedupe.
    pub functions: Vec<CompiledFunction>,
    /// By item and instance.
    pub statics: Vec<Static>,
    pub roots: Vec<InstanceKey>,
    /// Functions outside code names, with their symbols and how each is defined
    /// ([`crate::backend::Described::linkage`]).
    pub symbols: Vec<(InstanceKey, String, Linkage)>,
    pub static_symbols: Vec<(ItemKey, ContentId, String, Linkage)>,
    /// Functions reached through their slots: those an edit changes the body of and those
    /// an edit fills again under their own keys (`FnNode::slotted`).
    pub patchable: Vec<InstanceKey>,
    /// Functions of a live build whose code [`Build::tier_up`] replaces.
    pub tiered: Vec<InstanceKey>,
    pub deduped: Deduped,
    /// By key, each once.
    pub puts: Vec<Put>,
    pub stats: Stats,
    /// Every instance the walk reached, compiled or found, with the code hash it was made
    /// from, by key.
    pub inputs: Vec<(InstanceKey, CodeHash)>,
    /// The items of every instance and static, sorted, each once: the owners of the spans
    /// the code names.
    pub items: Vec<ItemKey>,
    /// Every instance the walk reached by key, with the name a debugger calls it
    /// ([`Described::name`]), those dedupe merged away included.
    pub debug_names: Vec<(InstanceKey, String)>,
    /// What each root reaches ([`Built::reached`]).
    pub reach: Reach,
    /// A lazy build's ([`Build::run_later`]) functions, every one the walk reached, in its
    /// order; empty for a whole build.
    pub walked: Vec<Walked>,
    /// A lazy build's entry, by key: the functions compiled ahead with those the start
    /// sections name.
    pub entry: Vec<InstanceKey>,
    /// Every instance the walk reached by key, whose it is for a dependency image
    /// ([`Owner`]), with the code hash of the generic it is an instance of.
    pub owners: Vec<(InstanceKey, Owner, CodeHash)>,
    /// Every static by item and instance, whose it is.
    pub static_owners: Vec<(ItemKey, ContentId, Owner)>,
    /// Every instance the walk reached whose body is written in a workspace crate
    /// ([`Described::editable`]), by key: the code a lint of the workspace's code reports on
    /// (`blaze::vecreport`). Not [`Self::owners`], which says what a dependency image holds
    /// and, in a build that links none, makes every crate but the library's the workspace's.
    pub editable: Vec<InstanceKey>,
    /// Of each of [`Built::inputs`], in its order, what test selection keys it by: what it is
    /// across versions ([`subject`] with its settings) and its code as it runs ([`crate::exec::fn_id`], `exec::bound`). A bound its item has none of is the error a test binary's
    /// [`crate::exec::ExecTable`] names, and no build's but its.
    pub versions: Vec<(ContentId, Result<ContentId, String>)>,
    /// A digest of each function, static and debug name, made with it ([`Digests`]).
    pub digests: Digests,
    /// Every memo entry the build took or put, sorted, each once: the shapes of its templates,
    /// the store keys its release, folded and thunk templates and its summaries are kept under,
    /// and a lazy build's recorded shapes. What a collection of the memo keeps for it
    /// ([`Memo::retain`]).
    pub memo_used: Vec<ContentId>,
    /// Every instance the walk reached by key, with what it is across edits of its body and
    /// its settings, told apart from instances side by side ([`subject`]), and each folded group's body with one made of
    /// its members' identities: what a profile of an earlier build names it by
    /// (docs/v2/plan/design/code-layout.md).
    pub identities: Vec<(InstanceKey, ContentId)>,
    /// The instances the front end inferred cold ([`Described::cold`]), by key: a full link
    /// puts them after every other function.
    pub cold: Vec<InstanceKey>,
    /// Every instance the walk reached, sorted by key, each once, with how it was made: its
    /// backend and the least origin that asked for it (`FnNode::least_origin`). The linker
    /// sums an image's code by them; nothing is keyed by them.
    pub made: Vec<(InstanceKey, crate::Made)>,
    /// Every dev template a whole build took from the store or made, sorted, each once (a lazy
    /// build's are not recorded yet): the kernel's entries, for the oracle.
    pub kernel: Vec<KernelEntry>,
    /// Why each dev template the store missed was not in it, as its key's parts, sorted, each
    /// once: made only when the build was asked to ([`Build::explain_misses`]), for `blaze why`
    /// (`blaze_kernel::why`).
    pub misses: Vec<blaze_kernel::Miss>,
    /// What the walk did: how many describes it asked the front end for, how many kept ones
    /// it took, and why it took none ([`Walk::fresh`]). Nothing is keyed by it.
    pub walk: Walk,
    /// Which whole `Built` this is, by content ([`BuiltId`]): every field above but the
    /// counts of the build's own work. The ID a build with nothing kept gives the same lists.
    pub id: BuiltId,
    /// What this `Built` changed since the last one of the same program, whose ID is its
    /// base: a consumer that holds state for the base applies it and reads nothing else;
    /// any other reads the whole lists ([`Delta`]).
    pub delta: Delta,
    /// What the build's walk and the passes after it did, counted ([`Work`]): of a build
    /// that patched the walk and the lists kept of the last one. Nothing is keyed by it.
    pub work: Work,
}

impl Built {
    /// The code hashes, arguments and static values of everything `root` reaches, itself and
    /// the callees its code builds in included, sorted and each once: what a test's result
    /// depends on apart from its settings, the code outside blaze's (`extern` symbols, C
    /// libraries) and the positions of the span tables its code names.
    ///
    /// # Panics
    ///
    /// When `root` is no function of the build: the caller asks of one of [`Built::roots`].
    pub fn reached(&self, root: &InstanceKey) -> Vec<CodeHash> {
        self.reach.of(root).unwrap_or_else(|| {
            panic!("{root:?} is no function of the build, so it reaches nothing")
        })
    }
}

/// What [`Build::tier_up`] gives: each tiered function's key, with the code for its slot.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct TieredUp {
    pub replaced: Vec<(InstanceKey, CompiledFunction)>,
    pub puts: Vec<Put>,
    pub stats: Stats,
    /// As [`Built::memo_used`].
    pub memo_used: Vec<ContentId>,
}

/// What [`Build::reach`] gives: everything the roots reach, as a build's walk reaches it
/// (change 16's reachability pass), with nothing prepared or compiled.
#[derive(Debug)]
pub struct Reached<I, S> {
    /// Every function, in key order (that of [`Built::inputs`]), with its unit as the
    /// build's policy makes it.
    pub functions: Vec<(I, Unit)>,
    /// Of each of `functions`, in its order, this build's target of each of its references:
    /// the `refs` [`Prepare::prepare`] takes.
    pub refs: Vec<Vec<Target>>,
    /// Every static and thread-local, by item and instance, with the targets of its
    /// references: what [`Prepare::compile_static`] takes.
    pub statics: Vec<(S, Vec<Target>)>,
}

/// A root of rustc's recursion limit's check (`driver/recursion.rs`), in the order rustc's root
/// collector visits them: what decides which path an instance is counted on. The build's own
/// roots are what it compiles; a check root that is none of them (a static no code names and
/// no linker keeps, the program's `main`) is described for what it refers to and walked by the
/// check, never compiled for it: what only it reaches is left out of the build.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum CheckRoot<I, S> {
    /// A function, and whose code asks for it.
    Function(I, Origin),
    /// A static, walked for what its value refers to.
    Static(S),
}

type FrontInstance<B> = <<B as Prepare>::Frontend as Frontend>::Instance;
type FrontStatic<B> = <<B as Prepare>::Frontend as Frontend>::Static;
type Node<B> = FnNode<FrontInstance<B>, FrontStatic<B>>;

/// A function of a walk, over the front end's instance `I` and static `S`: what a lazy run
/// keeps of it past its build ([`Later`]) is this, so it names no backend or database.
// clone: a chunk of the walk's nodes a kept walk shares, copied by the build that patches it
#[derive(Clone)]
struct FnNode<I, S> {
    unit: Unit,
    spec: Spec,
    then: Option<Spec>,
    /// Whether an edit changes its body, in a build that reaches edited code through slots:
    /// reached through its slot, and never built into or imported by another function.
    patchable: bool,
    /// Whether an edit can fill it again under its own key, though none changes its body
    /// ([`rebind`]): reached through its slot too, so no caller the image keeps in place
    /// holds a direct branch to code a splice replaces.
    rebound: bool,
    /// The part of [`Self::rebound`] its own describe decides ([`Described::over_editable`]):
    /// what [`rebind`]'s rule adds to, and what a patched walk evaluates the rule from again.
    rebound_own: bool,
    instance: I,
    /// Whose code the walk first asked for it as: what describing it again takes.
    requester: Origin,
    /// The least origin of every describe the walk keyed to it: what tells it apart
    /// ([`subject`]: its live slot, its test binary's table) from an instance of the same
    /// identity compiled for another origin. Each origin's describe of one instance gives one key, so
    /// two keys of one identity have no origin in common and so two least ones; the least,
    /// never `described.origin` (the describe kept, which a merge may swap), so it is the
    /// same whichever describe came first.
    least_origin: Origin,
    /// What the front end said of it: shared with the walk kept for the next build
    /// ([`Kept`]), which then takes it again without a describe.
    described: Arc<Described<I, S>>,
    /// [`digests::debug_name_digest`] of `described.name`, made where it is described.
    name_digest: ContentId,
    /// This build's targets of `described.refs`.
    refs: Vec<Target>,
    /// The functions its code builds in, at any depth, sorted.
    built_in: Vec<InstanceKey>,
}

pub const BUILT_IN_DOMAIN: &str = "blaze v2 built in 1";

/// What a unit's compiled code is kept under: its key, and the keys of the bodies it builds
/// in, as its own key takes callees by their interface. `blaze_global` makes a release key
/// again with it, to prove the function public.
pub fn stored(key: InstanceKey, built_in: &[InstanceKey]) -> InstanceKey {
    if built_in.is_empty() {
        return key;
    }
    let mut hasher = Hasher::new(BUILT_IN_DOMAIN);
    hasher.id(&key.0).update(&(built_in.len() as u64).to_le_bytes());
    for it in built_in {
        hasher.id(&it.0);
    }
    InstanceKey(hasher.finish())
}

pub const PLACED_DOMAIN: &str = "blaze v2 placed 2";

/// What a unit's code is kept under: [`stored`], the kind of each of its references, by
/// place, and which place is the function itself (`own`, [`crate::template::own_place`] of
/// the references its template is made against). A template is made against the places of its
/// references, and two bodies of one key can have other places (a fact of the arguments the
/// key leaves out adds a call), so how many there are and what each is are keyed; which callee
/// or symbol is at a place is not, as a template takes any, but for the function itself, which
/// its template holds as its own and never as a place (rule 21).
pub fn placed(stored: InstanceKey, refs: &[Target], own: Option<u32>) -> InstanceKey {
    let mut hasher = Hasher::new(PLACED_DOMAIN);
    hasher.id(&stored.0).update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        hasher.update(&[place_kind(target)]);
    }
    match own {
        Some(at) => hasher.update(&[1]).update(&at.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    InstanceKey(hasher.finish())
}

/// The store key of the dev template of `instance` (its template instance: its code is its
/// template code), compiled as `own` (the instance key its function is compiled under), whose
/// code builds in `built_in` (in the driver's order) and references `refs`, compiled by the
/// codegen of `toolchain` (`Spec::toolchain.id`):
/// `kernel::template(placed(stored(..), refs, own_place(own, refs)), ..)`. The one place the
/// driver and `blaze_global`'s public dev function rule make it, so the two cannot differ.
/// Every function compiled alone, with no imports, is kept under it, whatever its backend: a
/// whole build's Cranelift and stencil code (all but [`crate::Backend::is_release`]'s), a lazy
/// build's first compiles, and a lazy build's tier-up (`Later::tier_up`), LLVM's too. Code
/// compiled with imports is under [`release_template_key`]: a whole build's LLVM code, and a
/// whole live build's tier-up (`Build::tier_up`).
pub fn dev_template_key(
    instance: &Instance,
    own: InstanceKey,
    built_in: &[InstanceKey],
    refs: &[Target],
    toolchain: &ContentId,
) -> Key {
    let placed = placed(stored(instance.key(), built_in), refs, own_place(own, refs));
    kernel::template(&placed, &instance.settings, toolchain)
}

/// The references and the store key of the release compile of `instance` (its template
/// instance), compiled as `own` (the instance key its function is compiled under), whose code
/// builds in `built_in` (in the driver's order) and references `refs`, importing `imports` with
/// `imported_units`, `imported_refs` and `imported_built_in` (each imported function's
/// instance key as its export is compiled, at its own settings, its references and the
/// bodies its export builds in, in [`Imports::imported`]'s order):
/// `keys::release(imports.blob_key(with_imported(laid_out(placed(stored(..), refs, own_place(own, all)), places), ..)))`,
/// `all` and `places` those of [`release_refs`]: its template is made against `all`, so the
/// place of the function itself is among them, an import's call of it too. The one place the
/// driver and `blaze_global`'s public function rule make it, so the two cannot differ.
#[allow(clippy::too_many_arguments)]
pub fn release_template_key(
    instance: &Instance,
    own: InstanceKey,
    built_in: &[InstanceKey],
    refs: &[Target],
    imports: &Imports,
    imported_units: &[InstanceKey],
    imported_refs: &[&[Target]],
    imported_built_in: &[&[InstanceKey]],
) -> (Vec<Target>, Key) {
    let (all, places) = release_refs(refs, imported_refs);
    let placed = placed(stored(instance.key(), built_in), refs, own_place(own, &all));
    let laid = laid_out(placed, &places);
    let key = keys::release(&imports.blob_key(with_imported(
        laid,
        imported_units,
        imported_refs,
        imported_built_in,
    )));
    (all, key)
}

pub const IMPORTED_DOMAIN: &str = "blaze v2 release imported bodies 2";

/// `laid` (a release compile's [`laid_out`] key) with what each import's export is made of:
/// its [`placed`] stored key, of its unit's instance key (`imported_units`), the bodies it
/// builds in and its references. An import's export is its prepared IR at its own unit's
/// settings, which holds the bodies its MIR inliner built in (at `-Copt-level` 2 and 3).
/// [`Imports::imported`] names imports by the walk's key, which in a live build's tier-up is
/// the first tier's, not the settings the export is built at; and an edit of a body an import
/// builds in changes no import's key. Either way [`Imports::blob_key`] alone would give the
/// importer code made with another export (rule 21). All three lists' lengths are hashed:
/// the rule that proves a function public refuses lists that are not one per import.
pub fn with_imported(
    laid: InstanceKey,
    imported_units: &[InstanceKey],
    imported_refs: &[&[Target]],
    imported_built_in: &[&[InstanceKey]],
) -> InstanceKey {
    let mut hasher = Hasher::new(IMPORTED_DOMAIN);
    hasher.id(&laid.0);
    for len in [imported_units.len(), imported_refs.len(), imported_built_in.len()] {
        hasher.update(&(len as u64).to_le_bytes());
    }
    for ((unit, refs), built_in) in imported_units.iter().zip(imported_refs).zip(imported_built_in)
    {
        hasher.id(&placed(stored(*unit, built_in), refs, own_place(*unit, refs)).0);
    }
    InstanceKey(hasher.finish())
}

/// What a reference is at its place, for keys that take any target there.
pub(crate) fn place_kind(target: &Target) -> u8 {
    match target {
        Target::Function(_) => 0,
        Target::Direct(_) => 1,
        Target::Static { .. } => 2,
        Target::Data(_) => 3,
        Target::Extern(_) => 4,
        Target::Weak(_) => 5,
        Target::Position(_) => 6,
        Target::Coverage(_) => 7,
        Target::Counter(_) => 8,
        Target::ProfileCounters => 9,
    }
}

const FOLDED_IDENTITY_DOMAIN: &str = "blaze v2 folded identity 2";
pub const SUBJECT_DOMAIN: &str = "blaze v2 subject 1";

/// What a function is across edits of its body, telling apart instances side by side: its
/// identity ([`Described::identity`]: item, which instance, type arguments) and whose code it
/// is compiled as. The one ID of that concept: the live image's slots
/// ([`Built::identities`]), a layout profile's and perf map's names, a folded group's
/// members, what `blaze why` compares a dev template by, and, with its settings, test
/// selection's ([`crate::exec::fn_id`]).
///
/// The identity alone does not tell instances apart: a generic asked for by a member and by
/// a dependency is one identity, but its key's arguments and code are each asker's (what its
/// origin's units build bodies with and tell a debugger, what its origin builds in), so it can
/// be two keys, each with its own slot (`blaze_radix::Parts<..>::write` in `hir-def`'s tests,
/// arrayvec's `CapacityError<&[u8; 1024]>::new` in `blaze-profile`'s). Not its settings, so a
/// settings change is named as one ("its codegen settings") rather than read as a first
/// compute, and a live slot keeps its function across one.
pub fn subject(identity: ContentId, origin: Origin) -> ContentId {
    let mut hasher = Hasher::new(SUBJECT_DOMAIN);
    hasher.id(&identity);
    origin.hash_into(&mut hasher);
    hasher.finish()
}
/// The domain of a dev template's identity as a step `blaze why` records (`FnNode::missed`).
const TEMPLATE_STEP_DOMAIN: &str = "blaze v2 why dev template step 1";
pub const RELATIVE_FLIGHT_DOMAIN: &str = "blaze v2 relative flight 1";
pub const GROUP_FLIGHT_DOMAIN: &str = "blaze v2 group flight 2";

/// What a folded group's merge flies under ([`crate::flight`]): its key, and each member with
/// what its code is kept under ([`Group::stored`]), so the keys of its body and of every
/// thunk (`keys::thunk(key, stored)`) are its flight's. A landing is the memo holding exactly
/// those, and a member edited in a body it builds in, its instance key and the group's key
/// the same, asks for another thunk, so flies another merge.
fn group_flight(group: &Group) -> InstanceKey {
    let mut hasher = Hasher::new(GROUP_FLIGHT_DOMAIN);
    hasher.id(&group.key().0).update(&(group.members.len() as u64).to_le_bytes());
    for (member, stored) in group.members.iter().zip(&group.stored) {
        hasher.id(&member.0).id(&stored.0);
    }
    InstanceKey(hasher.finish())
}

impl<I, S> FnNode<I, S> {
    /// What its code is kept under.
    fn stored(&self) -> InstanceKey {
        let own = own_place(self.unit.instance.key(), &self.refs);
        placed(stored(self.unit.instance.key(), &self.built_in), &self.refs, own)
    }

    /// What its dev compile flies under ([`crate::flight`]): [`Self::stored`] and whether its
    /// template is relative to its own anchors ([`Described::anchors`]). One code is relative
    /// for one requester and absolute for another (a report or profile names it for one
    /// origin, or one origin's inliner builds bodies in), so two builds of the memo can compile
    /// one stored key in the two forms. A landing hands its template to every build that flew
    /// the key, which keeps it under its own template key: a relative build given an absolute
    /// template would keep another function's positions under a key with no name or place in
    /// it (rule 21), as [`Shape::relative`] keeps the two forms apart in the memo.
    fn flight(&self) -> InstanceKey {
        let stored = self.stored();
        if self.described.anchors.is_none() {
            return stored;
        }
        let mut hasher = Hasher::new(RELATIVE_FLIGHT_DOMAIN);
        hasher.id(&stored.0);
        InstanceKey(hasher.finish())
    }

    /// The store key of its dev template: the kernel's entry of its stored key, of its
    /// [`Self::template_instance`].
    fn template_key(&self) -> Key {
        dev_template_key(
            &self.template_instance(),
            self.unit.instance.key(),
            &self.built_in,
            &self.refs,
            &self.spec.toolchain.id,
        )
    }

    /// Its instance as its template is kept: its code is its template code
    /// ([`Described::template_code`]), which holds no name and no place where its template
    /// holds its own positions by table (`Described::anchors`).
    fn template_instance(&self) -> Instance {
        Instance { code: self.described.template_code, ..self.unit.instance }
    }

    /// What it is across edits of its body, telling apart instances side by side
    /// ([`subject`]), of its least origin ([`FnNode::least_origin`]).
    fn subject(&self) -> ContentId {
        subject(self.described.identity, self.least_origin)
    }

    /// Why its dev template under `at` ([`Self::template_key`]) was not in the store, as its
    /// key's parts by name, for `blaze why` (`blaze_kernel::why`): each body it builds in by
    /// path (with whose code it is where two share a path, and its subject, which edits of its
    /// body leave as it is, where two share both: two instances of one generic, `<[u8]>::as_ptr`
    /// and `<[u16]>::as_ptr`, are one path of one origin), so a callee whose code changed is
    /// named, and no two parts share a name (which `blaze_kernel::Miss::of_parts` refuses). Made
    /// on a miss only, and only when asked ([`Build::explain_misses`]).
    fn missed(&self, at: Key, nodes: &Sorted<InstanceKey, FnNode<I, S>>) -> blaze_kernel::Miss {
        use blaze_kernel::Part;
        let instance = self.template_instance();
        // alloc: once a miss, a part each part of its key
        let mut parts = vec![
            Part::new("the toolchain", Some(self.spec.toolchain.id)),
            // The front end's results, of no step blaze why records: a chain follows the
            // unit that built it (`blaze_kernel::Miss::via`), whose key covers their sources.
            Part::result("its code", Some(instance.code.0), None),
            Part::result("its type arguments' facts", Some(instance.args), None),
            Part::new("its codegen settings", Some(instance.settings.0)),
            Part::result(
                "the kinds of its references",
                Some(
                    placed(
                        InstanceKey(ContentId::default()),
                        &self.refs,
                        own_place(self.unit.instance.key(), &self.refs),
                    )
                    .0,
                ),
                None,
            ),
        ];
        // alloc: once a miss
        let callees: Vec<(InstanceKey, Option<&Self>, Option<&str>)> = self
            .built_in
            .iter()
            .map(|it| {
                let node = nodes.get(it);
                (*it, node, node.map(|node| node.described.name.as_str()))
            })
            .collect();
        for (key, node, path) in &callees {
            let shared = callees.iter().filter(|(_, _, other)| other == path).count() > 1;
            let origin = node.map(|it| it.described.origin);
            let same_origin = shared
                && callees
                    .iter()
                    .filter(|(_, other, it)| {
                        it == path && other.map(|it| it.described.origin) == origin
                    })
                    .count()
                    > 1;
            let name = match (*node, shared, same_origin) {
                // alloc: once a miss
                (Some(node), false, _) => {
                    format!("the code of `{}`, built in", node.described.name)
                }
                // alloc: once a miss
                (Some(node), true, false) => format!(
                    "the code of `{}` as {:?}'s, built in",
                    node.described.name, node.described.origin
                ),
                // alloc: once a miss
                (Some(node), true, true) => format!(
                    "the code of `{}` as {:?}'s, instance {}, built in",
                    node.described.name,
                    node.described.origin,
                    node.subject()
                ),
                // No subject without its node: told apart by its key.
                // alloc: once a miss
                (None, _, _) => {
                    format!("the code of a callee built in that its build did not walk, {}", key.0)
                }
            };
            let names = node.map(|it| it.described.name.clone()); // clone: once a miss
            parts.push(Part::result(name, Some(key.0), names));
        }
        // The step's identity: the function and its settings, and nothing of the unit that
        // asked for it. The template store is shared by every unit, so which unit's build
        // missed is no part of what the template is; one function built at two settings in
        // one build is two steps.
        let step = Hasher::new(TEMPLATE_STEP_DOMAIN)
            .id(&self.subject())
            .id(&instance.settings.0)
            .finish();
        blaze_kernel::Miss::of_parts(
            <kernel::DevTemplate as blaze_kernel::Step>::NAME,
            step,
            self.described.name.clone(), // clone: once a miss
            at.0,
            parts,
        )
    }

    /// Whether its callers reach it through its slot for an edit's sake: an edit changes its
    /// body ([`Self::patchable`]) or fills it again under its key ([`Self::rebound`]).
    fn slotted(&self) -> bool {
        self.patchable || self.rebound
    }

    /// Whether code outside the build may call it, so nothing is assumed of its callers; and
    /// it is never merged with another function, so it keeps a slot of its own and its
    /// callers' fills name its own key whatever an edit does to code equal to it.
    fn external(&self) -> bool {
        self.slotted() || self.then.is_some() || self.described.address == Address::Significant
    }
}

/// A static of a walk, over the front end's instance `I` and static `S`.
// clone: a chunk of the walk's statics a kept walk shares, copied by the build that patches it
#[derive(Clone)]
struct StaticNode<I, S> {
    value: S,
    described: DescribedStatic<I, S>,
    spec: Spec,
    refs: Vec<Target>,
}

type StaticOf<B> = StaticNode<FrontInstance<B>, FrontStatic<B>>;

/// Which static of a build: its item and which instance of it.
pub type StaticId = (ItemKey, ContentId);

/// A walk's nodes in key order ([`rows`]): found by key, changed one chunk at a time, and
/// shared between the build that walked and the walk kept for the next.
type Sorted<K, V> = rows::Rows<K, V>;

/// One compile of a release phase: `name` is the key callers' places and summaries name it
/// by, `unit` what is compiled (another key for a function tiered up).
struct Job<'n, B: Prepare> {
    name: InstanceKey,
    unit: Unit,
    spec: &'n Spec,
    node: &'n Node<B>,
}

/// A body `function` built in that none of `keys` is: code its store key does not hold. Each
/// held body holds its [`Described::held_codes`]: an import of it is its own code, a copy built
/// into a caller is its built-in code.
fn unkept_inline<I, S>(
    nodes: &Sorted<InstanceKey, FnNode<I, S>>,
    function: &CompiledFunction,
    keys: impl IntoIterator<Item = InstanceKey>,
) -> Option<blaze_items::CodeHash> {
    if function.inlined.is_empty() {
        return None;
    }
    let kept: HashSet<blaze_items::CodeHash> =
        // alloc: per compile that builds code in, sized by what it builds in
        keys.into_iter().filter_map(|it| nodes.get(&it)).flat_map(|it| it.described.held_codes()).collect();
    function.inlined.iter().find(|it| !kept.contains(*it)).copied()
}

/// The refusal of `node`'s compile, which built in the code `it` that its template's key
/// `under`, of the bodies `kept`, does not hold ([`unkept_inline`]): the function, the code by
/// the names of the build's instances of it, the key, and every body it holds.
fn unkept_message<I, S>(
    nodes: &Sorted<InstanceKey, FnNode<I, S>>,
    node: &FnNode<I, S>,
    it: blaze_items::CodeHash,
    kept: &[InstanceKey],
    under: &Key,
) -> String {
    let name = |key: &InstanceKey| {
        nodes.get(key).map_or("an instance of no node", |it| &it.described.name)
    };
    let of_it: Vec<&str> = (nodes.iter())
        .filter(|(_, it_node)| it_node.described.held_codes().contains(&it))
        .map(|(_, it_node)| &*it_node.described.name)
        .take(3)
        .collect(); // alloc: an error
    let held: Vec<&str> = kept.iter().map(name).collect(); // alloc: an error
    let of_it = match of_it.is_empty() {
        true => "no instance of the build".to_owned(), // alloc: an error
        false => format!("`{}`", of_it.join("`, `")),  // alloc: an error
    };
    // alloc: an error
    format!(
        "`{}` built in the code {it:?} ({of_it}), which its template's key {under:?} does not \
         hold: the key holds the code of the {} bodies [{}]",
        node.described.name,
        held.len(),
        held.join(", "),
    )
}

/// What a release compile of `job` importing `imports` may build in, all of which its key
/// holds ([`with_imported`]): its own bodies, its imports, and the bodies each import's
/// export builds in; sorted, each once.
fn release_kept<B: Prepare>(
    job: &Job<'_, B>,
    by_name: &HashMap<InstanceKey, &Job<'_, B>>,
    imports: &Imports,
) -> Vec<InstanceKey> {
    let mut kept: Vec<InstanceKey> = (job.node.built_in.iter().copied())
        .chain(imports.imported.iter().copied())
        .chain(imports.imported.iter().flat_map(|it| by_name[it].node.built_in.iter().copied()))
        // alloc: per release compile, sized by what it may build in
        .collect();
    kept.sort_unstable();
    kept.dedup();
    kept
}

impl<B: Prepare> Job<'_, B> {
    fn stored(&self) -> InstanceKey {
        let own = own_place(self.unit.instance.key(), &self.node.refs);
        placed(stored(self.unit.instance.key(), &self.node.built_in), &self.node.refs, own)
    }

    /// What its summary is kept and flown under: [`Self::stored`] and its callees.
    fn summarized(&self) -> InstanceKey {
        summarized(self.stored(), &self.node.refs)
    }
}

pub const SUMMARIZED_DOMAIN: &str = "blaze v2 summarized 1";

/// What a summary of `stored` with `refs` is kept under. A summary names callees by key, and a
/// unit's own key takes them by their interface: one body with other callees is another
/// summary. Each reference counts by position, a callee by its key.
fn summarized(stored: InstanceKey, refs: &[Target]) -> InstanceKey {
    let mut hasher = Hasher::new(SUMMARIZED_DOMAIN);
    hasher.id(&stored.0).update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        match callee(target) {
            Some(key) => hasher.update(&[1]).id(&key.0),
            None => hasher.update(&[0]),
        };
    }
    InstanceKey(hasher.finish())
}

fn error(unit: InstanceKey, item: ItemKey, what: impl Into<String>) -> CodegenError {
    CodegenError { unit, item, what: what.into(), front: None }
}

/// [`Prepare::prepare`] of `node` as `unit`, its shape [`Shape::relative`] when its template
/// is relative to its own anchors ([`Described::anchors`]). Every prepare of a function goes
/// through here, so the memo, its flights and a lazy build's recorded shapes all key one form.
fn prepare_node<B: Prepare>(
    backend: &B,
    frontend: &B::Frontend,
    unit: &Unit,
    spec: &Spec,
    node: &Node<B>,
) -> Result<(Shape, B::Prepared), CodegenError> {
    let (shape, made) = backend.prepare(frontend, unit, spec, &node.instance, &node.refs)?;
    let own = own_place(unit.instance.key(), &node.refs);
    Ok((shape.relative(node.described.anchors.is_some()).own_place(own), made))
}

/// `why`, the refusal of the function called `name`, with its message naming it where it does
/// not already (rule 19): a backend's error knows the unit's keys, not the function's path, so
/// "LLVM has no code for it: opcode `swizzle`" alone says nothing of which function.
fn naming(why: CodegenError, name: &str) -> CodegenError {
    if why.what.contains(name) {
        return why;
    }
    // alloc: an error
    CodegenError { what: format!("`{name}`: {}", why.what), ..why }
}

/// The refusals of symbols each defined by more than one function or static of a build (rule
/// 18), one error a symbol, in order of symbol: the linker's table of names holds one target a
/// name, and took whichever came last, so the program called one or the other by name. Two
/// panic runtimes' `__rust_panic_cleanup` did that (#1611). Each names the symbol and every
/// definition. Read from the walk, before anything is compiled.
fn symbols_defined_twice<B: Prepare>(
    nodes: &Sorted<InstanceKey, Node<B>>,
    statics: &Sorted<StaticId, StaticOf<B>>,
    errors: &mut Vec<CodegenError>,
) {
    // The definer's unit and item, and its name for the message. A static's unit is its value's
    // key, as the owners' check has it.
    let functions = nodes.iter().filter_map(|(key, node)| {
        Some((
            node.described.symbol.as_deref()?,
            node.described.name.as_str(),
            *key,
            node.unit.item,
        ))
    });
    let statics = statics.iter().filter_map(|((item, _), node)| {
        Some((
            node.described.symbol.as_deref()?,
            node.described.name.as_str(),
            InstanceKey(node.described.value),
            *item,
        ))
    });
    // By symbol, then definer's name: the message lists them in order of name (rule 25).
    // alloc: per build, its symbols, a few hundred
    let mut named: Vec<(&str, &str, InstanceKey, ItemKey)> = functions.chain(statics).collect();
    named.sort_unstable();
    for one in named.chunk_by(|a, b| a.0 == b.0).filter(|it| it.len() > 1) {
        let (symbol, unit, item) = (one[0].0, one[0].2, one[0].3);
        // alloc: the error's message
        let by: Vec<String> = one.iter().map(|it| format!("`{}`", it.1)).collect();
        let Some((last, rest)) = by.split_last() else { continue };
        let times = match one.len() {
            2 => "twice".to_owned(),   // alloc: the error's message
            n => format!("{n} times"), // alloc: the error's message
        };
        // alloc: the error's message
        let rest = rest.join(", by ");
        // alloc: the error's message
        let what = format!(
            "the symbol `{symbol}` is defined {times} in one program, by {rest} and by {last}"
        );
        errors.push(error(unit, item, what));
    }
}

/// The exports a release build's compiles need of imports its summaries left none of
/// ([`Build::release`]): each made once of the build's compiles, the others that need it
/// waiting for that one, a wait naming the function and the job making it; with the IR it was
/// made from, kept until the function's own compile takes it.
struct Exports<B: Prepare> {
    /// Each made, by function: `None` for one that failed, its error the maker's.
    made: blaze_hash::SharedByHash<InstanceKey, Option<Arc<B::Export>>>,
    /// The making under way of each, by function.
    making: blaze_graph::pump::Flights<()>,
    /// The IR each was made from, until that function's own compile takes it.
    spare: blaze_core::lock::Lock<HashMap<InstanceKey, B::Prepared>>,
}

impl<B: Prepare> Default for Exports<B> {
    fn default() -> Self {
        Exports {
            made: blaze_hash::SharedByHash::default(),
            making: blaze_graph::pump::Flights::default(),
            // alloc: per release build, the IR its exports were made from
            spare: blaze_core::lock::Lock::new("codegen.driver.export_spare", HashMap::new()),
        }
    }
}

impl<B: Prepare> Exports<B> {
    /// The export of `name`, made here unless another compile of the build makes it now,
    /// which this one then waits for; `None` when making it failed, the error in `errors` of
    /// the compile that made it.
    fn export(
        &self,
        build: &Build<'_, B>,
        by_name: &HashMap<InstanceKey, &Job<'_, B>>,
        name: InstanceKey,
        errors: &mut Vec<CodegenError>,
    ) -> Option<Arc<B::Export>> {
        if let Some(made) = self.made.pin().get(&name) {
            return made.clone();
        }
        let job = by_name[&name];
        let named = |it: &mut String| {
            use std::fmt::Write as _;
            _ = write!(
                it,
                "codegen: the export of `{}` {name:?}, which another compile of this build makes",
                job.node.described.name
            );
        };
        let ran = self.making.get_or_run_named(blaze_graph::Key(name.0.0), named, || {
            // Made by a run that ended since the look above.
            if self.made.pin().contains_key(&name) {
                return Ok(());
            }
            let made = (prepare_node(build.backend, build.frontend, &job.unit, job.spec, job.node))
                .and_then(|(_, mut made)| {
                    let (_, export) =
                        build.backend.summarize(&job.unit, job.spec, &mut made, &job.node.refs)?;
                    Ok((export, made))
                });
            match made {
                Ok((export, made)) => {
                    self.spare.lock().insert(name, made);
                    // alloc: per export made, shared by the compiles that import it
                    self.made.pin().insert(name, Some(Arc::new(export)));
                }
                Err(why) => {
                    errors.push(why);
                    self.made.pin().insert(name, None);
                }
            }
            Ok(())
        });
        if ran.is_err() {
            // alloc: a give-way's payload, once per task that gives way
            std::panic::resume_unwind(Box::new(blaze_graph::Cancelled));
        }
        self.made.pin().get(&name).cloned().flatten()
    }
}

/// What became of one item a build flew ([`Build::fly`]).
enum Flown<T, R> {
    /// This build ran it: what its run made.
    Ran(R),
    /// Another build's run landed it under this shape: the item, unrun here.
    Landed(T, ContentId),
}

/// A store read: what is there, nothing, or what is there and can't be read, which is an
/// error of the build already recorded and never a reason to compile again.
enum Read<T> {
    Hit(T),
    Miss,
    Failed,
}

/// Who put a value: the unit whose code it is and what of the walk it is (`of`), for the
/// refusal of another put of other bytes under its key. Ordered, so a build's puts are sorted
/// by key and then putter, whatever order they were pushed in (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct By {
    unit: InstanceKey,
    item: ItemKey,
    of: Of,
}

/// What of a walk put a value, by its key in the walk: what [`Out::finish`] names it by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Of {
    Function(InstanceKey),
    Static(StaticId),
    /// The template the memo kept under a store key, compiled earlier (by this build or
    /// another of the daemon's) for the same key ([`Memo::put_keyed`]).
    Memo,
    /// The folded body of the group whose first member is this function.
    Folded(InstanceKey),
}

impl By {
    /// The function `node` of the walk, compiled as `unit`.
    fn node(node: InstanceKey, unit: &Unit) -> By {
        By { unit: unit.instance.key(), item: unit.item, of: Of::Function(node) }
    }

    /// The static `id` of the walk, compiled as `unit`.
    fn static_(id: StaticId, unit: &Unit) -> By {
        By { unit: unit.instance.key(), item: unit.item, of: Of::Static(id) }
    }

    /// The folded body of the group whose first member is `first`, compiled as its `unit`.
    fn folded(first: InstanceKey, unit: &Unit) -> By {
        By { unit: unit.instance.key(), item: unit.item, of: Of::Folded(first) }
    }

    /// The memo's template under the store key `unit` compiled for, met by `unit`'s compile.
    fn memo(unit: &Unit) -> By {
        By { unit: unit.instance.key(), item: unit.item, of: Of::Memo }
    }
}

/// What a build collects as it goes.
#[derive(Default)]
struct Out {
    functions: Vec<CompiledFunction>,
    statics: Vec<Static>,
    /// Each with who put it, in the order put.
    puts: Vec<(By, Put)>,
    errors: Vec<CodegenError>,
    stats: Stats,
    /// What the profile of the functions pushed did to them: the build's one profile, which
    /// [`Out::profiled`] files under its ID.
    profile: ProfileCounts,
    /// The first function pushed built with a profile, by key and item, which
    /// [`Out::profiled`]'s error names.
    first_profiled: Option<(InstanceKey, ItemKey)>,
    /// Of each function pushed, by key ([`digests::function_digest`]).
    function_digests: Vec<(InstanceKey, ContentId)>,
    /// Of each static pushed, by item and instance ([`digests::static_definition`]).
    static_digests: Vec<(StaticId, ContentId)>,
    /// What it took and put in the memo ([`Built::memo_used`]).
    used: Vec<ContentId>,
    /// The dev templates it took from the store or made ([`Built::kernel`]).
    kernel: Vec<KernelEntry>,
    /// Why each dev template the store missed was not in it ([`Built::misses`]).
    misses: Vec<blaze_kernel::Miss>,
    /// Each folded group's body by its key, with its identity ([`folded_identity`]): a body
    /// is no node, and the address map names it too ([`Built::identities`]).
    folded: Vec<(InstanceKey, ContentId)>,
}

impl Out {
    /// What `piece` collected, one item of a loop on all cores that collected into its own,
    /// after what this one has: absorbed in the loop's order, it is what collecting into this
    /// one item after item would have made.
    fn absorb(&mut self, piece: Out) {
        let Out {
            functions,
            statics,
            puts,
            errors,
            stats,
            profile,
            first_profiled,
            function_digests,
            static_digests,
            used,
            kernel,
            misses,
            folded,
        } = piece;
        // alloc: per build, each grown once per item, sized by what it made
        self.functions.extend(functions);
        self.statics.extend(statics);
        self.puts.extend(puts);
        self.errors.extend(errors);
        self.stats.add(stats);
        self.profile = self.profile.plus(profile);
        if self.first_profiled.is_none() {
            self.first_profiled = first_profiled;
        }
        self.function_digests.extend(function_digests);
        self.static_digests.extend(static_digests);
        self.used.extend(used);
        self.kernel.extend(kernel);
        self.misses.extend(misses);
        self.folded.extend(folded);
    }

    /// A memo entry taken or put.
    fn used(&mut self, id: ContentId) {
        self.used.push(id);
    }

    /// A function of `item` into the build's functions, made from `code`, counted by what its
    /// profile says, with its digest.
    fn push(&mut self, code: CodeHash, item: ItemKey, function: CompiledFunction) {
        if let Some(profiled @ crate::Profiled::Used { .. }) = function.profile {
            self.first_profiled.get_or_insert((function.key, item));
            self.profile = self.profile.plus(ProfileCounts {
                used: 1,
                mismatched: u64::from(profiled.mismatched()),
                applied: u64::from(profiled.applied()),
            });
        }
        self.function_digests
            .push((function.key, digests::function_digest(code, &function.inlined)));
        self.functions.push(function);
    }

    /// The counts of the functions pushed so far filed under the build's profile, `pgo`
    /// (every function of a build has its policy's: `Policy::choose`). A function built with
    /// a profile in a build that uses none is an error naming how many and the first.
    fn profiled(&mut self, pgo: crate::settings::Pgo) {
        let counts = std::mem::take(&mut self.profile);
        let first = self.first_profiled.take();
        if counts.used == 0 {
            return;
        }
        match pgo {
            crate::settings::Pgo::Use(id) => {
                // alloc: once per build that uses a profile
                let at = self.stats.profiles.entry(id).or_default();
                *at = at.plus(counts);
            }
            other => {
                let (key, item) = first
                    .unwrap_or((InstanceKey(ContentId::default()), ItemKey(ContentId::default())));
                // alloc: an error, once
                let what = format!(
                    "{} functions were built with a profile (`Profiled::Used`) in a build whose policy \
                     uses none ({other:?}), the first instance {:x} of item {:x}",
                    counts.used, key.0, item.0
                );
                self.errors.push(error(key, item, what));
            }
        }
    }

    /// A static into the build's statics, with its digest made where it was made.
    fn push_static(&mut self, value: Static, digest: ContentId) {
        self.static_digests.push(((value.item, value.instance), digest));
        self.statics.push(value);
    }

    fn put(&mut self, by: By, key: Key, kind: Kind, bytes: Vec<u8>) {
        self.puts.push((by, Put { key, kind, bytes }));
    }

    /// A template that is no dev template of a function (a release compile, a folded body, a
    /// thunk), under `key`: put, and no kernel entry.
    fn template(&mut self, by: By, key: Key, template: &Template) {
        self.put(by, key, Kind::Code, blob::encode(template));
    }

    /// `template`, compiled by `by` (as `unit`), into the memo under `key`, a store key (a
    /// folded body's, a thunk's, a release compile's), and put: what is kept, to instantiate.
    /// When the memo already keeps another template under `key`, compiled earlier, neither is
    /// instantiated: both are put, so [`Out::finish`] refuses the build naming both and their
    /// content IDs (rule 21). Two templates of equal bytes are one.
    fn keyed<'m>(
        &mut self,
        memo: &'m Memo,
        by: By,
        unit: &Unit,
        key: Key,
        template: Template,
    ) -> Option<&'m Template> {
        match memo.put_keyed(key, template) {
            Ok(kept) => {
                self.template(by, key, kept);
                Some(kept)
            }
            Err(refused) => {
                let (kept, given) = (blob::encode(refused.kept), blob::encode(&refused.given));
                if kept == given {
                    self.put(by, key, Kind::Code, given);
                    return Some(refused.kept);
                }
                self.put(By::memo(unit), key, Kind::Code, kept);
                self.put(by, key, Kind::Code, given);
                None
            }
        }
    }

    /// The dev template of the walk's function `node` (as `unit`), made by this build (compiled,
    /// from the memo or a flight) under `key`: put, and its kernel entry recorded as made. A
    /// store hit records its entry where it is read (`hit: true`), so a cold and a warm build of
    /// one tree record the same entries.
    fn dev_template(&mut self, node: InstanceKey, unit: &Unit, key: Key, template: &Template) {
        self.kernel.push(KernelEntry {
            instance: node,
            step: blaze_kernel::steps::DEV_TEMPLATE,
            key,
            hit: false,
        });
        self.template(By::node(node, unit), key, template);
    }

    /// A compile's side table under its content ID: its bytes move to the put.
    fn side(&mut self, by: By, emitted: &mut Emitted) {
        match (emitted.side.take(), emitted.function.side) {
            (Some(bytes), Some(id)) => self.put(by, keys::side(&id), Kind::Debug, bytes),
            (None, None) => {}
            _ => self.errors.push(error(
                by.unit,
                by.item,
                "the compile's side table and the function's record of it disagree",
            )),
        }
    }

    /// Reads one value in place, or records why it can't.
    fn read<T>(
        &mut self,
        store: &impl Lookup,
        key: &Key,
        unit: &Unit,
        decode: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Read<T> {
        match store.read(key) {
            Ok(Some(bytes)) => match decode(bytes) {
                Ok(it) => Read::Hit(it),
                Err(why) => {
                    // alloc: an error
                    self.errors.push(error(
                        unit.instance.key(),
                        unit.item,
                        format!("the store's {key:?}: {why}"),
                    ));
                    Read::Failed
                }
            },
            Ok(None) => Read::Miss,
            Err(why) => {
                // alloc: an error
                self.errors.push(error(
                    unit.instance.key(),
                    unit.item,
                    format!("reading the store's {key:?}: {why}"),
                ));
                Read::Failed
            }
        }
    }

    /// The store check of the hit under `at` of function `name` ([`Build::verify_store`]),
    /// `recomputed` its template encoded again: counted, and bytes other than the stored are
    /// the unit's error.
    fn verify(&mut self, unit: &Unit, name: &str, store: &impl Lookup, at: Key, recomputed: &[u8]) {
        self.stats.verified += 1;
        let refused = |why: String| error(unit.instance.key(), unit.item, why);
        let stored = match store.read(&at) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // alloc: an error
                let why = format!(
                    "{}: `{name}`'s template {at:?}, a hit of this build, is no longer in the store",
                    blaze_store_check::VAR
                );
                self.errors.push(refused(why));
                return;
            }
            Err(why) => {
                // alloc: an error
                self.errors.push(refused(format!(
                    "{}: reading `{name}`'s template {at:?} again: {why}",
                    blaze_store_check::VAR
                )));
                return;
            }
        };
        let hit = blaze_store_check::Hit { function: name, identity: at.0, reads: &[], stored };
        if let Err(mismatch) = blaze_store_check::compare(&hit, recomputed) {
            self.errors.push(refused(mismatch.to_string()));
        }
    }

    /// The function of `template`, from `whence`, for `key` (called `name`) with `refs` and its
    /// own span tables anchored at `anchors`, into the build's functions.
    fn instantiate(
        &mut self,
        unit: &Unit,
        name: &str,
        key: InstanceKey,
        whence: Whence,
        template: &Template,
        refs: &[Target],
        anchors: OwnAnchors<'_>,
    ) {
        self.made(unit, name, key, whence, template.instantiate_at(key, refs, anchors));
    }

    /// A function made for `key` (called `name`) of the template from `whence`, into the
    /// build's functions, or why it could not be made.
    fn made(
        &mut self,
        unit: &Unit,
        name: &str,
        key: InstanceKey,
        whence: Whence,
        made: Result<CompiledFunction, TemplateError>,
    ) {
        match made {
            Ok(function) => self.push(unit.instance.code, unit.item, function),
            Err(why) => self.errors.push(unfit(key, unit.item, name, whence, &why)),
        }
    }

    /// A compile's template against `refs`, its own positions by table where its instance's
    /// span tables are anchored at `anchors` ([`crate::template::own_anchor`]), taking the
    /// compile's function over, or the error naming why it has none.
    fn template_of(
        &mut self,
        unit: &Unit,
        emitted: Emitted,
        refs: &[Target],
        anchors: OwnAnchors<'_>,
    ) -> Option<Template> {
        let key = emitted.function.key;
        match Template::of_owned_at(emitted.function, refs, &emitted.extra, anchors) {
            Ok(it) => Some(it),
            Err(why) => {
                // alloc: an error
                self.errors.push(error(key, unit.item, why.to_string()));
                None
            }
        }
    }

    /// What the build collected, sorted and each once. Two puts under one key are one when
    /// their kind and bytes are equal; of another kind or other bytes, the build is refused
    /// as the store would refuse the second ([`blaze_cas::Mismatch`], rule 21), with an error
    /// naming both putters (`name_of` gives what of the walk is called), the key, both kinds
    /// and both content IDs.
    fn finish<'n>(mut self, name_of: impl Fn(Of) -> Option<&'n str>) -> Finished {
        // By key, then putter, kind and bytes: which of one key's puts comes first is the
        // same whatever order the build pushed them in (rule 25).
        self.puts.sort_unstable_by(|a, b| {
            (a.1.key, a.0, a.1.kind)
                .cmp(&(b.1.key, b.0, b.1.kind))
                .then_with(|| a.1.bytes.cmp(&b.1.bytes))
        });
        // alloc: per build, what it gives the caller to put, sized by its puts
        let mut puts: Vec<Put> = Vec::with_capacity(self.puts.len());
        let mut kept_by: Option<By> = None;
        for (by, put) in std::mem::take(&mut self.puts) {
            match (puts.last(), kept_by) {
                (Some(kept), Some(first)) if kept.key == put.key => {
                    if kept.kind != put.kind || kept.bytes != put.bytes {
                        let mismatch = blaze_cas::Mismatch {
                            key: put.key,
                            kept: blaze_cas::content_of(&kept.bytes),
                            given: blaze_cas::content_of(&put.bytes),
                        };
                        let kinds = (kept.kind, put.kind);
                        let differ = template_difference(kinds, &kept.bytes, &put.bytes);
                        self.errors.push(put_twice(first, by, kinds, &mismatch, &differ, &name_of));
                    }
                }
                _ => {
                    kept_by = Some(by);
                    puts.push(put);
                }
            }
        }
        self.errors.sort_by_key(|it| (it.unit, it.item));
        self.errors.dedup();
        self.used.sort_unstable();
        self.used.dedup();
        self.kernel.sort_unstable();
        self.kernel.dedup();
        self.misses.sort_unstable();
        self.misses.dedup();
        Finished {
            functions: self.functions,
            statics: self.statics,
            puts,
            stats: self.stats,
            errors: self.errors,
            used: self.used,
            kernel: self.kernel,
            misses: self.misses,
        }
    }
}

/// What each function and static of a walk is called, by its key in it, for
/// [`Out::finish`]'s errors: a function's debug name, a static's path.
fn names_of<'n, I, S, T>(
    nodes: &'n Sorted<InstanceKey, FnNode<I, S>>,
    statics: Option<&'n Sorted<StaticId, StaticOf<T>>>,
) -> impl Fn(Of) -> Option<&'n str> + 'n
where
    T: Prepare,
{
    move |of| match of {
        Of::Function(key) | Of::Folded(key) => nodes.get(&key).map(|it| it.described.name.as_str()),
        Of::Static(id) => statics?.get(&id).map(|it| it.described.name.as_str()),
        Of::Memo => None,
    }
}

/// A second put under `mismatch.key` in one build, by `then`, of another kind or other bytes
/// than the first, by `first`: the build is refused, as the store refuses a
/// [`blaze_cas::Mismatch`], with an error naming both putters.
fn put_twice<'n>(
    first: By,
    then: By,
    (kept_kind, given_kind): (Kind, Kind),
    mismatch: &blaze_cas::Mismatch,
    differ: &str,
    name_of: &impl Fn(Of) -> Option<&'n str>,
) -> CodegenError {
    let who = |by: By| {
        let what = match by.of {
            Of::Function(_) => "the function",
            Of::Folded(_) => "the folded body of the group of the function",
            Of::Static(_) => "the static",
            Of::Memo => {
                return "the template the memo already kept under it (compiled earlier, by this build or another of the daemon's)".to_owned(); // alloc: an error
            }
        };
        match name_of(by.of) {
            // alloc: an error
            Some(name) => format!("{what} `{name}` (instance {:?}, item {:?})", by.unit, by.item),
            // alloc: an error
            None => format!(
                "{what} of instance {:?} (item {:?}), which the walk has no name of",
                by.unit, by.item
            ),
        }
    };
    let (key, kept, given) = (mismatch.key, mismatch.kept, mismatch.given);
    // The store compares content alone (a `Mismatch`): two kinds of one content would both be
    // taken there, and are refused here as the key's collision.
    let why = match (kept_kind != given_kind, kept != given) {
        (true, true) => "two kinds collide on one key, and the store would refuse the second as",
        (true, false) => "two kinds collide on one key, of one content",
        (false, _) => "the store would refuse the second as",
    };
    // alloc: an error
    let refusal = if kept == given { String::new() } else { format!(" {mismatch:?}") };
    // alloc: an error
    let what = format!(
        "this build is refused, and neither value is put: {} and {} put two values under one store key {key:?}, a {kept_kind:?} value of content {kept:?} and a {given_kind:?} value of content {given:?}, so the key misses an input of its value (rule 21: {why}{refusal}){differ}",
        who(first),
        who(then),
    );
    error(then.unit, then.item, what)
}

/// Where two templates put under one key differ, for [`put_twice`]'s error: the first field
/// of [`Template`] whose values are not equal, with both values (each cut to its first
/// differing entry for a list), so the error names the input the key misses. Empty when either
/// is no template or the two are of other kinds.
fn template_difference((kept_kind, given_kind): (Kind, Kind), kept: &[u8], given: &[u8]) -> String {
    if kept_kind != Kind::Code || given_kind != Kind::Code {
        return String::new();
    }
    let (Ok(kept), Ok(given)) = (blob::decode(kept), blob::decode(given)) else {
        return String::new();
    };
    // The first index at which two lists differ, and the entry each has there.
    fn first<T: std::fmt::Debug + PartialEq>(field: &str, a: &[T], b: &[T]) -> Option<String> {
        if a == b {
            return None;
        }
        let at = a.iter().zip(b).position(|(a, b)| a != b).unwrap_or(a.len().min(b.len()));
        // alloc: an error
        Some(format!(
            "; they differ in `{field}` (lengths {} and {}) first at {at}: {:?} and {:?}",
            a.len(),
            b.len(),
            a.get(at),
            b.get(at)
        ))
    }
    fn whole<T: std::fmt::Debug + PartialEq>(field: &str, a: &T, b: &T) -> Option<String> {
        // alloc: an error
        (a != b).then(|| format!("; they differ in `{field}`: {a:?} and {b:?}"))
    }
    let differ = [
        whole("arch", &kept.arch, &given.arch),
        whole("align", &kept.align, &given.align),
        first("code", &kept.code, &given.code),
        first("relocation rows", kept.relocs.rows(), given.relocs.rows()),
        first("relocation targets", kept.relocs.targets(), given.relocs.targets()),
        (kept.relocs.rows() == given.relocs.rows()
            && kept.relocs.targets() == given.relocs.targets()
            && kept.relocs != given.relocs)
            .then(|| "; they differ in their relocations' wide addends".to_owned()), // alloc: an error
        first("data", &kept.data, &given.data),
        first("inlined", &kept.inlined, &given.inlined),
        whole("side", &kept.side, &given.side),
        whole("unwind", &kept.unwind, &given.unwind),
        first("lines", &kept.lines, &given.lines),
        whole("refs", &kept.refs, &given.refs),
        first("externs", &kept.externs, &given.externs),
        whole("coverage", &kept.coverage, &given.coverage),
        whole("profile", &kept.profile, &given.profile),
    ];
    // alloc: an error
    differ.into_iter().flatten().collect()
}

/// What a build collected, sorted and each once.
struct Finished {
    functions: Vec<CompiledFunction>,
    statics: Vec<Static>,
    puts: Vec<Put>,
    stats: Stats,
    errors: Vec<CodegenError>,
    used: Vec<ContentId>,
    kernel: Vec<KernelEntry>,
    misses: Vec<blaze_kernel::Miss>,
}

/// Where a template a function is made of comes from, for the error that it does not fit.
#[derive(Clone, Copy, Debug)]
enum Whence {
    /// Kept in the store, or in the memo by its store key.
    Store(Key),
    /// The memo's, of a compile of the same shape.
    Shape(ContentId),
    /// The body folded for a group, kept in the store or the memo by this key: the function
    /// is the group's (its key [`fold::Group::key`]), named by its first member.
    Folded(Key),
}

/// Function `key` (`item`, called `name`) could not be made of the template from `whence`.
fn unfit(
    key: InstanceKey,
    item: ItemKey,
    name: &str,
    whence: Whence,
    why: &TemplateError,
) -> CodegenError {
    let what = match whence {
        // alloc: an error
        Whence::Folded(_) => {
            format!("the body folded for the group of `{name}` ({key:?}, the group's key)")
        }
        // alloc: an error
        Whence::Store(_) | Whence::Shape(_) => format!("`{name}` ({key:?})"),
    };
    // alloc: an error
    error(key, item, format!("{what}, of the template {whence:?}: {why}"))
}

/// The function of instance `key` with `refs` in a stored template blob, read in place: its
/// lists are decoded into a frame and each part copied once, into the function. The outer
/// `Err` is bytes that are no template; the inner is the template's own.
fn stored_function(
    bytes: &[u8],
    key: InstanceKey,
    refs: &[Target],
    anchors: OwnAnchors<'_>,
) -> Result<Result<CompiledFunction, TemplateError>, String> {
    frame::task(Tag::Codegen, |frame| {
        // alloc: an error
        let view = blob::decode_view(bytes, frame).map_err(|e| e.to_string())?;
        Ok(view.instantiate_at(key, refs, anchors))
    })
}

/// One build's codegen.
pub struct Build<'a, B: Prepare> {
    backend: &'a B,
    frontend: &'a B::Frontend,
    policy: &'a Policy,
    memo: &'a Memo,
    /// The masked IR of every function folded so far, kept across builds.
    masks: &'a Masks,
    /// The last run's functions, with the shapes its lazy checks recorded.
    last: Later<FrontInstance<B>, FrontStatic<B>>,
    /// Functions the last run's statics refer to.
    static_callees: blaze_hash::HashSet<InstanceKey, Global>,
    /// What other builds' waits for its compiles name it by ([`Build::owned_by`]).
    owner: &'a str,
    /// Which store hits are compiled again and checked ([`Build::verify_store`]).
    verify: blaze_store_check::Setting,
    /// Whether each store miss is explained ([`Build::explain_misses`]).
    why: bool,
    /// Whether functions an edit fills again under their own keys are reached through their
    /// slots ([`rebind`]): always, but in the control of the test that holds the rule.
    rebinds: bool,
    /// The describes of the walks before this one over the same front end, which this walk
    /// takes where the front end says they still hold and adds its own to ([`Build::keeping`]).
    kept: Option<&'a Kept<FrontInstance<B>, FrontStatic<B>>>,
    /// Whether the front end is asked before a kept describe is taken
    /// ([`Frontend::redescribe`]): always, but in the control of the test that holds the
    /// walk over kept describes to the walk over none.
    asks: bool,
    /// Whether a patched walk holds each changed function to its shape and renames the
    /// places that named its old key ([`patch`]): always, but in the controls of the test
    /// that holds the patched walk to the whole one.
    checks_shape: bool,
    renames: bool,
    /// The bug planted in a patched build, for a control of the test that holds it to the
    /// whole one: none, but in that control.
    planted: Option<Planted>,
}

/// A bug a control plants in a patched build ([`Build::planted`]): each takes away one rule
/// of [`patch`] or [`lists`], and the refinement's oracle must find the difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "made only by the controls of the tests"))]
enum Planted {
    /// A referrer of a function whose key changed keeps the function it was made of.
    KeptTemplates,
    /// The recursion limit's check is not run after an edge is added.
    SkippedClaims,
    /// A function made again keeps the owner its row had.
    KeptOwner,
    /// A dedupe class is left as it was when a member's code changed.
    UnsplitClass,
    /// The delta names no function the build removed.
    DeltaOmitsRemoved,
}

/// How many of a walk's functions are reached each way: what [`rebind`] counted, and the span
/// `codegen classify` records.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct SlotCounts {
    /// Through their slots: an edit changes their bodies.
    edited: usize,
    /// Through their slots: another crate's code over a workspace type, closure or item.
    over_editable: usize,
    /// Through their slots: they refer to an edited function, or build in a rebound one.
    refers: usize,
    /// Through their slots for a tier-up alone.
    tiered: usize,
    /// Called directly.
    direct: usize,
}

/// Marks the functions an edit can fill again under their own keys ([`FnNode::rebound`]), so
/// the walk resolves every reference to one through its slot.
///
/// A fill names each callee by key, so a function's fill changes under its own key when an
/// edit changes a callee's key and not its own. The functions that can happen to, beside
/// the edited ones: one over a workspace type, closure or item, which the front end says and
/// [`Build::node`] marked where the build has slots ([`Described::over_editable`]); one that
/// refers to an edited function (whose key every edit of its body changes); and one that
/// builds in a rebound function, whose references its own fill holds. The last is a fixed
/// point over [`Described::built_in`]: each pass marks a function or is the last, so it
/// ends within one pass per function and one more.
///
/// `asked` is the walk's key of each instance as each origin asks for it. With `rebinds`
/// off, nothing is marked (the planted bug of the rule's test) and the count says so.
fn rebind<I: Clone + Eq + Hash, S: Clone>(
    nodes: &mut Sorted<InstanceKey, FnNode<I, S>>,
    asked: &HashMap<(I, Origin), InstanceKey>,
    rebinds: bool,
) -> SlotCounts {
    // alloc: per build, a flag per function
    let mut refers: Vec<bool> = vec![false; nodes.len()];
    {
        // Each node's place in key order, and the nodes by place.
        // alloc: per build, sized by its units
        let list: Vec<&FnNode<I, S>> = nodes.values().collect();
        let at: blaze_hash::HashMap<InstanceKey, u32, Global> = {
            let mut at = blaze_hash::HashMap::with_capacity_in(list.len(), Global);
            for (place, key) in nodes.keys().enumerate() {
                at.insert(*key, place as u32);
            }
            at
        };
        // The place among the nodes of the function `node` refers to at `place`.
        let index = |node: &FnNode<I, S>, place: usize| -> Option<usize> {
            let FrontRef::Function(callee) = node.described.refs.get(place)? else { return None };
            // clone: per build, the walk's key of a callee; who asks for it is a copy
            let key = asked.get(&(callee.clone(), node.described.origin))?;
            at.get(key).map(|at| *at as usize)
        };
        for (place, node) in list.iter().enumerate() {
            refers[place] = rebinds
                && !node.slotted()
                && (0..node.described.refs.len())
                    .any(|at| index(node, at).is_some_and(|at| list[at].patchable));
        }
        let mut changed = rebinds;
        while changed {
            changed = false;
            for (at, node) in list.iter().enumerate() {
                if refers[at] || node.slotted() {
                    continue;
                }
                let builds_in = node.described.built_in.iter().any(|place| {
                    index(node, *place as usize).is_some_and(|it| refers[it] || list[it].rebound)
                });
                if builds_in {
                    refers[at] = true;
                    changed = true;
                }
            }
        }
    }
    let mut reached = SlotCounts::default();
    for (node, refers) in nodes.values_mut().zip(&refers) {
        if node.patchable {
            reached.edited += 1;
        } else if node.rebound {
            reached.over_editable += 1;
        } else if *refers {
            node.rebound = true;
            reached.refers += 1;
        } else if node.then.is_some() {
            reached.tiered += 1;
        } else {
            reached.direct += 1;
        }
    }
    reached
}

/// The function a target names, if it names one.
fn callee(target: &Target) -> Option<InstanceKey> {
    match target {
        Target::Function(key) | Target::Direct(key) => Some(*key),
        _ => None,
    }
}

impl<'a, B: Prepare> Build<'a, B> {
    pub fn new(
        backend: &'a B,
        frontend: &'a B::Frontend,
        policy: &'a Policy,
        memo: &'a Memo,
    ) -> Self {
        Build {
            backend,
            frontend,
            policy,
            memo,
            masks: Masks::shared(),
            last: Later::default(),
            static_callees: blaze_hash::HashSet::new_in(Global),
            owner: "a build",
            verify: blaze_store_check::Setting::OFF,
            why: false,
            rebinds: true,
            kept: None,
            asks: true,
            checks_shape: true,
            renames: true,
            planted: None,
        }
    }

    /// Plants `bug` in this build's patch ([`Planted`]), for a control.
    #[cfg(test)]
    fn planting(mut self, bug: Planted) -> Self {
        self.planted = Some(bug);
        self
    }

    /// The planted bug of a control of the patched walk: a changed function whose references
    /// are others keeps its old edges and places, so the walk keeps what it referred to.
    #[cfg(test)]
    fn patching_any_shape(mut self) -> Self {
        self.checks_shape = false;
        self
    }

    /// The planted bug of a control of the patched walk: the places that named a changed
    /// function's old key keep naming it.
    #[cfg(test)]
    fn without_renaming(mut self) -> Self {
        self.renames = false;
        self
    }

    /// Takes from `kept` each describe an earlier walk kept that the front end says still
    /// holds, and keeps this walk's there for the next (docs/v2/plan/design/incremental-walk.md).
    /// `kept` is of this front end's database and configuration and of no other: its
    /// instances are that database's. The walk reaches the same functions either way; with
    /// it, an unchanged function costs the front end's answer in place of a describe.
    pub fn keeping(mut self, kept: &'a Kept<FrontInstance<B>, FrontStatic<B>>) -> Self {
        self.kept = Some(kept);
        self
    }

    /// The planted bug of the test that holds the kept walk to the fresh one: a kept describe
    /// is taken without asking the front end, so a changed function keeps its old describe.
    #[cfg(test)]
    fn trusting_kept(mut self) -> Self {
        self.asks = false;
        self
    }

    /// The planted bug of the test that holds [`rebind`]'s rule: no function is rebound, so
    /// one an edit fills again under its own key is called directly, as before the rule.
    #[cfg(test)]
    fn without_rebinding(mut self) -> Self {
        self.rebinds = false;
        self
    }

    /// Gives back why each dev template the store missed was not in it ([`Built::misses`]):
    /// for a daemon that answers `blaze why`. Off, a miss costs nothing more than it did.
    pub fn explain_misses(mut self, why: bool) -> Self {
        self.why = why;
        self
    }

    /// Keeps masked IR in `masks` in place of the process's [`Masks::shared`].
    pub fn with_masks(mut self, masks: &'a Masks) -> Self {
        self.masks = masks;
        self
    }

    /// Names this build `owner` (its unit) in the spans of other builds' waits for the
    /// functions it compiles ([`crate::flight`]).
    pub fn owned_by(mut self, owner: &'a str) -> Self {
        self.owner = owner;
        self
    }

    /// The store check (`BLAZE_VERIFY_STORE`, [`blaze_store_check`]): each dev or release template
    /// the store gives that `setting` samples is compiled again, from its IR, with neither the
    /// store nor the memo, and its bytes must be the stored ones; a difference is that unit's
    /// error naming the function, its store key and the first differing byte.
    pub fn verify_store(mut self, setting: blaze_store_check::Setting) -> Self {
        self.verify = setting;
        self
    }

    fn node(
        &self,
        instance: &FrontInstance<B>,
        requester: Origin,
        described: Arc<Described<FrontInstance<B>, FrontStatic<B>>>,
    ) -> Result<Node<B>, CodegenError> {
        let choice =
            self.policy.choose(&described.origin, described.overflow_checks).map_err(|why| {
                let unit = Instance {
                    code: described.code,
                    args: described.args,
                    settings: crate::Settings(self.policy.id()),
                };
                // alloc: an error
                error(unit.key(), described.item, why.to_string())
            })?;
        let instance_of = Instance {
            code: described.code,
            args: described.args,
            settings: choice.first.settings(),
        };
        let unit = Unit {
            instance: instance_of,
            backend: choice.first.backend,
            arch: self.policy.toolchain.arch,
            item: described.item,
        };
        // Another crate's code over a workspace type, closure or item, where the build has
        // slots: an edit fills it again under its own key. [`rebind`] adds the ones that
        // only the walk's keys tell.
        let rebound_own =
            self.rebinds && choice.patchable && !described.editable && described.over_editable;
        Ok(FnNode {
            unit,
            spec: choice.first,
            then: choice.then,
            // Only code an edit reaches is patched: a generic of another crate a member
            // instantiates is the member's by origin, but no edit changes its body.
            patchable: choice.patchable && described.editable,
            rebound: rebound_own,
            rebound_own,
            // clone: per build, the walk's handle of the instance
            instance: instance.clone(),
            requester,
            least_origin: described.origin,
            name_digest: digests::debug_name_digest(&described.name),
            described,
            // alloc: none, empty until the walk resolves them
            refs: Vec::new(),
            // alloc: none, empty until the walk resolves them
            built_in: Vec::new(),
        })
    }

    /// What the front end says of `asked`, a function and whose code asks for it, in the
    /// walk's round `round` (from 0): the describe an earlier walk kept, where the front end
    /// says a describe now would give it ([`Frontend::redescribe`]); else a describe, kept
    /// for the next walk. A describe that fails is the build's error and nothing is kept of
    /// it, so the next walk asks again.
    fn described(
        &self,
        asked: &(FrontInstance<B>, Origin),
        round: usize,
        counts: &kept::Counts,
    ) -> Result<Arc<Described<FrontInstance<B>, FrontStatic<B>>>, CodegenError> {
        let (instance, requester) = asked;
        let now = match self.kept.and_then(|kept| kept.function(round, asked)) {
            Some(was) if !self.asks => {
                counts.verified();
                return Ok(was);
            }
            Some(was) => match self.frontend.redescribe(instance, requester, &was) {
                Ok(None) => {
                    counts.verified();
                    return Ok(was);
                }
                Ok(Some(now)) => {
                    counts.described();
                    now
                }
                Err(why) => {
                    counts.described();
                    return Err(why);
                }
            },
            None => {
                counts.described();
                self.frontend.describe(instance, requester)?
            }
        };
        // alloc: once per describe, shared by the walk's node and the walk kept
        let now = Arc::new(now);
        if let Some(kept) = self.kept {
            // clone: a reference count, once per describe
            kept.keep(round, asked, now.clone());
        }
        Ok(now)
    }

    fn static_node(
        &self,
        value: &FrontStatic<B>,
        described: DescribedStatic<FrontInstance<B>, FrontStatic<B>>,
    ) -> Result<StaticOf<B>, CodegenError> {
        let choice = self.policy.choose(&described.origin, None).map_err(|why| {
            // alloc: an error
            error(InstanceKey(described.value), described.item, why.to_string())
        })?;
        // clone: per build, the walk's handle of the static; its places are empty until resolved
        Ok(StaticNode { value: value.clone(), described, spec: choice.first, refs: Vec::new() })
    }

    /// Every instance and static the roots reach, each with its places resolved, and what
    /// the walk did ([`Walk`]). With `order`, each function's key is pushed to it once, in the
    /// order the walk reached it.
    ///
    /// The walk reaches everything from the roots again every build: one hash lookup and a
    /// few vector pushes a function, with no describe of one whose kept describe still holds
    /// ([`Self::described`]). Every pass after the levels (what the build's own roots reach,
    /// the order, the slots, the places, what each builds in) is over the walk's own tables,
    /// linear in its functions and references, each under its span.
    fn walk(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        mut order: Option<&mut Vec<InstanceKey>>,
        errors: &mut Vec<CodegenError>,
    ) -> (
        Sorted<InstanceKey, Node<B>>,
        Sorted<StaticId, StaticOf<B>>,
        Vec<InstanceKey>,
        Walk,
        Option<patch::Side<FrontInstance<B>, FrontStatic<B>>>,
    ) {
        let counts = kept::Counts::default();
        // Every static described, with its describe: what a kept walk asks again
        // ([`patch`]). Only a build that keeps its walk whole collects it.
        let keeps = self.kept.is_some() && order.is_none();
        // alloc: per build that keeps its walk, a describe a static
        let mut described_statics = Vec::new();
        // alloc: per build that keeps its walk, a describe a function
        let mut described_functions = Vec::new();
        let fresh = match self.kept {
            None => Some(Fresh::Unkept),
            Some(kept) if kept.is_empty() => Some(Fresh::Restart),
            Some(_) => None,
        };
        let mut nodes: blaze_hash::HashMap<InstanceKey, Node<B>, Global> =
            blaze_hash::HashMap::new_in(Global);
        let mut statics: blaze_hash::HashMap<StaticId, StaticOf<B>, Global> =
            blaze_hash::HashMap::new_in(Global);
        let mut asked: HashMap<(FrontInstance<B>, Origin), InstanceKey> = HashMap::new();
        let mut static_asked: HashMap<FrontStatic<B>, StaticId> = HashMap::new();
        // Every function and static reached, what each refers to and the paths rustc's
        // recursion limit counts on (`recursion`).
        let mut graph = recursion::Graph::new();
        let mut level = graph.start(roots, static_roots, check);
        let ordered = order.as_deref().map_or(0, Vec::len);
        let mut round = 1;
        loop {
            if level.is_empty() {
                // rustc's recursion limit, over everything described: it may send instances
                // the levels deferred back to be described first.
                if graph.check(self.frontend, &mut level, errors) {
                    continue;
                }
                // The walk runs a second time where the first closed facts of the whole
                // program that bodies are built against (`Frontend::closed`): it describes
                // each instance again, as its code and references may now be others. A third
                // is never needed: the second's facts are the first's, or the front end's
                // error.
                if !errors.is_empty() {
                    break;
                }
                match self.frontend.closed() {
                    Ok(false) => break,
                    Ok(true) if round == 1 => {}
                    Ok(true) => {
                        errors.push(error(
                            InstanceKey(ContentId::default()),
                            ItemKey(ContentId::default()),
                            // alloc: the error's message
                            format!(
                                "the front end made new facts of the whole program after walk \
                                 {round}, which the bodies it described were built against"
                            ),
                        ));
                        break;
                    }
                    Err(why) => {
                        errors.push(why);
                        break;
                    }
                }
                round += 1;
                tracing::info!(target: "blaze", round, "the walk runs again over the program's facts");
                nodes.clear();
                statics.clear();
                asked.clear();
                static_asked.clear();
                described_statics.clear();
                described_functions.clear();
                if let Some(order) = order.as_deref_mut() {
                    order.truncate(ordered);
                }
                level = graph.start(roots, static_roots, check);
                continue;
            }
            if self.frontend.stopped() {
                errors.push(stopped());
                break;
            }
            // Before the level is described: an instance past the limit on the levels' path is
            // deferred, never described by them, and what it reaches never walked from it.
            graph.level(self.frontend, &mut level, errors);
            self.frontend.reached(&level.functions, &level.statics);
            // The span of a describe is the front end's, opened where it describes
            // (`hir-ty-codegen`'s `codegen describe`, named for its function): a kept describe
            // the front end says still holds opens none, so the spans of a level are the
            // functions it described.
            // alloc: per build, sized by its units
            let described: Vec<_> = par::map(&level.functions, |asked| {
                self.described(asked, round - 1, &counts)
                    .and_then(|it| self.node(&asked.0, asked.1, it))
            });
            // alloc: per build, sized by its units
            let level_statics: Vec<_> = par::map(&level.statics, |value| {
                self.frontend.describe_static(value).and_then(|it| self.static_node(value, it))
            });
            let current = std::mem::replace(&mut level, recursion::Level::new());
            let functions = current
                .functions
                .into_iter()
                .zip(current.function_items)
                .zip(current.function_from);
            for (((asked_for, item), at), node) in functions.zip(described) {
                let node = match node {
                    Ok(node) => node,
                    Err(why) => {
                        graph.failed(item);
                        errors.push(why);
                        continue;
                    }
                };
                let key = node.unit.instance.key();
                if keeps {
                    // clone: per build that keeps its walk, the function's handle and a count
                    described_functions.push((asked_for.clone(), node.described.clone()));
                }
                asked.insert(asked_for, key);
                let origin = node.described.origin;
                graph.described(item, &node.described.refs, &origin, at, &mut level);
                match nodes.entry(key) {
                    blaze_hash::hash_map::Entry::Vacant(slot) => {
                        if let Some(order) = order.as_deref_mut() {
                            order.push(key);
                        }
                        slot.insert(node);
                    }
                    // One key described twice (an `asm!` function asked for by a dependency
                    // and the workspace, a generic made for arguments its code reads alike)
                    // is the image's if either says so, and then is the image's describer's
                    // whole: its code is the key's either way, and the image's copy refers
                    // to what that describer's references are, never the workspace's.
                    // Two describes under one key that disagree on what the code is mean the
                    // key misses an input (rule 21): refused, never one kept silently.
                    blaze_hash::hash_map::Entry::Occupied(mut it) => {
                        let (kept, new) = (&it.get().described, &node.described);
                        if kept.code != new.code
                            || kept.template_code != new.template_code
                            || kept.anchors != new.anchors
                            || kept.built_in_code != new.built_in_code
                            || kept.built_in.len() != new.built_in.len()
                            || kept.refs.len() != new.refs.len()
                        {
                            errors.push(error(
                                key,
                                new.item,
                                // alloc: the error's message
                                format!(
                                    "`{}` and `{}` have one instance key {key:?} but other code: \
                                     code {:?} against {:?}, template code {:?} against {:?}, \
                                     built in as {:?} against {:?}, {} \
                                     built-in callees against {}, {} references against {}; the \
                                     key misses an input their code reads",
                                    kept.name,
                                    new.name,
                                    kept.code,
                                    new.code,
                                    kept.template_code,
                                    new.template_code,
                                    kept.built_in_code,
                                    new.built_in_code,
                                    kept.built_in.len(),
                                    new.built_in.len(),
                                    kept.refs.len(),
                                    new.refs.len(),
                                ),
                            ));
                        } else {
                            let least = it.get().least_origin.min(node.least_origin);
                            if new.owner < kept.owner {
                                it.insert(node);
                            }
                            it.get_mut().least_origin = least;
                        }
                    }
                }
            }
            let statics_of =
                current.statics.into_iter().zip(current.static_items).zip(current.static_from);
            for (((value, item), at), node) in statics_of.zip(level_statics) {
                let node = match node {
                    Ok(node) => node,
                    Err(why) => {
                        graph.failed(item);
                        errors.push(why);
                        continue;
                    }
                };
                let id = (node.described.item, node.described.instance);
                if keeps {
                    // clone: per build that keeps its walk, the static's handle and describe
                    described_statics.push((value.clone(), node.described.clone()));
                }
                static_asked.insert(value, id);
                let origin = node.described.origin;
                graph.described(item, &node.described.refs, &origin, at, &mut level);
                statics.entry(id).or_insert(node);
            }
        }
        // What a check root alone reaches ([`CheckRoot`]) was described for the recursion
        // limit's check, never for the build: only what the build's own roots reach, through
        // each kept node's references as they resolve below, is built.
        let reaching = tracing::info_span!(
            target: "blaze",
            "codegen reach",
            functions = nodes.len(),
            statics = statics.len(),
        )
        .entered();
        if graph.checks_more() {
            // alloc: per build, what its own roots reach
            let mut functions: blaze_hash::HashSet<InstanceKey, Global> =
                blaze_hash::HashSet::new_in(Global);
            let mut values: blaze_hash::HashSet<StaticId, Global> =
                blaze_hash::HashSet::new_in(Global);
            // alloc: per build, the walk's two queues, from the roots
            let mut function_queue: Vec<InstanceKey> =
                roots.iter().filter_map(|it| asked.get(it).copied()).collect();
            // alloc: as above
            let mut value_queue: Vec<StaticId> =
                static_roots.iter().filter_map(|it| static_asked.get(it).copied()).collect();
            loop {
                let (refs, requester) = if let Some(key) = function_queue.pop() {
                    match nodes.get(&key) {
                        Some(node) if functions.insert(key) => {
                            (&node.described.refs, node.described.origin)
                        }
                        _ => continue,
                    }
                } else if let Some(id) = value_queue.pop() {
                    match statics.get(&id) {
                        Some(node) if values.insert(id) => {
                            (&node.described.refs, node.described.origin)
                        }
                        _ => continue,
                    }
                } else {
                    break;
                };
                for it in refs {
                    match it {
                        FrontRef::Function(callee) => {
                            // clone: per build, the walk's key of a callee; who asks for it is a copy
                            function_queue.extend(asked.get(&(callee.clone(), requester)).copied());
                        }
                        FrontRef::Static(value) => {
                            value_queue.extend(static_asked.get(value).copied())
                        }
                        FrontRef::Extern(_) | FrontRef::Weak(_) => {}
                    }
                }
            }
            nodes.retain(|key, _| functions.contains(key));
            statics.retain(|id, _| values.contains(id));
            if let Some(order) = order.as_deref_mut() {
                let mut at = 0;
                order.retain(|key| {
                    at += 1;
                    at <= ordered || functions.contains(key)
                });
            }
        }
        let (mut nodes, mut statics) = (Sorted::of(nodes), Sorted::of(statics));
        drop(reaching);

        // Which functions are reached through their slots, before any reference is resolved:
        // every caller of one then names its slot.
        {
            let span = tracing::info_span!(
                target: "blaze",
                "codegen classify",
                functions = nodes.len(),
                edited = tracing::field::Empty,
                over_editable = tracing::field::Empty,
                refers = tracing::field::Empty,
                tiered = tracing::field::Empty,
                direct = tracing::field::Empty,
            );
            let _entered = span.enter();
            let reached = rebind(&mut nodes, &asked, self.rebinds);
            span.record("edited", reached.edited);
            span.record("over_editable", reached.over_editable);
            span.record("refers", reached.refers);
            span.record("tiered", reached.tiered);
            span.record("direct", reached.direct);
        }
        let resolving = tracing::info_span!(
            target: "blaze",
            "codegen resolve",
            functions = nodes.len(),
            statics = statics.len(),
        )
        .entered();
        let through_slot: HashMap<InstanceKey, bool> =
            // alloc: per build, sized by its units
            nodes.iter().map(|(key, node)| (*key, node.slotted() || node.then.is_some())).collect();
        // What is defined weakly under a name ([`Linkage::Weak`]): every reference to it is by
        // that name, never by key, so a strong definition elsewhere on the link takes them all
        // over, as LLVM treats a weak definition as interposable and ld64 and lld bind every
        // reference to the strong one. The linker resolves the name to this definition when
        // nothing stronger defines it.
        let weakly = |symbol: &Option<String>, linkage: Linkage| match linkage {
            // clone: per build, the name of a weakly defined function or static, a few
            Linkage::Weak => symbol.clone(),
            Linkage::Strong => None,
        };
        let weak_functions: HashMap<InstanceKey, String> = nodes
            .iter()
            .filter_map(|(key, node)| {
                Some((*key, weakly(&node.described.symbol, node.described.linkage)?))
            })
            // alloc: per build, its weakly defined functions, a few
            .collect();
        let weak_statics: HashMap<(ItemKey, ContentId), String> = statics
            .iter()
            .filter_map(|(id, node)| {
                Some((*id, weakly(&node.described.symbol, node.described.linkage)?))
            })
            // alloc: per build, its weakly defined statics, a few
            .collect();
        let resolve = |refs: &[FrontRef<FrontInstance<B>, FrontStatic<B>>], requester: &Origin| {
            refs.iter()
                .map(|it| match it {
                    FrontRef::Function(callee) => {
                        // clone: per build, the walk's key of a callee; who asks for it is a copy
                        let key = asked.get(&(callee.clone(), *requester))?;
                        if let Some(name) = weak_functions.get(key) {
                            // clone: per build, a reference to a weakly defined function
                            return Some(Target::Extern(name.clone()));
                        }
                        Some(match through_slot.get(key) {
                            Some(true) => Target::Function(*key),
                            _ => Target::Direct(*key),
                        })
                    }
                    FrontRef::Static(value) => {
                        let (item, instance) = *static_asked.get(value)?;
                        if let Some(name) = weak_statics.get(&(item, instance)) {
                            // clone: per build, a reference to a weakly defined static
                            return Some(Target::Extern(name.clone()));
                        }
                        Some(Target::Static { item, instance })
                    }
                    // clone: per build, the instance's own target, kept by its node
                    FrontRef::Extern(name) => Some(Target::Extern(name.clone())),
                    // clone: per build, the instance's own target, kept by its node
                    FrontRef::Weak(name) => Some(Target::Weak(name.clone())),
                })
                // alloc: per build, an instance's places, kept by its node
                .collect::<Option<Vec<_>>>()
        };
        // An instance one of whose references failed has no places; that failure is already
        // an error of the build. One with no error of the build is the walk's bug, named here.
        let failed = !errors.is_empty();
        for (key, node) in nodes.iter_mut() {
            node.refs = match resolve(&node.described.refs, &node.described.origin) {
                Some(refs) => refs,
                None if failed => Vec::new(), // alloc: none, empty
                None => {
                    errors.push(error(
                        *key,
                        node.unit.item,
                        // alloc: an error
                        format!(
                            "`{}` refers to a function or static the build's walk never described, \
                             though no error of the build says why",
                            node.described.name
                        ),
                    ));
                    Vec::new() // alloc: none, empty
                }
            };
        }
        // What each builds in directly, then at any depth.
        let mut direct: blaze_hash::HashMap<InstanceKey, Vec<InstanceKey>, Global> =
            blaze_hash::HashMap::with_capacity_in(nodes.len(), Global);
        for (key, node) in nodes.iter() {
            if node.refs.is_empty() && !node.described.refs.is_empty() {
                continue;
            }
            let mut callees = Vec::with_capacity(node.described.built_in.len());
            // A tiered callee is reached through its slot too, but tiering it up changes its
            // code, not its MIR: the copy built in stays right.
            for &at in &node.described.built_in {
                match node.refs.get(at as usize).and_then(callee) {
                    Some(it) if nodes.get(&it).is_some_and(|it| it.patchable) => {
                        errors.push(error(
                            *key,
                            node.unit.item,
                            // alloc: an error
                            format!(
                                "builds in {it:?}, which is patchable: an edit patches it alone"
                            ),
                        ))
                    }
                    Some(it) => callees.push(it),
                    None => errors.push(error(
                        *key,
                        node.unit.item,
                        // alloc: an error
                        format!(
                            "builds in place {at}, which is not a function among its {} references",
                            node.refs.len()
                        ),
                    )),
                }
            }
            direct.insert(*key, callees);
        }
        // alloc: per build, reused across its units
        let (mut seen, mut queue) = (blaze_hash::HashSet::new_in(Global), Vec::new());
        for (key, node) in nodes.iter_mut() {
            seen.clear();
            queue.extend(direct.get(key).into_iter().flatten().copied());
            while let Some(it) = queue.pop() {
                if it != *key && seen.insert(it) {
                    queue.extend(direct.get(&it).into_iter().flatten().copied());
                }
            }
            // alloc: per build, what a unit builds in, kept by its node
            node.built_in = seen.iter().copied().collect();
            node.built_in.sort_unstable();
        }
        for node in statics.values_mut() {
            node.refs = match resolve(&node.described.refs, &node.described.origin) {
                Some(refs) => refs,
                None if failed => Vec::new(), // alloc: none, empty
                None => {
                    errors.push(error(
                        InstanceKey(node.described.value),
                        node.described.item,
                        // alloc: an error
                        format!(
                            "static `{}` refers to a function or static the build's walk never \
                             described, though no error of the build says why",
                            node.described.name
                        ),
                    ));
                    Vec::new() // alloc: none, empty
                }
            };
        }
        drop(resolving);
        let (described, verified) = counts.get();
        let walk = Walk {
            described,
            verified,
            reached: nodes.len(),
            dropped: 0,
            fresh,
            whole: None,
            visited: 0,
        };
        // alloc: per build, sized by its roots
        let roots = roots.iter().filter_map(|it| asked.get(it).copied()).collect();
        let side = (keeps && errors.is_empty()).then(|| patch::Side {
            asked,
            counted: graph.counts(),
            described: described_functions,
            statics: described_statics,
        });
        (nodes, statics, roots, walk, side)
    }

    /// `work` on each of `items` on all cores, in their order, beginning none once the front
    /// end says the build stopped ([`Frontend::stopped`], a relaxed load against a function's
    /// 50 to 200 µs): a stopped build gives each thread back within the one function it is in.
    /// Any left undone is one [`stopped`] error in `errors`.
    fn until_stopped<I, R>(
        &self,
        items: I,
        errors: &mut Vec<CodegenError>,
        work: impl Fn(I::Item) -> R + Sync,
    ) -> Vec<R>
    where
        I: IntoSource,
        R: Send,
    {
        let skipped = AtomicBool::new(false);
        // alloc: per build, sized by its units
        let done: Vec<R> = par::filter_map(items, |it| {
            if self.frontend.stopped() {
                skipped.store(true, Ordering::Relaxed);
                return None;
            }
            Some(work(it))
        });
        if skipped.into_inner() {
            errors.push(stopped());
        }
        done
    }

    /// Each of `items`, a flight key and what a run of it takes, once of all the memo's builds
    /// ([`crate::flight`]), as [`Plan`] decides: run here by `run` on all cores when no
    /// build landed it or runs it now, and landed as each run ends (`run` gives its landing and
    /// what this build keeps of it); another build's run waited for on its own key once this
    /// build's own runs ended, the wait a span naming what `name` calls the item (its unit,
    /// item and name); one let go of unlanded asked for again. In the items' order: each run
    /// here or landed, but those the build stopped before (one [`stopped`] error) and those
    /// given up (an error each, naming it).
    fn fly<'n, T: Send, R: Send>(
        &self,
        items: Vec<(InstanceKey, T)>,
        name: impl Fn(&T) -> (InstanceKey, ItemKey, &'n str),
        run: impl Fn(T) -> (Option<ContentId>, R) + Sync,
        errors: &mut Vec<CodegenError>,
    ) -> Vec<Flown<T, R>> {
        let flights = self.memo.flights();
        // alloc: what other builds' waits for this batch's compiles name it by, once per batch
        let owner: std::sync::Arc<str> = std::sync::Arc::from(self.owner);
        let mut plan = Plan::new(items.len());
        // alloc: per batch of store misses, sized by it
        let mut slots: Vec<Option<(InstanceKey, T)>> = items.into_iter().map(Some).collect();
        // alloc: per batch of store misses, sized by it
        let mut flown: Vec<Option<Flown<T, R>>> =
            std::iter::repeat_with(|| None).take(slots.len()).collect();
        'decide: loop {
            let decisions = plan.take_decisions();
            if decisions.is_empty() {
                break;
            }
            // alloc: per batch of store misses, sized by it
            let mut asks = Vec::new();
            for decision in &decisions {
                if let Decision::Ask(at) = *decision {
                    asks.push((at, slots[at].take().expect("an item asked for is held")));
                }
            }
            let asked = self.until_stopped(asks, errors, |(at, (key, item))| {
                let mut item = Some(item);
                let asked = flights
                    .ask(key, &owner, || run(item.take().expect("a run takes its item once")));
                (at, key, item, asked)
            });
            for (at, key, item, asked) in asked {
                match asked {
                    Asked::Ran(made) => {
                        plan.asked(at, Found::Ran);
                        flown[at] = Some(Flown::Ran(made));
                    }
                    Asked::Theirs => plan.asked(at, Found::Theirs),
                    Asked::Landed(shape) => plan.asked(at, Found::Landed(shape)),
                }
                if let Some(item) = item {
                    slots[at] = Some((key, item));
                }
            }
            if self.frontend.stopped() {
                break;
            }
            for decision in decisions {
                match decision {
                    Decision::Ask(_) => {}
                    Decision::Wait(at) => {
                        if self.frontend.stopped() {
                            errors.push(stopped());
                            break 'decide;
                        }
                        let (key, item) = slots[at].as_ref().expect("an item waited for is held");
                        let (_, _, called) = name(item);
                        let landing = flights.wait(*key, |it, owner| {
                            use std::fmt::Write as _;
                            _ = write!(it, "codegen: `{called}` {key:?}, which {owner} compiles");
                        });
                        plan.waited(at, landing);
                    }
                    Decision::Take(at, shape) => {
                        let (_, item) = slots[at].take().expect("an item taken is held");
                        flown[at] = Some(Flown::Landed(item, shape));
                    }
                    Decision::GiveUp { at, asks } => {
                        let (key, item) = slots[at].take().expect("an item given up is held");
                        let (unit, of, called) = name(&item);
                        errors.push(error(
                            unit,
                            of,
                            // alloc: an error
                            format!(
                                "`{called}` ({key:?}) given up: asked for {asks} times, each time another build of the memo compiled it and let it go unlanded (rule 18)"
                            ),
                        ));
                    }
                }
            }
        }
        // alloc: per batch of store misses, sized by it
        flown.into_iter().flatten().collect()
    }

    /// Builds the IR of each of `keys`, on all cores, until the build is stopped.
    fn prepare_all(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        keys: &[InstanceKey],
        errors: &mut Vec<CodegenError>,
    ) -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
        self.until_stopped(keys, errors, |key| {
            let node = &nodes[key];
            let made = prepare_node(self.backend, self.frontend, &node.unit, &node.spec, node);
            (*key, made)
        })
    }

    /// Compiles everything `roots` reach. Each root is an instance and whose code asks for it.
    /// The recursion limit's check walks from the roots in their order.
    pub fn run(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        self.run_with_statics(roots, &[], &[], store)
    }

    /// [`Build::run`], with statics no code names that the program has all the same and
    /// everything they reach: `#[used]` and exported statics, which a linker or the runtime
    /// finds by name. `check` is the recursion limit's roots in rustc's order ([`CheckRoot`]);
    /// each root of the build it leaves out follows them, functions then statics, in the order
    /// given.
    pub fn run_with_statics(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        self.run_as(roots, static_roots, check, None, store)
    }

    /// What `roots` and `static_roots` reach, walked as [`Build::run_with_statics`] walks
    /// them with no check roots of their own (the recursion limit's check walks from the roots
    /// in their order), for a caller that compiles the code itself (the compiled tier of proc
    /// macros): the same instances, units and places, and nothing prepared, compiled or read
    /// from the store. Each function of it is among what [`Built::reached`] names of a root.
    pub fn reach(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
    ) -> Result<Reached<FrontInstance<B>, FrontStatic<B>>, Vec<CodegenError>> {
        // alloc: none unless the walk fails
        let mut errors = Vec::new();
        let (nodes, statics, _, _, _) = self.walk(roots, static_roots, &[], None, &mut errors);
        // The caller links what it compiles as [`Build::run_as`] does: one definition a symbol.
        symbols_defined_twice::<B>(&nodes, &statics, &mut errors);
        if !errors.is_empty() {
            errors.sort_by_key(|it| (it.unit, it.item));
            errors.dedup();
            return Err(errors);
        }
        // alloc: per walk, what the caller compiles
        let mut functions = Vec::with_capacity(nodes.len());
        // alloc: per walk, what the caller compiles
        let mut refs = Vec::with_capacity(nodes.len());
        for node in nodes.values() {
            // clone: per walk, what the caller compiles: the walk's nodes are in shared chunks
            functions.push((node.instance.clone(), node.unit.clone()));
            refs.push(node.refs.clone()); // clone: as above
        }
        // clone: as above
        // alloc: per walk, what the caller compiles
        let statics =
            statics.values().map(|node| (node.value.clone(), node.refs.clone())).collect();
        Ok(Reached { functions, refs, statics })
    }

    /// A whole build, or with `later` a lazy one whose entry is the roots at those places
    /// ([`Build::run_later`]).
    fn run_as(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        later: Option<&[usize]>,
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        let mut out = Out::default();
        // alloc: none for a whole build; a lazy one's, sized by its units
        let mut order = Vec::new();
        // The walk: patched from the one kept of this program where every condition of a
        // patch holds ([`patch`]), else whole, saying which did not. The durable view is read
        // before any describe, so what is durable in it stays so while it is the same.
        let policy_id = self.policy.id();
        let view = self.frontend.durable_view();
        let seen = self.frontend.view();
        let program_walk =
            kept::program(&(roots, static_roots, check, policy_id.0, self.rebinds));
        // The last `Built` of this program by row, taken whatever the walk does: a build
        // that ends well keeps its own.
        let mut lists = match (self.kept, later) {
            (Some(kept), None) => kept.take_lists(program_walk),
            _ => None,
        };
        // Why the build's own walk is not one the next build of the program can patch, when
        // it is not: said as the build ends (`codegen delta`'s `walk_kept`).
        let mut unkept: Option<&'static str> = None;
        let (nodes, statics, root_keys, mut walk, keep, touched) = {
            let span = tracing::info_span!(
                target: "blaze",
                "codegen walk",
                // The one field of this span the profile keeps a column for (`function`):
                // whether the walk was patched or ran whole and why, with what it asked and
                // described, said as the walk ends. The counts below reach a subscriber that
                // reads fields; the profile's parquet has no column for them, so until
                // 2026-11-10 a 2 s walk's row said nothing of which walk it was.
                function = tracing::field::Empty,
                roots = roots.len(),
                described = tracing::field::Empty,
                verified = tracing::field::Empty,
                reached = tracing::field::Empty,
                visited = tracing::field::Empty,
                fresh = tracing::field::Empty,
                whole = tracing::field::Empty,
                asked = tracing::field::Empty,
                named = tracing::field::Empty,
                held = tracing::field::Empty,
                each_build = tracing::field::Empty,
                graph = tracing::field::Empty,
                claims = tracing::field::Empty,
                classified = tracing::field::Empty,
                resolved = tracing::field::Empty,
                built_in = tracing::field::Empty,
                owners = tracing::field::Empty,
            );
            let _entered = span.enter();
            let patched = match later {
                None => self.patched(program_walk, roots, static_roots, check, policy_id, view),
                Some(_) => Err("a lazy build's walk order is not kept"),
            };
            let walked = match patched {
                Ok(patch::Patched { nodes, statics, roots, walk, keep, touched, work }) => {
                    span.record("asked", work.asked);
                    span.record("named", work.named);
                    span.record("held", work.held);
                    span.record("each_build", work.each_build);
                    span.record("graph", work.graph);
                    span.record("claims", work.claims);
                    span.record("classified", work.classified);
                    span.record("resolved", work.resolved);
                    span.record("built_in", work.built_in);
                    span.record("owners", work.owners);
                    span.record(
                        "function",
                        tracing::field::display(format_args!(
                            "patched: asked {} functions and statics ({} each build, {} of {} \
                             named changed), described {}",
                            work.asked, work.each_build, work.held, work.named, work.described
                        )),
                    );
                    (nodes, statics, roots, walk, Some(keep), Some((touched, work)))
                }
                Err(why) => {
                    let (nodes, statics, root_keys, mut walk, side) = self.walk(
                        roots,
                        static_roots,
                        check,
                        later.is_some().then_some(&mut order),
                        &mut out.errors,
                    );
                    walk.whole = Some(why);
                    let described = side.is_some();
                    let keep = side.and_then(|side| {
                        self.keep_of(&nodes, &statics, side, roots, static_roots, check)
                    });
                    unkept = match (&keep, self.kept, later) {
                        (Some(_), _, _) => None,
                        (None, None, _) => Some("the build keeps no walk"),
                        (None, Some(_), Some(_)) => Some("a lazy build's walk order is not kept"),
                        (None, Some(_), None) if !described => Some("the walk had an error"),
                        (None, Some(_), None) => {
                            Some("the walk's tables do not agree with what its roots reach")
                        }
                    };
                    (nodes, statics, root_keys, walk, keep, None)
                }
            };
            let walk = &walked.3;
            span.record("described", walk.described);
            span.record("verified", walk.verified);
            span.record("reached", walk.reached);
            span.record("visited", walk.visited);
            if let Some(fresh) = walk.fresh {
                span.record("fresh", fresh.why());
            }
            if let Some(whole) = walk.whole {
                span.record("whole", whole);
                span.record(
                    "function",
                    tracing::field::display(format_args!(
                        "whole ({whole}): described {}, verified {}, reached {}",
                        walk.described, walk.verified, walk.reached
                    )),
                );
            }
            walked
        };
        // A patched walk with the lists kept of the last `Built`: the rows of what the walk
        // touched are made again, and the `Built` is the kept one with them ([`lists`]).
        // What a whole walk checks of its result (the owners, the symbols) the patch
        // checked of what it touched.
        let mut work = Work::default();
        let nodes = match (touched, lists.take()) {
            (Some((touched, patch_work)), Some(kept_lists)) => {
                // alloc: per build, sized by its statics' references
                self.static_callees =
                    statics.values().flat_map(|it| it.refs.iter().filter_map(callee)).collect();
                match self.patch_lists(
                    kept_lists,
                    &nodes,
                    &statics,
                    &root_keys,
                    &touched,
                    patch_work,
                    walk,
                    store,
                ) {
                    lists::Patch::Built(built, kept_lists) => {
                        // alloc: once per build, the walk its `Later` and the kept walk share
                        let nodes = Arc::new(nodes);
                        // clone: a reference count, once per build
                        self.last = Later { nodes: nodes.clone(), ..Later::default() };
                        if let (Some(kept), Some(keep), Some(view)) = (self.kept, keep, view) {
                            kept.keep_lists(program_walk, kept_lists);
                            self.keep_walk(
                                program_walk,
                                roots,
                                static_roots,
                                check,
                                policy_id,
                                view,
                                seen,
                                nodes,
                                statics,
                                keep,
                            );
                        }
                        return Ok(*built);
                    }
                    lists::Patch::Refused(errors) => {
                        // alloc: once per build, the walk its `Later` holds
                        self.last = Later { nodes: Arc::new(nodes), ..Later::default() };
                        return Err(errors);
                    }
                    // The passes run whole over the patched walk, and the delta is made
                    // against the lists as they were.
                    lists::Patch::Whole(why, kept_lists) => {
                        tracing::info!(target: "blaze", why, "the lists are made whole");
                        work.asked = patch_work.asked;
                        lists = Some(kept_lists);
                        nodes
                    }
                }
            }
            (_, kept_lists) => {
                lists = kept_lists;
                nodes
            }
        };
        let owning =
            tracing::info_span!(target: "blaze", "codegen owners", functions = nodes.len())
                .entered();
        // Over the columns, whose iterators clone: the owners are read before they are checked.
        let owned = owners::owners(
            &nodes.iter().map(|(key, node)| owners::Function {
                key: *key,
                item: node.unit.item,
                owner: node.described.owner,
                code: node.unit.instance.code,
                refs: &node.refs,
                name: &node.described.name,
                origin: node.described.origin,
            }),
            &statics.iter().map(|(id, node)| owners::StaticOf {
                id: *id,
                unit: InstanceKey(node.described.value),
                owner: node.described.owner,
                refs: &node.refs,
                symbol: node.described.symbol.as_deref(),
                origin: node.described.origin,
            }),
            &mut out.errors,
        );
        // alloc: per build, the keys of its functions written in workspace crates
        let editable: Vec<InstanceKey> = nodes
            .iter()
            .filter(|(_, node)| node.described.editable)
            .map(|(key, _)| *key)
            .collect();
        drop(owning);
        {
            let _span = tracing::info_span!(
                target: "blaze",
                "codegen symbols",
                functions = nodes.len(),
                statics = statics.len(),
            )
            .entered();
            symbols_defined_twice::<B>(&nodes, &statics, &mut out.errors);
        }
        // Set with `self.last`, refused or not: `tier_up` reads the two together.
        // alloc: per build, sized by its statics' references
        self.static_callees =
            statics.values().flat_map(|it| it.refs.iter().filter_map(callee)).collect();
        if !out.errors.is_empty() {
            let errors = out.finish(names_of::<_, _, B>(&nodes, Some(&statics))).errors;
            // A refused build is still the last run, as one whose compiles failed is: a lazy or
            // live compile after it never reads an earlier build's walk.
            // alloc: once per build, the walk its `Later` holds
            self.last = Later { nodes: Arc::new(nodes), ..Later::default() };
            return Err(errors);
        }
        out.stats.instances = nodes.len();
        out.stats.statics = statics.len();

        let lazily = match later {
            None => {
                {
                    let _span = tracing::info_span!(target: "blaze", "codegen templates", functions = nodes.len()).entered();
                    self.dev(&nodes, store, &mut out);
                }
                if self.frontend.stopped() {
                    // alloc: an error, once
                    return Err(vec![stopped()]);
                }
                let release: Vec<Job<'_, B>> = nodes
                    .iter()
                    .filter(|(_, node)| node.spec.backend.is_release())
                    // clone: per build, a release job's unit (plain IDs)
                    .map(|(key, node)| Job {
                        name: *key,
                        unit: node.unit.clone(),
                        spec: &node.spec,
                        node,
                    })
                    // alloc: per build, sized by its units
                    .collect();
                {
                    let _span = tracing::info_span!(target: "blaze", "codegen release", functions = release.len()).entered();
                    self.release(&nodes, &release, store, &mut out);
                }
                later::Lazily::default()
            }
            Some(entry) => self.later(&nodes, &statics, &root_keys, entry, &order, store, &mut out),
        };
        if self.frontend.stopped() {
            // alloc: an error, once
            return Err(vec![stopped()]);
        }
        {
            let _span =
                tracing::info_span!(target: "blaze", "codegen statics", statics = statics.len())
                    .entered();
            self.statics(&statics, None, store, &mut out);
        }
        out.profiled(self.policy.pgo);
        let _finishing =
            tracing::info_span!(target: "blaze", "codegen finish", functions = nodes.len())
                .entered();

        let mergeable = |key: &InstanceKey| match nodes.get(key) {
            Some(node) => !node.external(),
            None => true,
        };
        let deduping =
            tracing::info_span!(target: "blaze", "codegen dedupe", functions = out.functions.len())
                .entered();
        // A build kept by row holds each function as it was made: what dedupe changes of
        // them is kept as given.
        let keeps_rows = self.kept.is_some() && later.is_none();
        let mut originals = Originals::default();
        let deduped = dedupe_keeping(
            &mut out.functions,
            &mut out.statics,
            &mergeable,
            keeps_rows.then_some(&mut originals),
        );
        drop(deduping);
        let deduped = match deduped {
            Ok((deduped, renamed)) => {
                // A static that named a function merged away has another value: its digest
                // is made again. `static_digests` is in `statics`' order, each pushed with its
                // static.
                for at in renamed {
                    let value = &out.statics[at];
                    debug_assert_eq!(
                        out.static_digests[at].0,
                        (value.item, value.instance),
                        "pushed together"
                    );
                    out.static_digests[at].1 = digests::static_definition(value);
                }
                deduped
            }
            Err(why) => {
                let item = nodes.get(&why.unit()).map_or(why.item(), |it| it.unit.item);
                out.errors.push(error(why.unit(), item, why.to_string()));
                return Err(out.finish(names_of::<_, _, B>(&nodes, Some(&statics))).errors);
            }
        };
        out.statics.sort_by_key(|it| (it.item, it.instance));
        let mut static_digests = std::mem::take(&mut out.static_digests);
        static_digests.sort_unstable_by_key(|it| it.0);
        let pieces = Digests {
            functions: digests::of_kept(std::mem::take(&mut out.function_digests), &out.functions),
            statics: static_digests,
            // alloc: per build, what it gives the linker
            debug_names: nodes.iter().map(|(key, node)| (*key, node.name_digest)).collect(),
        };

        // clone: per build, the symbols the build gives the linker
        let built_symbols = |node: &Node<B>| node.described.symbol.clone();
        let symbols = nodes
            .iter()
            .filter_map(|(key, node)| {
                Some((*key, built_symbols(node)?, node.described.linkage))
            })
            // alloc: per build, what it gives the linker
            .collect();
        let static_symbols = statics
            .iter()
            // clone: per build, the symbols the build gives the linker
            .filter_map(|((item, instance), node)| {
                Some((*item, *instance, node.described.symbol.clone()?, node.described.linkage))
            })
            // alloc: per build, what it gives the linker
            .collect();
        // alloc: per build, what it gives the linker
        let patchable = nodes.iter().filter(|it| it.1.slotted()).map(|it| *it.0).collect();
        // alloc: per build, what it gives the linker
        let tiered = nodes.iter().filter(|it| it.1.then.is_some()).map(|it| *it.0).collect();
        // alloc: per build, what it gives the linker and the store's record
        let inputs = nodes.iter().map(|(key, node)| (*key, node.unit.instance.code)).collect();
        let mut items: Vec<ItemKey> = nodes
            .values()
            .map(|it| it.unit.item)
            .chain(statics.iter().map(|(id, _)| id.0))
            // alloc: per build, what it gives the linker
            .collect();
        items.sort_unstable();
        items.dedup();
        let debug_names = nodes
            .iter()
            // clone: per build, the names the build gives the linker
            .map(|(key, node)| (*key, node.described.name.clone()))
            // alloc: per build, what it gives the linker
            .collect();
        // alloc: per build, what it gives the linker's address map
        let mut identities: Vec<(InstanceKey, ContentId)> =
            nodes.iter().map(|(key, node)| (*key, node.subject())).collect();
        identities.append(&mut out.folded);
        // alloc: per build, what it gives the linker
        let cold = nodes.iter().filter(|it| it.1.described.cold).map(|it| *it.0).collect();
        let made = nodes
            .iter()
            .map(|(key, node)| {
                (*key, crate::Made { backend: node.spec.backend, origin: node.least_origin })
            })
            // alloc: per build, what it gives the linker
            .collect();
        let reach = Reach::new(
            nodes.iter().map(|(key, node)| {
                let marks = [node.unit.instance.code.0, node.unit.instance.args];
                (*key, reach::Node { marks, refs: &node.refs })
            }),
            statics.iter().map(|(id, node)| {
                let marks = [node.described.value, node.described.instance];
                (*id, reach::Node { marks, refs: &node.refs })
            }),
        );
        // Each instance's two hashes are its own: on every core, in key order.
        // alloc: per build, what it gives the test binary's table
        // alloc: per build, its nodes in key order, for the cores to share
        let in_order: Vec<&Node<B>> = nodes.values().collect();
        let versions = par::map(&in_order, |node| {
            let id = crate::exec::fn_id(node.subject(), node.unit.instance.settings);
            let item_of = |key: &InstanceKey| nodes.get(key).map(|it| it.unit.item);
            (id, crate::exec::bound(&node.described.bound, &node.refs, item_of))
        });
        out.used.extend(lazily.shapes.values().copied());
        let Finished {
            functions,
            statics: compiled_statics,
            puts,
            stats,
            errors,
            used,
            kernel,
            misses,
        } = out.finish(names_of::<_, _, B>(&nodes, Some(&statics)));
        // alloc: once per build, the walk its `Later` and the kept walk share
        let nodes = Arc::new(nodes);
        // clone: a reference count, once per build
        self.last = Later { nodes: nodes.clone(), shapes: lazily.shapes };
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut built = Built {
            functions,
            statics: compiled_statics,
            roots: root_keys,
            symbols,
            static_symbols,
            patchable,
            tiered,
            deduped,
            puts,
            stats,
            inputs,
            items,
            debug_names,
            reach,
            walked: lazily.walked,
            entry: lazily.entry,
            owners: owned.functions,
            static_owners: owned.statics,
            editable,
            versions,
            digests: pieces,
            memo_used: used,
            identities,
            cold,
            made,
            kernel,
            misses,
            walk,
            id: BuiltId::default(),
            delta: Delta::default(),
            work,
        };
        // What the build changed since the last build of the same program: the program is
        // every input of the walk beside the front end's own, which the kept walk is of
        // (its database and configuration): the roots, the check's roots, the policy and
        // which kind of build with which entry.
        let span = tracing::info_span!(
            target: "blaze",
            "codegen delta",
            functions = built.inputs.len(),
            added = tracing::field::Empty,
            changed = tracing::field::Empty,
            removed = tracing::field::Empty,
            // Whether the next build of the program has a walk and lists to patch (`yes`),
            // or why it has none: a first edit that walks whole after a cold build is
            // explained by the cold build's own row.
            walk_kept = tracing::field::Empty,
            lists_kept = tracing::field::Empty,
            kept_functions = tracing::field::Empty,
        );
        let _entered = span.enter();
        let program = kept::program(&(roots, static_roots, check, policy_id.0, later));
        // The base: the last `Built` of the program, kept by row or, for a build that is
        // not, by its entries.
        let before: Option<Arc<delta::Last>> = match lists.take() {
            // alloc: once per whole build after one kept by row, its entries
            Some(lists) => Some(Arc::new(lists.into_last())),
            None => self.kept.and_then(|kept| kept.last(program)),
        };
        let (entries, codes) = delta::function_entries_and_codes(&built);
        let now = delta::Last::of_entries(&built, entries, before.as_deref());
        let fresh = match self.kept {
            None => Some(Fresh::Unkept),
            Some(_) => Some(walk.fresh.unwrap_or(Fresh::First)),
        };
        built.delta = delta::between(before.as_ref(), &now, fresh);
        built.id = now.id;
        span.record("added", built.delta.functions.added.len());
        span.record("changed", built.delta.functions.changed.len());
        span.record("removed", built.delta.functions.removed.len());
        walk.dropped = built.delta.functions.removed.len();
        built.walk = walk;
        let mut lists_unkept: Option<&'static str> = match (self.kept, later) {
            (None, _) => Some("the build keeps no walk"),
            (Some(_), Some(_)) => Some("a lazy build's lists are kept by its entries"),
            (Some(_), None) => None,
        };
        if let Some(kept) = self.kept {
            // Kept by row for the next build to patch, where every function is compiled
            // alone; else by its entries, for the next build's delta.
            let now = match keeps_rows {
                true => match lists::Lists::of_whole(&*nodes, &statics, &built, &originals, now, &codes)
                {
                    Ok(lists) => {
                        kept.keep_lists(program_walk, lists);
                        kept.forget_built(program);
                        None
                    }
                    Err(now) => {
                        lists_unkept = Some(
                            "a function is folded or built by a release backend, or the \
                             build's lists are not a row a function",
                        );
                        Some(now)
                    }
                },
                false => Some(now),
            };
            if let Some(now) = now {
                // alloc: once per build, what the next build's delta is made against
                kept.built(program, Arc::new(now));
            }
        }
        if unkept.is_none() && view.is_none() {
            unkept = Some("the front end tells no durable view");
        }
        let (walk_kept, lists_kept) = (unkept.unwrap_or("yes"), lists_unkept.unwrap_or("yes"));
        span.record("walk_kept", walk_kept);
        span.record("lists_kept", lists_kept);
        span.record("kept_functions", built.inputs.len());
        tracing::info!(
            target: "blaze",
            owner = self.owner,
            walk_kept,
            lists_kept,
            functions = built.inputs.len(),
            "what the build keeps of its program for the next build to patch"
        );
        // The walk of a build that ended well is the one the next build of the program
        // patches: with the front end's views from before its describes.
        if let (Some(keep), Some(view)) = (keep, view) {
            self.keep_walk(
                program_walk,
                roots,
                static_roots,
                check,
                policy_id,
                view,
                seen,
                nodes,
                statics,
                keep,
            );
        }
        Ok(built)
    }

    /// A live build's tiered functions at their last tier (#807): LLVM's code for the slots
    /// the build filled with Cranelift's. Nothing patchable is imported, as an edit replaces
    /// it alone.
    pub fn tier_up(&self, store: &impl Lookup) -> Result<TieredUp, Vec<CodegenError>> {
        let mut out = Out::default();
        let jobs: Vec<Job<'_, B>> = self
            .last
            .nodes
            .iter()
            .filter_map(|(key, node)| {
                let spec = node.then.as_ref()?;
                let instance = Instance { settings: spec.settings(), ..node.unit.instance };
                // clone: per tier-up, a job's unit (plain IDs)
                let unit = Unit { instance, backend: spec.backend, ..node.unit.clone() };
                Some(Job { name: *key, unit, spec, node })
            })
            // alloc: per tier-up, sized by its units
            .collect();
        let names: Vec<(InstanceKey, InstanceKey)> =
            // alloc: per tier-up, sized by its units
            jobs.iter().map(|job| (job.name, job.unit.instance.key())).collect();
        // Each slot's code is replaced by the function of its tier-up unit: two names of one
        // unit (first tiers of one code and arguments that differ only in what the last tier
        // drops) would leave the second without a replacement, so they are refused.
        let mut first_of: blaze_hash::HashMap<InstanceKey, &Job<'_, B>, Global> =
            // alloc: per tier-up, sized by its units
            blaze_hash::HashMap::with_capacity_in(jobs.len(), Global);
        for job in &jobs {
            if let Some(first) = first_of.insert(job.unit.instance.key(), job) {
                out.errors.push(error(
                    job.name,
                    job.unit.item,
                    // alloc: an error
                    format!(
                        "the function `{}` ({:?}) tiers up to the unit {:?}, as `{}` ({:?}) does: one compile can replace only one slot's code",
                        job.node.described.name,
                        job.name,
                        job.unit.instance.key(),
                        first.node.described.name,
                        first.name
                    ),
                ));
            }
        }
        if !out.errors.is_empty() {
            return Err(out.errors);
        }
        self.release(&*self.last.nodes, &jobs, store, &mut out);
        out.profiled(self.policy.pgo);
        let Finished { functions, puts, stats, errors, used, .. } =
            out.finish(names_of::<_, _, B>(&*self.last.nodes, None));
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut by_key: HashMap<InstanceKey, CompiledFunction> =
            // alloc: per tier-up, sized by its units
            functions.into_iter().map(|it| (it.key, it)).collect();
        let replaced = names
            .into_iter()
            .filter_map(|(name, key)| Some((name, by_key.remove(&key)?)))
            // alloc: per tier-up, sized by its units
            .collect();
        Ok(TieredUp { replaced, puts, stats, memo_used: used })
    }

    fn dev(&self, nodes: &Sorted<InstanceKey, Node<B>>, store: &impl Lookup, out: &mut Out) {
        let dev: Vec<InstanceKey> = nodes
            .iter()
            .filter(|(_, node)| !node.spec.backend.is_release())
            .map(|(key, _)| *key)
            // alloc: per build, sized by its units
            .collect();
        self.dev_of(nodes, &dev, store, out);
    }

    /// The functions `dev` of `nodes`, none of a release backend, each made of its template:
    /// from the store, else the memo, else compiled; those the policy folds first. A whole
    /// build's are all of its dev functions; a patched one's are those its walk touched
    /// ([`lists`]), none of which is folded.
    fn dev_of(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        dev: &[InstanceKey],
        store: &impl Lookup,
        out: &mut Out,
    ) {
        let mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)> = HashMap::new();

        // Folding: every candidate's masked IR from its own IR, prepared in this build. No key
        // known before preparing holds what a masked data object is told by (its positions'
        // anchors, its file names), so a mask kept from another build or unit could name
        // other data than the IR a group merges. Each form interned once in `self.masks`
        // (hashed only when new).
        // alloc: per build, sized by its units
        let candidates_of: Vec<InstanceKey> =
            dev.iter().filter(|key| nodes[*key].spec.fold).copied().collect();
        // alloc: per build, sized by its units
        let mut masks: Vec<(InstanceKey, Option<Masked>)> = Vec::with_capacity(candidates_of.len());
        for (key, made) in self.prepare_all(nodes, &candidates_of, &mut out.errors) {
            match made {
                Ok((shape, made)) => {
                    let node = &nodes[&key];
                    masks.push((key, self.backend.masked(&node.unit, &made, &node.refs)));
                    prepared.insert(key, (shape, made));
                }
                Err(why) => out.errors.push(why),
            }
        }
        masks.sort_unstable_by_key(|it| it.0);
        let forms = self.masks.table();
        let masked: Vec<(InstanceKey, Option<Masking<'_>>)> = (masks.iter())
            .map(|(key, it)| (*key, it.as_ref().map(|it| Masking::of(forms, it))))
            // alloc: per build, sized by its units
            .collect();
        // alloc: per build, sized by its units
        let mut candidates = Vec::new();
        for (key, it) in &masked {
            let (node, Some(it)) = (&nodes[key], it) else { continue };
            let places = node.refs.len();
            let beyond = it.constants.iter().find_map(|constant| match *constant {
                fold::Constant::Place(at) if at as usize >= places => Some(at),
                _ => None,
            });
            match beyond {
                Some(at) => out.errors.push(error(
                    *key,
                    node.unit.item,
                    // alloc: an error
                    format!("its masked IR names place {at} of its {places} references"),
                )),
                // What its code is kept under: the body of a group it is first in is its
                // code rewritten, so that body's key covers it (`fold::Group::first`), and its thunk's
                // key does (`fold::Group::stored`).
                None => candidates.push((
                    *key,
                    node.stored(),
                    node.unit.instance.settings,
                    *it,
                    node.refs.as_slice(),
                )),
            }
        }
        let groups = fold::groups(&candidates, &fold::Limits::default());
        // alloc: per build, sized by its units
        let grouped: HashSet<InstanceKey> =
            groups.iter().flat_map(|it| it.members.iter().copied()).collect();

        // Every other dev unit: its template from the store, the memo, or a compile.
        // alloc: per build, sized by its units
        let mut missing = Vec::new();
        // alloc: per build, the store hits the store check samples
        let mut sampled = Vec::new();
        for key in dev.iter().filter(|key| !grouped.contains(*key)) {
            let node = &nodes[key];
            let at = node.template_key();
            let read = out.read(store, &at, &node.unit, |it| {
                stored_function(it, *key, &node.refs, node.described.anchors.as_ref())
            });
            match read {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    out.kernel.push(KernelEntry {
                        instance: *key,
                        step: blaze_kernel::steps::DEV_TEMPLATE,
                        key: at,
                        hit: true,
                    });
                    let ir = prepared.remove(key);
                    if self.verify.sampled(&at.0) {
                        sampled.push((*key, at, ir.map(|it| it.1)));
                    }
                    out.made(&node.unit, &node.described.name, *key, Whence::Store(at), made);
                }
                Read::Miss => {
                    if self.why {
                        out.misses.push(node.missed(at, nodes));
                    }
                    missing.push(*key);
                }
                Read::Failed => {
                    prepared.remove(key);
                }
            }
        }
        self.verify_dev(nodes, sampled, store, out);
        // Each function the store missed is compiled by one build of the memo
        // ([`crate::flight`]), by its stored key: this one when it asks first, landed the
        // moment its compile ends; else the other's template, waited for, that function
        // alone, once this build's own compiles ended.
        let items: Vec<(InstanceKey, (InstanceKey, Option<(Shape, B::Prepared)>))> =
            // alloc: per build, sized by its store misses
            missing.into_iter().map(|key| (nodes[&key].flight(), (key, prepared.remove(&key)))).collect();
        let name = |(key, _): &(InstanceKey, Option<(Shape, B::Prepared)>)| {
            let node = &nodes[key];
            (*key, node.unit.item, node.described.name.as_str())
        };
        let flown = self.fly(
            items,
            name,
            |(key, own)| self.compile_missing(nodes, key, own),
            &mut out.errors,
        );
        for it in flown {
            match it {
                Flown::Ran(piece) => out.absorb(piece),
                Flown::Landed((key, own), shape) => {
                    self.landed(nodes, key, shape, own.as_ref().map(|(own, _)| own), out)
                }
            }
        }
        if self.frontend.stopped() {
            out.errors.push(stopped());
            return;
        }

        self.folded(nodes, &groups, prepared, store, out);
    }

    /// The store check of `sampled`, dev functions whose templates the store gave
    /// ([`Build::verify_store`]): each compiled again from its IR (the one folding prepared,
    /// else prepared here) with neither the store nor the memo, and its template's bytes
    /// compared with the stored.
    fn verify_dev(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        sampled: Vec<(InstanceKey, Key, Option<B::Prepared>)>,
        store: &impl Lookup,
        out: &mut Out,
    ) {
        if sampled.is_empty() {
            return;
        }
        // alloc: per build, the store hits the store check samples
        let unprepared: Vec<InstanceKey> =
            sampled.iter().filter(|it| it.2.is_none()).map(|it| it.0).collect();
        let mut made: HashMap<InstanceKey, B::Prepared> = HashMap::new();
        for (key, it) in self.prepare_all(nodes, &unprepared, &mut out.errors) {
            match it {
                Ok((_, ir)) => {
                    made.insert(key, ir);
                }
                Err(why) => out.errors.push(why),
            }
        }
        let jobs: Vec<(InstanceKey, Key, B::Prepared)> = (sampled.into_iter())
            .filter_map(|(key, at, ir)| Some((key, at, ir.or_else(|| made.remove(&key))?)))
            // alloc: per build, the store hits the store check samples
            .collect();
        let emitted = self.until_stopped(jobs, &mut out.errors, |(key, at, ir)| {
            let node = &nodes[&key];
            let emitted = self.backend.emit(&node.unit, &node.spec, ir, &node.refs, None);
            (key, at, emitted.map_err(|why| naming(why, &node.described.name)))
        });
        for (key, at, emitted) in emitted {
            let node = &nodes[&key];
            let emitted = match emitted {
                Ok(it) if it.function.key == key => it,
                Ok(it) => {
                    let what = format!(
                        "{}: compiled again as {:?}",
                        blaze_store_check::VAR,
                        it.function.key
                    ); // alloc: an error
                    out.errors.push(error(key, node.unit.item, what));
                    continue;
                }
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            if let Some(it) = unkept_inline(nodes, &emitted.function, node.built_in.iter().copied())
            {
                // alloc: an error
                out.errors.push(error(
                    key,
                    node.unit.item,
                    unkept_message(nodes, node, it, &node.built_in, &at),
                ));
                continue;
            }
            let Some(template) =
                out.template_of(&node.unit, emitted, &node.refs, node.described.anchors.as_ref())
            else {
                continue;
            };
            out.verify(&node.unit, &node.described.name, store, at, &blob::encode(&template));
        }
    }

    /// Function `key`, whose template another build compiled into the memo under `shape`,
    /// made with this build's places: neither prepared nor compiled here. Where this build
    /// prepared it already (a fold candidate), its own shape must be the one landed: two
    /// shapes under one stored key are refused (rule 21), naming both.
    fn landed(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        key: InstanceKey,
        shape: ContentId,
        own: Option<&Shape>,
        out: &mut Out,
    ) {
        let node = &nodes[&key];
        if let Some(own) = own {
            let own = own.key();
            if own != shape {
                let (name, stored) = (&node.described.name, node.flight());
                out.errors.push(error(
                    key,
                    node.unit.item,
                    // alloc: an error
                    format!(
                        "the function `{name}` is refused: this build prepared it as shape {own:?}, and another build of the memo landed shape {shape:?} under the same flight key {stored:?}, so the key misses an input of its IR (rule 21)"
                    ),
                ));
                return;
            }
        }
        out.used(shape);
        match self.memo.get(&shape) {
            Some(template) => {
                out.stats.flown += 1;
                out.dev_template(key, &node.unit, node.template_key(), template);
                out.instantiate(
                    &node.unit,
                    &node.described.name,
                    key,
                    Whence::Shape(shape),
                    template,
                    &node.refs,
                    node.described.anchors.as_ref(),
                );
            }
            // A landing is dropped with its template ([`Memo::retain`]), between builds.
            None => out.errors.push(error(
                key,
                node.unit.item,
                // alloc: an error
                format!(
                    "its compile landed under shape {shape:?}, which the memo has no template of"
                ),
            )),
        }
    }

    /// Compiles function `key`, which the store missed and this build runs the flight of: its
    /// IR `own` when folding prepared it, else prepared here, and compiled unless the memo has
    /// its shape's template. One compile of a shape at once, of all the memo's builds: another
    /// function's of the same shape under way is waited for, and its template taken. What
    /// lands (the shape its template is kept under in the memo, put there before this returns)
    /// and what this build collected of it.
    fn compile_missing(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        key: InstanceKey,
        own: Option<(Shape, B::Prepared)>,
    ) -> (Option<ContentId>, Out) {
        let mut out = Out::default();
        let node = &nodes[&key];
        let (compiled, made) = match own {
            Some(it) => it,
            None => match prepare_node(self.backend, self.frontend, &node.unit, &node.spec, node) {
                Ok(it) => it,
                Err(why) => {
                    out.errors.push(why);
                    return (None, out);
                }
            },
        };
        let shape = compiled.key();
        out.used(shape);
        if self.memo.get(&shape).is_none() {
            let named = |it: &mut String| {
                use std::fmt::Write as _;
                _ = write!(
                    it,
                    "codegen: shape {shape:?} of `{}`, which another function of it compiles",
                    node.described.name
                );
            };
            let mut here = false;
            self.memo.flights().shape(shape, named, || {
                // Compiled by the run of another function of the shape that ended meanwhile.
                if self.memo.get(&shape).is_some() {
                    return;
                }
                let mut emitted =
                    match self.backend.emit(&node.unit, &node.spec, made, &node.refs, None) {
                        Ok(it) if it.function.key == key => it,
                        Ok(it) => {
                            // alloc: an error
                            out.errors.push(error(
                                key,
                                node.unit.item,
                                format!("compiled as {:?}", it.function.key),
                            ));
                            return;
                        }
                        Err(why) => {
                            out.errors.push(naming(why, &node.described.name));
                            return;
                        }
                    };
                if let Some(it) =
                    unkept_inline(nodes, &emitted.function, node.built_in.iter().copied())
                {
                    let message =
                        unkept_message(nodes, node, it, &node.built_in, &node.template_key());
                    // alloc: an error
                    out.errors.push(error(key, node.unit.item, message));
                    return;
                }
                out.stats.compiled += 1;
                out.side(By::node(key, &node.unit), &mut emitted);
                let Some(template) = out.template_of(
                    &node.unit,
                    emitted,
                    &node.refs,
                    node.described.anchors.as_ref(),
                ) else {
                    return;
                };
                let kept = self.memo.put(compiled, template);
                out.dev_template(key, &node.unit, node.template_key(), kept);
                out.instantiate(
                    &node.unit,
                    &node.described.name,
                    key,
                    Whence::Shape(shape),
                    kept,
                    &node.refs,
                    node.described.anchors.as_ref(),
                );
                here = true;
            });
            if here {
                return (Some(shape), out);
            }
        }
        match self.memo.get(&shape) {
            Some(template) => {
                out.stats.memo += 1;
                out.dev_template(key, &node.unit, node.template_key(), template);
                out.instantiate(
                    &node.unit,
                    &node.described.name,
                    key,
                    Whence::Shape(shape),
                    template,
                    &node.refs,
                    node.described.anchors.as_ref(),
                );
                (Some(shape), out)
            }
            // This function's compile failed (its error is in `out`), or another function's
            // of the shape did, waited for here.
            None if !out.errors.is_empty() => (None, out),
            None => {
                out.errors.push(error(
                    key,
                    node.unit.item,
                    // alloc: an error
                    format!("not compiled: the compile of its shape {shape:?}, by another function of it, failed"),
                ));
                (None, out)
            }
        }
    }

    /// Each group's body and thunks, from the store or merged from the members' IR.
    fn folded(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        groups: &[Group],
        mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)>,
        store: &impl Lookup,
        out: &mut Out,
    ) {
        let thunk_refs =
            |group: &Group, member: &InstanceKey| group.thunk_refs(&nodes[member].refs);
        // alloc: per build, sized by its groups
        let mut merging = Vec::new();
        for group in groups {
            let key = group.key();
            out.folded
                .push((key, folded_identity(group.members.iter().map(|it| nodes[it].subject()))));
            let first = &nodes[&group.members[0]];
            // alloc: an error
            let decode = |it: &[u8]| blob::decode(it).map_err(|e| e.to_string());
            let body = out.read(store, &keys::folded(&key), &first.unit, decode);
            let mut thunks = Vec::with_capacity(group.members.len());
            let mut failed = matches!(body, Read::Failed);
            for (member, stored) in group.members.iter().zip(&group.stored) {
                match out.read(store, &keys::thunk(&key, stored), &nodes[member].unit, decode) {
                    Read::Hit(it) => thunks.push(it),
                    Read::Miss => {}
                    Read::Failed => failed = true,
                }
            }
            match body {
                _ if failed => {}
                Read::Hit(body) if thunks.len() == group.members.len() => {
                    out.stats.stored += 1 + thunks.len();
                    out.stats.folded += group.members.len();
                    let whence = Whence::Folded(keys::folded(&key));
                    out.instantiate(
                        &first.unit,
                        &first.described.name,
                        key,
                        whence,
                        &body,
                        &first.refs,
                        first.described.anchors.as_ref(),
                    );
                    for ((member, stored), thunk) in
                        group.members.iter().zip(&group.stored).zip(&thunks)
                    {
                        let node = &nodes[member];
                        let whence = Whence::Store(keys::thunk(&key, stored));
                        let refs = thunk_refs(group, member);
                        out.instantiate(
                            &node.unit,
                            &node.described.name,
                            *member,
                            whence,
                            thunk,
                            &refs,
                            node.described.anchors.as_ref(),
                        );
                    }
                }
                _ => merging.push(group),
            }
        }
        // Each group the store missed is merged by one build of the memo ([`crate::flight`]),
        // by its key and members; its body and thunks are kept in the memo under their store
        // keys, so a build that waited for it or asks after takes them there.
        let items: Vec<(InstanceKey, (&Group, Vec<Option<(Shape, B::Prepared)>>))> = merging
            .into_iter()
            // alloc: per group the store missed, its members' IR
            .map(|group| {
                (
                    group_flight(group),
                    (group, group.members.iter().map(|it| prepared.remove(it)).collect()),
                )
            })
            // alloc: per build, sized by its groups
            .collect();
        let name = |(group, _): &(&Group, Vec<Option<(Shape, B::Prepared)>>)| {
            let first = &nodes[&group.members[0]];
            (group.key(), first.unit.item, first.described.name.as_str())
        };
        let flown = self.fly(
            items,
            name,
            |(group, own)| self.merge_group(nodes, group, own),
            &mut out.errors,
        );
        for it in flown {
            match it {
                Flown::Ran(piece) => out.absorb(piece),
                Flown::Landed((group, _), _) => self.landed_group(nodes, group, out),
            }
        }
        if self.frontend.stopped() {
            out.errors.push(stopped());
        }
    }

    /// Merges `group`, which the store missed and this build runs the flight of, from its
    /// members' IR: `own`, each member's that folding prepared, else prepared here. What lands
    /// (its body's id in the memo, once its body and every thunk are there) and what this build
    /// collected of it.
    fn merge_group(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        group: &Group,
        own: Vec<Option<(Shape, B::Prepared)>>,
    ) -> (Option<ContentId>, Out) {
        let mut out = Out::default();
        let thunk_refs =
            |group: &Group, member: &InstanceKey| group.thunk_refs(&nodes[member].refs);
        let members: Vec<Option<_>> = (group.members.iter().zip(own))
            .map(|(member, own)| {
                let node = &nodes[member];
                let made = match own {
                    Some((_, made)) => made,
                    None => match prepare_node(
                        self.backend,
                        self.frontend,
                        &node.unit,
                        &node.spec,
                        node,
                    ) {
                        Ok((_, made)) => made,
                        Err(why) => {
                            out.errors.push(why);
                            return None;
                        }
                    },
                };
                Some((&node.unit, &node.spec, made, node.refs.as_slice()))
            })
            // alloc: per merge, the members the backend takes
            .collect();
        // A member that failed to prepare is an error of the build already.
        let Some(members) = members.into_iter().collect::<Option<Vec<_>>>() else {
            return (None, out);
        };
        if self.frontend.stopped() {
            return (None, out);
        }
        let key = group.key();
        let first = &nodes[&group.members[0]];
        let mut merged = match self.backend.merge(group, members) {
            Ok(it) => it,
            Err(why) => {
                out.errors.push(why);
                return (None, out);
            }
        };
        if merged.body.function.key != key || merged.thunks.len() != group.members.len() {
            out.errors.push(error(key, first.unit.item, "a merge that is not of its group"));
            return (None, out);
        }
        out.stats.compiled += 1 + merged.thunks.len();
        out.stats.folded += group.members.len();
        out.side(By::folded(group.members[0], &first.unit), &mut merged.body);
        let body_at = keys::folded(&key);
        let mut whole = true;
        match out.template_of(
            &first.unit,
            merged.body,
            &first.refs,
            first.described.anchors.as_ref(),
        ) {
            Some(body) => {
                out.used(body_at.0);
                let by = By::folded(group.members[0], &first.unit);
                match out.keyed(self.memo, by, &first.unit, body_at, body) {
                    Some(kept) => {
                        let whence = Whence::Folded(body_at);
                        out.instantiate(
                            &first.unit,
                            &first.described.name,
                            key,
                            whence,
                            kept,
                            &first.refs,
                            first.described.anchors.as_ref(),
                        );
                    }
                    None => whole = false,
                }
            }
            None => whole = false,
        }
        for ((member, stored), mut thunk) in
            group.members.iter().zip(&group.stored).zip(merged.thunks)
        {
            let node = &nodes[member];
            if thunk.function.key != *member {
                out.errors.push(error(*member, node.unit.item, "a thunk of another member"));
                whole = false;
                continue;
            }
            out.side(By::node(*member, &node.unit), &mut thunk);
            let refs = thunk_refs(group, member);
            let at = keys::thunk(&key, stored);
            match out.template_of(&node.unit, thunk, &refs, node.described.anchors.as_ref()) {
                Some(template) => {
                    out.used(at.0);
                    match out.keyed(
                        self.memo,
                        By::node(*member, &node.unit),
                        &node.unit,
                        at,
                        template,
                    ) {
                        Some(kept) => {
                            out.instantiate(
                                &node.unit,
                                &node.described.name,
                                *member,
                                Whence::Store(at),
                                kept,
                                &refs,
                                node.described.anchors.as_ref(),
                            );
                        }
                        None => whole = false,
                    }
                }
                None => whole = false,
            }
        }
        (whole.then_some(body_at.0), out)
    }

    /// Group `group`, which another build merged into the memo, made with this build's places:
    /// its body and each member's thunk by their store keys, nothing prepared or merged here.
    /// Nothing is compared: this build holds no template of its own here, and each the memo
    /// has under a store key went in through [`Memo::put_keyed`], which refuses another, so a
    /// read of it is as a store hit is.
    fn landed_group(&self, nodes: &Sorted<InstanceKey, Node<B>>, group: &Group, out: &mut Out) {
        let key = group.key();
        let first = &nodes[&group.members[0]];
        let body_at = keys::folded(&key);
        let thunks: Option<Vec<(&InstanceKey, Key, &Template)>> = group
            .members
            .iter()
            .zip(&group.stored)
            .map(|(member, stored)| {
                let at = keys::thunk(&key, stored);
                Some((member, at, self.memo.get(&at.0)?))
            })
            // alloc: per group another build merged, sized by its members
            .collect();
        let (Some(body), Some(thunks)) = (self.memo.get(&body_at.0), thunks) else {
            // A landing is dropped with its templates ([`Memo::retain`]), between builds.
            out.errors.push(error(
                key,
                first.unit.item,
                // alloc: an error
                format!("its merge landed under {body_at:?}, and the memo lacks its body or one of its thunks"),
            ));
            return;
        };
        out.stats.flown += 1 + thunks.len();
        out.stats.folded += group.members.len();
        out.used(body_at.0);
        out.template(By::folded(group.members[0], &first.unit), body_at, body);
        out.instantiate(
            &first.unit,
            &first.described.name,
            key,
            Whence::Folded(body_at),
            body,
            &first.refs,
            first.described.anchors.as_ref(),
        );
        for (member, at, thunk) in thunks {
            let node = &nodes[member];
            out.used(at.0);
            out.template(By::node(*member, &node.unit), at, thunk);
            let refs = group.thunk_refs(&node.refs);
            out.instantiate(
                &node.unit,
                &node.described.name,
                *member,
                Whence::Store(at),
                thunk,
                &refs,
                node.described.anchors.as_ref(),
            );
        }
    }

    fn release(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        jobs: &[Job<'_, B>],
        store: &impl Lookup,
        out: &mut Out,
    ) {
        if jobs.is_empty() {
            return;
        }
        // alloc: per build, sized by its units
        let by_name: HashMap<InstanceKey, &Job<'_, B>> =
            jobs.iter().map(|job| (job.name, job)).collect();
        let prepare = |names: &[InstanceKey],
                       errors: &mut Vec<CodegenError>|
         -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
            self.until_stopped(names, errors, |name| {
                let job = by_name[name];
                let made = prepare_node(self.backend, self.frontend, &job.unit, job.spec, job.node);
                (*name, made)
            })
        };

        // Every summary: stored, else from the IR, which is kept for the compile, else the one
        // another build of the memo made ([`crate::flight`]), by its store key: this build then
        // prepares the function only if it compiles or imports it. Sorted by key once, in
        // `imports::decide`.
        let mut summaries: blaze_hash::HashMap<InstanceKey, Summary, Global> =
            blaze_hash::HashMap::with_capacity_in(jobs.len(), Global);
        let mut prepared: HashMap<InstanceKey, B::Prepared> = HashMap::new();
        let mut exports: HashMap<InstanceKey, B::Export> = HashMap::new();
        // alloc: per build, sized by its units
        let mut unsummarized = Vec::new();
        {
            let _span = tracing::info_span!(target: "blaze", "codegen summaries read", functions = jobs.len()).entered();
            for job in jobs {
                let read = out.read(store, &keys::summary(&job.summarized()), &job.unit, |it| {
                    // alloc: an error
                    Summary::decode(it).map_err(|e| e.to_string())
                });
                match read {
                    Read::Hit(summary) => {
                        summaries.insert(job.name, summary);
                    }
                    Read::Miss => unsummarized.push(job.name),
                    Read::Failed => {}
                }
            }
        }
        let summarize = |made: Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)>,
                         errors: &mut Vec<CodegenError>| {
            self.until_stopped(made, errors, |(name, made)| {
                let job = by_name[&name];
                let summarized = made.and_then(|(_, mut made)| {
                    let (summary, export) =
                        self.backend.summarize(&job.unit, job.spec, &mut made, &job.node.refs)?;
                    Ok((summary, export, made))
                });
                (name, summarized)
            })
        };
        let summary_flight =
            |name: &InstanceKey| InstanceKey(keys::summary(&by_name[name].summarized()).0);
        let flights = self.memo.flights();
        // Each summary the store missed is made by one build of the memo, by its flight key,
        // and landed the moment it is kept; else the other's, waited for alone.
        // alloc: per build, sized by its store misses
        let items: Vec<(InstanceKey, InstanceKey)> =
            unsummarized.iter().map(|name| (summary_flight(name), *name)).collect();
        let name = |name: &InstanceKey| {
            let job = by_name[name];
            (job.name, job.unit.item, job.node.described.name.as_str())
        };
        let summarize_one = |name: InstanceKey| {
            let mut piece = Out::default();
            let job = by_name[&name];
            let made = prepare_node(self.backend, self.frontend, &job.unit, job.spec, job.node);
            let summarized = made.and_then(|(_, mut made)| {
                let (summary, export) =
                    self.backend.summarize(&job.unit, job.spec, &mut made, &job.node.refs)?;
                Ok((summary, export, made))
            });
            match summarized {
                Ok((summary, export, made)) => {
                    let bytes = summary.encode();
                    piece.put(
                        By::node(job.name, &job.unit),
                        keys::summary(&job.summarized()),
                        Kind::Query,
                        bytes,
                    );
                    let flight = summary_flight(&name);
                    piece.used(flight.0);
                    // clone: per summary made, the one the memo's other builds take
                    if !flights.keep_summary(flight, summary.clone()) {
                        piece.errors.push(error(
                            job.name,
                            job.unit.item,
                            // alloc: an error
                            format!(
                                "the function `{}` is refused: its summary and another build's are not one under the same flight key {flight:?}, so the key misses an input of its summary (rule 21)",
                                job.node.described.name
                            ),
                        ));
                    }
                    (Some(flight.0), (piece, Some((name, summary, export, made))))
                }
                Err(why) => {
                    piece.errors.push(naming(why, &job.node.described.name));
                    (None, (piece, None))
                }
            }
        };
        // alloc: per batch of summaries another build made, sized by it
        let mut flown = Vec::new();
        for it in self.fly(items, name, summarize_one, &mut out.errors) {
            match it {
                Flown::Ran((piece, made)) => {
                    out.absorb(piece);
                    if let Some((name, summary, export, made)) = made {
                        summaries.insert(name, summary);
                        exports.insert(name, export);
                        prepared.insert(name, made);
                    }
                }
                Flown::Landed(name, _) => flown.push((name, summary_flight(&name))),
            }
        }
        self.flown_summaries(&by_name, flown, &mut summaries, out);
        if self.frontend.stopped() {
            out.errors.push(stopped());
            return;
        }
        // What only this build knows of a function: whether code outside it calls it, or
        // anything without a summary (another phase's unit, a static's value) refers to it.
        for (name, summary) in &mut summaries {
            summary.key = *name;
        }
        let outside: blaze_hash::HashSet<InstanceKey, Global> = jobs
            .iter()
            .filter(|job| job.node.external())
            .map(|job| job.name)
            .chain(
                nodes
                    .iter()
                    .filter(|(key, _)| !by_name.contains_key(*key))
                    .flat_map(|(_, node)| node.refs.iter().filter_map(callee)),
            )
            .chain(self.static_callees.iter().copied())
            // alloc: per build, sized by its units
            .collect();
        imports::close(&mut summaries, &outside);
        let decided = match imports::decide(&summaries, &imports::Limits::default()) {
            Ok(it) => it,
            Err(why) => {
                let job = &jobs[0];
                // alloc: an error
                out.errors.push(error(
                    job.name,
                    job.unit.item,
                    format!("deciding the imports: {why}"),
                ));
                return;
            }
        };
        let patchable = |key: &InstanceKey| nodes.get(key).is_some_and(|node| node.patchable);
        // The functions already refused, each by the error that says why (its summary failed,
        // or its stored one could not be read): one without a decision for that reason is not
        // refused a second time.
        let refused: blaze_hash::HashSet<InstanceKey, Global> =
            // alloc: per build, sized by its errors
            out.errors.iter().map(|it| it.unit).collect();
        // `None` for a function the decision has no entry of: one with no summary, which is
        // refused, never compiled as if it imported nothing.
        // A rebound function's body is no edit's, so a copy of it never goes stale; but the
        // importer's fill then holds its references, which an edit renames: only a function
        // itself reached through its slot imports one.
        let rebound = |key: &InstanceKey| nodes.get(key).is_some_and(|node| node.rebound);
        let imports_of = |name: &InstanceKey| -> Option<Imports> {
            // clone: per release unit, its decision, trimmed of what it may not import
            let mut imports = decided.get(name)?.clone();
            let slotted = nodes.get(name).is_some_and(|node| node.slotted());
            imports.imported.retain(|it| {
                !patchable(it) && (slotted || !rebound(it)) && by_name.contains_key(it)
            });
            imports.callees.retain(|it| !patchable(&it.0));
            imports.dead.retain(|it| !patchable(&it.0));
            Some(imports)
        };

        // Each compile by its blob key: stored, else compiled with its imports' exports.
        // alloc: per build, sized by its units
        let mut missing = Vec::new();
        // alloc: per build, the store hits the store check samples
        let mut sampled = Vec::new();
        for job in jobs {
            let Some(imports) = imports_of(&job.name) else {
                if refused.contains(&job.name) || refused.contains(&job.unit.instance.key()) {
                    continue;
                }
                out.errors.push(error(
                    job.name,
                    job.unit.item,
                    // alloc: an error
                    format!(
                        "the function `{}` has no import decision: no summary of it was made or flown in, so it can't be compiled with its imports",
                        job.node.described.name
                    ),
                ));
                continue;
            };
            let imported: Vec<&[Target]> =
                // alloc: per release unit, sized by its imports
                imports.imported.iter().map(|it| by_name[it].node.refs.as_slice()).collect();
            let imported_built_in: Vec<&[InstanceKey]> =
                // alloc: per release unit, sized by its imports
                imports.imported.iter().map(|it| by_name[it].node.built_in.as_slice()).collect();
            // Each import's unit as its export is built, at its own settings: in a tier-up
            // its walk key is the first tier's.
            let imported_units: Vec<InstanceKey> =
                // alloc: per release unit, sized by its imports
                imports.imported.iter().map(|it| by_name[it].unit.instance.key()).collect();
            let (refs, blob_key) = release_template_key(
                &Instance { code: job.node.described.template_code, ..job.unit.instance },
                job.unit.instance.key(),
                &job.node.built_in,
                &job.node.refs,
                &imports,
                &imported_units,
                &imported,
                &imported_built_in,
            );
            let key = job.unit.instance.key();
            match out.read(store, &blob_key, &job.unit, |it| {
                stored_function(it, key, &refs, job.node.described.anchors.as_ref())
            }) {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    out.made(
                        &job.unit,
                        &job.node.described.name,
                        key,
                        Whence::Store(blob_key),
                        made,
                    );
                    if self.verify.sampled(&blob_key.0) {
                        sampled.push((job.name, imports, refs, blob_key));
                    }
                }
                Read::Miss => missing.push((job.name, imports, refs, blob_key)),
                Read::Failed => {}
            }
        }
        // Each compile the store missed is one build's of the memo ([`crate::flight`]), by its
        // blob key; its template is kept in the memo under that key, a domain of its own, and
        // lands the moment it is, so a build that waited for it or asks after takes it there.
        // What a compile needs, its IR and its imports' exports, is made in its run, so only
        // the build that runs it prepares for it: its IR as the summaries left it, else made
        // here; an import's export as the summaries left it, else made once of this build's
        // runs ([`Exports`]), its IR then kept for that import's own compile.
        let items: Vec<(
            InstanceKey,
            (InstanceKey, Imports, Vec<Target>, Key, Option<B::Prepared>),
        )> = (missing.into_iter())
            .map(|(name, imports, refs, blob_key)| {
                (InstanceKey(blob_key.0), (name, imports, refs, blob_key, prepared.remove(&name)))
            })
            // alloc: per build, sized by its store misses
            .collect();
        let name = |it: &(InstanceKey, Imports, Vec<Target>, Key, Option<B::Prepared>)| {
            let job = by_name[&it.0];
            (job.name, job.unit.item, job.node.described.name.as_str())
        };
        let made_here = Exports::<B>::default();
        let compile = |(name, imports, refs, blob_key, made): (
            InstanceKey,
            Imports,
            Vec<Target>,
            Key,
            Option<B::Prepared>,
        )| {
            let mut out = Out::default();
            let job = by_name[&name];
            let key = job.unit.instance.key();
            let made = match made.or_else(|| made_here.spare.lock().remove(&name)) {
                Some(made) => made,
                None => {
                    match prepare_node(self.backend, self.frontend, &job.unit, job.spec, job.node) {
                        Ok((_, made)) => made,
                        Err(why) => {
                            out.errors.push(why);
                            return (None, out);
                        }
                    }
                }
            };
            // Each import's export: the summaries', else made here; one that failed is an
            // error of the build already, and this compile's that it was not made.
            let made_exports: Vec<(InstanceKey, Arc<B::Export>)> = (imports.imported.iter())
                .filter(|it| !exports.contains_key(*it))
                .filter_map(|it| {
                    Some((*it, made_here.export(self, &by_name, *it, &mut out.errors)?))
                })
                // alloc: per release compile, sized by its imports
                .collect();
            let export_of = |it: &InstanceKey| {
                exports
                    .get(it)
                    .or_else(|| made_exports.iter().find(|made| made.0 == *it).map(|made| &*made.1))
            };
            let mut emitted =
                match self.emit_release(job, &by_name, export_of, &imports, &refs, made) {
                    Ok(it) if it.function.key == key => it,
                    Ok(it) => {
                        // alloc: an error
                        out.errors.push(error(
                            key,
                            job.unit.item,
                            format!("compiled as {:?}", it.function.key),
                        ));
                        return (None, out);
                    }
                    Err(why) => {
                        out.errors.push(why);
                        return (None, out);
                    }
                };
            let kept = release_kept(job, &by_name, &imports);
            if let Some(it) = unkept_inline(nodes, &emitted.function, kept.iter().copied()) {
                // alloc: an error
                out.errors.push(error(
                    key,
                    job.unit.item,
                    unkept_message(nodes, job.node, it, &kept, &blob_key),
                ));
                return (None, out);
            }
            out.stats.compiled += 1;
            out.stats.imported += imports.imported.len();
            out.side(By::node(job.name, &job.unit), &mut emitted);
            let Some(template) =
                out.template_of(&job.unit, emitted, &refs, job.node.described.anchors.as_ref())
            else {
                return (None, out);
            };
            out.used(blob_key.0);
            let Some(kept) =
                out.keyed(self.memo, By::node(job.name, &job.unit), &job.unit, blob_key, template)
            else {
                return (None, out);
            };
            out.instantiate(
                &job.unit,
                &job.node.described.name,
                key,
                Whence::Store(blob_key),
                kept,
                &refs,
                job.node.described.anchors.as_ref(),
            );
            (Some(blob_key.0), out)
        };
        for it in self.fly(items, name, compile, &mut out.errors) {
            match it {
                Flown::Ran(piece) => out.absorb(piece),
                Flown::Landed((name, _, refs, blob_key, _), _) => {
                    self.landed_release(&by_name, &name, &refs, blob_key, out)
                }
            }
        }
        if self.frontend.stopped() {
            out.errors.push(stopped());
            return;
        }

        // The store check of the release templates the store gave ([`Build::verify_store`]):
        // each compiled again with its imports' exports, neither the store nor the memo read.
        if sampled.is_empty() {
            return;
        }
        let mut needed: Vec<InstanceKey> = (sampled.iter())
            .flat_map(|(name, imports, _, _)| {
                let own = std::iter::once(name).filter(|it| !prepared.contains_key(*it));
                own.chain(imports.imported.iter().filter(|it| !exports.contains_key(*it)))
            })
            .copied()
            // alloc: per build, the store hits the store check samples
            .collect();
        needed.sort();
        needed.dedup();
        let made = prepare(&needed, &mut out.errors);
        for (name, summarized) in summarize(made, &mut out.errors) {
            match summarized {
                Ok((_, export, made)) => {
                    exports.entry(name).or_insert(export);
                    prepared.entry(name).or_insert(made);
                }
                Err(why) => out.errors.push(naming(why, &by_name[&name].node.described.name)),
            }
        }
        if self.frontend.stopped() {
            out.errors.push(stopped());
            return;
        }
        let compiles: Vec<_> = (sampled.into_iter())
            .filter_map(|(name, imports, refs, blob_key)| {
                Some((name, imports, refs, blob_key, prepared.remove(&name)?))
            })
            // alloc: per build, the store hits the store check samples
            .collect();
        let exports = &exports;
        let emitted = self.until_stopped(
            compiles,
            &mut out.errors,
            |(name, imports, refs, blob_key, made)| {
                let emitted = self.emit_release(
                    by_name[&name],
                    &by_name,
                    |it| exports.get(it),
                    &imports,
                    &refs,
                    made,
                );
                (name, imports, refs, blob_key, emitted)
            },
        );
        for (name, imports, refs, blob_key, emitted) in emitted {
            let job = by_name[&name];
            let key = job.unit.instance.key();
            match emitted {
                Ok(it) if it.function.key == key => {
                    let kept = release_kept(job, &by_name, &imports);
                    if let Some(code) = unkept_inline(nodes, &it.function, kept.iter().copied()) {
                        let message = unkept_message(nodes, job.node, code, &kept, &blob_key);
                        // alloc: an error
                        out.errors.push(error(
                            key,
                            job.unit.item,
                            format!("{}: {message}", blaze_store_check::VAR),
                        ));
                        continue;
                    }
                    if let Some(template) =
                        out.template_of(&job.unit, it, &refs, job.node.described.anchors.as_ref())
                    {
                        out.verify(
                            &job.unit,
                            &job.node.described.name,
                            store,
                            blob_key,
                            &blob::encode(&template),
                        );
                    }
                }
                Ok(it) => {
                    let what = format!(
                        "{}: compiled again as {:?}",
                        blaze_store_check::VAR,
                        it.function.key
                    ); // alloc: an error
                    out.errors.push(error(key, job.unit.item, what));
                }
                Err(why) => out.errors.push(why),
            }
        }
    }

    /// Release compile `job` of `made` with `refs` and its `imports`, each import's export
    /// what `export_of` gives.
    fn emit_release<'x>(
        &self,
        job: &Job<'_, B>,
        by_name: &HashMap<InstanceKey, &Job<'_, B>>,
        export_of: impl Fn(&InstanceKey) -> Option<&'x B::Export>,
        imports: &Imports,
        refs: &[Target],
        made: B::Prepared,
    ) -> Result<Emitted, CodegenError>
    where
        B::Export: 'x,
    {
        let list: Result<Vec<Import<'_, B::Export>>, InstanceKey> = imports
            .imported
            .iter()
            .map(|it| {
                Ok(Import {
                    key: *it,
                    export: export_of(it).ok_or(*it)?,
                    refs: &by_name[it].node.refs,
                })
            })
            // alloc: per release compile, sized by its imports
            .collect();
        match list {
            Ok(list) => self
                .backend
                .emit(&job.unit, job.spec, made, refs, Some((imports, list.as_slice())))
                .map_err(|why| naming(why, &job.node.described.name)),
            Err(import) => {
                let named = by_name
                    .get(&import)
                    .map_or("a function of no job", |it| &it.node.described.name);
                // alloc: an error
                let what = format!(
                    "the function `{}` imports `{named}` ({import:?}), whose export was not made",
                    job.node.described.name
                );
                Err(error(job.unit.instance.key(), job.unit.item, what))
            }
        }
    }

    /// Release compile `name` with `refs`, which another build compiled into the memo under
    /// its blob key `blob_key`, made with this build's places: nothing prepared or compiled.
    /// Nothing is compared, as in [`Self::landed_group`]: the template went in through
    /// [`Memo::put_keyed`].
    fn landed_release(
        &self,
        by_name: &HashMap<InstanceKey, &Job<'_, B>>,
        name: &InstanceKey,
        refs: &[Target],
        blob_key: Key,
        out: &mut Out,
    ) {
        let job = by_name[name];
        let key = job.unit.instance.key();
        match self.memo.get(&blob_key.0) {
            Some(template) => {
                out.used(blob_key.0);
                out.stats.flown += 1;
                out.template(By::node(job.name, &job.unit), blob_key, template);
                out.instantiate(
                    &job.unit,
                    &job.node.described.name,
                    key,
                    Whence::Store(blob_key),
                    template,
                    refs,
                    job.node.described.anchors.as_ref(),
                );
            }
            // A landing is dropped with its template ([`Memo::retain`]), between builds.
            None => out.errors.push(error(
                key,
                job.unit.item,
                // alloc: an error
                format!("its compile landed under {blob_key:?}, which the memo has no template of"),
            )),
        }
    }

    /// The summaries of `flown`, release units each with the flight key another build landed
    /// its summary under, into `summaries`: nothing prepared or summarized here.
    fn flown_summaries(
        &self,
        by_name: &HashMap<InstanceKey, &Job<'_, B>>,
        flown: Vec<(InstanceKey, InstanceKey)>,
        summaries: &mut blaze_hash::HashMap<InstanceKey, Summary, Global>,
        out: &mut Out,
    ) {
        if flown.is_empty() {
            return;
        }
        // alloc: per batch of summaries another build made, sized by it
        let keys: Vec<InstanceKey> = flown.iter().map(|it| it.1).collect();
        for ((name, flight), summary) in flown.into_iter().zip(self.memo.flights().summaries(&keys))
        {
            match summary {
                Some(summary) => {
                    let bytes = summary.encode();
                    out.put(
                        By::node(name, &by_name[&name].unit),
                        keys::summary(&by_name[&name].summarized()),
                        Kind::Query,
                        bytes,
                    );
                    out.used(flight.0);
                    summaries.insert(name, summary);
                }
                // A landing is dropped with its summary ([`Memo::retain`]), between builds.
                None => {
                    let job = by_name[&name];
                    out.errors.push(error(
                        job.unit.instance.key(),
                        job.unit.item,
                        "its summary landed, and the memo has none of it",
                    ));
                }
            }
        }
    }

    /// The definition of each static of `statics`, or with `only` (sorted) of those alone:
    /// from the store, else compiled.
    fn statics(
        &self,
        statics: &Sorted<StaticId, StaticOf<B>>,
        only: Option<&[StaticId]>,
        store: &impl Lookup,
        out: &mut Out,
    ) {
        let unit_of = |node: &StaticOf<B>| Unit {
            instance: Instance {
                code: blaze_items::CodeHash(node.described.value),
                args: node.described.instance,
                settings: node.spec.settings(),
            },
            backend: node.spec.backend,
            arch: self.policy.toolchain.arch,
            item: node.described.item,
        };
        // alloc: per build, sized by its statics
        let (mut hits, mut missing) = (Vec::new(), Vec::new());
        // alloc: per build, the statics it makes
        let made: Vec<(&StaticId, &StaticOf<B>)> = match only {
            None => statics.iter().collect(),
            Some(only) => only.iter().filter_map(|id| Some((id, statics.get(id)?))).collect(),
        };
        for (id, node) in made {
            let key =
                keys::static_value(&definition(&node.described, &node.refs), &node.spec.settings());
            let unit = unit_of(node);
            // alloc: an error
            match out
                .read(store, &key, &unit, |it| blob::decode_static(it).map_err(|e| e.to_string()))
            {
                Read::Hit(template) => hits.push((id, node, template)),
                Read::Miss => missing.push((id, node, key)),
                Read::Failed => {}
            }
        }
        out.stats.stored += hits.len();
        // Each value and its digest on all cores, where it is made.
        // alloc: per build, sized by its statics
        let instantiated: Vec<_> = par::map(hits, |(id, node, template)| {
            let made = template.instantiate(id.0, id.1, &node.refs).map(|value| {
                let digest = digests::static_definition(&value);
                (value, digest)
            });
            (id, node, made)
        });
        for (id, node, made) in instantiated {
            match made {
                Ok((value, digest)) => out.push_static(value, digest),
                // alloc: an error
                Err(why) => {
                    out.errors.push(error(unit_of(node).instance.key(), id.0, why.to_string()))
                }
            }
        }
        let compiled = self.until_stopped(missing, &mut out.errors, |(id, node, key)| {
            let made =
                self.backend.compile_static(self.frontend, &node.spec, &node.value, &node.refs);
            let made = made.map(|value| {
                let digest = digests::static_definition(&value);
                (value, digest)
            });
            (id, node, key, made)
        });
        for (id, node, key, made) in compiled {
            let unit = unit_of(node);
            let described = &node.described;
            let (value, digest) = match made {
                Ok((it, _)) if (it.item, it.instance) != *id => {
                    out.errors.push(error(unit.instance.key(), id.0, "compiled as another static"));
                    continue;
                }
                Ok((it, _))
                    if (it.mutable, it.thread_local, &it.section)
                        != (described.mutable, described.thread_local, &described.section) =>
                {
                    out.errors.push(error(
                        unit.instance.key(),
                        id.0,
                        "compiled with another mutability, thread-locality or section than the front end described",
                    ));
                    continue;
                }
                Ok(it) => it,
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            out.stats.compiled += 1;
            match StaticTemplate::of(&value, &node.refs) {
                Ok(template) => {
                    out.put(
                        By::static_(*id, &unit),
                        key,
                        Kind::Code,
                        blob::encode_static(&template),
                    );
                    out.push_static(value, digest);
                }
                // alloc: an error
                Err(why) => out.errors.push(error(unit.instance.key(), id.0, why.to_string())),
            }
        }
    }
}

pub const STATIC_DEFINITION_DOMAIN: &str = "blaze v2 static definition 2";

/// Everything a static's compiled definition is made of besides which static it is: what
/// its template ([`StaticTemplate`]) is kept under, so it covers each field the template
/// keeps.
/// - `align`, `init`, `data` and `imports`: [`DescribedStatic::value`], the content of its
///   evaluated value with its layout and what its externs are imported as;
/// - `mutable`, `thread_local`, `section`: here;
/// - `refs`: how many references this build resolved (`refs`) and the kind of each
///   ([`place_kind`]), as [`placed`] covers a function's. Which target is at a place is not
///   covered, as a template takes any.
fn definition<I, S>(described: &DescribedStatic<I, S>, refs: &[Target]) -> ContentId {
    let mut hasher = Hasher::new(STATIC_DEFINITION_DOMAIN);
    hasher.id(&described.value).update(&[described.mutable as u8, described.thread_local as u8]);
    match &described.section {
        Some(section) => hasher.update(&[1]).part(section.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        hasher.update(&[place_kind(target)]);
    }
    hasher.finish()
}

/// The memo shared by every build of a daemon.
pub fn shared_memo() -> Arc<Memo> {
    // alloc: once a daemon
    Arc::new(Memo::new())
}

/// What a folded group's body is across edits ([`Built::identities`]): its members'
/// identities, sorted, which an edit of a member's body keeps as it does each member's.
fn folded_identity(members: impl Iterator<Item = ContentId>) -> ContentId {
    // alloc: once per folded group, its members' identities
    let mut members: Vec<ContentId> = members.collect();
    members.sort_unstable();
    let mut hasher = Hasher::new(FOLDED_IDENTITY_DOMAIN);
    hasher.update(&(members.len() as u64).to_le_bytes());
    for it in &members {
        hasher.id(it);
    }
    hasher.finish()
}

/// The error of a build stopped ([`Frontend::stopped`]): its outcome was decided without its
/// code, or its job was cancelled or gave way. Of no instance, so its keys are empty. Its
/// caller reports the check's errors or posts the job again, never this.
fn stopped() -> CodegenError {
    let none = (InstanceKey(ContentId::default()), ItemKey(ContentId::default()));
    error(
        none.0,
        none.1,
        "stopped: a body the build compiles failed its check, or its job was cancelled or gave way",
    )
}

mod masks;
pub use masks::Masks;
pub(crate) mod reach;
pub use reach::Reach;
mod owners;
pub use owners::Owner;
mod delta;
pub use delta::{BuiltId, Changes, Delta, Fresh};
mod kept;
pub use kept::{Kept, Walk, Work};
mod later;
mod lists;
mod patch;
mod recursion;
mod rows;
mod support;
pub use later::{Later, Walked, runs_at_start};
pub mod digests;
pub use digests::{Digests, static_definition};

#[cfg(test)]
mod tests;
