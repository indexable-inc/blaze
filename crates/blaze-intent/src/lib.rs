//! Edits by meaning and declared intent (docs/v2/plan/design/compile-time.md, ideas 9 and 10;
//! docs/v2/plan/lanes/fastcompile/intent.md).
//!
//! [`apply`] makes a request's [`Edit`]s in the files of a view the front end has lowered:
//! each finds its target by path ([`path`]), replaces only that text and reparses only what it
//! touched ([`file::Working::splice`]), and says which items it changed and how. A rename finds
//! every reference through the front end's name resolution, never by text search ([`rename`]),
//! and so does a replace, which retargets every reference to one item to another ([`replace`]).
//! What comes out is the VFS's text edits ([`blaze_vfs::Op::Edit`], and the moves of a renamed
//! module's files): committing them gives exactly the bytes, and so the root, a text edit
//! would. The trees reparsed in place come out too ([`Seed`]), for the front end to take in
//! place of parsing the new text again.
//!
//! [`ahead`] is declared intent's half that needs the tests' last runs: which test binaries and
//! which functions a [`blaze_api::Request::WillTest`] needs built.

#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

pub mod ahead;
pub mod edit;
pub mod file;
pub mod path;
pub mod rename;
pub mod replace;
pub mod retarget;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::{Db, FileId, FileText};
use blaze_api::{ApiError, Edit, Edited, EditedItem};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_vfs::Op;
use hir::view::Semantics;
use syntax::{
    Edition, Parse, SourceFile, SyntaxKind,
    view::{self as ast, AstNode as _},
};

use crate::{
    edit::{At, Touch},
    file::Working,
};

/// The view's files as the job that edits sees them.
pub trait Files {
    /// The front end's file at `path` in the view (relative to its root, as the VFS names
    /// it), and its bytes there.
    fn file(&self, path: &str) -> Option<(FileId, &[u8])>;
    /// The view's path of a front-end file; `None` for one outside the view (a registry
    /// dependency's, the standard library's).
    fn path(&self, file: FileId) -> Option<&str>;
}

/// A file's tree after the edits, parsed in place: what the front end's parse of the file whose
/// text has the content ID `content` (a [`FileText`]'s) and `edition` gives. Only a tree with
/// no syntax errors is one: the errors of a tree reparsed in place may come in another order.
/// It is for [`base_db::EditionedFileId::seed_parse`], which takes it only while `content` is
/// the file's text.
pub struct Seed {
    /// The file in the database the edits were made against: the same file in a front end
    /// moved on from it to the committed view.
    pub file: FileId,
    pub content: ContentId,
    pub edition: Edition,
    pub parse: Parse<SourceFile>,
}

/// What [`apply`] made.
pub struct Applied {
    pub edited: Edited,
    reparsed: Vec<Reparsed>,
}

/// A file the edits changed, as they left it.
struct Reparsed {
    file: FileId,
    parse: Parse<SourceFile>,
    /// Where the view's bytes end in the tree's text.
    end: usize,
    edition: Edition,
    appended: Option<&'static str>,
}

impl Applied {
    /// The trees to seed the front end with, made when asked: each hashes its file's new bytes
    /// and checks its whole tree for errors, which a caller that seeds nothing never pays.
    pub fn seeds(&self) -> impl Iterator<Item = Seed> + '_ {
        self.reparsed.iter().filter(|file| file.parse.errors().is_empty()).map(|file| Seed {
            file: file.file,
            content: FileText::content_of(&file.parse.root().file().text()[..file.end], file.appended),
            edition: file.edition,
            parse: file.parse.clone(), // clone: two `Arc` counts, the seed's own
        })
    }
}

/// The files a request's edits work in, as they leave them.
pub(crate) struct Edits<'a, F: Files> {
    pub db: &'a Db,
    pub sema: Semantics<'a>,
    pub files: &'a F,
    pub working: Vec<Working>,
    pub ids: Vec<FileId>,
    /// Each working copy's tree before any splice, as the front end has it: the views `sema`
    /// answers for.
    pub roots: Vec<ast::Node<'a>>,
    pub by_path: HashMap<Box<str>, u32>,
    pub by_id: HashMap<FileId, u32>,
    pub touches: Vec<Touch>,
    /// The moves of renamed modules' files, after every text edit.
    pub moves: Vec<Op>,
}

/// Makes `edits`, in order, in the view whose front end is `db` and whose files are `files`.
/// A rename or a replace resolves in the view as it is, so each comes before the request's
/// other edits. The request's replaces are made together, in one pass at the first of them.
pub fn apply<F: Files>(db: &Db, files: &F, edits: &[Edit]) -> Result<Applied, ApiError> {
    let mut cx = Edits {
        db,
        sema: Semantics::new(db),
        files,
        working: Vec::new(),
        ids: Vec::new(),
        roots: Vec::new(), // alloc: one view per file the request opens, beside `ids`
        by_path: HashMap::default(),
        by_id: HashMap::default(),
        touches: Vec::new(),
        moves: Vec::new(),
    };
    let mut others_made = false;
    let mut replaced = false;
    for edit in edits {
        match edit {
            Edit::Rename { to, item } => {
                if others_made {
                    return Err(ApiError::new(
                        format!(
                            "the rename of `{}` comes after another kind of edit: a rename \
                             resolves in the view as it is, so renames come first",
                            item.item
                        ),
                        Some(item.file.clone()),
                    ));
                }
                rename::rename(&mut cx, item, to)?;
            }
            Edit::Replace { .. } if replaced => {}
            Edit::Replace { .. } => {
                let late = edits
                    .iter()
                    .skip_while(|it| matches!(it, Edit::Rename { .. } | Edit::Replace { .. }))
                    .find_map(|it| match it {
                        Edit::Replace { from, .. } => Some(from),
                        _ => None,
                    });
                if let Some(from) = late {
                    return Err(ApiError::new(
                        format!(
                            "the replace of `{from}` comes after another kind of edit: a replace \
                             resolves in the view as it is, so replaces and renames come first"
                        ),
                        None,
                    ));
                }
                // alloc: the request's replaces, made together in one pass
                let asked: Vec<(&str, &str)> = edits
                    .iter()
                    .filter_map(|it| match it {
                        Edit::Replace { to, from } => Some((from.as_str(), to.as_str())),
                        _ => None,
                    })
                    .collect();
                replace::replace(&mut cx, &asked)?;
                replaced = true;
            }
            _ => {
                others_made = true;
                let path = edit::file_of(edit).expect("an edit other than a replace names its file");
                let at = cx.open(path).map_err(|what| ApiError::new(what, Some(path.to_owned())))?;
                edit::apply(&mut cx.working[at as usize], at, edit, &mut cx.touches)
                    .map_err(|what| ApiError::new(what, Some(path.to_owned())))?;
            }
        }
    }
    Ok(cx.finish())
}

impl<F: Files> Edits<'_, F> {
    /// The working copy of the view's file at `path`, opened on first use.
    pub fn open(&mut self, path: &str) -> Result<u32, String> {
        if let Some(&at) = self.by_path.get(path) {
            return Ok(at);
        }
        let (file, _) = self
            .files
            .file(path)
            .ok_or_else(|| format!("{path} is no file of the view the front end has"))?;
        self.load(file, path)
    }

    /// The working copy of the front end's `file`, opened on first use.
    pub fn open_id(&mut self, file: FileId) -> Result<u32, String> {
        if let Some(&at) = self.by_id.get(&file) {
            return Ok(at);
        }
        let files = self.files;
        let path = files.path(file).ok_or_else(|| format!("file {file:?} the edit reaches is outside the view"))?;
        self.load(file, path)
    }

    fn load(&mut self, file: FileId, path: &str) -> Result<u32, String> {
        let (_, bytes) = self.files.file(path).ok_or_else(|| format!("{path} is not in the view"))?;
        let bytes = std::str::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))?;
        let editioned = self.sema.attach_first_edition(file);
        let root = self.sema.parse(editioned).syntax();
        let text = self.db.file_text(file).text(self.db);
        if !begins_with(text, bytes) {
            return Err(format!(
                "the front end's text of {path} is not the view's bytes: the job lowered another view"
            ));
        }
        let parse = editioned.parse(self.db).clone();
        let source = parse.root().file().source();
        if !source.ptr_eq(text) && **source != **text {
            return Err(format!("the front end's tree of {path} is not of its text"));
        }
        let working = Working::new(path, editioned.edition(self.db), bytes.len(), parse);
        let at = u32::try_from(self.working.len()).expect("fewer than 2^32 files");
        self.working.push(working);
        self.ids.push(file);
        self.roots.push(root);
        self.by_path.insert(path.into(), at);
        self.by_id.insert(file, at);
        Ok(at)
    }

    /// The edits' ops, the items they touched, named and keyed in the view as it was, and the
    /// files they changed, to seed when asked.
    fn finish(self) -> Applied {
        let Edits { db, sema, working, ids, roots, mut touches, moves, .. } = self;
        let mut ops: Vec<Op> = Vec::with_capacity(working.iter().map(|it| it.splices.len()).sum::<usize>() + moves.len());
        ops.extend(working.iter().flat_map(Working::ops));
        ops.extend(moves);
        touches.sort_unstable();
        touches.dedup();
        let mut items: Vec<EditedItem> = touches
            .iter()
            .map(|touch| {
                let file = &working[touch.file as usize];
                let path = edit::path_at(file, &touch.at);
                let key = match &touch.at {
                    At::Item(offset) => {
                        let node = edit::item_at(roots[touch.file as usize], *offset).expect("a touched item starts there");
                        key_of(&sema, db, node)
                    }
                    At::File => file_key(&sema, db, ids[touch.file as usize]),
                    At::Added(_) => None,
                };
                EditedItem { how: touch.how, key, file: file.path.to_string(), path }
            })
            .collect();
        items.sort_unstable_by(|a, b| (&a.file, &a.path, a.how).cmp(&(&b.file, &b.path, b.how)));
        items.dedup();
        let reparsed = working
            .into_iter()
            .zip(&ids)
            .filter(|(file, _)| !file.splices.is_empty())
            .map(|(file, id)| Reparsed {
                file: *id,
                appended: db.file_text(*id).appended_root_module(db),
                end: file.end,
                edition: file.edition,
                parse: file.parse,
            })
            .collect();
        Applied { edited: Edited { ops, items }, reparsed }
    }
}

/// Whether `text` begins with `bytes`: no compare when they are the same memory, as a view's
/// file the front end reads in place is.
fn begins_with(text: &str, bytes: &str) -> bool {
    text.len() >= bytes.len()
        && (std::ptr::eq(text.as_ptr(), bytes.as_ptr()) || text.as_bytes()[..bytes.len()] == *bytes.as_bytes())
}

/// The key ([`blaze_items::ItemKey`]) of the item `node` is, in the view the front end has;
/// `None` for one it has none of (cfg'd out, or no item with a key).
pub(crate) fn key_of<'db>(
    sema: &Semantics<'db>,
    db: &Db,
    node: ast::Node<'db>,
) -> Option<ContentId> {
    use blaze_items::ItemId;
    use hir_def::{
        AdtId, ConstId, FunctionId, ImplId, MacroId, ModuleId, StaticId, TraitId, TypeAliasId,
    };

    let id = match node.kind() {
        SyntaxKind::FN => ItemId::Function(FunctionId::try_from(sema.to_fn_def(ast::Fn::cast(node)?)?).ok()?),
        SyntaxKind::STRUCT => ItemId::Adt(AdtId::StructId(sema.to_struct_def(ast::Struct::cast(node)?)?.into())),
        SyntaxKind::ENUM => ItemId::Adt(AdtId::EnumId(sema.to_enum_def(ast::Enum::cast(node)?)?.into())),
        SyntaxKind::UNION => ItemId::Adt(AdtId::UnionId(sema.to_def(ast::Union::cast(node)?)?.into())),
        SyntaxKind::TRAIT => ItemId::Trait(TraitId::from(sema.to_trait_def(ast::Trait::cast(node)?)?)),
        SyntaxKind::IMPL => ItemId::Impl(ImplId::try_from(sema.to_impl_def(ast::Impl::cast(node)?)?).ok()?),
        SyntaxKind::CONST => ItemId::Const(ConstId::from(sema.to_const_def(ast::Const::cast(node)?)?)),
        SyntaxKind::STATIC => ItemId::Static(StaticId::from(sema.to_static_def(ast::Static::cast(node)?)?)),
        SyntaxKind::TYPE_ALIAS => ItemId::Alias(TypeAliasId::from(sema.to_def(ast::TypeAlias::cast(node)?)?)),
        SyntaxKind::MACRO_RULES => {
            ItemId::Macro(MacroId::from(sema.to_macro_def(ast::Macro::MacroRules(ast::MacroRules::cast(node)?))?))
        }
        SyntaxKind::MACRO_DEF => {
            ItemId::Macro(MacroId::from(sema.to_macro_def(ast::Macro::MacroDef(ast::MacroDef::cast(node)?))?))
        }
        SyntaxKind::MODULE => {
            let module = ModuleId::from(sema.to_module_def(ast::Module::cast(node)?)?);
            return Some(blaze_items::key::module_key(db, module).0);
        }
        _ => return None,
    };
    Some(blaze_items::item_key(db, id).0)
}

/// The key of the module a file is.
fn file_key(sema: &Semantics<'_>, db: &Db, file: FileId) -> Option<ContentId> {
    let module = sema.file_to_module_def(file)?;
    Some(blaze_items::key::module_key(db, module.into()).0)
}