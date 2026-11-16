//! The two traits: everything the type checker may ask of the item layer
//! (docs/v2/plan/design/bottom.md, point 4).

use core::alloc::Allocator;

use blaze_id::{ContentId, CrateItemKey, ItemKey};

use crate::crate_facts::{CrateGraphView, LangItemsView, NoImplsOfView};
use crate::error::{NotBuilt, SurfaceError};
use crate::file_items::{FileItem, FileItemsView};
use crate::key_index::KeyEntry;
use crate::kind::{ByteRange, ItemKind, Namespace, Place, Target};
use crate::locals::{AnyItem, Local, LocalTable};
use crate::module_decls::ModuleDeclsView;
use crate::module_scope::ScopeView;
use crate::names::{CrateRootView, MacroDefView, ModuleFileView, TextualEntryView};
use crate::proof::HasText;

/// What may be asked of any crate's items: every method reads records, and none can reach
/// text. A dependency is only its records: a missing one is [`SurfaceError::NoRecord`] (or its
/// file and crate forms), made by that crate's own task, never by the asker.
///
/// Each question has two forms: by [`ItemKey`], the portable form records hold, and by
/// [`Local`], the task's own number for an item (`local_*`), which goes through the task's
/// [`LocalTable`] and is written once here for every implementation. A stage's entry point takes
/// `&impl Items`; helpers behind it stay on the concrete database.
///
/// The next methods, which need the type checker's types and so are not in this crate yet:
/// `signature` (the lowered signature, types by item key), `fields`, `variants`,
/// `assoc_items`, `predicates`, `supertraits`, `type_attrs` (split from the signature so each is
/// cut off on its own), `impl_header` and the `impl_index` bucket of a (crate, trait or
/// self-type shape), `body_facts` (hidden types, const values, discriminants) and `expansion`
/// (the items or tokens one macro call makes). Until then an item made by a macro call, whose
/// place is an expansion, answers [`NotBuilt`] where its file row would be read.
pub trait Items {
    /// The item's entry of its module's `KeyIndex` page: its place and its parent. Refused as
    /// `NoItem` when no page of its crate holds the key.
    fn entry(&self, item: ItemKey) -> Result<KeyEntry, SurfaceError>;

    /// The `FileItems` of the file with content `file`.
    fn file_items(&self, file: ContentId) -> Result<FileItemsView<'_>, SurfaceError>;

    /// The `ModuleDecls` of a module.
    fn decls(&self, module: ItemKey) -> Result<ModuleDeclsView<'_>, SurfaceError>;

    /// The `ModuleScope` of a module.
    fn scope(&self, module: ItemKey) -> Result<ScopeView<'_>, SurfaceError>;

    /// The `TextualEntry` of a module.
    fn textual_entry(&self, module: ItemKey) -> Result<TextualEntryView<'_>, SurfaceError>;

    /// The `ModuleFile` of a module.
    fn module_file(&self, module: ItemKey) -> Result<ModuleFileView<'_>, SurfaceError>;

    /// The `MacroDef` of a macro.
    fn macro_def(&self, item: ItemKey) -> Result<MacroDefView<'_>, SurfaceError>;

    /// The root record of a crate.
    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootView<'_>, SurfaceError>;

    /// The `LangItems` of a crate.
    fn lang_items(&self, krate: CrateItemKey) -> Result<LangItemsView<'_>, SurfaceError>;

    /// The `CrateGraph` a crate sees.
    fn crate_graph(&self, krate: CrateItemKey) -> Result<CrateGraphView<'_>, SurfaceError>;

    /// The `NoImplsOf` of a crate.
    fn no_impls_of(&self, krate: CrateItemKey) -> Result<NoImplsOfView<'_>, SurfaceError>;

    /// Where the item is written.
    fn place(&self, item: ItemKey) -> Result<Place, SurfaceError> {
        Ok(self.entry(item)?.place)
    }

    /// The item it is in; `None` for a crate's root module.
    fn parent(&self, item: ItemKey) -> Result<Option<ItemKey>, SurfaceError> {
        Ok(self.entry(item)?.parent)
    }

    /// The item's row of its file, with the file's content. An item an expansion made is
    /// [`NotBuilt`] until expansion records are.
    fn file_item(&self, item: ItemKey) -> Result<(ContentId, FileItem<'_>), SurfaceError> {
        match self.place(item)? {
            Place::File { file, index } => {
                let row = self.file_items(file)?.item(index).ok_or(SurfaceError::NoRecord {
                    fact: "the item's row of its FileItems",
                    item,
                })?;
                Ok((file, row))
            }
            Place::Expansion { .. } => Err(NotBuilt { method: "file_item", item }.into()),
        }
    }

    /// What the item is.
    fn kind(&self, item: ItemKey) -> Result<ItemKind, SurfaceError> {
        match self.place(item)? {
            Place::File { .. } => Ok(self.file_item(item)?.1.kind),
            Place::Expansion { .. } => Err(NotBuilt { method: "kind", item }.into()),
        }
    }

    /// What `name` in `ns` is bound to in `module`'s scope, by one probe of its record.
    fn lookup(
        &self,
        module: ItemKey,
        ns: Namespace,
        name: &str,
    ) -> Result<Option<Target<ItemKey>>, SurfaceError> {
        Ok(self.scope(module)?.get(ns, name).map(|binding| binding.target))
    }

    /// [`Items::place`] of a task's local.
    fn local_place<A: Allocator + Clone>(
        &self,
        locals: &LocalTable<AnyItem, A>,
        item: Local<AnyItem>,
    ) -> Result<Place, SurfaceError> {
        self.place(locals.key(item)?)
    }

    /// [`Items::kind`] of a task's local.
    fn local_kind<A: Allocator + Clone>(
        &self,
        locals: &LocalTable<AnyItem, A>,
        item: Local<AnyItem>,
    ) -> Result<ItemKind, SurfaceError> {
        self.kind(locals.key(item)?)
    }

    /// [`Items::parent`] of a task's local, the parent numbered in the task's table.
    fn local_parent<A: Allocator + Clone>(
        &self,
        locals: &mut LocalTable<AnyItem, A>,
        item: Local<AnyItem>,
    ) -> Result<Option<Local<AnyItem>>, SurfaceError> {
        match self.parent(locals.key(item)?)? {
            Some(parent) => Ok(Some(locals.intern(parent)?)),
            None => Ok(None),
        }
    }

    /// [`Items::scope`] of a task's local module.
    fn local_scope<A: Allocator + Clone>(
        &self,
        locals: &LocalTable<AnyItem, A>,
        module: Local<AnyItem>,
    ) -> Result<ScopeView<'_>, SurfaceError> {
        self.scope(locals.key(module)?)
    }

    /// [`Items::lookup`] in a task's local module, what it finds numbered in the task's table.
    fn local_lookup<A: Allocator + Clone>(
        &self,
        locals: &mut LocalTable<AnyItem, A>,
        module: Local<AnyItem>,
        ns: Namespace,
        name: &str,
    ) -> Result<Option<Target<Local<AnyItem>>>, SurfaceError> {
        match self.lookup(locals.key(module)?, ns, name)? {
            Some(target) => Ok(Some(target.try_map(|key| locals.intern(key))?)),
            None => Ok(None),
        }
    }
}

/// What may be asked of the crate a task compiles, whose text the task holds: every method
/// takes the [`HasText`] the task's driver made, and refuses one of another crate.
pub trait Source: Items {
    /// The crate whose text this is.
    fn krate(&self) -> CrateItemKey;

    /// The text of a file of this crate. An implementation refuses a proof of another crate
    /// with [`Source::check`] before it reads anything.
    fn text(&self, proof: HasText, file: ContentId) -> Result<&str, SurfaceError>;

    /// That `proof` is of this crate.
    fn check(&self, proof: HasText) -> Result<(), SurfaceError> {
        match proof.krate() == self.krate() {
            true => Ok(()),
            false => Err(SurfaceError::WrongCrate { proof: proof.krate(), krate: self.krate() }),
        }
    }

    /// The text of the item's signature.
    fn signature_text(&self, proof: HasText, item: ItemKey) -> Result<&str, SurfaceError> {
        range_text(self, proof, item, |row| row.signature, "the signature's bytes in its file")
    }

    /// The text of the item's body; empty when it has none.
    fn body_text(&self, proof: HasText, item: ItemKey) -> Result<&str, SurfaceError> {
        range_text(self, proof, item, |row| row.body, "the body's bytes in its file")
    }

    /// The text of the item's outer attributes and doc comments.
    fn attrs_text(&self, proof: HasText, item: ItemKey) -> Result<&str, SurfaceError> {
        range_text(self, proof, item, |row| row.attrs, "the attributes' bytes in its file")
    }

    /// [`Source::signature_text`] of a task's local.
    fn local_signature_text<A: Allocator + Clone>(
        &self,
        proof: HasText,
        locals: &LocalTable<AnyItem, A>,
        item: Local<AnyItem>,
    ) -> Result<&str, SurfaceError> {
        self.signature_text(proof, locals.key(item)?)
    }

    /// [`Source::body_text`] of a task's local.
    fn local_body_text<A: Allocator + Clone>(
        &self,
        proof: HasText,
        locals: &LocalTable<AnyItem, A>,
        item: Local<AnyItem>,
    ) -> Result<&str, SurfaceError> {
        self.body_text(proof, locals.key(item)?)
    }
}

/// The bytes `pick` gives of the item's row, in its file's text. A range outside the text, or
/// one cutting a character, is a `FileItems` record of other bytes than the text: refused as
/// no record of `fact`.
fn range_text<'s, S: Source + ?Sized>(
    source: &'s S,
    proof: HasText,
    item: ItemKey,
    pick: impl FnOnce(&FileItem<'_>) -> ByteRange,
    fact: &'static str,
) -> Result<&'s str, SurfaceError> {
    source.check(proof)?;
    let (file, row) = source.file_item(item)?;
    let range = pick(&row).in_file(row.at).ok_or(SurfaceError::NoRecord { fact, item })?;
    source.text(proof, file)?.get(range).ok_or(SurfaceError::NoRecord { fact, item })
}
