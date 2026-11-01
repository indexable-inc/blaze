//! [`Semantics`] over borrowed syntax views (frontend.md item 16): what the source means, taking
//! `syntax::view` nodes by value and giving views borrowed from the database, `'db` from
//! `db: &'db Db`. Callers write `use hir::view::Semantics;`.
//!
//! A view does not know its file. Every tree this hands out (`parse`, `parse_or_expand`,
//! `expand`, `source`, ...) is recorded by its address with its [`HirFileId`], and a view is
//! put in its file again by its tree's address ([`Semantics::hir_file_for`]). The dyn maps,
//! file-to-module tables and anon-const caches are [`SemanticsImpl`]'s, which hold ids and
//! kind tags only.

mod analyze;
mod has_source;
mod source_to_def;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, fmt, ptr, sync::Arc};

use base_db::{Db, toolchain_channel};
use blaze_hash::{HashMap, HashSet};
use hir_def::{
    expr_store::ExprOrPatSource,
        nameres::ModuleOrigin,
};
use hir_expand::{AstId, EditionedFileId, ExpandResult, HirFileId, MacroCallId};
use hir_ty::next_solver::AnyImplId;
use span::FileId;
use syntax::{
    AstPtr, FlatTree, SyntaxNodePtr,
    view::{self as ast, AstNode as _, Node, Token, Typed},
};

use crate::{Adt, Crate, ExpressionStoreOwner, Impl, InFile, Module, semantics::SemanticsImpl};

pub use self::{
    has_source::{FieldSourceView, HasSourceView, LocalSourceView, ModuleSourceView},
    source_to_def::ToDef,
};

/// [`crate::Semantics`] over views: what the build path reads syntax through.
pub struct Semantics<'db> {
    pub db: &'db Db,
    pub(super) imp: SemanticsImpl<'db>,
    /// The file of each tree this handed out, by the address of the file's tree.
    roots: RefCell<HashMap<usize, HirFileId>>,
}

impl fmt::Debug for Semantics<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "view::Semantics {{ ... }}")
    }
}

/// The key of the tree a view is in: the address of its file's tree. A tree recorded here is
/// borrowed for `'db` from the database's memo, so no other tree takes its address while a
/// `Semantics<'db>` is alive.
#[inline]
fn tree_key(node: Node<'_>) -> usize {
    ptr::from_ref::<FlatTree>(node.file()).addr()
}

thread_local! {
    /// The `roots` of the last `Semantics` dropped on this thread, emptied: the next one takes
    /// its allocation, as the source-to-def tables are carried (`SourceToDefCache::take`).
    static ROOTS: RefCell<HashMap<usize, HirFileId>> = RefCell::default();
}

impl Drop for Semantics<'_> {
    fn drop(&mut self) {
        let mut roots = std::mem::take(self.roots.get_mut());
        roots.clear();
        let _ = ROOTS.try_with(|it| it.replace(roots));
    }
}

impl<'db> Semantics<'db> {
    pub fn new(db: &'db Db) -> Semantics<'db> {
        Semantics { db, imp: SemanticsImpl::new(db), roots: RefCell::new(ROOTS.take()) }
    }

    /// Like `new`, with a file taken as a module of only those of `crates` it is in, when it
    /// is in any.
    pub fn new_within(db: &'db Db, crates: Arc<HashSet<base_db::Crate>>) -> Semantics<'db> {
        let mut imp = SemanticsImpl::new(db);
        imp.s2d_cache.get_mut().within = Some(crates);
        Semantics { db, imp, roots: RefCell::new(ROOTS.take()) }
    }

    /// Records `root`'s tree as the tree of `file_id`.
    pub(super) fn cache(&self, root: Node<'db>, file_id: HirFileId) {
        assert!(root.parent().is_none(), "{root:?} of {file_id:?} is not a tree's root");
        let prev = self.roots.borrow_mut().insert(tree_key(root), file_id);
        assert!(
            prev.is_none() || prev == Some(file_id),
            "the tree of {file_id:?} was handed out as the tree of {prev:?}"
        );
    }

    fn lookup(&self, node: Node<'db>) -> Option<HirFileId> {
        self.roots.borrow().get(&tree_key(node)).copied()
    }

    /// `node` in the file it is of.
    ///
    /// # Panics
    ///
    /// When `node` is of a tree this `Semantics` did not hand out, as the handle version does.
    pub(super) fn find_file(&self, node: Node<'db>) -> InFile<Node<'db>> {
        let file_id = self.lookup(node).unwrap_or_else(|| {
            panic!(
                "\n\nFailed to lookup {node:?} in this view::Semantics.\n\
                 Make sure to only query views of trees this Semantics handed out \
                 (parse, parse_or_expand, expand, source, ...).\n\
                 root node:   {:?}\n\
                 known trees: {}\n\n",
                node.tree_top(),
                self.roots.borrow().len(),
            )
        });
        InFile::new(file_id, node)
    }

    pub(super) fn wrap_node_infile<N: ast::AstNode<'db>>(&self, node: N) -> InFile<N> {
        InFile::new(self.find_file(node.syntax()).file_id, node)
    }

    pub(super) fn wrap_token_infile(&self, token: Token<'db>) -> InFile<Token<'db>> {
        let parent = token.parent().expect("a token of a tree has a parent node");
        InFile::new(self.find_file(parent).file_id, token)
    }

    pub fn hir_file_for(&self, node: Node<'db>) -> HirFileId {
        self.find_file(node).file_id
    }

    pub fn assert_contains_node(&self, node: Node<'db>) {
        self.find_file(node);
    }

    pub fn parse(&self, file_id: EditionedFileId) -> ast::SourceFile<'db> {
        let tree = file_id.parse(self.db).view();
        self.cache(tree.syntax(), file_id.into());
        tree
    }

    pub fn parse_guess_edition(&self, file_id: FileId) -> ast::SourceFile<'db> {
        self.parse(self.imp.attach_first_edition(file_id))
    }

    pub fn parse_or_expand(&self, file_id: HirFileId) -> Node<'db> {
        let root = file_id.root(self.db);
        self.cache(root, file_id);
        root
    }

    pub fn to_view<N: syntax::AstNode + Typed>(&self, ptr: InFile<AstPtr<N>>) -> N::Of<'db> {
        ptr.value.to_view(self.parse_or_expand(ptr.file_id))
    }

    pub fn to_view_syntax(&self, ptr: InFile<SyntaxNodePtr>) -> Node<'db> {
        ast::to_node(ptr.value, self.parse_or_expand(ptr.file_id))
    }

    pub fn expand(&self, file_id: MacroCallId) -> ExpandResult<Node<'db>> {
        let res = file_id.parse_macro_expansion(self.db).as_ref().map(|it| it.0.root());
        self.cache(res.value, file_id.into());
        res
    }

    /// Search for a definition's source and record its tree.
    pub fn source<Def: HasSourceView>(&self, def: Def) -> Option<InFile<Def::Ast<'db>>> {
        let res = def.source_view(self.db)?;
        self.cache(res.value.syntax().tree_top(), res.file_id);
        Some(res)
    }

    pub fn find_parent_file(&self, file_id: HirFileId) -> Option<InFile<Node<'db>>> {
        match file_id {
            HirFileId::FileId(file_id) => {
                let module = self.file_modules(file_id.file_id(self.db)).next()?;
                match module.id.def_map(self.db).module(self.db, module.id).origin {
                    ModuleOrigin::CrateRoot { .. } => None,
                    ModuleOrigin::File { declaration, declaration_tree_id, .. } => {
                        let file_id = declaration_tree_id.file_id();
                        let node = AstId::new(file_id, declaration).to_view(self.db).syntax();
                        self.cache(node.tree_top(), file_id);
                        Some(InFile::new(file_id, node))
                    }
                    _ => unreachable!("FileId can only belong to a file module"),
                }
            }
            HirFileId::MacroFile(macro_file) => {
                let node = macro_file.loc(self.db).to_node_view(self.db);
                self.cache(node.value.tree_top(), node.file_id);
                Some(node)
            }
        }
    }

    /// The node of the module: for a file module the *definition* file's root, not the
    /// declaration.
    pub fn module_definition_node(&self, module: Module) -> InFile<Node<'db>> {
        let definition = module.definition_source_view(self.db).map(|it| it.node());
        self.cache(definition.value.tree_top(), definition.file_id);
        definition
    }

    pub fn file_to_module_def(&self, file: impl Into<FileId>) -> Option<Module> {
        self.file_modules(file.into()).next()
    }

    pub fn file_to_module_defs(&self, file: impl Into<FileId>) -> impl Iterator<Item = Module> {
        self.file_modules(file.into())
    }

    pub fn hir_file_to_module_def(&self, file: impl Into<HirFileId>) -> Option<Module> {
        self.hir_file_to_module_defs(file).next()
    }

    pub fn hir_file_to_module_defs(
        &self,
        file: impl Into<HirFileId>,
    ) -> impl Iterator<Item = Module> {
        let file = file.into().original_file_respecting_includes(self.db).file_id(self.db);
        self.file_modules(file)
    }

    pub fn is_nightly(&self, krate: Crate) -> bool {
        let toolchain = toolchain_channel(self.db, krate.into());
        // `toolchain == None` means we're in some detached files. Since we have no information on
        // the toolchain being used, let's just allow unstable items to be listed.
        matches!(toolchain, Some(base_db::ReleaseChannel::Nightly) | None)
    }

    pub fn first_crate(&self, file: FileId) -> Option<Crate> {
        self.imp.first_crate(file)
    }

    pub fn attach_first_edition_opt(&self, file: FileId) -> Option<EditionedFileId> {
        self.imp.attach_first_edition_opt(file)
    }

    pub fn attach_first_edition(&self, file: FileId) -> EditionedFileId {
        self.imp.attach_first_edition(file)
    }

    pub fn adjust_edition(&self, file_id: HirFileId) -> HirFileId {
        self.imp.adjust_edition(file_id)
    }

    pub fn get_unsafe_ops(&self, def: ExpressionStoreOwner) -> HashSet<ExprOrPatSource> {
        self.imp.get_unsafe_ops(def)
    }

    pub fn impl_generated_from_derive(&self, impl_: Impl) -> Option<Adt> {
        let id = match impl_.id {
            AnyImplId::ImplId(id) => id,
            AnyImplId::BuiltinDeriveImplId(id) => return Some(id.loc(self.db).adt.into()),
        };
        let source = hir_def::src::HasSource::ast_ptr(id.loc(self.db), self.db);
        let mut file_id = source.file_id;
        let adt_ast_id = loop {
            let macro_call = file_id.macro_file()?;
            match macro_call.loc(self.db).kind {
                hir_expand::MacroCallKind::Derive { ast_id, .. } => break ast_id,
                hir_expand::MacroCallKind::FnLike { ast_id, .. } => file_id = ast_id.file_id,
                hir_expand::MacroCallKind::Attr { ast_id, .. } => file_id = ast_id.file_id,
            }
        };
        let adt = adt_ast_id.to_view(self.db);
        self.cache(adt.syntax().tree_top(), adt_ast_id.file_id);
        self.to_def2(InFile::new(adt_ast_id.file_id, adt))
    }
}
