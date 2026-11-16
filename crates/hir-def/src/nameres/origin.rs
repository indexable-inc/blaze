//! Where a module's items are written ([`ModuleOrigin`]), read from the records: the crate
//! root from the crate, a module of a file or an inline `mod` from its `ModuleFile` record and
//! its declaration's place (`key::ast_id_of`), a block's module from its block. A module
//! declared inside a block is in no record yet (the scope of a block), a typed stop.

use base_db::{Db, EditionedFileId, OrNeed};
use blaze_surface::Items;
use hir_expand::{AstId, ErasedAstId, HirFileId, InFile};
use span::ROOT_ERASED_FILE_AST_ID;
use syntax::{
    ast,
    view::{self, AstNode as _},
};
use tt::TextRange;

use crate::{BlockId, ModuleId, key, nameres::ModuleData};

/// Where a module's items are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleOrigin {
    CrateRoot {
        definition: EditionedFileId,
    },
    /// A `mod name;` whose items are in a file of their own (never an expansion's).
    File {
        is_mod_rs: bool,
        declaration: AstId<ast::Module>,
        definition: EditionedFileId,
    },
    /// A `mod name { .. }`.
    Inline {
        definition: AstId<ast::Module>,
    },
    /// The module a block with items is (it holds only those items).
    BlockExpr {
        id: BlockId,
        block: AstId<ast::BlockExpr>,
    },
}

impl ModuleOrigin {
    /// Where `module` is written.
    pub fn of(db: &Db, module: ModuleId) -> ModuleOrigin {
        let parent = module.containing_module_inside_def_map(db);
        let Some(key) = module.key(db) else {
            return match (module.block(db), parent) {
                (Some(block), None) => {
                    let id = block.to_static();
                    ModuleOrigin::BlockExpr { id, block: id.ast_id(db) }
                }
                _ => crate::nameres::block_scope_stop(db, module, "the origin of a module in a block"),
            };
        };
        let krate = module.krate(db);
        if parent.is_none() && module.block(db).is_none() {
            return ModuleOrigin::CrateRoot { definition: krate.root_file_id(db) };
        }
        let declaration = key::ast_id_of::<ast::Module>(db, key);
        let file = db.items().module_file(key).or_need();
        if file.inline().is_some() {
            return ModuleOrigin::Inline { definition: declaration };
        }
        let Some(file_id) = db.file_id(file.file()) else {
            panic!("module {key:x}: its file {:x} is not of the task's file table", file.file())
        };
        let definition = EditionedFileId::new_with_extensions(
            db,
            file_id,
            krate.data(db).edition,
            krate.extensions(db),
        );
        let is_mod_rs = file.path().rsplit('/').next() == Some("mod.rs");
        ModuleOrigin::File { is_mod_rs, declaration, definition }
    }

    pub fn declaration(&self) -> Option<AstId<ast::Module>> {
        match *self {
            ModuleOrigin::File { declaration, .. } => Some(declaration),
            ModuleOrigin::Inline { definition } => Some(definition),
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::BlockExpr { .. } => None,
        }
    }

    pub fn file_id(&self) -> Option<EditionedFileId> {
        match *self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                Some(definition)
            }
            ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. } => None,
        }
    }

    pub fn is_inline(&self) -> bool {
        matches!(self, ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. })
    }

    /// The node that defines the module, a view of the tree the database keeps: a file, a
    /// `mod foo {}` or a block.
    pub fn definition_source_view<'db>(&self, db: &'db Db) -> InFile<ModuleSourceView<'db>> {
        match *self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                InFile::new(
                    definition.into(),
                    ModuleSourceView::SourceFile(definition.parse(db).view()),
                )
            }
            ModuleOrigin::Inline { definition } => {
                InFile::new(definition.file_id, ModuleSourceView::Module(definition.to_view(db)))
            }
            ModuleOrigin::BlockExpr { block, .. } => {
                InFile::new(block.file_id, ModuleSourceView::BlockExpr(block.to_view(db)))
            }
        }
    }

    /// The file the defining node is in, without a parse.
    pub fn definition_source_file_id(&self) -> HirFileId {
        match *self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                definition.into()
            }
            ModuleOrigin::Inline { definition } => definition.file_id,
            ModuleOrigin::BlockExpr { block, .. } => block.file_id,
        }
    }

    pub fn definition_source_range(&self, db: &Db) -> InFile<TextRange> {
        match *self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                InFile::new(
                    definition.into(),
                    ErasedAstId::new(definition.into(), ROOT_ERASED_FILE_AST_ID).to_range(db),
                )
            }
            ModuleOrigin::Inline { definition } => {
                InFile::new(definition.file_id, definition.to_range(db))
            }
            ModuleOrigin::BlockExpr { block, .. } => InFile::new(block.file_id, block.to_range(db)),
        }
    }
}

impl ModuleData {
    /// Where the module is written ([`ModuleOrigin::of`]).
    pub fn origin(self, db: &Db) -> ModuleOrigin {
        ModuleOrigin::of(db, self.id)
    }

    pub fn definition_source_view(self, db: &Db) -> InFile<ModuleSourceView<'_>> {
        self.origin(db).definition_source_view(db)
    }

    pub fn definition_source_file_id(self, db: &Db) -> HirFileId {
        self.origin(db).definition_source_file_id()
    }

    pub fn definition_source_range(self, db: &Db) -> InFile<TextRange> {
        self.origin(db).definition_source_range(db)
    }

    /// The node that declares the module, a `mod foo;` or a `mod foo {}`; `None` for the
    /// crate root or a block.
    pub fn declaration_source(self, db: &Db) -> Option<InFile<ast::Module>> {
        let decl = self.origin(db).declaration()?;
        Some(InFile { file_id: decl.file_id, value: decl.to_node(db) })
    }

    pub fn declaration_source_view(self, db: &Db) -> Option<InFile<view::Module<'_>>> {
        let decl = self.origin(db).declaration()?;
        Some(InFile::new(decl.file_id, decl.to_view(db)))
    }

    pub fn declaration_source_range(self, db: &Db) -> Option<InFile<TextRange>> {
        let decl = self.origin(db).declaration()?;
        Some(InFile { file_id: decl.file_id, value: decl.to_range(db) })
    }
}

/// The node that defines a module, as views of the tree the database keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleSourceView<'db> {
    SourceFile(view::SourceFile<'db>),
    Module(view::Module<'db>),
    BlockExpr(view::BlockExpr<'db>),
}

impl<'db> ModuleSourceView<'db> {
    pub fn node(self) -> view::Node<'db> {
        match self {
            ModuleSourceView::SourceFile(it) => it.syntax(),
            ModuleSourceView::Module(it) => it.syntax(),
            ModuleSourceView::BlockExpr(it) => it.syntax(),
        }
    }
}
