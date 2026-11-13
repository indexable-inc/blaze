//! What the crate root's attributes say that a by-piece pass reads of the whole crate
//! ([`crate_settings`]): the features `#![feature]` enables, the channel, the tools
//! `#![register_tool]` registers and where the root is. A piece's probe (`pieces::piece_clean`)
//! has no crate owner in its walk, so it is given these instead; the check reads the same from
//! its own walk and refuses to go on when the two differ (`checks::check`), so a probe is
//! never given other settings than the passes it stands for (keys-cover-every-input).
//!
//! Read off the root's outline (`hir_def::signatures::outline_root`): a body edit leaves the
//! outline, and so this, the same, and an edit of the root's items that leaves its attributes
//! the same runs it again to the same value, which backdates. Nothing in it is a place.
//!
//! Sans IO: what is read is the database's, through its queries.

#![blaze::sans_io]

use std::ops::ControlFlow;

use base_db::{Db, EditionedFileId};
use blaze_mem::{Tag, frame};
use either::Either;
use hir::{HirFileId, InFile};
use syntax::view::{self as ast, AstNode as _};

use crate::{
    checks::{attrs, features},
    cx::{Cx, Files},
    lints::syntax::attrs::{Attr, AttrKind, Owner, Target},
};

/// The settings of a crate a piece's probe is given ([`crate_settings`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CrateSettings {
    /// The crate root's file: where an inner attribute written elsewhere is told to go.
    pub(crate) root: Option<EditionedFileId>,
    /// What `#![feature]` enables, as `features::configured` takes it (a removed feature left
    /// out), the language features first, each in the order written.
    pub(crate) features: Box<[Box<str>]>,
    /// `features::Features::nightly`.
    pub(crate) nightly: bool,
    /// `features::Features::crate_nightly`.
    pub(crate) crate_nightly: bool,
    /// What `#![register_tool(..)]` registers, in the order written.
    pub(crate) tools: Box<[Box<str>]>,
}

impl CrateSettings {
    /// The settings of a crate whose root is `root`, whose features are `features` and whose
    /// registered tools are `tools`: what the check reads off its own walk, and what
    /// [`crate_settings`] reads off the root's outline.
    pub(crate) fn of_parts(
        root: Option<EditionedFileId>,
        features: &features::Features,
        tools: &[String],
    ) -> CrateSettings {
        CrateSettings {
            root,
            // alloc: a query's value, once per crate, one entry a feature
            features: features.names_in_stable_order().map(Box::from).collect(),
            nightly: features.nightly,
            crate_nightly: features.crate_nightly,
            // alloc: as above, one entry a tool
            tools: tools.iter().map(|it| Box::from(it.as_str())).collect(),
        }
    }

    /// Whether `#![feature]` enables `name`.
    pub(crate) fn enabled(&self, name: &str) -> bool {
        self.features.iter().any(|it| &**it == name)
    }
}

/// The [`CrateSettings`] of `krate`, read off its root's outline: the root's inner attributes
/// as `cfg` and `cfg_attr` leave them, made the crate's owner as the check's walk makes it
/// (`lints::syntax::attrs::owners`), then read by the check's own readers of it
/// (`features::configured`, `attrs::registered_tools`). What they say of a malformed
/// attribute is the check's to give, at the crate root, and is dropped here.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_settings(db: &Db, krate: base_db::Crate) -> CrateSettings {
    frame::task(Tag::Diagnostics, |frame| {
        let cx = Cx::new(db, hir::Crate::from(krate), Files::unnamed(), frame);
        let root = cx.krate.root_module(db).as_source_file_id(db);
        let owner = root.map(|file| root_owner(&cx, file));
        let owners = owner.as_slice();
        let (features, _) = features::configured(&cx, owners, &features::Flags::default());
        let tools = attrs::registered_tools(&cx, owners);
        #[cfg(test)]
        let tools = planted_tools(db, tools);
        CrateSettings::of_parts(root, &features, &tools)
    })
}

/// `tools`, or none when the tests planted [`Plant::SettingsWithoutTools`] in `db`.
///
/// [`Plant::SettingsWithoutTools`]: crate::pieces::tests::Plant::SettingsWithoutTools
#[cfg(test)]
fn planted_tools(db: &Db, mut tools: Vec<String>) -> Vec<String> {
    use crate::pieces::tests::{Plant, planted};
    if planted(db) == Some(Plant::SettingsWithoutTools) {
        tools.clear();
    }
    tools
}

/// The crate's owner of its root's outline: the root's inner attributes and its outer ones
/// (none, of a file), as `cfg` and `cfg_attr` leave them, a doc comment left out (no reader of
/// the settings reads one).
fn root_owner<'db>(cx: &Cx<'_, 'db>, file: EditionedFileId) -> Owner<'db> {
    let db = cx.db;
    let root = hir_def::signatures::outline_root(db, file).syntax();
    // alloc: setup, once per crate, the root's attributes
    let mut attrs = Vec::new();
    outline_attrs(cx, HirFileId::from(file), root, &mut attrs);
    // `-Z crate-attr`'s, each a file of its own, after the root's own inner attributes, as the
    // walk's crate owner has them (`lints::syntax::attrs::owners`).
    for &applied in base_db::applied_crate_attr_files(db, cx.krate.into()) {
        let source = applied.parse(db).view();
        outline_attrs(cx, HirFileId::from(applied), source.syntax(), &mut attrs);
    }
    Owner { node: InFile::new(HirFileId::from(file), root), target: Target::Crate, attrs, buffered_at: None }
}

/// The attributes `root` (the root of file `file_id`) holds, as `cfg` and `cfg_attr` leave
/// them, a doc comment left out, pushed to `attrs`.
fn outline_attrs<'db>(cx: &Cx<'_, 'db>, file_id: HirFileId, root: ast::Node<'db>, attrs: &mut Vec<Attr<'db>>) {
    let db = cx.db;
    if let Some(owner) = ast::AnyHasAttrs::cast(root) {
        let written = ast::attrs_with_doc_including_inner(&owner).map(|it| match it {
            ast::AnyAttr::Attr(it) => Either::Left(it),
            ast::AnyAttr::DocComment(it) => Either::Right(it),
        });
        let cfg = cx.krate.cfg(db);
        hir_expand::attrs::expand_cfg_attr_with_doc_comments_view::<_, ()>(
            written,
            || cfg,
            |it| {
                match it {
                    Either::Left((ast::Meta::CfgMeta(_), _)) | Either::Right(_) => {}
                    Either::Left((meta, written)) => {
                        let unsafety = meta.syntax().parent().and_then(ast::UnsafeMeta::cast);
                        let top = written.meta();
                        let from_cfg_attr = matches!(top, Some(ast::Meta::CfgAttrMeta(_)))
                            || matches!(top, Some(ast::Meta::UnsafeMeta(it)) if matches!(it.meta(), Some(ast::Meta::CfgAttrMeta(_))));
                        attrs.push(Attr {
                            file: file_id,
                            kind: AttrKind::Meta { meta, written, unsafety, from_cfg_attr },
                            inner: written.kind().is_inner(),
                        });
                    }
                }
                ControlFlow::Continue(())
            },
        );
    }
}
