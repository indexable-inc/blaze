//! When *constructing* `hir`, we start at some parent syntax node and recursively
//! lower the children.
//!
//! This module allows one to go in the opposite direction: start with a syntax
//! node for a *child*, and get its hir.
//!
//! A container's children are in many files (the file of its item and every macro expansion
//! in it), and a [`DynMap`] is of one: it is given each child's pointer with the pointer's
//! file and keeps those of its own. A check of the file before an insert here only saves the
//! work of making a pointer the map would not keep.

use base_db::Db;
use either::Either;
use hir_expand::{HirFileId, InFile};
use span::AstIdNode;
use syntax::{AstPtr, ast, view};

use hir_def::{
    AdtId, AssocItemId, AstIdLoc, AttrDefId, DefWithBodyId, EnumId, FieldId, GenericDefId, ImplId,
    LifetimeParamId, LocalFieldId, Lookup, MacroId, ModuleDefId, ModuleId, TraitId,
    TypeOrConstParamId, VariantId,
    attrs::{AttrFlags, Docs},
    dyn_map::{
        DynMap,
        keys::{self, Key},
    },
    expr_store::{Body, ExpressionStore},
    hir::generics::GenericParams,
    item_scope::ItemScope,
    signatures::{EnumSignature, ImplSignature, TraitSignature},
    src::{HasSource, enabled_fields_view},
};

#[cfg(test)]
thread_local! {
    /// The bug a test planted: a body's block is given to the map as a node of the file asked
    /// for, whatever file it is of (what the map of a body held before a map was of one file).
    pub(crate) static BLOCKS_OF_ANY_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the test on this thread planted [`BLOCKS_OF_ANY_FILE`]; never outside a test.
fn blocks_of_any_file() -> bool {
    #[cfg(test)]
    let planted = BLOCKS_OF_ANY_FILE.get();
    #[cfg(not(test))]
    let planted = false;
    planted
}

pub(crate) trait ChildBySource {
    fn child_by_source(&self, db: &Db, file_id: HirFileId) -> DynMap {
        let mut res = DynMap::of(file_id);
        self.child_by_source_to(db, &mut res);
        res
    }
    /// The children of `self` whose syntax is in `res`'s file ([`DynMap::file`]), added to it.
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap);
}

impl ChildBySource for Docs {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        self.macro_calls().filter(|(ast_id, _)| ast_id.file_id == file_id).for_each(
            |(ast_id, call_id)| {
                let ptr = ast_id.to_ptr(db);
                res.insert(keys::MACRO_CALL, ast_id.with_value(ptr), call_id);
            },
        );
    }
}

impl ChildBySource for AttrDefId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        if let Some(docs) = AttrFlags::docs(db, *self) {
            docs.child_by_source_to(db, res);
        }
    }
}

impl ChildBySource for TraitId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        let data = self.trait_items(db);

        data.macro_calls().filter(|(ast_id, _)| ast_id.file_id == file_id).for_each(
            |(ast_id, call_id)| {
                let ptr = ast_id.to_ptr(db);
                if let Some(ptr) = ptr.cast::<ast::MacroCall>() {
                    res.insert(keys::MACRO_CALL, ast_id.with_value(ptr), call_id);
                } else {
                    res.insert(keys::ATTR_MACRO_CALL, ast_id.with_value(ptr), call_id);
                }
            },
        );
        data.items.iter().for_each(|&(_, item)| {
            add_assoc_item(db, res, item);
        });
        let (signature, source_map) = TraitSignature::with_source_map(db, *self);
        add_blocks(db, &signature.store, res);
        source_map.expansions().filter(|(ast, _)| ast.file_id == file_id).for_each(
            |(ast, &exp_id)| {
                res.insert(keys::MACRO_CALL, *ast, exp_id);
            },
        );

        AttrDefId::from(*self).child_by_source_to(db, res);
    }
}

impl ChildBySource for ImplId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        let data = self.impl_items(db);
        data.macro_calls().filter(|(ast_id, _)| ast_id.file_id == file_id).for_each(
            |(ast_id, call_id)| {
                let ptr = ast_id.to_ptr(db);
                if let Some(ptr) = ptr.cast::<ast::MacroCall>() {
                    res.insert(keys::MACRO_CALL, ast_id.with_value(ptr), call_id);
                } else {
                    res.insert(keys::ATTR_MACRO_CALL, ast_id.with_value(ptr), call_id);
                }
            },
        );
        data.items.iter().for_each(|&(_, item)| {
            add_assoc_item(db, res, item);
        });
        let (signature, source_map) = ImplSignature::with_source_map(db, *self);
        add_blocks(db, &signature.store, res);
        source_map.expansions().filter(|(ast, _)| ast.file_id == file_id).for_each(
            |(ast, &exp_id)| {
                res.insert(keys::MACRO_CALL, *ast, exp_id);
            },
        );

        AttrDefId::from(*self).child_by_source_to(db, res);
    }
}

impl ChildBySource for ModuleId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let def_map = self.def_map(db);
        let module_data = &def_map.module(db, *self);
        module_data.scope(db).child_by_source_to(db, res);

        AttrDefId::from(*self).child_by_source_to(db, res);
    }
}

impl ChildBySource for ItemScope {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        self.declarations().for_each(|item| add_module_def(db, res, item));
        self.impls().for_each(|imp| insert_item_loc(db, res, imp, keys::IMPL));
        self.extern_blocks()
            .for_each(|extern_block| insert_item_loc(db, res, extern_block, keys::EXTERN_BLOCK));
        self.extern_crate_decls()
            .for_each(|ext| insert_item_loc(db, res, ext, keys::EXTERN_CRATE));
        self.use_decls().for_each(|ext| insert_item_loc(db, res, ext, keys::USE));
        self.unnamed_consts().for_each(|konst| insert_item_loc(db, res, konst, keys::CONST));
        self.attr_macro_invocs().filter(|(id, _)| id.file_id == file_id).for_each(
            |(ast_id, call_id)| {
                let ptr = ast_id.to_ptr(db);
                res.insert(keys::ATTR_MACRO_CALL, ast_id.with_value(ptr), call_id);
            },
        );
        self.legacy_macros().for_each(|(_, ids)| {
            ids.iter().for_each(|&id| {
                if let MacroId::MacroRulesId(id) = id {
                    let loc = id.lookup(db);
                    if loc.id.file_id == file_id {
                        res.insert(keys::MACRO_RULES, loc.ast_ptr(db), id);
                    }
                }
            })
        });
        self.derive_macro_invocs().filter(|(id, _)| id.file_id == file_id).for_each(
            |(ast_id, calls)| {
                let adt_file = ast_id.file_id;
                let adt = view::AnyHasAttrs::new(ast_id.to_view(db));
                calls.for_each(|(attr_id, call_id, calls)| {
                    // FIXME: Is this the right crate?
                    let krate = call_id.lookup(db).krate;
                    let (_, meta) = attr_id.find_attr_range_with_source_view(db, krate, adt);
                    res.insert(
                        keys::DERIVE_MACRO_CALL,
                        InFile::new(adt_file, AstPtr::<ast::Meta>::of(meta)),
                        (attr_id, call_id, calls.into()),
                    );
                });
            },
        );
        self.iter_macro_invoc().filter(|(id, _)| id.file_id == file_id).for_each(
            |(ast_id, &call)| {
                let ptr = ast_id.to_ptr(db);
                res.insert(keys::MACRO_CALL, ast_id.with_value(ptr), call);
            },
        );
        fn add_module_def(db: &Db, map: &mut DynMap, item: ModuleDefId) {
            match item {
                ModuleDefId::FunctionId(id) => insert_item_loc(db, map, id, keys::FUNCTION),
                ModuleDefId::ConstId(id) => insert_item_loc(db, map, id, keys::CONST),
                ModuleDefId::TypeAliasId(id) => insert_item_loc(db, map, id, keys::TYPE_ALIAS),
                ModuleDefId::StaticId(id) => insert_item_loc(db, map, id, keys::STATIC),
                ModuleDefId::TraitId(id) => insert_item_loc(db, map, id, keys::TRAIT),
                ModuleDefId::AdtId(adt) => match adt {
                    AdtId::StructId(id) => insert_item_loc(db, map, id, keys::STRUCT),
                    AdtId::UnionId(id) => insert_item_loc(db, map, id, keys::UNION),
                    AdtId::EnumId(id) => insert_item_loc(db, map, id, keys::ENUM),
                },
                ModuleDefId::MacroId(id) => match id {
                    MacroId::Macro2Id(id) => insert_item_loc(db, map, id, keys::MACRO2),
                    MacroId::MacroRulesId(id) => insert_item_loc(db, map, id, keys::MACRO_RULES),
                    MacroId::ProcMacroId(id) => insert_item_loc(db, map, id, keys::PROC_MACRO),
                },
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => (),
            }
        }
    }
}

impl ChildBySource for VariantId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        let parent = *self;
        let fields = enabled_fields_view(db, parent);
        let fields_file = fields.file_id;
        // The ids count every field the cfg keeps, so the walk is not cut short by the file.
        for (idx, source) in fields.value.enumerate() {
            let local_id = LocalFieldId::from_raw(la_arena::RawIdx::from(idx as u32));
            let id = FieldId { parent, local_id };
            match source {
                Either::Left(source) => res.insert(
                    keys::TUPLE_FIELD,
                    InFile::new(fields_file, AstPtr::<ast::TupleField>::of(source)),
                    id,
                ),
                Either::Right(source) => res.insert(
                    keys::RECORD_FIELD,
                    InFile::new(fields_file, AstPtr::<ast::RecordField>::of(source)),
                    id,
                ),
            }
        }
        let (variant_fields, sm) = self.fields_with_source_map(db);
        add_blocks(db, &variant_fields.store, res);
        sm.expansions().filter(|(ast, _)| ast.file_id == file_id).for_each(|(ast, &exp_id)| {
            res.insert(keys::MACRO_CALL, *ast, exp_id);
        });

        AttrDefId::from(*self).child_by_source_to(db, res);
        AttrFlags::fields_docs(db, *self)
            .values()
            .flatten()
            .for_each(|docs| docs.child_by_source_to(db, res));
    }
}

impl ChildBySource for EnumId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        let loc = &self.lookup(db);
        if file_id != loc.id.file_id {
            return;
        }

        let ast_id_map = loc.id.file_id.ast_id_map(db);

        self.enum_variants(db).variants.values().for_each(|&(variant, _)| {
            let ptr = ast_id_map.get(variant.lookup(db).id.value);
            res.insert(keys::ENUM_VARIANT, InFile::new(loc.id.file_id, ptr), variant);
        });
        let (signature, source_map) = EnumSignature::with_source_map(db, *self);
        add_blocks(db, &signature.store, res);
        source_map
            .expansions()
            .filter(|(ast, _)| ast.file_id == file_id)
            .for_each(|(ast, &exp_id)| res.insert(keys::MACRO_CALL, *ast, exp_id));

        AttrDefId::from(*self).child_by_source_to(db, res);
    }
}

impl ChildBySource for DefWithBodyId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        let (body, sm) = Body::with_source_map(db, *self);
        if let &DefWithBodyId::VariantId(v) = self {
            VariantId::EnumVariantId(v).child_by_source_to(db, res)
        }

        sm.expansions().filter(|(ast, _)| ast.file_id == file_id).for_each(|(ast, &exp_id)| {
            res.insert(keys::MACRO_CALL, *ast, exp_id);
        });

        add_blocks(db, body, res);
    }
}

/// The blocks of `store` and the items written in them, added to `res`: a body's, or a
/// signature's, whose blocks are those of its anonymous constants (an array's length or a
/// const argument in a type, `fn f() -> [u8; { fn g() {} 1 }]`). Every block is merged into
/// the map of the store's owner, as each adds inner items to it.
fn add_blocks(db: &Db, store: &ExpressionStore, res: &mut DynMap) {
    let file_id = res.file();
    for (block, def_map) in store.blocks(db) {
        def_map.module(db, def_map.root).scope(db).child_by_source_to(db, res);
        // The blocks of a store are in its file and in every macro expansion in it; two
        // expansions of the same text have a block at the same range each.
        let ast_id = block.ast_id(db);
        let block_file = if blocks_of_any_file() { file_id } else { ast_id.file_id };
        if block_file == file_id {
            res.insert(keys::BLOCK, InFile::new(block_file, ast_id.to_ptr(db)), block);
        }
    }
}

impl ChildBySource for GenericDefId {
    fn child_by_source_to(&self, db: &Db, res: &mut DynMap) {
        let file_id = res.file();
        // Before the file is looked at: a block may be in a macro expansion in the signature.
        add_blocks(db, ExpressionStore::of(db, (*self).into()), res);
        let (gfile_id, generic_params_list) = self.file_id_and_params_of_view(db);
        if gfile_id != file_id {
            return;
        }

        let (generic_params, _, source_map) = GenericParams::with_source_map(db, *self);
        let mut toc_idx_iter = generic_params.iter_type_or_consts().map(|(idx, _)| idx);
        let lts_idx_iter = generic_params.iter_lt().map(|(idx, _)| idx);

        // For traits the first type index is `Self`, skip it.
        if let GenericDefId::TraitId(_) = *self {
            toc_idx_iter.next().unwrap(); // advance_by(1);
        }

        if let Some(generic_params_list) = generic_params_list {
            for (local_id, ast_param) in
                toc_idx_iter.zip(generic_params_list.type_or_const_params())
            {
                let id = TypeOrConstParamId { parent: *self, local_id };
                match ast_param {
                    view::TypeOrConstParam::Type(a) => res.insert(
                        keys::TYPE_PARAM,
                        InFile::new(gfile_id, AstPtr::<ast::TypeParam>::of(a)),
                        id,
                    ),
                    view::TypeOrConstParam::Const(a) => res.insert(
                        keys::CONST_PARAM,
                        InFile::new(gfile_id, AstPtr::<ast::ConstParam>::of(a)),
                        id,
                    ),
                }
            }
            for (local_id, ast_param) in lts_idx_iter.zip(generic_params_list.lifetime_params()) {
                let id = LifetimeParamId { parent: *self, local_id };
                res.insert(
                    keys::LIFETIME_PARAM,
                    InFile::new(gfile_id, AstPtr::<ast::LifetimeParam>::of(ast_param)),
                    id,
                );
            }
        }

        source_map
            .expansions()
            .filter(|(ast, _)| ast.file_id == file_id)
            .for_each(|(ast, &exp_id)| res.insert(keys::MACRO_CALL, *ast, exp_id));

        let attr_def = match *self {
            GenericDefId::AdtId(it) => AttrDefId::from(it),
            GenericDefId::ConstId(it) => it.into(),
            GenericDefId::FunctionId(it) => it.into(),
            GenericDefId::ImplId(it) => it.into(),
            GenericDefId::StaticId(it) => it.into(),
            GenericDefId::TraitId(it) => it.into(),
            GenericDefId::TypeAliasId(it) => it.into(),
        };
        attr_def.child_by_source_to(db, res);
    }
}

fn insert_item_loc<ID, N, Data>(db: &Db, res: &mut DynMap, id: ID, key: Key<N, ID>)
where
    ID: Lookup<Data = Data> + 'static,
    Data: AstIdLoc<Ast = N>,
    N: AstIdNode + 'static,
    keys::AstPtrPolicy<N, ID>: keys::Slot<N, ID>,
{
    let loc = id.lookup(db);
    if loc.ast_id().file_id == res.file() {
        res.insert(key, loc.ast_ptr(db), id)
    }
}

fn add_assoc_item(db: &Db, res: &mut DynMap, item: AssocItemId) {
    match item {
        AssocItemId::FunctionId(func) => insert_item_loc(db, res, func, keys::FUNCTION),
        AssocItemId::ConstId(konst) => insert_item_loc(db, res, konst, keys::CONST),
        AssocItemId::TypeAliasId(ty) => insert_item_loc(db, res, ty, keys::TYPE_ALIAS),
    }
}
