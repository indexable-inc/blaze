//! `SourceToDefCtx`'s walks from syntax to defs, over views: the same containers, the same dyn
//! maps (in [`SemanticsImpl`](crate::SemanticsImpl)'s cache, keyed by `AstPtr` kind tags) and
//! the same answers.
//! A view's pointer is `AstPtr::of(view)`, so no lookup makes a handle.

use std::{convert::Infallible, iter, mem, ops::ControlFlow};

use base_db::Db;
use either::Either;
use hir_def::{
    AdtId, BlockId, BuiltinDeriveImplId, ConstId, ConstParamId, DefWithBodyId, EnumId,
    EnumVariantId, ExpressionStoreOwnerId, ExternBlockId, ExternCrateId, FieldId, FunctionId,
    GenericDefId, GenericParamId, HasModule as _, ImplId, LifetimeParamId, MacroId, ModuleId,
    StaticId, StructId, TraitId, TypeAliasId, TypeParamId, UnionId, UseId, VariantId,
    dyn_map::{
        DynMap,
        keys::{self, Key},
    },
    expr_store::{Body, ExpressionStore},
    hir::{BindingId, Expr, LabelId},
    nameres::block_def_map,
};
use hir_expand::{
    ExpandResult, FileRange, HirFileId, InFile, InMacroFile, MacroCallId,
    attrs::{AstPathExt, AttrId, expand_cfg_attr_view},
    builtin::{BuiltinFnLikeExpander, EagerExpander},
    files::{FileRangeWrapper, InRealFile},
    name::AsName,
};
use span::{FileId, Span, SyntaxContext};
use stdx::TupleExt;
use syntax::{
    AstPtr, SmolStr, SmolStrBuilder, SyntaxNodePtr, T, TextRange,
    ast as tag,
    view::{self as ast, AstNode, HasAttrs as _, HasName as _, Node, Token, Typed},
};

use super::Semantics;
use crate::{
    Crate, InlineAsmOperand, Macro,
    semantics::{
        LintAttr,
        child_by_source::ChildBySource,
        kmerge2,
        source_to_def::{ChildContainer, SourceToDefCtx},
    },
};

/// Tokens a descent into macros holds, with their contexts: inline up to the few a span
/// usually maps to, so a warm descent allocates nothing.
pub(super) type DescentTokens<'db> = smallvec::SmallVec<[(Token<'db>, SyntaxContext); 4]>;

/// A view a def is found from.
pub trait ToDef<'db>: AstNode<'db> + Copy {
    type Def;
    fn to_def(sema: &Semantics<'db>, src: InFile<Self>) -> Option<Self::Def>;
}

macro_rules! to_def_impls {
    ($(($def:ty, $ast:ident, $meth:ident)),* ,) => {$(
        impl<'db> ToDef<'db> for ast::$ast<'db> {
            type Def = $def;
            fn to_def(sema: &Semantics<'db>, src: InFile<Self>) -> Option<Self::Def> {
                sema.with_ctx(|ctx| ctx.$meth(src)).map(<$def>::from)
            }
        }
    )*}
}

to_def_impls![
    (crate::Module, Module, module_to_def),
    (crate::Module, SourceFile, source_file_to_def),
    (crate::Struct, Struct, struct_to_def),
    (crate::Enum, Enum, enum_to_def),
    (crate::Union, Union, union_to_def),
    (crate::Trait, Trait, trait_to_def),
    (crate::Impl, Impl, impl_to_def),
    (crate::TypeAlias, TypeAlias, type_alias_to_def),
    (crate::Const, Const, const_to_def),
    (crate::Static, Static, static_to_def),
    (crate::Function, Fn, fn_to_def),
    (crate::Field, RecordField, record_field_to_def),
    (crate::Field, TupleField, tuple_field_to_def),
    (crate::EnumVariant, Variant, enum_variant_to_def),
    (crate::TypeParam, TypeParam, type_param_to_def),
    (crate::LifetimeParam, LifetimeParam, lifetime_param_to_def),
    (crate::ConstParam, ConstParam, const_param_to_def),
    (crate::GenericParam, GenericParam, generic_param_to_def),
    (crate::Macro, Macro, macro_to_def),
    (crate::Local<'db>, SelfParam, self_param_to_def),
    (crate::Label, Label, label_to_def),
    (crate::Adt, Adt, adt_to_def),
    (crate::ExternCrateDecl, ExternCrate, extern_crate_to_def),
    (crate::InlineAsmOperand, AsmOperandNamed, asm_operand_to_def),
    (crate::ExternBlock, ExternBlock, extern_block_to_def),
    (crate::UseDecl, Use, use_to_def),
    (MacroCallId, MacroCall, macro_call_to_macro_call),
];

impl<'db> ToDef<'db> for ast::IdentPat<'db> {
    type Def = crate::Local<'db>;

    fn to_def(sema: &Semantics<'db>, src: InFile<Self>) -> Option<Self::Def> {
        sema.with_ctx(|ctx| ctx.bind_pat_to_def(src))
    }
}

/// [`SourceToDefCtx`] over views: the shared cache, and the `Semantics` whose trees it records.
pub(super) struct ViewCtx<'db, 'a> {
    sema: &'a Semantics<'db>,
    s2d: SourceToDefCtx<'db, 'a>,
    /// The crate of the macro call whose expansion the query's node is in, `None` for a node
    /// of a file: the crate whose module a file of two crates is, on the way out of the
    /// expansion (see [`ViewCtx::file_module`]).
    call_crate: Option<base_db::Crate>,
}

impl<'db> Semantics<'db> {
    pub(super) fn with_ctx<T>(&self, f: impl FnOnce(&mut ViewCtx<'db, '_>) -> T) -> T {
        let mut cache = self.imp.s2d_cache.borrow_mut();
        let mut ctx =
            ViewCtx { sema: self, s2d: SourceToDefCtx { db: self.db, cache: &mut cache }, call_crate: None };
        f(&mut ctx)
    }

    pub fn to_def<T: ToDef<'db>>(&self, src: T) -> Option<T::Def> {
        let src = self.wrap_node_infile(src);
        T::to_def(self, src)
    }

    pub fn to_def2<T: ToDef<'db>>(&self, src: InFile<T>) -> Option<T::Def> {
        T::to_def(self, src)
    }

    /// The modules of `file`: those of the crate def maps, else the one only a block's def map
    /// has ([`ViewCtx::block_declared_module`]).
    pub(super) fn file_modules(&self, file: FileId) -> impl Iterator<Item = crate::Module> + use<> {
        let found = self.with_ctx(|ctx| {
            // alloc: copy out of the cache borrow, inline for the common single module
            let found = ctx.s2d.file_to_def(file).to_owned();
            match found.is_empty() {
                true => ctx.block_declared_module(None, file).into_iter().collect(), // alloc: one module, inline
                false => found,
            }
        });
        found.into_iter().map(crate::Module::from)
    }

    pub fn to_adt_def(&self, a: ast::Adt<'db>) -> Option<crate::Adt> {
        self.to_def(a)
    }

    pub fn to_const_def(&self, c: ast::Const<'db>) -> Option<crate::Const> {
        self.to_def(c)
    }

    pub fn to_enum_def(&self, e: ast::Enum<'db>) -> Option<crate::Enum> {
        self.to_def(e)
    }

    pub fn to_enum_variant_def(&self, v: ast::Variant<'db>) -> Option<crate::EnumVariant> {
        self.to_def(v)
    }

    pub fn to_fn_def(&self, f: ast::Fn<'db>) -> Option<crate::Function> {
        self.to_def(f)
    }

    pub fn to_impl_def(&self, i: ast::Impl<'db>) -> Option<crate::Impl> {
        self.to_def(i)
    }

    pub fn to_macro_def(&self, m: ast::Macro<'db>) -> Option<Macro> {
        self.to_def(m)
    }

    pub fn to_module_def(&self, m: ast::Module<'db>) -> Option<crate::Module> {
        self.to_def(m)
    }

    pub fn to_static_def(&self, s: ast::Static<'db>) -> Option<crate::Static> {
        self.to_def(s)
    }

    pub fn to_struct_def(&self, s: ast::Struct<'db>) -> Option<crate::Struct> {
        self.to_def(s)
    }

    pub fn to_trait_def(&self, t: ast::Trait<'db>) -> Option<crate::Trait> {
        self.to_def(t)
    }

    pub fn to_type_alias_def(&self, t: ast::TypeAlias<'db>) -> Option<crate::TypeAlias> {
        self.to_def(t)
    }

    pub fn to_union_def(&self, u: ast::Union<'db>) -> Option<crate::Union> {
        self.to_def(u)
    }

    /// The container `node` is a child of.
    pub(super) fn find_container(&self, node: InFile<Node<'db>>) -> Option<ChildContainer> {
        self.with_ctx(|ctx| ctx.find_container(node))
    }

    pub fn store_owner_for(&self, node: InFile<Node<'db>>) -> Option<crate::ExpressionStoreOwner> {
        let container = self.find_container(node)?;
        container.as_expression_store_owner().map(|id| id.into())
    }

    pub fn expand_macro_call(&self, macro_call: ast::MacroCall<'db>) -> Option<InFile<Node<'db>>> {
        let file_id = self.to_def(macro_call)?;
        let node = self.parse_or_expand(file_id.into());
        Some(InFile::new(file_id.into(), node))
    }

    /// Expands the macro if it isn't one of the built-in ones that expand to custom syntax or
    /// dummy expansions.
    pub fn expand_allowed_builtins(
        &self,
        macro_call: ast::MacroCall<'db>,
    ) -> Option<ExpandResult<Node<'db>>> {
        let file_id = self.to_def(macro_call)?;
        let macro_call = file_id.loc(self.db);

        let skip = matches!(
            macro_call.def.kind,
            hir_expand::MacroDefKind::BuiltIn(
                _,
                BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
                    | BuiltinFnLikeExpander::ModulePath
                    | BuiltinFnLikeExpander::Asm
                    | BuiltinFnLikeExpander::GlobalAsm
                    | BuiltinFnLikeExpander::NakedAsm
                    | BuiltinFnLikeExpander::LogSyntax
                    | BuiltinFnLikeExpander::TraceMacros
                    | BuiltinFnLikeExpander::FormatArgs
                    | BuiltinFnLikeExpander::FormatArgsNl
                    | BuiltinFnLikeExpander::ConstFormatArgs,
            ) | hir_expand::MacroDefKind::BuiltInEager(_, EagerExpander::CompileError)
        );
        if skip {
            return None;
        }
        Some(self.expand(file_id))
    }

    /// If `item` has an attribute macro attached to it, expands it.
    pub fn expand_attr_macro(
        &self,
        item: ast::Item<'db>,
    ) -> Option<ExpandResult<InFile<Node<'db>>>> {
        let src = self.wrap_node_infile(item);
        let macro_call_id = self.with_ctx(|ctx| ctx.item_to_macro_call(src))?;
        Some(self.expand(macro_call_id).map(|it| InFile::new(macro_call_id.into(), it)))
    }

    pub fn expand_derive_as_pseudo_attr_macro(&self, attr: ast::Meta<'db>) -> Option<Node<'db>> {
        let adt = attr.parent_attr()?.syntax().parent().and_then(ast::Adt::cast)?;
        let src = self.wrap_node_infile(attr);
        let call_id = self.with_ctx(|ctx| {
            ctx.attr_to_derive_macro_call(src.with_value(adt), src).map(|(_, it, _)| it)
        })?;
        Some(self.parse_or_expand(call_id.into()))
    }

    pub fn resolve_derive_macro(&self, attr: ast::Meta<'db>) -> Option<Vec<Option<Macro>>> {
        let calls = self.derive_macro_calls(attr)?;
        self.with_ctx(|ctx| {
            Some(
                calls
                    .into_iter()
                    .map(|call| match call? {
                        Either::Left(call) => {
                            macro_call_to_macro_id(ctx, call).map(|id| Macro { id })
                        }
                        Either::Right(call) => {
                            let call = call.loc(self.db);
                            let krate = call.krate(self.db);
                            let lang_items = hir_def::lang_item::lang_items(self.db, krate);
                            call.trait_.derive_macro(lang_items).map(|id| Macro { id })
                        }
                    })
                    .collect(), // alloc: one answer per derive, what the handle version gives
            )
        })
    }

    pub fn expand_derive_macro(
        &self,
        attr: ast::Meta<'db>,
    ) -> Option<Vec<Option<ExpandResult<Node<'db>>>>> {
        let res = self
            .derive_macro_calls(attr)?
            .into_iter()
            .map(|call| {
                let file_id = call?.left()?;
                let ExpandResult { value, err } = file_id.parse_macro_expansion(self.db);
                let root = value.0.root();
                self.cache(root, file_id.into());
                // alloc: an `Arc` count of the expansion's error, as the handle version.
                Some(ExpandResult { value: root, err: err.clone() })
            })
            .collect(); // alloc: one expansion per derive, what the handle version gives
        Some(res)
    }

    fn derive_macro_calls(
        &self,
        attr: ast::Meta<'db>,
    ) -> Option<Vec<Option<Either<MacroCallId, BuiltinDeriveImplId>>>> {
        let adt = attr.parent_attr()?.syntax().parent().and_then(ast::Adt::cast)?;
        let file_id = self.find_file(adt.syntax()).file_id;
        let adt = InFile::new(file_id, adt);
        let src = InFile::new(file_id, attr);
        self.with_ctx(|ctx| {
            let (.., res) = ctx.attr_to_derive_macro_call(adt, src)?;
            Some(res.to_vec()) // alloc: the derive calls, out of the cache's borrow
        })
    }

    pub fn is_derive_annotated(&self, adt: InFile<ast::Adt<'db>>) -> bool {
        self.with_ctx(|ctx| ctx.file_of_adt_has_derives(adt))
    }

    pub fn is_attr_macro_call(&self, item: InFile<ast::Item<'db>>) -> bool {
        self.with_ctx(|ctx| ctx.item_to_macro_call(item).is_some())
    }

    pub fn token_ancestors_with_macros(
        &self,
        token: Token<'db>,
    ) -> impl Iterator<Item = Node<'db>> + Clone + '_ {
        token.parent().into_iter().flat_map(move |parent| self.ancestors_with_macros(parent))
    }

    /// Iterates the ancestors of the given node, climbing up macro expansions while doing so.
    pub fn ancestors_with_macros(
        &self,
        node: Node<'db>,
    ) -> impl Iterator<Item = Node<'db>> + Clone + '_ {
        let node = self.find_file(node);
        self.ancestors_with_macros_file(node).map(|it| it.value)
    }

    /// Iterates the ancestors of the given node, climbing up macro expansions while doing so.
    pub fn ancestors_with_macros_file(
        &self,
        node: InFile<Node<'db>>,
    ) -> impl Iterator<Item = InFile<Node<'db>>> + Clone + '_ {
        iter::successors(Some(node), move |&InFile { file_id, value }| match value.parent() {
            Some(parent) => Some(InFile::new(file_id, parent)),
            None => {
                let macro_file = file_id.macro_file()?;
                self.with_ctx(|ctx| ctx.expansion_arg(macro_file))
                    .map(|node| node?.parent())
                    .transpose()
            }
        })
    }

    pub fn ancestors_at_offset_with_macros(
        &self,
        node: Node<'db>,
        offset: syntax::TextSize,
    ) -> impl Iterator<Item = Node<'db>> + '_ {
        kmerge2(
            node.token_at_offset(offset).map(|token| self.token_ancestors_with_macros(token)),
            |node1, node2| node1.text_range().len() < node2.text_range().len(),
        )
    }

    /// Attempts to map the node out of macro expanded files returning the original file range.
    /// If upmapping is not possible, this will fall back to the range of the macro call of the
    /// macro file the node resides in.
    pub fn original_range(&self, node: Node<'db>) -> FileRange {
        self.find_file(node).original_file_range_rooted_view(self.db)
    }

    /// Attempts to map the node out of macro expanded files returning the original file range.
    pub fn original_range_opt(&self, node: Node<'db>) -> Option<FileRange> {
        self.find_file(node)
            .original_file_range_opt_view(self.db)
            .filter(|(_, ctx)| ctx.is_root())
            .map(TupleExt::head)
    }

    /// Attempts to map the node out of macro expanded files.
    /// This only works for attribute expansions, as other ones do not have nodes as input.
    pub fn original_ast_node<N: AstNode<'db>>(&self, node: N) -> Option<N> {
        self.wrap_node_infile(node).original_ast_node_rooted_view(self.db).map(
            |InRealFile { file_id, value }| {
                self.cache(value.syntax().tree_top(), file_id.into());
                value
            },
        )
    }

    /// Attempts to map the node out of macro expanded files.
    /// This only works for attribute expansions, as other ones do not have nodes as input.
    pub fn original_syntax_node_rooted(&self, node: Node<'db>) -> Option<Node<'db>> {
        self.find_file(node).original_syntax_node_rooted_view(self.db).map(
            |InRealFile { file_id, value }| {
                self.cache(value.tree_top(), file_id.into());
                value
            },
        )
    }

    pub fn diagnostics_display_range(
        &self,
        src: InFile<SyntaxNodePtr>,
    ) -> FileRangeWrapper<FileId> {
        let root = self.parse_or_expand(src.file_id);
        let node = src.map(|it| ast::to_node(it, root));
        let FileRange { file_id, range } = node.original_file_range_rooted_view(self.db);
        FileRangeWrapper { file_id: file_id.file_id(self.db), range }
    }

    pub fn diagnostics_display_range_for_range(
        &self,
        src: InFile<TextRange>,
    ) -> FileRangeWrapper<FileId> {
        let FileRange { file_id, range } = src.original_node_file_range_rooted(self.db);
        FileRangeWrapper { file_id: file_id.file_id(self.db), range }
    }

    pub fn lint_attrs(
        &self,
        file_id: FileId,
        krate: Crate,
        item: ast::AnyHasAttrs<'db>,
    ) -> impl DoubleEndedIterator<Item = (LintAttr, SmolStr)> + use<'db> {
        let mut cfg_options = None;
        let cfg_options = || *cfg_options.get_or_insert_with(|| krate.id.cfg_options(self.db));

        let is_crate_root = file_id == krate.root_file(self.db);
        let is_source_file = ast::SourceFile::can_cast(item.syntax().kind());
        let crate_attrs = match is_crate_root && is_source_file {
            true => base_db::applied_crate_attr_files(self.db, krate.id),
            false => &[],
        };
        let crate_attrs = crate_attrs.iter().flat_map(|&file| file.parse(self.db).view().attrs());

        let mut result = Vec::new(); // alloc: the item's lint attributes, what the handle version gives
        expand_cfg_attr_view::<Infallible>(
            ast::attrs_including_inner(&item).chain(crate_attrs),
            cfg_options,
            |attr, _| {
                let ast::Meta::TokenTreeMeta(attr) = attr else {
                    return ControlFlow::Continue(());
                };
                let (Some(segment), Some(tt)) = (attr.path().as_one_segment(), attr.token_tree())
                else {
                    return ControlFlow::Continue(());
                };
                let lint_attr = match &*segment {
                    "allow" => LintAttr::Allow,
                    "expect" => LintAttr::Expect,
                    "warn" => LintAttr::Warn,
                    "deny" => LintAttr::Deny,
                    "forbid" => LintAttr::Forbid,
                    _ => return ControlFlow::Continue(()),
                };
                let mut lint = SmolStrBuilder::new();
                for token in tt.syntax().children_with_tokens().filter_map(|it| it.into_token()) {
                    match token.kind() {
                        T![:] | T![::] => lint.push_str(token.text()),
                        kind if kind.is_any_identifier() => lint.push_str(token.text()),
                        T![,] => {
                            let lint = mem::replace(&mut lint, SmolStrBuilder::new()).finish();
                            if !lint.is_empty() {
                                result.push((lint_attr, lint));
                            }
                        }
                        _ => {}
                    }
                }
                let lint = lint.finish();
                if !lint.is_empty() {
                    result.push((lint_attr, lint));
                }

                ControlFlow::Continue(())
            },
        );
        result.into_iter()
    }
}

/// The macro a call expands, found from its definition's node.
pub(super) fn macro_call_to_macro_id(
    ctx: &mut ViewCtx<'_, '_>,
    macro_call_id: MacroCallId,
) -> Option<MacroId> {
    let db = ctx.s2d.db;
    let loc = macro_call_id.loc(db);
    match loc.def.ast_id() {
        Either::Left(it) => {
            let node = it.to_view(db);
            ctx.macro_to_def(InFile::new(it.file_id, node))
        }
        Either::Right(it) => {
            let node = it.to_view(db);
            ctx.proc_macro_to_def(InFile::new(it.file_id, node))
        }
    }
}

impl<'db> ViewCtx<'db, '_> {
    fn db(&self) -> &'db Db {
        self.s2d.db
    }

    /// The argument of `macro_file`'s call, its expansion and (for `include!`) the argument's
    /// tree recorded with the `Semantics`.
    fn expansion_arg(&mut self, macro_file: MacroCallId) -> InFile<Option<Node<'db>>> {
        let db = self.db();
        let info = self.s2d.cache.get_or_insert_expansion(db, macro_file);
        let expanded = info.expanded_view();
        let arg = info.arg_view();
        self.sema.cache(expanded.value, expanded.file_id.into());
        if macro_file.is_include_macro(db)
            && let Some(arg_node) = arg.value
        {
            self.sema.cache(arg_node.tree_top(), arg.file_id);
        }
        arg
    }

    /// The tokens `span` maps down to in `macro_file`'s expansion, pushed onto `out`, with the
    /// expansion's tree recorded: how many were pushed, `None` when the expansion maps nothing
    /// down from its file.
    pub(super) fn map_range_down(
        &mut self,
        macro_file: MacroCallId,
        span: Span,
        out: &mut DescentTokens<'db>,
    ) -> Option<u32> {
        let db = self.db();
        let info = self.s2d.cache.get_or_insert_expansion(db, macro_file);
        let expanded = info.expanded_view();
        let InMacroFile { value: tokens, .. } = info.map_range_down_view(span)?;
        let before = out.len();
        out.extend(tokens);
        self.sema.cache(expanded.value, expanded.file_id.into());
        Some(u32::try_from(out.len() - before).expect("a span mapped down to more than u32::MAX tokens"))
    }

    fn file_module(&mut self, file_id: HirFileId) -> Option<ModuleId> {
        let db = self.db();
        let file = file_id.original_file(db).file_id(db);
        // A file of two crates has a module in each, but a macro file's items are only in its
        // call's crate: the other's module maps none of them to a def. So is a module of the
        // file the walk out of the expansion comes to (an inline `mod` around the call).
        let krate = file_id.macro_file().map(|it| it.loc(db).krate).or(self.call_crate);
        let found = match krate {
            Some(krate) => self.s2d.file_to_def_in(krate, file),
            None => self.s2d.file_to_def(file).first().copied(),
        };
        found.or_else(|| self.block_declared_module(krate, file))
    }

    /// The module of `file` when only a block's def map has it: the file of a `mod name;`
    /// declared in a block (`fn main() { #[path = "m.rs"] mod m; }`), which no crate def map
    /// lists. Searched from the crate's own module files down through the out-of-line modules
    /// their blocks declare, each found module recorded as its file's, so resolving a `mod` in a
    /// file found so reads that record and the search runs once per file.
    #[cold]
    fn block_declared_module(&mut self, krate: Option<base_db::Crate>, file: FileId) -> Option<ModuleId> {
        let db = self.db();
        let relevant = base_db::relevant_crates(db, file);
        let crates = krate.into_iter().chain(relevant.iter().copied().filter(|_| krate.is_none()));
        for krate in crates {
            let def_map = hir_def::nameres::crate_def_map(db, krate);
            let files = def_map.modules(db).filter_map(|(_, data)| data.origin.file_id());
            let mut pending: Vec<hir_expand::EditionedFileId> = files.collect(); // alloc: the files searched, once per file no crate def map has
            // alloc: as above
            let mut seen: blaze_hash::HashSet<hir_expand::EditionedFileId> = pending.iter().copied().collect();
            while let Some(searched) = pending.pop() {
                let root = self.sema.parse(searched).syntax();
                for node in root.descendants() {
                    let Some(module) = ast::Module::cast(node) else { continue };
                    let in_block = node.ancestors().any(|it| it.kind() == syntax::SyntaxKind::BLOCK_EXPR);
                    if module.item_list().is_some() || !in_block {
                        continue;
                    }
                    let Some(child) = self.module_to_def(InFile::new(searched.into(), module)) else { continue };
                    let Some(child_file) = child.def_map(db).module(db, child).origin.file_id() else { continue };
                    let recorded = self.s2d.cache.file_to_def_cache.entry(child_file.file_id(db)).or_default();
                    if !recorded.contains(&child) {
                        recorded.push(child);
                    }
                    if child_file.file_id(db) == file {
                        return Some(child);
                    }
                    if seen.insert(child_file) {
                        pending.push(child_file);
                    }
                }
            }
        }
        None
    }

    pub(super) fn module_to_def(&mut self, src: InFile<ast::Module<'db>>) -> Option<ModuleId> {
        let _p = tracing::info_span!("module_to_def").entered();
        let parent_declaration = self
            .parent_ancestors_with_macros(src.map(|it| it.syntax()), |_, ancestor, _| {
                ancestor.map(Either::<ast::Module<'db>, ast::BlockExpr<'db>>::cast).transpose()
            })
            .map(|it| it.transpose());

        let parent_module = match parent_declaration {
            Some(Either::Right(parent_block)) => self
                .block_to_def(parent_block)
                .map(|block| block_def_map(self.db(), block).root_module_id()),
            Some(Either::Left(parent_declaration)) => self.module_to_def(parent_declaration),
            None => self.file_module(src.file_id),
        }?;

        let child_name = src.value.name()?.as_name();
        let def_map = parent_module.def_map(self.db());
        let &child_id =
            def_map.module(self.db(), parent_module).children(self.db()).get(&child_name)?;
        Some(child_id)
    }

    pub(super) fn source_file_to_def(
        &mut self,
        src: InFile<ast::SourceFile<'db>>,
    ) -> Option<ModuleId> {
        let _p = tracing::info_span!("source_file_to_def").entered();
        self.file_module(src.file_id)
    }

    pub(super) fn trait_to_def(&mut self, src: InFile<ast::Trait<'db>>) -> Option<TraitId> {
        self.to_def(src, keys::TRAIT)
    }
    pub(super) fn impl_to_def(&mut self, src: InFile<ast::Impl<'db>>) -> Option<ImplId> {
        self.to_def(src, keys::IMPL)
    }
    pub(super) fn fn_to_def(&mut self, src: InFile<ast::Fn<'db>>) -> Option<FunctionId> {
        self.to_def(src, keys::FUNCTION)
    }
    pub(super) fn struct_to_def(&mut self, src: InFile<ast::Struct<'db>>) -> Option<StructId> {
        self.to_def(src, keys::STRUCT)
    }
    pub(super) fn enum_to_def(&mut self, src: InFile<ast::Enum<'db>>) -> Option<EnumId> {
        self.to_def(src, keys::ENUM)
    }
    pub(super) fn union_to_def(&mut self, src: InFile<ast::Union<'db>>) -> Option<UnionId> {
        self.to_def(src, keys::UNION)
    }
    pub(super) fn static_to_def(&mut self, src: InFile<ast::Static<'db>>) -> Option<StaticId> {
        self.to_def(src, keys::STATIC)
    }
    pub(super) fn const_to_def(&mut self, src: InFile<ast::Const<'db>>) -> Option<ConstId> {
        self.to_def(src, keys::CONST)
    }
    pub(super) fn type_alias_to_def(
        &mut self,
        src: InFile<ast::TypeAlias<'db>>,
    ) -> Option<TypeAliasId> {
        self.to_def(src, keys::TYPE_ALIAS)
    }
    pub(super) fn record_field_to_def(
        &mut self,
        src: InFile<ast::RecordField<'db>>,
    ) -> Option<FieldId> {
        self.to_def(src, keys::RECORD_FIELD)
    }
    pub(super) fn tuple_field_to_def(
        &mut self,
        src: InFile<ast::TupleField<'db>>,
    ) -> Option<FieldId> {
        self.to_def(src, keys::TUPLE_FIELD)
    }
    pub(super) fn block_to_def(&mut self, src: InFile<ast::BlockExpr<'db>>) -> Option<BlockId> {
        self.to_def(src, keys::BLOCK)
    }
    pub(super) fn enum_variant_to_def(
        &mut self,
        src: InFile<ast::Variant<'db>>,
    ) -> Option<EnumVariantId> {
        self.to_def(src, keys::ENUM_VARIANT)
    }
    pub(super) fn extern_crate_to_def(
        &mut self,
        src: InFile<ast::ExternCrate<'db>>,
    ) -> Option<ExternCrateId> {
        self.to_def(src, keys::EXTERN_CRATE)
    }
    pub(super) fn extern_block_to_def(
        &mut self,
        src: InFile<ast::ExternBlock<'db>>,
    ) -> Option<ExternBlockId> {
        self.to_def(src, keys::EXTERN_BLOCK)
    }
    #[allow(dead_code)]
    pub(super) fn use_to_def(&mut self, src: InFile<ast::Use<'db>>) -> Option<UseId> {
        self.to_def(src, keys::USE)
    }
    pub(super) fn adt_to_def(
        &mut self,
        InFile { file_id, value }: InFile<ast::Adt<'db>>,
    ) -> Option<AdtId> {
        match value {
            ast::Adt::Enum(it) => self.enum_to_def(InFile::new(file_id, it)).map(AdtId::EnumId),
            ast::Adt::Struct(it) => {
                self.struct_to_def(InFile::new(file_id, it)).map(AdtId::StructId)
            }
            ast::Adt::Union(it) => self.union_to_def(InFile::new(file_id, it)).map(AdtId::UnionId),
        }
    }

    pub(super) fn asm_operand_to_def(
        &mut self,
        src: InFile<ast::AsmOperandNamed<'db>>,
    ) -> Option<InlineAsmOperand> {
        let asm = src.value.syntax().parent().and_then(ast::AsmExpr::cast)?;
        let index = asm
            .asm_pieces()
            .filter_map(|it| match it {
                ast::AsmPiece::AsmOperandNamed(it) => Some(it),
                _ => None,
            })
            .position(|it| it == src.value)?;
        let container =
            self.find_container(src.map(|it| it.syntax()))?.as_expression_store_owner()?;
        let (_, source_map) = ExpressionStore::with_source_map(self.db(), container);
        let expr = source_map.node_expr_view(src.with_value(ast::Expr::AsmExpr(asm)))?.as_expr()?;
        Some(InlineAsmOperand { owner: container, expr, index })
    }

    pub(super) fn bind_pat_to_def(
        &mut self,
        src: InFile<ast::IdentPat<'db>>,
    ) -> Option<crate::Local<'db>> {
        let container =
            self.find_container(src.map(|it| it.syntax()))?.as_expression_store_owner()?;
        let (store, source_map) = ExpressionStore::with_source_map(self.db(), container);
        let pat_id = source_map.node_pat_view(src.map(ast::Pat::IdentPat))?;
        // the pattern could resolve to a constant, verify that this is not the case
        let pat_id = pat_id.as_pat()?;
        if let crate::Pat::Bind { id, .. } = store.pat(pat_id) {
            let parent_infer =
                self.sema.imp.infer_body_for_expr_or_pat(container, store, pat_id.into())?;
            Some(crate::Local { parent: container, parent_infer, binding_id: id })
        } else {
            None
        }
    }

    pub(super) fn self_param_to_def(
        &mut self,
        src: InFile<ast::SelfParam<'db>>,
    ) -> Option<(DefWithBodyId, BindingId)> {
        let container = self
            .find_container(src.map(|it| it.syntax()))?
            .as_expression_store_owner()?
            .as_def_with_body()?;
        let body = Body::of(self.db(), container);
        Some((container, body.self_param?.user_written))
    }

    pub(super) fn label_to_def(
        &mut self,
        src: InFile<ast::Label<'db>>,
    ) -> Option<(ExpressionStoreOwnerId, LabelId)> {
        let container =
            self.find_container(src.map(|it| it.syntax()))?.as_expression_store_owner()?;
        let (_, source_map) = ExpressionStore::with_source_map(self.db(), container);
        let label_id = source_map.node_label_view(src)?;
        Some((container, label_id))
    }

    pub(super) fn label_ref_to_def(
        &mut self,
        src: InFile<ast::Lifetime<'db>>,
    ) -> Option<(ExpressionStoreOwnerId, LabelId)> {
        let break_or_continue = ast::Expr::cast(src.value.syntax().parent()?)?;
        let container =
            self.find_container(src.map(|it| it.syntax()))?.as_expression_store_owner()?;
        let (store, source_map) = ExpressionStore::with_source_map(self.db(), container);
        let break_or_continue =
            source_map.node_expr_view(src.with_value(break_or_continue))?.as_expr()?;
        let (Expr::Break { label, .. } | Expr::Continue { label }) = store.expr(break_or_continue)
        else {
            return None;
        };
        Some((container, label?))
    }

    /// (AttrId, derive attribute call id, derive call ids)
    pub(super) fn attr_to_derive_macro_call(
        &mut self,
        item: InFile<ast::Adt<'db>>,
        src: InFile<ast::Meta<'db>>,
    ) -> Option<(AttrId, MacroCallId, &[Option<Either<MacroCallId, BuiltinDeriveImplId>>])> {
        let map = self.dyn_map(item.map(|it| it.syntax()))?;
        map[keys::DERIVE_MACRO_CALL]
            .get(&AstPtr::<tag::Meta>::of(src.value))
            .map(|&(attr_id, call_id, ref ids)| (attr_id, call_id, &**ids))
    }

    pub(super) fn file_of_adt_has_derives(&mut self, adt: InFile<ast::Adt<'db>>) -> bool {
        self.dyn_map(adt.map(|it| it.syntax()))
            .as_ref()
            .is_some_and(|map| !map[keys::DERIVE_MACRO_CALL].is_empty())
    }

    fn to_def<N, ID>(&mut self, src: InFile<N::Of<'db>>, key: Key<N, ID>) -> Option<ID>
    where
        N: syntax::AstNode + Typed + 'static,
        ID: Copy + 'static,
        keys::AstPtrPolicy<N, ID>: keys::Slot<N, ID>,
    {
        let node = InFile::new(src.file_id, src.value.syntax());
        let ptr = AstPtr::<N>::of(src.value);
        self.dyn_map(node)?[key].get(&ptr).copied()
    }

    fn dyn_map(&mut self, src: InFile<Node<'db>>) -> Option<&DynMap> {
        let container = self.find_container(src)?;
        Some(self.cache_for(container, src.file_id))
    }

    fn cache_for(&mut self, container: ChildContainer, file_id: HirFileId) -> &DynMap {
        let db = self.db();
        self.s2d.cache.dynmap_cache.entry((container, file_id)).or_insert_with(|| match container {
            ChildContainer::DefWithBodyId(it) => it.child_by_source(db, file_id),
            ChildContainer::ModuleId(it) => it.child_by_source(db, file_id),
            ChildContainer::TraitId(it) => it.child_by_source(db, file_id),
            ChildContainer::ImplId(it) => it.child_by_source(db, file_id),
            ChildContainer::EnumId(it) => it.child_by_source(db, file_id),
            ChildContainer::VariantId(it) => it.child_by_source(db, file_id),
            ChildContainer::GenericDefId(it) => it.child_by_source(db, file_id),
        })
    }

    pub(super) fn item_to_macro_call(&mut self, src: InFile<ast::Item<'db>>) -> Option<MacroCallId> {
        self.to_def(src, keys::ATTR_MACRO_CALL)
    }

    pub(super) fn macro_call_to_macro_call(
        &mut self,
        src: InFile<ast::MacroCall<'db>>,
    ) -> Option<MacroCallId> {
        self.to_def(src, keys::MACRO_CALL)
    }

    pub(super) fn type_param_to_def(
        &mut self,
        src: InFile<ast::TypeParam<'db>>,
    ) -> Option<TypeParamId> {
        let container: ChildContainer =
            self.find_generic_param_container(src.map(|it| it.syntax()))?.into();
        let dyn_map = self.cache_for(container, src.file_id);
        dyn_map[keys::TYPE_PARAM]
            .get(&AstPtr::<tag::TypeParam>::of(src.value))
            .copied()
            .map(TypeParamId::from_unchecked)
    }

    pub(super) fn lifetime_param_to_def(
        &mut self,
        src: InFile<ast::LifetimeParam<'db>>,
    ) -> Option<LifetimeParamId> {
        let container: ChildContainer =
            self.find_generic_param_container(src.map(|it| it.syntax()))?.into();
        let dyn_map = self.cache_for(container, src.file_id);
        dyn_map[keys::LIFETIME_PARAM].get(&AstPtr::<tag::LifetimeParam>::of(src.value)).copied()
    }

    pub(super) fn const_param_to_def(
        &mut self,
        src: InFile<ast::ConstParam<'db>>,
    ) -> Option<ConstParamId> {
        let container: ChildContainer =
            self.find_generic_param_container(src.map(|it| it.syntax()))?.into();
        let dyn_map = self.cache_for(container, src.file_id);
        dyn_map[keys::CONST_PARAM]
            .get(&AstPtr::<tag::ConstParam>::of(src.value))
            .copied()
            .map(ConstParamId::from_unchecked)
    }

    pub(super) fn generic_param_to_def(
        &mut self,
        InFile { file_id, value }: InFile<ast::GenericParam<'db>>,
    ) -> Option<GenericParamId> {
        match value {
            ast::GenericParam::ConstParam(it) => {
                self.const_param_to_def(InFile::new(file_id, it)).map(GenericParamId::ConstParamId)
            }
            ast::GenericParam::LifetimeParam(it) => self
                .lifetime_param_to_def(InFile::new(file_id, it))
                .map(GenericParamId::LifetimeParamId),
            ast::GenericParam::TypeParam(it) => {
                self.type_param_to_def(InFile::new(file_id, it)).map(GenericParamId::TypeParamId)
            }
        }
    }

    pub(super) fn macro_to_def(&mut self, src: InFile<ast::Macro<'db>>) -> Option<MacroId> {
        let map = self.dyn_map(src.map(|it| it.syntax()))?;
        match src.value {
            ast::Macro::MacroRules(value) => map[keys::MACRO_RULES]
                .get(&AstPtr::<tag::MacroRules>::of(value))
                .copied()
                .map(MacroId::from),
            ast::Macro::MacroDef(value) => map[keys::MACRO2]
                .get(&AstPtr::<tag::MacroDef>::of(value))
                .copied()
                .map(MacroId::from),
        }
    }

    pub(super) fn proc_macro_to_def(&mut self, src: InFile<ast::Fn<'db>>) -> Option<MacroId> {
        let map = self.dyn_map(src.map(|it| it.syntax()))?;
        map[keys::PROC_MACRO].get(&AstPtr::<tag::Fn>::of(src.value)).copied().map(MacroId::from)
    }

    pub(super) fn find_container(&mut self, src: InFile<Node<'db>>) -> Option<ChildContainer> {
        let _p = tracing::info_span!("find_container").entered();
        if self.call_crate.is_none()
            && let Some(macro_file) = src.file_id.macro_file()
        {
            self.call_crate = Some(macro_file.loc(self.db()).krate);
        }
        let def = self.parent_ancestors_with_macros(src, |this, container, child| {
            this.container_to_def(container, child)
        });
        if let Some(def) = def {
            return Some(def);
        }
        Some(self.file_module(src.file_id)?.into())
    }

    fn find_generic_param_container(&mut self, src: InFile<Node<'db>>) -> Option<GenericDefId> {
        self.parent_ancestors_with_macros(src, |this, InFile { file_id, value }, _| {
            match ast::Item::cast(value)? {
                ast::Item::Fn(it) => this.fn_to_def(InFile::new(file_id, it)).map(Into::into),
                ast::Item::Struct(it) => {
                    this.struct_to_def(InFile::new(file_id, it)).map(Into::into)
                }
                ast::Item::Enum(it) => this.enum_to_def(InFile::new(file_id, it)).map(Into::into),
                ast::Item::Trait(it) => this.trait_to_def(InFile::new(file_id, it)).map(Into::into),
                ast::Item::TypeAlias(it) => {
                    this.type_alias_to_def(InFile::new(file_id, it)).map(Into::into)
                }
                ast::Item::Impl(it) => this.impl_to_def(InFile::new(file_id, it)).map(Into::into),
                _ => None,
            }
        })
    }

    /// Skips the attributed item that caused the macro invocation we are climbing up
    fn parent_ancestors_with_macros<T>(
        &mut self,
        node: InFile<Node<'db>>,
        mut cb: impl FnMut(
            &mut Self,
            /*parent: */ InFile<Node<'db>>,
            /*child: */ Node<'db>,
        ) -> Option<T>,
    ) -> Option<T> {
        let parent = |this: &mut Self, node: InFile<Node<'db>>| match node.value.parent() {
            Some(parent) => Some(node.with_value(parent)),
            None => {
                let macro_file = node.file_id.macro_file()?;
                this.expansion_arg(macro_file).map(|node| node?.parent()).transpose()
            }
        };
        let mut deepest_child_in_same_file = node;
        let mut node = node;
        while let Some(parent) = parent(self, node) {
            if parent.file_id != node.file_id {
                deepest_child_in_same_file = parent;
            }
            if let Some(res) = cb(self, parent, deepest_child_in_same_file.value) {
                return Some(res);
            }
            node = parent;
        }
        None
    }

    fn container_to_def(
        &mut self,
        container: InFile<Node<'db>>,
        child: Node<'db>,
    ) -> Option<ChildContainer> {
        let child_start = child.text_range().start();
        let cont = if let Some(item) = ast::Item::cast(container.value) {
            match item {
                ast::Item::Module(it) => self.module_to_def(container.with_value(it))?.into(),
                ast::Item::Trait(it) => self.trait_to_def(container.with_value(it))?.into(),
                ast::Item::Impl(it) => self.impl_to_def(container.with_value(it))?.into(),
                ast::Item::Enum(it) => self.enum_to_def(container.with_value(it))?.into(),
                ast::Item::TypeAlias(it) => ChildContainer::GenericDefId(
                    self.type_alias_to_def(container.with_value(it))?.into(),
                ),
                ast::Item::Struct(it) => {
                    let def = self.struct_to_def(container.with_value(it))?;
                    let is_in_body = it
                        .field_list()
                        .is_some_and(|it| it.syntax().text_range().contains(child_start));
                    if is_in_body {
                        VariantId::from(def).into()
                    } else {
                        ChildContainer::GenericDefId(def.into())
                    }
                }
                ast::Item::Union(it) => {
                    let def = self.union_to_def(container.with_value(it))?;
                    let is_in_body = it
                        .record_field_list()
                        .is_some_and(|it| it.syntax().text_range().contains(child_start));
                    if is_in_body {
                        VariantId::from(def).into()
                    } else {
                        ChildContainer::GenericDefId(def.into())
                    }
                }
                ast::Item::Fn(it) => {
                    let def = self.fn_to_def(container.with_value(it))?;
                    let is_in_body =
                        it.body().is_some_and(|it| it.syntax().text_range().contains(child_start));
                    let in_param_pat = || {
                        it.param_list().is_some_and(|it| {
                            it.self_param()
                                .and_then(|it| {
                                    Some(TextRange::new(
                                        it.syntax().text_range().start(),
                                        it.name()?.syntax().text_range().end(),
                                    ))
                                })
                                .is_some_and(|r| r.contains_inclusive(child_start))
                                || it
                                    .params()
                                    .filter_map(|it| it.pat())
                                    .any(|it| it.syntax().text_range().contains(child_start))
                        })
                    };
                    if is_in_body || in_param_pat() {
                        DefWithBodyId::from(def).into()
                    } else {
                        ChildContainer::GenericDefId(def.into())
                    }
                }
                ast::Item::Static(it) => {
                    let def = self.static_to_def(container.with_value(it))?;
                    let is_in_body =
                        it.body().is_some_and(|it| it.syntax().text_range().contains(child_start));
                    if is_in_body {
                        DefWithBodyId::from(def).into()
                    } else {
                        ChildContainer::GenericDefId(def.into())
                    }
                }
                ast::Item::Const(it) => {
                    let def = self.const_to_def(container.with_value(it))?;
                    let is_in_body =
                        it.body().is_some_and(|it| it.syntax().text_range().contains(child_start));
                    if is_in_body {
                        DefWithBodyId::from(def).into()
                    } else {
                        ChildContainer::GenericDefId(def.into())
                    }
                }
                _ => return None,
            }
        } else if let Some(it) = ast::Variant::cast(container.value) {
            let def = self.enum_variant_to_def(InFile::new(container.file_id, it))?;
            let is_in_body = it.eq_token().is_some_and(|it| it.text_range().end() < child_start);
            if is_in_body { DefWithBodyId::from(def).into() } else { VariantId::from(def).into() }
        } else {
            let it = match Either::<ast::Pat<'db>, ast::Name<'db>>::cast(container.value)? {
                Either::Left(it) => ast::Param::cast(it.syntax().parent()?)?.syntax().parent(),
                Either::Right(it) => ast::SelfParam::cast(it.syntax().parent()?)?.syntax().parent(),
            }
            .and_then(ast::ParamList::cast)?
            .syntax()
            .parent()
            .and_then(ast::Fn::cast)?;
            let def = self.fn_to_def(InFile::new(container.file_id, it))?;
            DefWithBodyId::from(def).into()
        };
        Some(cont)
    }
}
