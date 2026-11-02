//! Which item: every [`ItemKey`], and the way back from one to the item it names.
//!
//! A key is like rustc's `DefPathHash`: the key of what the item is in (its module, impl,
//! trait, enum or body), its kind, its name and where it is written, as the AST id of its
//! declaration in the file that declares it. That AST id is the kind of syntax, a hash of the
//! name and an index among the declarations of that file with both equal (`span::ast_id`), so
//! it does not move when another item is added, removed or edited; an item a macro wrote is in
//! that macro's expansion, named by the key of the call. A key never says what the item is:
//! an edit of its signature or body keeps it, a rename or a move gives a new one.
//!
//! Like a `DefPathHash`, a key also says where it is: its first 8 bytes are its crate key's,
//! and the top bit of byte 8 says whether it is inside a body (an item of a block, a closure,
//! an opaque type). [`resolve`] reads both and looks in one index of one crate.
//!
//! A crate is named by its content, never by its `Crate` id: its name, version, origin, kind
//! of target, edition, crate attributes, dependencies (by name and key) and the cfg it is
//! built with. Features are in the cfg: one package is built
//! twice in one build, for the host and for the target, with different features and, when not
//! cross compiling, the same target cfg. Turning on a feature names its crate's items anew;
//! their hashes stay what they were, so it costs queries, not code. Two crates of one build
//! with equal keys, or keys with equal first 8 bytes, stop the build with a
//! [`CrateKeyClash`] ([`check_crate_keys`]), before any key is resolved.
//!
//! [`resolve`] goes back from a key to the item of the current database, by indexes over every
//! crate's items: one of the items of its modules, impls and traits (with their constructors),
//! one per body of the items of its blocks, its closures, its anonymous constants and its
//! opaque types. A key it does not find names nothing that is still there.
//!
//! A crate's two indexes are its largest tables that live for the session, so each is a
//! [`KeyIndex`] over a reserved region ([`RegionVec`], [`Tag::Other`]: an item's identity is no
//! one subsystem's) that grows in place while the crate's items are walked, then is sorted once.

use std::{fmt, mem};

use base_db::{Db, Crate, TargetKind};
use blaze_format::cursor::Put;
use blaze_ids::{ContentId, Hasher, radix};
use blaze_mem::{Frame, MemError, RegionVec, Tag, frame, scratch};
use hir_def::{
    AdtId, BuiltinDeriveImplId, DefWithBodyId, EnumId, EnumVariantId, ExpressionStoreOwnerId,
    GenericDefId, HasModule, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId,
    ModuleIdLt, TraitId, VariantId,
    expr_store::ExpressionStore,
    hir::{ClosureKind, Expr, ExprId},
    item_tree::FieldsShape,
    lang_item::LangItemTarget,
    nameres::DefMap,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_expand::{HirFileId, MacroCallKind, name::Name};
use hir_ty::{
    ImplTraitId, InferBodyId, SelfAndAssocBounds,
    db::{
        AnonConstId, ClosureKey, HirDatabase, InternedClosureId, InternedCoroutineClosureId,
        InternedCoroutineId, InternedOpaqueTyId,
    },
    next_solver::{Ctor, SolverDefId},
};
use span::ErasedFileAstId;
use syntax::view::HasName;

use crate::{ItemId, ItemKey, NAME_DOMAIN, NameHash, SimplifiedType};

pub const KEY_DOMAIN: &str = "blaze v2 item key 2";
pub const CRATE_DOMAIN: &str = "blaze v2 crate key 2";
pub const SIMPLIFIED_DOMAIN: &str = "blaze v2 simplified type 1";

/// What a key names, in the current database.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Resolved {
    Item(ItemId),
    Module(ModuleId),
    Variant(EnumVariantId),
    /// A closure, coroutine or coroutine closure: the `Expr::Closure` at `expr` of the
    /// expression store of `store`. A coroutine closure has two keys, one as the closure
    /// whose type it has and one as itself; both resolve here.
    Closure { store: ExpressionStoreOwnerId, expr: ExprId, kind: ClosureKind },
    /// The `index`th `impl Trait` of the return type of a function or of a type alias.
    Opaque { owner: ItemId, index: u32 },
    BuiltinDerive(BuiltinDeriveImplId),
    /// The constructor of a tuple or unit struct or variant ([`ctor_key`]).
    Ctor(Ctor),
    /// An anonymous constant: the root expression `expr` of the expression store of `store`
    /// ([`anon_const_key`]). What it is interned as is made by the lowering of its store.
    AnonConst { store: ExpressionStoreOwnerId, expr: ExprId },
}

/// The key of a crate: its content, never its id (see the module doc).
#[blaze_query::tracked(returns(copy))]
pub fn crate_key(db: &Db, krate: Crate) -> ContentId {
    let data = krate.data(db);
    let extra = krate.extra_data(db);
    let mut h = Hasher::new(CRATE_DOMAIN);
    h.opt_part(extra.display_name.as_ref().map(|name| name.canonical_name().as_str()))
        .opt_part(extra.version.as_deref());
    data.origin.hash_into(&mut h);
    match data.target_kind {
        TargetKind::Bin => h.update(&[0]),
        TargetKind::Lib { is_proc_macro } => h.update(&[1, is_proc_macro as u8]),
        TargetKind::Example => h.update(&[2]),
        TargetKind::Test => h.update(&[3]),
        TargetKind::Bench => h.update(&[4]),
        TargetKind::BuildScript => h.update(&[5]),
        TargetKind::Other => h.update(&[6]),
    };
    // What a path in it resolves to: its edition, its `-Z crate-attr`s and each dependency by
    // its name, its flags and its own key. One package of one cfg built twice in one graph (a
    // build dependency reached from the host side and the target side) has two `Crate`s only
    // when these differ, and its items then name different things.
    h.update(&[data.edition as u8]);
    h.update(&(data.crate_attrs.len() as u64).to_le_bytes());
    for attr in data.crate_attrs.iter() {
        h.part(attr.as_bytes());
    }
    h.update(&(data.dependencies.len() as u64).to_le_bytes());
    for dep in &data.dependencies {
        h.part(dep.name.symbol().as_str().as_bytes())
            .update(&[u8::from(dep.is_prelude()), u8::from(dep.is_sysroot()), u8::from(dep.is_private())])
            .id(&crate_key(db, dep.crate_id));
    }
    // Every atom's bytes in one buffer, sorted as byte strings; an atom is its start and end
    // in the buffer, packed into a `u64`.
    let range = |atom: u64| (atom >> 32) as usize..(atom as u32) as usize;
    scratch::with_bytes(|bytes| {
        scratch::with_u64s(|atoms| {
            for atom in krate.cfg_options(db) {
                let start = bytes.len();
                match atom {
                    cfg::CfgAtom::Flag(name) => {
                        bytes.push(0);
                        bytes.put_bytes_u64(name.as_str().as_bytes());
                    }
                    cfg::CfgAtom::KeyValue { key, value } => {
                        bytes.push(1);
                        bytes.put_bytes_u64(key.as_str().as_bytes());
                        bytes.put_bytes_u64(value.as_str().as_bytes());
                    }
                }
                let end = u32::try_from(bytes.len()).expect("blaze-items: a crate's cfg under 4 GiB");
                atoms.push(u64::from(start as u32) << 32 | u64::from(end));
            }
            atoms.sort_unstable_by(|&a, &b| bytes[range(a)].cmp(&bytes[range(b)]));
            h.update(&(atoms.len() as u64).to_le_bytes());
            for &atom in atoms.iter() {
                h.part(&bytes[range(atom)]);
            }
        })
    });
    h.finish()
}

/// A key's preimage: the bytes a [`Hasher`] would be given (`part` is length-prefixed, `id`
/// the 32 bytes), kept so that many keys hash in one batch and come out as one at a time.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn update(&mut self, bytes: &[u8]) -> &mut Bytes {
        self.0.put_bytes(bytes);
        self
    }

    fn part(&mut self, bytes: &[u8]) -> &mut Bytes {
        self.0.put_bytes_u64(bytes);
        self
    }

    fn id(&mut self, id: &ContentId) -> &mut Bytes {
        self.update(&id.0)
    }

    fn opt(&mut self, part: Option<&str>) -> &mut Bytes {
        match part {
            None => self.update(&[0]),
            Some(part) => self.update(&[1]).part(part.as_bytes()),
        }
    }
}

/// One key's preimage written by `write` into the worker's byte buffer, and its hash: what
/// `write` gives, and `ContentId::of(KEY_DOMAIN, preimage)`.
fn hashed<R>(write: impl FnOnce(&mut Bytes) -> R) -> (R, ContentId) {
    scratch::with_bytes(|buffer| {
        let mut b = Bytes(mem::take(buffer));
        let written = write(&mut b);
        let hash = ContentId::of(KEY_DOMAIN, &b.0);
        *buffer = b.0;
        (written, hash)
    })
}

/// The top bit of a key's byte 8: the key is of something inside a body (an item of a block,
/// a block's module, a closure, an anonymous constant, an opaque type).
const INNER: u8 = 0x80;

/// A key has rustc's `DefPathHash` shape, so [`resolve`] goes straight to one index of one
/// crate: bytes 0..8 are the first 8 of its crate's key, byte 8's top bit is [`INNER`], and the
/// other 247 bits are the hash of its preimage.
impl ItemKey {
    fn crate_prefix(&self) -> [u8; 8] {
        *self.0.0.first_chunk().expect("a key is 32 bytes")
    }

    fn is_inner(&self) -> bool {
        self.0.0[8] & INNER != 0
    }
}

fn shaped(prefix: [u8; 8], inner: bool, hash: ContentId) -> ItemKey {
    let mut bytes = hash.0;
    bytes[..8].copy_from_slice(&prefix);
    bytes[8] = (bytes[8] & !INNER) | if inner { INNER } else { 0 };
    ItemKey(ContentId(bytes))
}

/// The key of something in `parent`: its crate, inside a body if its parent is or `inner` says.
fn under(parent: &ItemKey, inner: bool, hash: ContentId) -> ItemKey {
    shaped(parent.crate_prefix(), parent.is_inner() || inner, hash)
}

fn crate_prefix(krate: &ContentId) -> [u8; 8] {
    *krate.0.first_chunk().expect("a key is 32 bytes")
}

/// The preimages of many keys in one buffer, `ends[i]` where the `i`th ends, hashed in one
/// batch: BLAKE3 over 16 inputs at a time with AVX-512, 4 with NEON, each exactly
/// `ContentId::of(KEY_DOMAIN, it)`. The preimages and their ends are written into the worker's
/// scratch buffers, so a warm batch allocates only its rows, which the query keeps.
struct Batch<T> {
    rows: Vec<(T, ItemKey)>,
    bytes: Bytes,
    ends: Vec<u32>,
    /// The scratch buffers `bytes` and `ends` were taken from, given back by [`Batch::finish`].
    lent: (scratch::Lent<Vec<u8>>, scratch::Lent<Vec<u32>>),
}

impl<T: Copy + Ord> Batch<T> {
    fn with_capacity(count: usize) -> Batch<T> {
        let mut lent = (scratch::lend_bytes(), scratch::lend_u32s());
        let mut bytes = mem::take(&mut *lent.0);
        bytes.reserve(count * 96);
        let mut ends = mem::take(&mut *lent.1);
        ends.reserve(count);
        Batch { rows: Vec::with_capacity(count), bytes: Bytes(bytes), ends, lent }
    }

    /// Adds `id`, whose preimage `write` appends and whose parent it returns.
    fn push(&mut self, id: T, write: impl FnOnce(&mut Bytes) -> ItemKey) {
        let parent = write(&mut self.bytes);
        self.rows.push((id, parent));
        self.ends.push(u32::try_from(self.bytes.0.len()).expect("a batch of keys under 4 GiB"));
    }

    /// The keys, sorted by id for [`find`].
    fn finish(self) -> Vec<(T, ItemKey)> {
        let Batch { mut rows, bytes, ends, mut lent } = self;
        let hashes = frame::task(Tag::Other, |frame| {
            let mut inputs: Vec<&[u8], &Frame> = Vec::with_capacity_in(ends.len(), frame);
            let mut start = 0;
            for &end in &ends {
                inputs.push(&bytes.0[start..end as usize]);
                start = end as usize;
            }
            ContentId::of_many(KEY_DOMAIN, &inputs)
        });
        for ((_, parent), hash) in rows.iter_mut().zip(hashes) {
            *parent = under(parent, false, hash);
        }
        *lent.0 = bytes.0;
        *lent.1 = ends;
        rows.sort_unstable_by_key(|&(id, _)| id);
        rows
    }
}

/// The key of `id` in keys [`Batch::finish`] gave.
fn find<T: Copy + Ord>(keys: &[(T, ItemKey)], id: T) -> Option<ItemKey> {
    keys.binary_search_by_key(&id, |&(it, _)| it).ok().map(|at| keys[at].1)
}

/// The key of a module: of its crate for a crate's root, of its block for a block's, else of
/// its parent and its name.
pub fn module_key(db: &Db, module: ModuleId) -> ItemKey {
    module_key_in(db, module)
}

#[blaze_query::tracked(returns(copy))]
fn module_key_in<'db>(db: &'db Db, module: ModuleIdLt<'db>) -> ItemKey {
    // Each parent's key is had before the preimage is written, so the recursion up to the
    // crate's root holds no scratch buffer.
    match module.containing_module_inside_def_map(db) {
        Some(parent) => {
            let name = module.name(db);
            let parent = module_key_in(db, parent);
            let ((), hash) = hashed(|b| {
                b.id(&parent.0).update(b"mod").opt(name.as_ref().map(|it| it.as_str()));
            });
            under(&parent, false, hash)
        }
        None => match module.block(db) {
            Some(block) => {
                let ast = block.ast_id(db);
                let parent = module_key_in(db, block.module(db));
                let ((), hash) = hashed(|b| {
                    b.id(&parent.0).update(b"block");
                    written(db, b, ast.file_id, ast.value.erase());
                });
                under(&parent, true, hash)
            }
            None => {
                let krate = crate_key(db, module.krate(db));
                let ((), hash) = hashed(|b| {
                    b.update(b"crate").id(&krate);
                });
                shaped(crate_prefix(&krate), false, hash)
            }
        },
    }
}

/// Where a declaration is written: the file (an expansion named by its call) and the raw AST
/// id in it.
fn written(db: &Db, b: &mut Bytes, file: HirFileId, ast: ErasedFileAstId) {
    file_key(db, b, file);
    b.update(&ast.into_raw().to_le_bytes());
}

fn file_key(db: &Db, b: &mut Bytes, file: HirFileId) {
    match file {
        HirFileId::FileId(_) => {
            b.update(&[0]);
        }
        HirFileId::MacroFile(call) => {
            let loc = call.loc(db);
            b.update(&[1]);
            written(db, b, loc.kind.file_id(), loc.kind.erased_ast_id());
            match &loc.kind {
                MacroCallKind::FnLike { .. } => b.update(&[0]),
                MacroCallKind::Derive { derive_attr_index, derive_index, .. } => b
                    .update(&[1])
                    .update(&derive_attr_index.item_tree_index().to_le_bytes())
                    .update(&derive_index.to_le_bytes()),
                MacroCallKind::Attr { censored_attr_ids, .. } => b
                    .update(&[2])
                    .update(&censored_attr_ids.invoc_attr().item_tree_index().to_le_bytes()),
            };
        }
    }
}

fn container_key(db: &Db, container: ItemContainerId) -> (ItemKey, &'static [u8]) {
    match container {
        ItemContainerId::ModuleId(it) => (module_key(db, it), b""),
        ItemContainerId::ImplId(it) => (item_key(db, ItemId::Impl(it)), b""),
        ItemContainerId::TraitId(it) => (item_key(db, ItemId::Trait(it)), b""),
        // An extern block names nothing: its items are in its module's namespaces.
        ItemContainerId::ExternBlockId(it) => (module_key(db, it.lookup(db).container), b"extern "),
    }
}

/// What an item's key is hashed in a batch with: the items of its module, impl or trait.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Siblings {
    Module(ModuleId),
    Impl(ImplId),
    Trait(TraitId),
}

fn siblings_of_container(db: &Db, container: ItemContainerId) -> Siblings {
    match container {
        ItemContainerId::ModuleId(it) => Siblings::Module(it),
        ItemContainerId::ImplId(it) => Siblings::Impl(it),
        ItemContainerId::TraitId(it) => Siblings::Trait(it),
        ItemContainerId::ExternBlockId(it) => Siblings::Module(it.lookup(db).container),
    }
}

fn siblings(db: &Db, id: ItemId) -> Siblings {
    match id {
        ItemId::Function(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Const(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Static(it) => siblings_of_container(db, it.lookup(db).container),
        ItemId::Alias(it) => siblings_of_container(db, it.lookup(db).container),
        other => Siblings::Module(other.module(db)),
    }
}

/// The key of an item (see the module doc), hashed in one batch with its siblings'.
#[blaze_query::tracked(returns(copy))]
pub fn item_key(db: &Db, id: ItemId) -> ItemKey {
    let batch = match siblings(db, id) {
        Siblings::Module(it) => module_item_keys(db, it),
        Siblings::Impl(it) => impl_item_keys(db, it),
        Siblings::Trait(it) => trait_item_keys(db, it),
    };
    match find(batch, id) {
        Some(key) => key,
        // An item its module's scope does not list, as a `macro_rules!` that a later one of
        // its name shadows: the same bytes, hashed alone.
        None => item_key_alone(db, id),
    }
}

/// An item's key hashed by itself, which the batch gives bit for bit.
pub(crate) fn item_key_alone(db: &Db, id: ItemId) -> ItemKey {
    let (parent, hash) = hashed(|b| item_preimage(db, id, b));
    under(&parent, false, hash)
}

/// The keys of the items a module's scope declares, sorted by id: 40 bytes per item, found by
/// binary search.
#[blaze_query::tracked(returns(ref))]
fn module_item_keys<'db>(
    db: &'db Db,
    module: ModuleIdLt<'db>,
) -> Vec<(ItemId, ItemKey)> {
    let scope = module.def_map(db).module(db, module).scope(db);
    // alloc: the memo's own list, its length known only once the scope is walked.
    let mut ids = Vec::new();
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => ids.push(ItemId::Function(it)),
            ModuleDefId::AdtId(it) => ids.push(ItemId::Adt(it)),
            ModuleDefId::ConstId(it) => ids.push(ItemId::Const(it)),
            ModuleDefId::StaticId(it) => ids.push(ItemId::Static(it)),
            ModuleDefId::TraitId(it) => ids.push(ItemId::Trait(it)),
            ModuleDefId::TypeAliasId(it) => ids.push(ItemId::Alias(it)),
            ModuleDefId::MacroId(it) => ids.push(ItemId::Macro(it)),
            ModuleDefId::EnumVariantId(_) | ModuleDefId::ModuleId(_) | ModuleDefId::BuiltinType(_) => {}
        }
    }
    ids.extend(scope.impls().map(ItemId::Impl));
    ids.extend(scope.unnamed_consts().map(ItemId::Const));
    for (_, macros) in scope.legacy_macros() {
        ids.extend(macros.iter().map(|&it| ItemId::Macro(it)));
    }
    let here = blaze_query::db::AsId::as_id(&module);
    ids.retain(|&id| {
        matches!(siblings(db, id), Siblings::Module(it) if blaze_query::db::AsId::as_id(&it) == here)
    });
    ids.sort_unstable();
    ids.dedup();
    let mut batch = Batch::with_capacity(ids.len());
    for id in ids {
        batch.push(id, |b| item_preimage(db, id, b));
    }
    batch.finish()
}

#[blaze_query::tracked(returns(ref))]
fn impl_item_keys(db: &Db, impl_: ImplId) -> Vec<(ItemId, ItemKey)> {
    let items = &impl_.impl_items(db).items;
    let mut batch = Batch::with_capacity(items.len());
    for &(_, it) in items.iter() {
        batch.push(ItemId::from(it), |b| item_preimage(db, it.into(), b));
    }
    batch.finish()
}

#[blaze_query::tracked(returns(ref))]
fn trait_item_keys(db: &Db, trait_: TraitId) -> Vec<(ItemId, ItemKey)> {
    let items = &trait_.trait_items(db).items;
    let mut batch = Batch::with_capacity(items.len());
    for &(_, it) in items.iter() {
        batch.push(ItemId::from(it), |b| item_preimage(db, it.into(), b));
    }
    batch.finish()
}

/// Appends the bytes an item's key is the hash of to `b`: its parent's key, its kind, its name
/// (the interned symbol's bytes, read in place) and where it is written. Gives the parent.
fn item_preimage(db: &Db, id: ItemId, b: &mut Bytes) -> ItemKey {
    fn head(b: &mut Bytes, parent: ItemKey, prefix: &[u8], tag: &[u8], name: Option<&str>) {
        b.id(&parent.0).part(prefix).part(tag).opt(name);
    }
    let (parent, file, ast) = match id {
        ItemId::Function(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            head(b, parent, prefix, b"fn", Some(FunctionSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::StructId(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            head(b, parent, b"", b"struct", Some(StructSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::EnumId(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            head(b, parent, b"", b"enum", Some(EnumSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Adt(AdtId::UnionId(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            head(b, parent, b"", b"union", Some(UnionSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Trait(it) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            head(b, parent, b"", b"trait", Some(TraitSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Impl(it) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            head(b, parent, b"", b"impl", None);
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Const(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            let signature = ConstSignature::of(db, it);
            head(b, parent, prefix, b"const", signature.name.as_ref().map(|it| it.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Static(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            head(b, parent, prefix, b"static", Some(StaticSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Alias(it) => {
            let loc = it.lookup(db);
            let (parent, prefix) = container_key(db, loc.container);
            head(b, parent, prefix, b"type", Some(TypeAliasSignature::of(db, it).name.as_str()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        // hir-def keeps no macro's name but in its syntax.
        ItemId::Macro(MacroId::MacroRulesId(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            let name = loc.id.to_view(db).name();
            head(b, parent, b"", b"macro_rules", name.map(|it| it.text()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Macro(MacroId::Macro2Id(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            let name = loc.id.to_view(db).name();
            head(b, parent, b"", b"macro", name.map(|it| it.text()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
        ItemId::Macro(MacroId::ProcMacroId(it)) => {
            let loc = it.lookup(db);
            let parent = module_key(db, loc.container);
            let name = loc.id.to_view(db).name();
            head(b, parent, b"", b"proc_macro", name.map(|it| it.text()));
            (parent, loc.id.file_id, loc.id.value.erase())
        }
    };
    written(db, b, file, ast);
    parent
}

/// The key of an enum variant: its enum's and its name.
pub fn variant_key(db: &Db, variant: EnumVariantId) -> ItemKey {
    let loc = variant.lookup(db);
    match find(enum_variant_keys(db, loc.parent), variant) {
        Some(key) => key,
        None => {
            let (parent, hash) = hashed(|b| variant_preimage(db, loc.parent, &loc.name, b));
            under(&parent, false, hash)
        }
    }
}

fn variant_preimage(db: &Db, parent: EnumId, name: &Name, b: &mut Bytes) -> ItemKey {
    let parent = item_key(db, ItemId::Adt(parent.into()));
    b.id(&parent.0).part(b"variant").part(name.as_str().as_bytes());
    parent
}

#[blaze_query::tracked(returns(ref))]
fn enum_variant_keys(db: &Db, enum_: EnumId) -> Vec<(EnumVariantId, ItemKey)> {
    let variants = &enum_.enum_variants(db).variants;
    let mut batch = Batch::with_capacity(variants.len());
    for (name, &(variant, _)) in variants.iter() {
        batch.push(variant, |b| variant_preimage(db, enum_, name, b));
    }
    batch.finish()
}

fn variant_id_key(db: &Db, variant: VariantId) -> ItemKey {
    match variant {
        VariantId::EnumVariantId(it) => variant_key(db, it),
        VariantId::StructId(it) => item_key(db, ItemId::Adt(it.into())),
        VariantId::UnionId(it) => item_key(db, ItemId::Adt(it.into())),
    }
}

pub(crate) fn generic_def_item(def: GenericDefId) -> ItemId {
    match def {
        GenericDefId::AdtId(it) => ItemId::Adt(it),
        GenericDefId::ConstId(it) => ItemId::Const(it),
        GenericDefId::FunctionId(it) => ItemId::Function(it),
        GenericDefId::ImplId(it) => ItemId::Impl(it),
        GenericDefId::StaticId(it) => ItemId::Static(it),
        GenericDefId::TraitId(it) => ItemId::Trait(it),
        GenericDefId::TypeAliasId(it) => ItemId::Alias(it),
    }
}

/// The key of an expression store: of a signature, a body or the fields of a variant.
pub fn store_key(db: &Db, store: ExpressionStoreOwnerId) -> ItemKey {
    let (parent, tag): (_, &[u8]) = match store {
        ExpressionStoreOwnerId::Signature(def) => (item_key(db, generic_def_item(def)), b"signature"),
        ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(it)) => {
            (variant_key(db, it), b"discriminant")
        }
        ExpressionStoreOwnerId::Body(body) => (item_key(db, ItemId::of_body(db, body)), b"body"),
        ExpressionStoreOwnerId::VariantFields(it) => (variant_id_key(db, it), b"fields"),
    };
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&parent.0).part(tag);
    under(&parent, true, h.finish())
}

/// The key of an anonymous constant (an array length, a const argument): its store's and its
/// root expression's index there.
pub fn anon_const_key<'db>(db: &'db Db, anon: AnonConstId<'db>) -> ItemKey {
    let loc = anon.loc(db);
    anon_const_key_of(db, loc.owner, loc.expr)
}

pub(crate) fn anon_const_key_of(db: &Db, store: ExpressionStoreOwnerId, expr: ExprId) -> ItemKey {
    let parent = store_key(db, store);
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&parent.0).part(b"anon const").update(&expr.into_raw().into_u32().to_le_bytes());
    under(&parent, true, h.finish())
}

/// The key of a tuple or unit struct's or variant's constructor: under its struct's or
/// variant's.
pub fn ctor_key(db: &Db, ctor: Ctor) -> ItemKey {
    let of = match ctor {
        Ctor::Struct(it) => item_key(db, ItemId::Adt(it.into())),
        Ctor::Enum(it) => variant_key(db, it),
    };
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&of.0).part(b"ctor");
    under(&of, false, h.finish())
}

fn closure_key_of(
    db: &Db,
    store: ExpressionStoreOwnerId,
    tag: &'static [u8],
    nth: u32,
) -> ItemKey {
    let parent = store_key(db, store);
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&parent.0).part(tag).update(&nth.to_le_bytes());
    under(&parent, true, h.finish())
}

fn store_of<'db>(db: &'db Db, owner: InferBodyId<'db>) -> ExpressionStoreOwnerId {
    owner.expression_store_owner(db)
}

/// The key of a closure: its store's and its place among the store's closures (an edit before
/// it in the body keeps it, as [`ClosureKey`] does).
pub fn closure_key<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> ItemKey {
    let loc = closure.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"closure", ClosureKey::nth(db, loc))
}

pub fn coroutine_key<'db>(db: &'db Db, coroutine: InternedCoroutineId<'db>) -> ItemKey {
    let loc = coroutine.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"coroutine", ClosureKey::nth(db, loc))
}

pub fn coroutine_closure_key<'db>(
    db: &'db Db,
    closure: InternedCoroutineClosureId<'db>,
) -> ItemKey {
    let loc = closure.loc(db);
    closure_key_of(db, store_of(db, loc.owner), b"coroutine closure", ClosureKey::nth(db, loc))
}

/// The key of the `index`th opaque type of `owner`: a function's return type or a type
/// alias's type.
pub fn opaque_key(db: &Db, owner: ItemId, index: u32) -> ItemKey {
    let parent = item_key(db, owner);
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&parent.0).part(b"opaque").update(&index.to_le_bytes());
    under(&parent, true, h.finish())
}

/// The item an opaque type is of and its index there.
pub(crate) fn opaque_of<'db>(db: &'db Db, opaque: InternedOpaqueTyId<'db>) -> (ItemId, u32) {
    match opaque.loc(db) {
        ImplTraitId::ReturnTypeImplTrait(f, index) => {
            (ItemId::Function(f), index.into_raw().into_u32())
        }
        ImplTraitId::TypeAliasImplTrait(a, index) => {
            (ItemId::Alias(a), index.into_raw().into_u32())
        }
    }
}

pub fn opaque_ty_key<'db>(db: &'db Db, opaque: InternedOpaqueTyId<'db>) -> ItemKey {
    let (owner, index) = opaque_of(db, opaque);
    opaque_key(db, owner, index)
}

/// The key of an impl a built-in derive writes: its type's, the trait and which derive.
pub fn builtin_derive_key(db: &Db, id: BuiltinDeriveImplId) -> ItemKey {
    let loc = id.loc(db);
    let parent = item_key(db, ItemId::Adt(loc.adt));
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&parent.0)
        .part(b"builtin derive")
        .part(loc.trait_.name().as_str().as_bytes())
        .update(&loc.derive_attr_id.item_tree_index().to_le_bytes())
        .update(&loc.derive_index.to_le_bytes());
    under(&parent, false, h.finish())
}

/// The key of anything the solver names by a `SolverDefId`.
pub fn def_key<'db>(db: &'db Db, def: SolverDefId<'db>) -> ItemKey {
    match def {
        SolverDefId::AdtId(it) => item_key(db, ItemId::Adt(it)),
        SolverDefId::ConstId(it) => item_key(db, ItemId::Const(it)),
        SolverDefId::FunctionId(it) => item_key(db, ItemId::Function(it)),
        SolverDefId::ImplId(it) => item_key(db, ItemId::Impl(it)),
        SolverDefId::BuiltinDeriveImplId(it) => builtin_derive_key(db, it),
        SolverDefId::StaticId(it) => item_key(db, ItemId::Static(it)),
        SolverDefId::AnonConstId(it) => anon_const_key(db, it),
        SolverDefId::TraitId(it) => item_key(db, ItemId::Trait(it)),
        SolverDefId::TypeAliasId(it) => item_key(db, ItemId::Alias(it)),
        SolverDefId::InternedClosureId(it) => closure_key(db, it),
        SolverDefId::InternedCoroutineId(it) => coroutine_key(db, it),
        SolverDefId::InternedCoroutineClosureId(it) => coroutine_closure_key(db, it),
        SolverDefId::InternedOpaqueTyId(it) => opaque_ty_key(db, it),
        SolverDefId::EnumVariantId(it) => variant_key(db, it),
        SolverDefId::Ctor(ctor) => ctor_key(db, ctor),
    }
}

/// A lang item's target by key: as the `SolverDefId` it is, a macro by its item key.
pub fn lang_item_key(db: &Db, target: LangItemTarget) -> ItemKey {
    let def = match target {
        LangItemTarget::EnumId(it) => SolverDefId::AdtId(it.into()),
        LangItemTarget::StructId(it) => SolverDefId::AdtId(it.into()),
        LangItemTarget::UnionId(it) => SolverDefId::AdtId(it.into()),
        LangItemTarget::FunctionId(it) => SolverDefId::FunctionId(it),
        LangItemTarget::ImplId(it) => SolverDefId::ImplId(it),
        LangItemTarget::StaticId(it) => SolverDefId::StaticId(it),
        LangItemTarget::TypeAliasId(it) => SolverDefId::TypeAliasId(it),
        LangItemTarget::TraitId(it) => SolverDefId::TraitId(it),
        LangItemTarget::EnumVariantId(it) => SolverDefId::EnumVariantId(it),
        LangItemTarget::ConstId(it) => SolverDefId::ConstId(it),
        LangItemTarget::MacroId(it) => return item_key(db, ItemId::Macro(it)),
    };
    def_key(db, def)
}

pub fn name_hash(name: &str) -> NameHash {
    NameHash(ContentId::of(NAME_DOMAIN, name.as_bytes()))
}

/// A simplified self type ([`crate::ImplIndexKey`]), with its def named by key.
pub fn simplified<'db>(
    db: &'db Db,
    ty: &hir_ty::next_solver::SimplifiedType<'db>,
) -> SimplifiedType {
    use rustc_type_ir::fast_reject::SimplifiedType as S;
    let mut h = Hasher::new(SIMPLIFIED_DOMAIN);
    match *ty {
        S::Bool => h.update(&[0]),
        S::Char => h.update(&[1]),
        S::Int(it) => h.update(&[2]).part(it.name_str().as_bytes()),
        S::Uint(it) => h.update(&[3]).part(it.name_str().as_bytes()),
        S::Float(it) => h.update(&[4]).part(it.name_str().as_bytes()),
        S::Adt(def) => h.update(&[5]).id(&def_key(db, def).0),
        S::Foreign(def) => h.update(&[6]).id(&def_key(db, def).0),
        S::Str => h.update(&[7]),
        S::Array => h.update(&[8]),
        S::Slice => h.update(&[9]),
        S::Ref(m) => h.update(&[10, m.is_mut() as u8]),
        S::Ptr(m) => h.update(&[11, m.is_mut() as u8]),
        S::Never => h.update(&[12]),
        S::Tuple(n) => h.update(&[13]).update(&(n as u64).to_le_bytes()),
        S::MarkerTraitObject => h.update(&[14]),
        S::Trait(def) => h.update(&[15]).id(&def_key(db, def).0),
        S::Closure(def) => h.update(&[16]).id(&def_key(db, def).0),
        S::Coroutine(def) => h.update(&[17]).id(&def_key(db, def).0),
        S::CoroutineWitness(def) => h.update(&[18]).id(&def_key(db, def).0),
        S::Function(n) => h.update(&[19]).update(&(n as u64).to_le_bytes()),
        S::UnsafeBinder => h.update(&[20]),
        S::Placeholder => h.update(&[21]),
        S::Error => h.update(&[22]),
    };
    SimplifiedType(h.finish())
}

/// The simplified self type of a type that simplifies to nothing (a parameter, an alias): only
/// blanket impls unify with it, so `Implements` for it is a trait's blanket impls alone.
pub fn unsimplified() -> SimplifiedType {
    SimplifiedType(Hasher::new(SIMPLIFIED_DOMAIN).update(&[u8::MAX]).finish())
}

/// The path of an item as the program sees it (`type_name`, `module_path!`, a symbol's
/// demangled name): its crate's name, its modules, a `{block}` for each block it is in, and
/// its name; an impl's item under `<impl>`.
pub fn path(db: &Db, id: ItemId) -> String {
    // alloc: the caller keeps the path (a message, a symbol's name): the edge.
    let mut out = String::new();
    path_segments(db, id, &mut |it| out.push_str(it));
    out
}

/// What `h.part(path(db, id).as_bytes())` gives a hasher, with no string built: `write` gets
/// the length as 8 little-endian bytes, then the path's bytes a segment at a time.
pub(crate) fn path_part(db: &Db, id: ItemId, write: &mut impl FnMut(&[u8])) {
    let mut len = 0u64;
    path_segments(db, id, &mut |it| len += it.len() as u64);
    write(&len.to_le_bytes());
    path_segments(db, id, &mut |it| write(it.as_bytes()));
}

/// [`path`] a segment at a time, each an interned symbol's bytes read in place.
fn path_segments(db: &Db, id: ItemId, write: &mut impl FnMut(&str)) {
    let (module, container) = match id {
        ItemId::Function(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Const(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Static(it) => assoc_container(db, it.lookup(db).container),
        ItemId::Alias(it) => assoc_container(db, it.lookup(db).container),
        other => (other.module(db), None),
    };
    module_path(db, module, write);
    match container {
        Some(ItemContainerId::TraitId(it)) => {
            write("::");
            write(TraitSignature::of(db, it).name.as_str());
        }
        Some(ItemContainerId::ImplId(_)) => {
            write("::");
            write("<impl>");
        }
        Some(ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_)) | None => {}
    }
    write("::");
    item_name(db, id, write);
}

/// The module an associated item's path goes through, and its trait or impl if it has one.
fn assoc_container(
    db: &Db,
    container: ItemContainerId,
) -> (ModuleId, Option<ItemContainerId>) {
    match container {
        ItemContainerId::ModuleId(it) => (it, None),
        ItemContainerId::ExternBlockId(it) => (it.lookup(db).container, None),
        ItemContainerId::TraitId(it) => (it.lookup(db).container, Some(container)),
        ItemContainerId::ImplId(it) => (it.lookup(db).container, Some(container)),
    }
}

fn module_path<'db>(
    db: &'db Db,
    module: ModuleIdLt<'db>,
    write: &mut impl FnMut(&str),
) {
    match module.containing_module(db) {
        Some(parent) => {
            module_path(db, parent, write);
            write("::");
            match module.name(db) {
                Some(name) => write(name.as_str()),
                None => write("{block}"),
            }
        }
        None => match &module.krate(db).extra_data(db).display_name {
            Some(name) => write(name.crate_name().symbol().as_str()),
            None => write("{crate}"),
        },
    }
}

/// Writes an item's name, `_` for one without.
fn item_name(db: &Db, id: ItemId, write: &mut impl FnMut(&str)) {
    fn macro_name(name: Option<syntax::view::Name<'_>>, write: &mut impl FnMut(&str)) {
        write(name.map_or("_", |it| it.text()))
    }
    match id {
        ItemId::Function(it) => write(FunctionSignature::of(db, it).name.as_str()),
        ItemId::Adt(AdtId::StructId(it)) => write(StructSignature::of(db, it).name.as_str()),
        ItemId::Adt(AdtId::EnumId(it)) => write(EnumSignature::of(db, it).name.as_str()),
        ItemId::Adt(AdtId::UnionId(it)) => write(UnionSignature::of(db, it).name.as_str()),
        ItemId::Trait(it) => write(TraitSignature::of(db, it).name.as_str()),
        ItemId::Impl(_) => write("<impl>"),
        ItemId::Const(it) => {
            write(ConstSignature::of(db, it).name.as_ref().map_or("_", |it| it.as_str()))
        }
        ItemId::Static(it) => write(StaticSignature::of(db, it).name.as_str()),
        ItemId::Alias(it) => write(TypeAliasSignature::of(db, it).name.as_str()),
        // hir-def keeps no macro's name but in its syntax.
        ItemId::Macro(MacroId::MacroRulesId(it)) => {
            macro_name(it.lookup(db).id.to_view(db).name(), write)
        }
        ItemId::Macro(MacroId::Macro2Id(it)) => macro_name(it.lookup(db).id.to_view(db).name(), write),
        ItemId::Macro(MacroId::ProcMacroId(it)) => {
            macro_name(it.lookup(db).id.to_view(db).name(), write)
        }
    }
}

impl ItemId {
    pub fn module(self, db: &Db) -> ModuleId {
        match self {
            ItemId::Function(it) => it.module(db),
            ItemId::Adt(it) => it.module(db),
            ItemId::Trait(it) => it.module(db),
            ItemId::Impl(it) => it.module(db),
            ItemId::Const(it) => it.module(db),
            ItemId::Static(it) => it.module(db),
            ItemId::Alias(it) => it.module(db),
            ItemId::Macro(it) => it.module(db),
        }
    }
}

/// The item of the current database a key names, if one still has it (see the module doc).
pub fn resolve(db: &Db, key: ItemKey) -> Option<Resolved> {
    let krates = checked_crates(db);
    let prefix = u64::from_le_bytes(key.crate_prefix());
    let at = krates.binary_search_by_key(&prefix, |&(it, _)| it).ok()?;
    let krate = krates[at].1;
    let index =
        if key.is_inner() { crate_inner_items(db, krate) } else { &crate_items(db, krate).index };
    index.get(&key)
}

/// The crate of the current database whose [`crate_key`] is `key`, if one has it.
pub fn resolve_crate(db: &Db, key: &ContentId) -> Option<Crate> {
    let krates = checked_crates(db);
    let prefix = u64::from_le_bytes(crate_prefix(key));
    let at = krates.binary_search_by_key(&prefix, |&(it, _)| it).ok()?;
    let krate = krates[at].1;
    (crate_key(db, krate) == *key).then_some(krate)
}

/// The most keys one crate's index holds: address space only, committed as the index grows.
/// A crate past it stops with [`MemError::Exhausted`] naming the index.
pub(crate) const MAX_KEYS: usize = 1 << 24;

/// A key and what it names in one index.
pub(crate) type Row<V> = (ItemKey, V);

/// Keys and what each names, sorted by [`order`] then key, and a directory of `2^bits + 1`
/// row starts by the top `bits` of the order: the rows of order `o` are in
/// `starts[o >> (64 - bits)]..starts[(o >> (64 - bits)) + 1]`. Past its crate's 9 bits a key is
/// BLAKE3's output, so with a bucket per row a run holds one row on average and a lookup reads
/// one directory word and one row: two cache misses, no hashing, no probing.
///
/// The rows grow in place in a reserved region while the crate is walked; the directory is
/// sized once from the row count, one allocation per index.
pub(crate) struct KeyIndex<V: Copy> {
    rows: RegionVec<Row<V>>,
    starts: Box<[u32]>,
    bits: u32,
}

/// The 64 bits of a key a directory is over: bytes 8..16 without the inner bit, big-endian so
/// they order as the key's bytes do.
fn order(key: &ItemKey) -> u64 {
    let word: [u8; 8] = *key.0.0[8..16].first_chunk().expect("a key is 32 bytes");
    u64::from_be_bytes(word) << 1
}

fn bucket(order: u64, bits: u32) -> usize {
    if bits == 0 { 0 } else { (order >> (64 - bits)) as usize }
}

/// Stops with `error`: an index that cannot have its memory stops the build, naming it.
#[cold]
#[track_caller]
fn stop(what: &str, error: MemError) -> ! {
    panic!("blaze-items: {what}: {error}")
}

/// An empty column of an index's rows, `MAX_KEYS` of them reserved.
#[track_caller]
pub(crate) fn key_rows<T>(what: &str) -> RegionVec<T> {
    RegionVec::new(MAX_KEYS, Tag::Other).unwrap_or_else(|error| stop(what, error))
}

/// Sorts `rows` by order then key and drops repeats in place, returning how many are left.
/// A key twice must name one thing.
#[track_caller]
pub(crate) fn sort_distinct<V: Copy + PartialEq + fmt::Debug>(rows: &mut [Row<V>]) -> usize {
    rows.sort_unstable_by_key(|&(key, _)| (order(&key), key));
    let mut kept = 0;
    for at in 0..rows.len() {
        let (key, resolved) = rows[at];
        if kept > 0 && rows[kept - 1].0 == key {
            let old = rows[kept - 1].1;
            assert_eq!(
                old, resolved,
                "blaze-items: two items with one key {key:?}: {old:?} and {resolved:?}"
            );
            continue;
        }
        rows[kept] = (key, resolved);
        kept += 1;
    }
    kept
}

impl<V: Copy + PartialEq + fmt::Debug> KeyIndex<V> {
    /// The index of `rows`, in any order and with repeats.
    #[track_caller]
    pub(crate) fn new(mut rows: RegionVec<Row<V>>) -> KeyIndex<V> {
        let kept = sort_distinct(&mut rows);
        rows.truncate(kept);
        rows.shrink_to(kept);
        let len = u32::try_from(kept).expect("blaze-items: an index of fewer than 2^32 keys");
        let bits = kept.next_power_of_two().trailing_zeros();
        let buckets = 1usize << bits;
        let mut starts = Vec::with_capacity(buckets + 1);
        let mut row = 0;
        for at in 0..buckets {
            while row < kept && bucket(order(&rows[row].0), bits) < at {
                row += 1;
            }
            starts.push(row as u32);
        }
        starts.push(len);
        KeyIndex { rows, starts: starts.into_boxed_slice(), bits }
    }

    pub(crate) fn get(&self, key: &ItemKey) -> Option<V> {
        let at = bucket(order(key), self.bits);
        let run = self.starts[at] as usize..self.starts[at + 1] as usize;
        self.rows[run].iter().find(|(it, _)| it == key).map(|&(_, value)| value)
    }

    pub(crate) fn rows(&self) -> &[Row<V>] {
        &self.rows
    }
}

impl<V: Copy + PartialEq> PartialEq for KeyIndex<V> {
    fn eq(&self, other: &KeyIndex<V>) -> bool {
        self.rows[..] == other.rows[..]
    }
}

impl<V: Copy + Eq> Eq for KeyIndex<V> {}

impl<V: Copy + fmt::Debug> fmt::Debug for KeyIndex<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.rows.iter().map(|(key, value)| (key, value))).finish()
    }
}

/// Hashing for maps keyed by a key, which is a BLAKE3 hash already: its words are folded into
/// the map's hash, which was computed once with the key and is carried, not hashed again.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct KeyHashing;

impl std::hash::BuildHasher for KeyHashing {
    type Hasher = KeyHasher;

    fn build_hasher(&self) -> KeyHasher {
        KeyHasher(0)
    }
}

pub(crate) struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    // A `[u8; 32]` is hashed as its length and then its bytes: the length is the same for
    // every key, so only the bytes count.
    fn write_usize(&mut self, _: usize) {}

    fn write(&mut self, bytes: &[u8]) {
        // A key's: its first 9 bytes are its crate's and one bit, the same for a whole index,
        // and its last 8 are as random as BLAKE3 makes them.
        if let Ok(key) = <&[u8; 32]>::try_from(bytes) {
            self.0 = self.0.rotate_left(29) ^ u64::from_le_bytes(*key.last_chunk::<8>().expect("32 bytes"));
            return;
        }
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = self.0.rotate_left(29) ^ u64::from_le_bytes(word);
        }
    }
}

/// Two crates of one build that [`crate_key`] does not tell apart: an item of one would be
/// taken for the other's, so the build stops ([`check_crate_keys`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CrateKeyClash {
    pub crates: [ClashingCrate; 2],
    /// Whether the two keys are one; else only their first 8 bytes, the prefix an item key
    /// holds, are (a 64-bit collision).
    pub same_key: bool,
}

/// A crate of a [`CrateKeyClash`], as the error names it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClashingCrate {
    pub name: String,
    pub version: Option<String>,
    pub origin: String,
    pub kind: String,
}

impl ClashingCrate {
    fn of(db: &Db, krate: Crate) -> ClashingCrate {
        let extra = krate.extra_data(db);
        let data = krate.data(db);
        let name = extra.display_name.as_ref().map_or("<unnamed>", |it| it.canonical_name().as_str());
        ClashingCrate {
            name: name.to_owned(), // alloc: an error's, once
            version: extra.version.clone(), // clone: an error's, once
            origin: format!("{:?}", data.origin), // alloc: an error's, once
            kind: format!("{:?}", data.target_kind), // alloc: an error's, once
        }
    }
}

impl fmt::Display for ClashingCrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{} ({}, {})", self.name, self.version.as_deref().unwrap_or("?"), self.origin, self.kind)
    }
}

impl fmt::Display for CrateKeyClash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b] = &self.crates;
        match self.same_key {
            true => write!(
                f,
                "crates {a} and {b} have one key: the same name, version, origin, kind of target, \
                 edition, crate attributes, dependencies and cfg, so the crate graph holds one crate twice"
            ),
            false => write!(f, "crates {a} and {b} have keys with equal first 8 bytes (a 64-bit collision)"),
        }
    }
}

impl std::error::Error for CrateKeyClash {}

/// Stops the build when two of its crates have one key or one key prefix. A build checks it
/// once its crate graph is set and before any key is resolved: [`resolve`] and
/// [`resolve_crate`] hold it.
pub fn check_crate_keys(db: &Db) -> Result<(), CrateKeyClash> {
    match crates_by_prefix(db) {
        Ok(_) => Ok(()),
        Err(clash) => Err(clash.clone()), // clone: an error, once
    }
}

/// Every crate of the build by the first 8 bytes of its key, sorted; the first two crates found
/// with one prefix as an error.
#[blaze_query::tracked(returns(ref))]
fn crates_by_prefix(db: &Db) -> Result<Vec<(u64, Crate)>, CrateKeyClash> {
    let mut krates: Vec<(u64, Crate)> = base_db::all_crates(db)
        .iter()
        .map(|&krate| (u64::from_le_bytes(crate_prefix(&crate_key(db, krate))), krate))
        // alloc: the memo's list, one per build, at its final length.
        .collect();
    radix::sort_copy_by_key(&mut krates, |&(prefix, _)| prefix);
    for pair in krates.windows(2) {
        let ((prefix, krate), (other_prefix, other)) = (pair[0], pair[1]);
        if prefix == other_prefix {
            return Err(CrateKeyClash {
                crates: [ClashingCrate::of(db, krate), ClashingCrate::of(db, other)],
                same_key: crate_key(db, krate) == crate_key(db, other),
            });
        }
    }
    Ok(krates)
}

/// [`crates_by_prefix`] of a build [`check_crate_keys`] let through.
fn checked_crates(db: &Db) -> &[(u64, Crate)] {
    match crates_by_prefix(db) {
        Ok(it) => it,
        Err(clash) => panic!("blaze-items: a key resolved in a build check_crate_keys stops: {clash}"),
    }
}

/// Every item of `krate` with an [`ItemId`], those of its bodies' blocks included.
pub(crate) fn crate_item_ids(db: &Db, krate: Crate) -> impl Iterator<Item = ItemId> + '_ {
    let outer = crate_items(db, krate).index.rows();
    let inner = crate_inner_items(db, krate).rows();
    outer.iter().chain(inner).filter_map(|&(_, it)| match it {
        Resolved::Item(id) => Some(id),
        _ => None,
    })
}

/// What has a key inside a crate's stores and items' types, in one index: a key that is no
/// module, impl or trait item is one lookup, not one per store of every crate.
#[blaze_query::tracked(returns(ref))]
fn crate_inner_items(db: &Db, krate: Crate) -> KeyIndex<Resolved> {
    const WHAT: &str = "a crate's index of keys inside bodies";
    let mut rows = key_rows(WHAT);
    for &inner in crate_items(db, krate).inner.iter() {
        if let Err(error) = rows.try_extend_from_slice(inner_items(db, inner, ())) {
            stop(WHAT, error)
        }
    }
    KeyIndex::new(rows)
}

/// Where else an item has things with keys: a store, with its closures and the items of its
/// blocks, or the opaque types of an item's type, which are counted by lowering it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Inner {
    Store(ExpressionStoreOwnerId),
    Opaques(ItemId),
}

/// A crate's index of its modules', impls' and traits' items, and every store and item type
/// in it to look inside.
#[derive(Debug)]
struct Items {
    index: KeyIndex<Resolved>,
    inner: RegionVec<Inner>,
}

impl PartialEq for Items {
    fn eq(&self, other: &Items) -> bool {
        self.index == other.index && self.inner[..] == other.inner[..]
    }
}

impl Eq for Items {}

/// Where [`def_map_items`] puts what it finds: keys with what they name, and where else to look.
trait Sink {
    fn add(&mut self, key: ItemKey, resolved: Resolved);
    fn inner(&mut self, inner: Inner);
}

/// A crate's rows while its modules are walked, growing in place in reserved regions.
struct CrateRows {
    rows: RegionVec<Row<Resolved>>,
    inner: RegionVec<Inner>,
}

const CRATE_KEYS: &str = "a crate's index of keys";
const CRATE_STORES: &str = "a crate's list of stores";

impl Sink for CrateRows {
    fn add(&mut self, key: ItemKey, resolved: Resolved) {
        if let Err(error) = self.rows.try_push((key, resolved)) {
            stop(CRATE_KEYS, error)
        }
    }

    fn inner(&mut self, inner: Inner) {
        if let Err(error) = self.inner.try_push(inner) {
            stop(CRATE_STORES, error)
        }
    }
}

/// One store's rows, in its task's frame.
struct FrameRows<'f> {
    rows: Vec<Row<Resolved>, &'f Frame>,
    inner: Vec<Inner, &'f Frame>,
}

impl<'f> FrameRows<'f> {
    fn new(frame: &'f Frame) -> FrameRows<'f> {
        FrameRows { rows: Vec::new_in(frame), inner: Vec::new_in(frame) }
    }
}

impl Sink for FrameRows<'_> {
    fn add(&mut self, key: ItemKey, resolved: Resolved) {
        self.rows.push((key, resolved));
    }

    fn inner(&mut self, inner: Inner) {
        self.inner.push(inner);
    }
}

/// The items of a crate's modules, impls and traits, and where to look for the rest.
#[blaze_query::tracked(returns(ref))]
fn crate_items(db: &Db, krate: Crate) -> Items {
    let mut rows = CrateRows { rows: key_rows(CRATE_KEYS), inner: key_rows(CRATE_STORES) };
    def_map_items(db, hir_def::nameres::crate_def_map(db, krate), &mut rows);
    let CrateRows { rows, mut inner } = rows;
    let stores = inner.len();
    inner.shrink_to(stores);
    Items { index: KeyIndex::new(rows), inner }
}

/// What has a key inside one store or one item's type, transitively (the items of a block
/// have stores of their own), sorted by [`order`] and distinct.
// The unit makes the arguments an interned key: `Inner` alone is no ID.
#[blaze_query::tracked(returns(ref))]
fn inner_items(db: &Db, inner: Inner, _: ()) -> Box<[Row<Resolved>]> {
    frame::task(Tag::Other, |frame| inner_rows(db, inner, frame))
}

fn inner_rows(db: &Db, inner: Inner, frame: &Frame) -> Box<[Row<Resolved>]> {
    let mut items = FrameRows::new(frame);
    match inner {
        Inner::Opaques(owner) => {
            let count = match owner {
                ItemId::Function(it) => SelfAndAssocBounds::return_type_impl_traits(db, it).len(),
                ItemId::Alias(it) => SelfAndAssocBounds::type_alias_opaque_count(db, it),
                _ => 0,
            };
            for index in 0..count as u32 {
                items.add(opaque_key(db, owner, index), Resolved::Opaque { owner, index });
            }
        }
        Inner::Store(store) => {
            let exprs = ExpressionStore::of(db, store);
            let mut roots = exprs.expr_roots();
            // A body's last root is the body itself, keyed by its item, not an anonymous
            // constant: its index moves with every edit inside the body.
            if let ExpressionStoreOwnerId::Body(_) = store {
                roots.next_back();
            }
            for expr in roots {
                items.add(anon_const_key_of(db, store, expr), Resolved::AnonConst { store, expr });
            }
            let mut nth = 0u32;
            for (expr, it) in exprs.exprs() {
                let Expr::Closure { closure_kind, .. } = it else { continue };
                let kind = closure_kind;
                let resolved = Resolved::Closure { store, expr, kind };
                match kind {
                    ClosureKind::Closure => {
                        items.add(closure_key_of(db, store, b"closure", nth), resolved)
                    }
                    ClosureKind::CoroutineClosure(_) => {
                        items.add(closure_key_of(db, store, b"closure", nth), resolved);
                        items.add(closure_key_of(db, store, b"coroutine closure", nth), resolved);
                    }
                    ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. } => {
                        items.add(closure_key_of(db, store, b"coroutine", nth), resolved)
                    }
                }
                nth += 1;
            }
            let mut blocks = FrameRows::new(frame);
            for (_, block) in exprs.blocks(db) {
                def_map_items(db, block, &mut blocks);
            }
            for &inner in blocks.inner.iter() {
                items.rows.extend_from_slice(inner_items(db, inner, ()));
            }
            items.rows.extend_from_slice(&blocks.rows);
        }
    }
    let kept = sort_distinct(&mut items.rows);
    Box::from(&items.rows[..kept])
}

/// The items of a def map's modules, and where to look inside them.
fn def_map_items(db: &Db, def_map: &DefMap, items: &mut impl Sink) {
    for (module, data) in def_map.modules(db) {
        items.add(module_key(db, module), Resolved::Module(module));
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => item(db, ItemId::Function(it), items),
                ModuleDefId::AdtId(it) => item(db, ItemId::Adt(it), items),
                ModuleDefId::ConstId(it) => item(db, ItemId::Const(it), items),
                ModuleDefId::StaticId(it) => item(db, ItemId::Static(it), items),
                ModuleDefId::TraitId(it) => {
                    item(db, ItemId::Trait(it), items);
                    for &(_, assoc) in &it.trait_items(db).items {
                        item(db, assoc.into(), items);
                    }
                }
                ModuleDefId::TypeAliasId(it) => item(db, ItemId::Alias(it), items),
                ModuleDefId::MacroId(it) => item(db, ItemId::Macro(it), items),
                ModuleDefId::EnumVariantId(_)
                | ModuleDefId::ModuleId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.impls() {
            item(db, ItemId::Impl(it), items);
            for &(_, assoc) in &it.impl_items(db).items {
                item(db, assoc.into(), items);
            }
        }
        for it in scope.unnamed_consts() {
            item(db, ItemId::Const(it), items);
        }
        for (_, macros) in scope.legacy_macros() {
            for &it in macros {
                if it.module(db) == module {
                    item(db, ItemId::Macro(it), items);
                }
            }
        }
        for it in scope.builtin_derive_impls() {
            items.add(builtin_derive_key(db, it), Resolved::BuiltinDerive(it));
        }
    }
}

/// Whether a struct or variant has a constructor function: a tuple or unit one has.
fn has_ctor(db: &Db, variant: VariantId) -> bool {
    !matches!(variant.fields(db).shape, FieldsShape::Record)
}

/// An item, its enum's variants, and where to look inside it.
fn item(db: &Db, id: ItemId, items: &mut impl Sink) {
    items.add(item_key(db, id), Resolved::Item(id));
    if let Some(def) = id.generic_def() {
        items.inner(Inner::Store(ExpressionStoreOwnerId::Signature(def)));
    }
    if let Some(body) = id.body() {
        items.inner(Inner::Store(ExpressionStoreOwnerId::Body(body)));
    }
    match id {
        ItemId::Function(_) | ItemId::Alias(_) => items.inner(Inner::Opaques(id)),
        ItemId::Adt(AdtId::EnumId(it)) => {
            for (_, &(variant, _)) in &it.enum_variants(db).variants {
                items.add(variant_key(db, variant), Resolved::Variant(variant));
                if has_ctor(db, variant.into()) {
                    let ctor = Ctor::Enum(variant);
                    items.add(ctor_key(db, ctor), Resolved::Ctor(ctor));
                }
                items.inner(Inner::Store(ExpressionStoreOwnerId::Body(variant.into())));
                items.inner(Inner::Store(ExpressionStoreOwnerId::VariantFields(variant.into())));
            }
        }
        ItemId::Adt(AdtId::StructId(it)) => {
            if has_ctor(db, it.into()) {
                let ctor = Ctor::Struct(it);
                items.add(ctor_key(db, ctor), Resolved::Ctor(ctor));
            }
            items.inner(Inner::Store(ExpressionStoreOwnerId::VariantFields(it.into())))
        }
        ItemId::Adt(AdtId::UnionId(it)) => {
            items.inner(Inner::Store(ExpressionStoreOwnerId::VariantFields(it.into())))
        }
        _ => {}
    }
}
