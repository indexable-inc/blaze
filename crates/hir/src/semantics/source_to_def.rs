//! What the syntax-to-def walks share: the per-`Semantics` caches (dyn maps, file to module,
//! `include!` files, expansions) carried from one `Semantics` to the next on a thread, the file
//! to module lookup and [`ChildContainer`]. The walks over views are in
//! `view/source_to_def.rs`; the algorithm they follow:
//!
//! This is a very interesting module, and, in some sense, can be considered the
//! heart of the IDE parts of rust-analyzer.
//!
//! This module solves the following problem:
//!
//! > Given a piece of syntax, find the corresponding semantic definition (def).
//!
//! This problem is a part of more-or-less every IDE feature implemented. Every
//! IDE functionality (like goto to definition), conceptually starts with a
//! specific cursor position in a file. Starting with this text offset, we first
//! figure out what syntactic construct we are at: is this a pattern, an
//! expression, an item definition.
//!
//! Knowing only the syntax gives us relatively little info. For example,
//! looking at the syntax of the function we can realize that it is a part of an
//! `impl` block, but we won't be able to tell what trait function the current
//! function overrides, and whether it does that correctly. For that, we need to
//! go from `ast::Fn` to [`crate::Function`], and that's exactly what this
//! module does.
//!
//! As syntax trees are values and don't know their place of origin/identity,
//! this module also requires `InFile` wrappers to understand which specific
//! real or macro-expanded file the tree comes from.
//!
//! The actual algorithm to resolve syntax to def is curious in two aspects:
//!
//! * It is recursive
//! * It uses the inverse algorithm (what is the syntax for this def?)
//!
//! Specifically, the algorithm goes like this:
//!
//! 1. Find the syntactic container for the syntax. For example, field's
//!    container is the struct, and the struct's container is a module.
//! 2. Recursively get the def corresponding to container.
//! 3. Ask the container def for all child defs. These child defs contain
//!    the answer and answer's siblings.
//! 4. For each child def, ask for its source.
//! 5. The child def whose source is the syntax node we've started with
//!    is the answer.
//!
//! It's interesting that both Roslyn and Kotlin contain very similar code
//! shape.
//!
//! Let's take a look at Roslyn:
//!
//!   <https://github.com/dotnet/roslyn/blob/36a0c338d6621cc5fe34b79d414074a95a6a489c/src/Compilers/CSharp/Portable/Compilation/SyntaxTreeSemanticModel.cs#L1403-L1429>
//!   <https://sourceroslyn.io/#Microsoft.CodeAnalysis.CSharp/Compilation/SyntaxTreeSemanticModel.cs,1403>
//!
//! The `GetDeclaredType` takes `Syntax` as input, and returns `Symbol` as
//! output. First, it retrieves a `Symbol` for parent `Syntax`:
//!
//! * <https://sourceroslyn.io/#Microsoft.CodeAnalysis.CSharp/Compilation/SyntaxTreeSemanticModel.cs,1423>
//!
//! Then, it iterates parent symbol's children, looking for one which has the
//! same text span as the original node:
//!
//!   <https://sourceroslyn.io/#Microsoft.CodeAnalysis.CSharp/Compilation/SyntaxTreeSemanticModel.cs,1786>
//!
//! Now, let's look at Kotlin:
//!
//!   <https://github.com/JetBrains/kotlin/blob/a288b8b00e4754a1872b164999c6d3f3b8c8994a/idea/idea-frontend-fir/idea-fir-low-level-api/src/org/jetbrains/kotlin/idea/fir/low/level/api/FirModuleResolveStateImpl.kt#L93-L125>
//!
//! This function starts with a syntax node (`KtExpression` is syntax, like all
//! `Kt` nodes), and returns a def. It uses
//! `getNonLocalContainingOrThisDeclaration` to get syntactic container for a
//! current node. Then, `findSourceNonLocalFirDeclaration` gets `Fir` for this
//! parent. Finally, `findElementIn` function traverses `Fir` children to find
//! one with the same source we originally started with.
//!
//! One question is left though -- where does the recursion stops? This happens
//! when we get to the file syntax node, which doesn't have a syntactic parent.
//! In that case, we loop through all the crates that might contain this file
//! and look for a module whose source is the given file.
//!
//! Note that the logic in this module is somewhat fundamentally imprecise --
//! due to conditional compilation and `#[path]` attributes, there's no
//! injective mapping from syntax nodes to defs. This is not an edge case --
//! more or less every item in a `lib.rs` is a part of two distinct crates: a
//! library with `--cfg test` and a library without.
//!
//! At the moment, we don't really handle this well and return the first answer
//! that works. Ideally, we should first let the caller to pick a specific
//! active crate for a given position, and then provide an API to resolve all
//! syntax nodes against this specific crate.

use std::{collections::hash_map::Entry, sync::Arc};

use base_db::{Crate, Db, relevant_crates};
use blaze_hash::{HashMap, HashSet};
use hir_def::{
    AdtId, DefWithBodyId, EnumId, ExpressionStoreOwnerId, GenericDefId, ImplId, Lookup, ModuleId,
    TraitId, VariantId, dyn_map::DynMap, nameres::crate_def_map,
};
use hir_expand::{EditionedFileId, ExpansionInfo, HirFileId, MacroCallId};
use smallvec::SmallVec;
use span::FileId;
use stdx::impl_from;

use crate::db::HirDatabase;

#[derive(Default)]
pub(super) struct SourceToDefCache<'db> {
    pub(super) dynmap_cache: HashMap<(ChildContainer, HirFileId), DynMap>,
    /// Where each macro file's expansion is in `expansions`.
    expansion_at: HashMap<MacroCallId, u32>,
    expansions: Vec<ExpansionInfo<'db>>,
    pub(super) file_to_def_cache: HashMap<FileId, SmallVec<[ModuleId; 1]>>,
    pub(super) included_file_cache: HashMap<EditionedFileId, Option<MacroCallId>>,
    /// The crates a file is taken to be a module of, of those it may be, when any is.
    pub(super) within: Option<Arc<HashSet<Crate>>>,
}

/// The crates `file` may be a module of: those `within` first, then the others. The file of a
/// package is in the source root of each of its targets, and each would otherwise have its
/// module tree built.
fn crates_of<'a>(
    db: &'a Db,
    within: Option<&'a HashSet<Crate>>,
    file: FileId,
    first: bool,
) -> impl Iterator<Item = Crate> + 'a {
    relevant_crates(db, file)
        .iter()
        .copied()
        .filter(move |it| within.is_some_and(|within| within.contains(it)) == first)
}

impl<'db> SourceToDefCache<'db> {
    pub(super) fn get_or_insert_include_for(
        &mut self,
        db: &Db,
        file: EditionedFileId,
    ) -> Option<MacroCallId> {
        if let Some(&m) = self.included_file_cache.get(&file) {
            return m;
        }
        self.included_file_cache.insert(file, None);
        let within = self.within.as_deref();
        for first in [true, false] {
            for crate_id in crates_of(db, within, file.file_id(db), first) {
                for &(macro_call_id, file_id) in hir_def::include_macro_invoc(db, crate_id) {
                    self.included_file_cache.insert(file_id, Some(macro_call_id));
                }
            }
            if let Some(&Some(it)) = self.included_file_cache.get(&file) {
                return Some(it);
            }
        }
        None
    }

    pub(super) fn get_or_insert_expansion(
        &mut self,
        db: &'db Db,
        macro_file: MacroCallId,
    ) -> &ExpansionInfo<'db> {
        let at = match self.expansion_at.entry(macro_file) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let exp_info = macro_file.expansion_info(db);
                let at = u32::try_from(self.expansions.len())
                    .expect("more than u32::MAX macro expansions in one `Semantics`");
                self.expansions.push(exp_info);
                *it.insert(at)
            }
        };
        &self.expansions[at as usize]
    }
}

pub(super) struct SourceToDefCtx<'db, 'cache> {
    pub(super) db: &'db Db,
    pub(super) cache: &'cache mut SourceToDefCache<'db>,
}

impl<'db> SourceToDefCtx<'db, '_> {
    pub(super) fn file_to_def(&mut self, file: FileId) -> &SmallVec<[ModuleId; 1]> {
        let _p = tracing::info_span!("SourceToDefCtx::file_to_def").entered();
        if !self.cache.file_to_def_cache.contains_key(&file) {
            // clone: `Arc` bump so the set outlives `&mut self` below, once per file cache miss
            let within = self.cache.within.clone();
            let mut mods = self.modules_in(crates_of(self.db, within.as_deref(), file, true), file);
            if mods.is_empty() {
                mods = self.modules_in(crates_of(self.db, within.as_deref(), file, false), file);
            }
            self.cache.file_to_def_cache.insert(file, mods);
        }
        &self.cache.file_to_def_cache[&file]
    }

    /// The first module of `file` in `krate`, whether or not `krate` is `within`.
    pub(super) fn file_to_def_in(&mut self, krate: Crate, file: FileId) -> Option<ModuleId> {
        let db = self.db;
        if let Some(&module) = self.file_to_def(file).iter().find(|it| it.krate(db) == krate) {
            return Some(module);
        }
        self.modules_in(std::iter::once(krate), file).first().copied()
    }

    fn modules_in(
        &mut self,
        crates: impl Iterator<Item = Crate>,
        file: FileId,
    ) -> SmallVec<[ModuleId; 1]> {
        let mut mods = SmallVec::new();
        for crate_id in crates {
            // A module only a block's def map has is the view's `block_declared_module`.
            let crate_def_map = crate_def_map(self.db, crate_id);
            let n_mods = mods.len();
            let modules = |file| crate_def_map.modules_for_file(self.db, file);
            mods.extend(modules(file));
            if mods.len() == n_mods {
                mods.extend(
                    hir_def::include_macro_invoc(self.db, crate_id)
                        .iter()
                        .filter(|&&(_, file_id)| file_id.file_id(self.db) == file)
                        .flat_map(|&(macro_call_id, file_id)| {
                            self.cache.included_file_cache.insert(file_id, Some(macro_call_id));
                            modules(
                                macro_call_id
                                    .lookup(self.db)
                                    .kind
                                    .file_id()
                                    .original_file(self.db)
                                    .file_id(self.db),
                            )
                        }),
                );
            }
        }
        if mods.is_empty() {
            // FIXME: detached file
        }
        mods
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ChildContainer {
    DefWithBodyId(DefWithBodyId),
    ModuleId(ModuleId),
    TraitId(TraitId),
    ImplId(ImplId),
    EnumId(EnumId),
    VariantId(VariantId),
    /// XXX: this might be the same def as, for example an `EnumId`. However,
    /// here the children are generic parameters, and not, eg enum variants.
    GenericDefId(GenericDefId),
}
impl_from! {
    DefWithBodyId,
    ModuleId,
    TraitId,
    ImplId,
    EnumId,
    VariantId,
    GenericDefId
    for ChildContainer
}

impl ChildContainer {
    pub(crate) fn as_expression_store_owner(self) -> Option<ExpressionStoreOwnerId> {
        match self {
            ChildContainer::DefWithBodyId(it) => Some(it.into()),
            ChildContainer::ModuleId(_) => None,
            ChildContainer::TraitId(it) => {
                Some(ExpressionStoreOwnerId::Signature(GenericDefId::TraitId(it)))
            }
            ChildContainer::EnumId(it) => {
                Some(ExpressionStoreOwnerId::Signature(GenericDefId::AdtId(it.into())))
            }
            ChildContainer::ImplId(it) => {
                Some(ExpressionStoreOwnerId::Signature(GenericDefId::ImplId(it)))
            }
            ChildContainer::VariantId(_) => None,
            ChildContainer::GenericDefId(it) => Some(it.into()),
        }
    }
}

/// What a `Semantics` leaves to the next one made on its thread, at the database nonce and
/// revision it was made at. A request's `Semantics` would otherwise grow every table from
/// empty and rebuild every `DynMap` it reads.
#[derive(Default)]
struct Carry {
    at: Option<(base_db::Nonce, blaze_query::db::View)>,
    dynmap_cache: HashMap<(ChildContainer, HirFileId), DynMap>,
    expansion_at: HashMap<MacroCallId, u32>,
    /// Always empty: its storage, for the next `Semantics`' expansions.
    expansions: Vec<ExpansionInfo<'static>>,
    file_to_def_cache: HashMap<FileId, SmallVec<[ModuleId; 1]>>,
    included_file_cache: HashMap<EditionedFileId, Option<MacroCallId>>,
}

/// More dyn maps than this are not carried: a thread that walked many files keeps their
/// tables' storage but not the maps.
const CARRIED_DYN_MAPS: usize = 4096;

thread_local! {
    static CARRY: std::cell::RefCell<Option<Carry>> = const { std::cell::RefCell::new(None) };
}

impl<'db> SourceToDefCache<'db> {
    /// The thread's carried caches for a `Semantics` of `db`: whole when the last `Semantics`
    /// on this thread was of the same database at the same revision, else empty with their
    /// capacity.
    pub(super) fn take(db: &'db Db) -> SourceToDefCache<'db> {
        let mut carry =
            CARRY.try_with(|it| it.borrow_mut().take()).ok().flatten().unwrap_or_default();
        if carry.at != Some(db.nonce_and_revision()) {
            carry.dynmap_cache.clear();
            carry.file_to_def_cache.clear();
            carry.included_file_cache.clear();
        }
        SourceToDefCache {
            dynmap_cache: carry.dynmap_cache,
            expansion_at: carry.expansion_at,
            expansions: crate::scratch::recycle(carry.expansions),
            file_to_def_cache: carry.file_to_def_cache,
            included_file_cache: carry.included_file_cache,
            within: None,
        }
    }

    /// Leaves `self` to the next `Semantics` on this thread. A cache that answered for some
    /// crates only (`within`) leaves no file answers.
    pub(super) fn give(mut self, db: &Db) {
        if self.within.is_some() {
            self.file_to_def_cache.clear();
            self.included_file_cache.clear();
        }
        if self.dynmap_cache.len() > CARRIED_DYN_MAPS {
            self.dynmap_cache.clear();
        }
        self.expansion_at.clear();
        let carry = Carry {
            at: Some(db.nonce_and_revision()),
            dynmap_cache: self.dynmap_cache,
            expansion_at: self.expansion_at,
            expansions: crate::scratch::recycle(self.expansions),
            file_to_def_cache: self.file_to_def_cache,
            included_file_cache: self.included_file_cache,
        };
        // A `Semantics` alive inside another one's request leaves nothing: the outer one's
        // tables are the larger.
        let _ = CARRY.try_with(|it| {
            let mut slot = it.borrow_mut();
            if slot.is_none() {
                *slot = Some(carry);
            }
        });
    }
}
