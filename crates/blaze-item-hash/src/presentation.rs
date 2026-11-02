//! A body's presentation (docs/v2/plan/design/item-hash.md, section 3.7): what its identity
//! leaves out and its code can still show. Inference, MIR and borrow check see a body
//! anonymously (its identity); debug info, `Location::caller`, panic messages and
//! `line!`/`column!`/`file!`/`module_path!` see its names and where it is. Codegen keys its
//! output by both, so a rename or a move reuses the store's records and still rebuilds what
//! prints the names.
//!
//! It hashes the item's path (its crate, modules and name), the impl or trait it is in by key,
//! the names of its bindings and labels in order, its owner's generic and lifetime names, the
//! path of each file it is in, and the line and column (in chars and in display width) where
//! `self` and each expression, pattern, label and type of its source map starts and ends. Byte
//! offsets are left out: an edit above the body that keeps its lines leaves it as it was.
//!
//! Names of other items that reach the output (a callee's symbol, a type's debug info name)
//! are those items' presentation, not this body's: codegen keys by them as well.

use base_db::{Db, FileId};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, key};
use hir_def::{
    DefWithBodyId, GenericDefId, ItemContainerId, Lookup as _, SyntheticSyntax,
    expr_store::Body, hir::generics::GenericParams,
};
use hir_expand::{FileRange, InFile};
use span::TextRange;

pub const PRESENTATION_DOMAIN: &str = "blaze v2 item presentation 1";

/// Ends a list of the hash, after entries that each start with 0 or 1.
const END: u8 = 2;

/// The presentation hash of `def`'s body.
#[blaze_query::tracked(returns(copy))]
pub fn presentation(db: &Db, def: DefWithBodyId) -> ContentId {
    let mut h = Hasher::new(PRESENTATION_DOMAIN);
    // alloc: the item's path, once per body.
    h.part(key::path(db, ItemId::of_body(db, def)).as_bytes());
    if let DefWithBodyId::VariantId(variant) = def {
        h.id(&key::variant_key(db, variant).0);
    }
    let generic = def.generic_def(db);
    generic_names(db, generic, &mut h);
    if let Some((parent, item)) = parent(db, generic) {
        // The impl or trait by key: a method's symbol names its impl's self type and trait.
        h.update(&[1]).id(&key::item_key(db, item).0);
        generic_names(db, parent, &mut h);
    }
    h.update(&[END]);

    let (body, map) = Body::with_source_map(db, def);
    for (_, binding) in body.bindings() {
        h.part(binding.name.as_str().as_bytes());
    }
    h.update(&[END]);
    for (_, label) in body.labels() {
        h.part(label.name.as_str().as_bytes());
    }
    h.update(&[END]);

    // alloc: the files the body is in, almost always one.
    let mut files = Vec::with_capacity(1);
    let mut places = Places { db, h: &mut h, files: &mut files };
    match map.self_param_syntax() {
        Some(it) => places.place(it.map(|ptr| ptr.text_range())),
        None => _ = places.h.update(&[0]),
    }
    for (expr, _) in body.exprs() {
        places.synthetic(map.expr_syntax(expr).map(|it| it.map(|ptr| ptr.text_range())));
    }
    for (pat, _) in body.pats() {
        places.synthetic(map.pat_syntax(pat).map(|it| it.map(|ptr| ptr.text_range())));
    }
    for (label, _) in body.labels() {
        places.place(map.label_syntax(label).map(|ptr| ptr.text_range()));
    }
    for (ty, _) in body.type_refs() {
        places.synthetic(map.type_syntax(ty).map(|it| it.map(|ptr| ptr.text_range())));
    }
    h.update(&[END]);
    for &file in &files {
        let root = db.file_source_root(file).source_root_id(db);
        let path = db.source_root(root).source_root_ref(db).path_for_file(&file);
        // alloc: the file's path as text, once per file of the body.
        h.opt_part(path.map(|it| it.to_string()));
    }
    h.finish()
}

/// The names of `def`'s own generic params: each type or const param's (none for an
/// `impl Trait` param's), then each lifetime's.
fn generic_names(db: &Db, def: GenericDefId, h: &mut Hasher) {
    let params = GenericParams::of(db, def);
    for (_, param) in params.iter_type_or_consts() {
        h.opt_part(param.name().map(|it| it.as_str().as_bytes()));
    }
    h.update(&[END]);
    for (_, param) in params.iter_lt() {
        h.update(&[1]).part(param.name.as_str().as_bytes());
    }
    h.update(&[END]);
}

/// The impl or trait whose generic params `def`'s body also sees, and it as an item.
fn parent(db: &Db, def: GenericDefId) -> Option<(GenericDefId, ItemId)> {
    let container = match def {
        GenericDefId::FunctionId(it) => it.lookup(db).container,
        GenericDefId::ConstId(it) => it.lookup(db).container,
        GenericDefId::TypeAliasId(it) => it.lookup(db).container,
        GenericDefId::StaticId(_)
        | GenericDefId::AdtId(_)
        | GenericDefId::TraitId(_)
        | GenericDefId::ImplId(_) => return None,
    };
    match container {
        ItemContainerId::ImplId(it) => Some((it.into(), ItemId::Impl(it))),
        ItemContainerId::TraitId(it) => Some((it.into(), ItemId::Trait(it))),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    }
}

/// Hashes where each part of a body is, each file by its index in `files`.
struct Places<'a> {
    db: &'a Db,
    h: &'a mut Hasher,
    files: &'a mut Vec<FileId>,
}

impl Places<'_> {
    fn synthetic(&mut self, source: Result<InFile<TextRange>, SyntheticSyntax>) {
        match source {
            Ok(source) => self.place(source),
            Err(SyntheticSyntax) => {
                self.h.update(&[0]);
            }
        }
    }

    /// Where `source` is in the file it is written in: what rustc reports of it.
    fn place(&mut self, source: InFile<TextRange>) {
        let FileRange { file_id, range } = source.original_node_file_range_rooted(self.db);
        let file = file_id.file_id(self.db);
        let index = match self.files.iter().position(|&it| it == file) {
            Some(index) => index,
            None => {
                self.files.push(file);
                self.files.len() - 1
            }
        };
        self.h.update(&[1]).update(&(index as u32).to_le_bytes());
        let text = self.db.file_text(file);
        let lines = base_db::line_index(self.db, text);
        for at in [range.start(), range.end()] {
            let place = lines.place(at);
            let text = text.text(self.db);
            let (line, chars, display) = match place {
                Some(place) => (
                    place.line(),
                    place.char_column(text).unwrap_or(u32::MAX),
                    place.display_column(text).unwrap_or(u32::MAX),
                ),
                None => (u32::MAX, u32::MAX, u32::MAX),
            };
            self.h
                .update(&line.to_le_bytes())
                .update(&chars.to_le_bytes())
                .update(&display.to_le_bytes());
        }
    }
}
