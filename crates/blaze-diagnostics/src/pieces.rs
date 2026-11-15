//! A crate's check by pieces (plan 12, E1): a file's outline, its items with every body that
//! is its own span anchor cut out (`hir_def::signatures::file_outline`), or one such body. A
//! group of passes keeps, per piece, one bit: whether it finds nothing there
//! ([`piece_clean`]), run on the piece's own position-free tree in a probe of the piece
//! ([`Cx::piece_probe`]), so a body edit leaves every other piece's memo verified, where a
//! memo of a pass over the live tree reads the file's parse and runs again at every edit of
//! the file. The check sweeps every piece's memo ([`unclean`]) and runs the group's passes in
//! its own context only on the pieces not found clean ([`Cx::scoped`]).
//!
//! What it promises: a piece is clean only when the group's passes, run by the build on the
//! whole crate, find nothing in it. A read the piece's key does not cover (anything asked of
//! [`Cx::vsema`], a span, a place, the live tree, a macro's expansion, an `#[expect]`, the
//! build's flags or tables) stops the probe and leaves the piece unclean: slower, never wrong,
//! and counted by its reason in the span `crate pieces sweep`. A group whose passes are not
//! yet run in a probe ([`Group::converted`]) leaves every piece unclean, counted as
//! `unconverted`.
//!
//! Sans IO: what is read is the database's, through its queries.

#![blaze::sans_io]

#[cfg(test)]
pub(crate) mod tests;

use std::{alloc::Global, panic::AssertUnwindSafe};

use base_db::{Db, EditionedFileId};
use blaze_mem::{Frame, Tag, frame};
use hir::HirFileId;
use hir_def::DefWithBodyId;
use span::FileId;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::{Beyond, Cx, Unwound},
    lints::{CommandLine, registry::LintSet},
};

/// One piece of a crate's syntax a by-piece pass is run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Piece {
    /// The file's outline: its tree with each cut body left out.
    Outline(EditionedFileId),
    /// One body the outline cuts out, by its owner.
    Body(DefWithBodyId),
}

/// A set of passes by what they read (plan 12 section 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Group {
    /// The piece's syntax, the crate's cfg, features and edition: the syntax lints.
    Syntax,
    /// The item and its crate's names.
    Items,
    /// Who uses what, crate-wide.
    Uses,
}

impl Group {
    /// Every group, in the order their passes run.
    pub(crate) const ALL: [Group; 3] = [Group::Syntax, Group::Items, Group::Uses];

    /// The group's name in the span `crate pieces sweep`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Group::Syntax => "syntax",
            Group::Items => "items",
            Group::Uses => "uses",
        }
    }

    /// Whether the group's passes are run in a probe: of the syntax group, the syntax lints
    /// (`lints::syntax::lints`) and the passes of `checks::check` that read a node's own syntax
    /// and the crate's settings (`checks::syntax_clean`: `macros`, `feature expansion`, `ast
    /// validation`, `feature gates`, `attributes`, `checked attributes`), given the crate
    /// root's settings (`checks::settings::crate_settings`) in place of the crate owner a probe's
    /// walk does not have. The items group asks [`Cx::vsema`] and the crate's tables in every
    /// pass, and the uses group is read crate-wide until its passes fold the pieces' uses (plan
    /// 12 E2): each of their pieces is unclean.
    pub(crate) const fn converted(self) -> bool {
        matches!(self, Group::Syntax)
    }
}

/// What a piece's memo is asked under beyond the database: its key beside the piece, so a memo
/// is never read for other passes or flags than it ran with (keys-cover-every-input).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PieceGate {
    pub(crate) group: Group,
    /// The crate checked: a file of two crates is a piece of each.
    pub(crate) krate: base_db::Crate,
    /// The lints that may fire anywhere in the crate ([`Cx::lint_may_fire`]).
    pub(crate) may_fire: LintSet,
    /// Whether rustc makes code of the crate (`CommandLine::codegen`).
    pub(crate) codegen: bool,
    /// Whether the flags check cfgs (`--check-cfg`): the probe is not given the flags, so a
    /// `cfg` in a piece is then beyond its key.
    pub(crate) check_cfg: bool,
}

impl PieceGate {
    /// The gate of `group` in the check of `cx` with the flags `cmd`.
    pub(crate) fn of(cx: &Cx<'_, '_>, group: Group, cmd: &CommandLine) -> PieceGate {
        PieceGate {
            group,
            krate: cx.krate.base(),
            may_fire: cx.may_fire_set(),
            codegen: cmd.codegen,
            check_cfg: cmd.check_cfg.is_given(),
        }
    }
}

/// What a piece's probe found ([`piece_clean`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Verdict {
    /// The passes find nothing in the piece.
    Clean,
    /// A pass found something with no read beyond the key.
    Found,
    /// The probe was asked something its key does not cover, the first reason kept. A pass
    /// that finds something asks for a span first, so most findings are counted as `Place`.
    Beyond(Beyond),
}

/// The pieces of a crate in the walk's order, with where each body is cut in the live file.
pub(crate) struct Pieces<'f> {
    /// Per file of `Walk::of`'s module order, its outline, then its cut bodies by where they
    /// start. A file one of whose cut bodies has no owner the crate knows has no piece, and is
    /// walked whole.
    pub(crate) list: Vec<Piece, &'f Frame>,
    /// Of each of `list`, by index, the live range its body is cut at; none of an outline.
    pub(crate) cuts: Vec<Option<TextRange>, &'f Frame>,
    /// The files that declare a module whose file has an inner attribute the check reads as
    /// the `mod` item's (`checks::module_attrs_elsewhere`): such a file's outline is never
    /// clean, so the outline of the file that declares it is walked with it ([`unclean`]).
    pub(crate) declarers: Vec<EditionedFileId, &'f Frame>,
}

/// The pieces of `cx.krate` ([`Pieces`]), made in the check's frame from the live files: no
/// query of their own, as the ranges move with every edit.
pub(crate) fn pieces<'f>(cx: &Cx<'f, '_>) -> Pieces<'f> {
    let db = cx.db;
    let frame = cx.frame();
    let mut owners: blaze_hash::HashMap<(HirFileId, span::ErasedFileAstId), DefWithBodyId, Global> =
        blaze_hash::HashMap::new_in(Global); // alloc: setup, once per check, one entry a body
    for owner in hir_def::nameres::demand::crate_body_owners(db, cx.krate.base()) {
        let at = owner.erased_ast_id(db);
        owners.insert((at.file_id, at.value), owner);
    }
    let mut list = Vec::new_in(frame);
    let mut cuts = Vec::new_in(frame);
    let mut declarers = Vec::new_in(frame);
    let mut seen: blaze_hash::HashSet<EditionedFileId, Global> = blaze_hash::HashSet::new_in(Global); // alloc: setup, one entry a file
    let mut defs = Vec::new_in(frame);
    for module in crate::checks::modules(cx) {
        let Some(file) = module.as_source_file_id(db) else { continue };
        if !seen.insert(file) {
            continue;
        }
        if crate::checks::module_attrs_elsewhere(db, file)
            && let Some(declared) = module.declaration_source_range(db)
        {
            declarers.push(declared.original_node_file_range_rooted(db).file_id);
        }
        let file_id = HirFileId::from(file);
        let cut = hir_def::signatures::cut_bodies(db, file);
        defs.clear();
        let owned = cut.iter().all(|(_, anchor)| match owners.get(&(file_id, *anchor)) {
            Some(&def) => {
                defs.push(def);
                true
            }
            None => false,
        });
        if !owned {
            continue;
        }
        list.push(Piece::Outline(file));
        cuts.push(None);
        for (&(range, _), &def) in cut.iter().zip(defs.iter()) {
            list.push(Piece::Body(def));
            cuts.push(Some(range));
        }
    }
    Pieces { list, cuts, declarers }
}

/// The file a piece is of and its own tree: the outline's root, or the body's node in the
/// body's own syntax (`hir_def::signatures::cut_body_syntax`); of a const's or a static's
/// body, its item there. None for a body whose syntax has no cut body (the probe is then
/// beyond its key).
///
/// Why a const's or static's item: the outline holds an empty block in place of each cut
/// body, so a pass at the item that reads its value (`unused_braces` and `unused_parens` of
/// `const C: u32 = { 1 + 1 };`, `lints::syntax::delims`) finds nothing in the outline, and
/// the body alone has no item to come to. Walked from its item, the body's probe comes to the
/// item with its value; what it finds again of the item's signature only leaves the body
/// unclean too. A fn's item is not walked: the passes at a fn read only whether it has a body,
/// which the outline's empty block says, and of a method a probe from its item would not know
/// the `impl` it is in (every `&self` would be refused as a free fn's).
pub(crate) fn piece_root(db: &Db, piece: Piece) -> Option<(HirFileId, ast::Node<'_>)> {
    match piece {
        Piece::Outline(file) => {
            Some((HirFileId::from(file), hir_def::signatures::outline_root(db, file).syntax()))
        }
        Piece::Body(def) => {
            let file = def.erased_ast_id(db).file_id;
            let body = hir_def::signatures::cut_body_syntax(db, def)?;
            let root = match def {
                DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) => {
                    body.parent().expect("a const's or a static's value is in its item")
                }
                _ => body,
            };
            Some((file, root))
        }
    }
}

/// Whether the passes of `gate.group` find nothing in `piece`: run in [`Cx::piece_probe`],
/// whose first read beyond the key unwinds ([`Unwound`], `hir::view::Asked`) and is caught
/// here, every other panic going on. One small value, kept by the database, so the next
/// check verifies it where this one ran it.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn piece_clean(db: &Db, piece: Piece, gate: PieceGate) -> Verdict {
    #[cfg(test)]
    tests::PROBED.lock().push((db.nonce_and_revision().0, piece, gate.group));
    #[cfg(test)]
    tests::plant_reads(db, piece);
    if !gate.group.converted() {
        return Verdict::Beyond(Beyond::Other);
    }
    let krate = hir::Crate::from(gate.krate);
    frame::task(Tag::Diagnostics, |frame| {
        let cx = Cx::piece_probe(db, krate, frame, gate.may_fire, piece, gate.check_cfg);
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
            crate::checks::syntax_clean(&cx)
                && crate::lints::syntax::lints(&cx, &CommandLine::NONE).is_empty()
        }));
        match ran {
            Ok(true) => match cx.beyond_why() {
                None => Verdict::Clean,
                Some(why) => Verdict::Beyond(why),
            },
            Ok(false) => Verdict::Found,
            Err(payload) if payload.is::<Unwound>() || payload.is::<hir::view::Asked>() => {
                Verdict::Beyond(cx.beyond_why().expect("a probe that unwound noted why"))
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// How many pieces a sweep left unclean, by why.
#[derive(Default)]
struct Counts {
    found: usize,
    semantics: usize,
    expansion: usize,
    place: usize,
    expect: usize,
    other: usize,
}

/// Of `pieces`, those the passes of `gate` are not found to find nothing in, in order: every
/// piece's memo ([`piece_clean`]) asked at once on the database's workers ([`Db::run_all`]),
/// then read here in order, as the body lints' sweep does. The crate root's outline is never
/// clean: its attributes are the crate's, read by passes crate-wide; nor is the outline of a
/// file that declares a module whose inner attributes the check reads with the `mod` item
/// ([`Pieces::declarers`]), walked with that module's file, never clean either. Span `crate pieces
/// sweep`: `group`, `pieces`, and once swept `unclean`, `found`, `by_semantics`,
/// `by_expansion`, `by_place`, `by_expect`, `by_other` (the first reason each unclean piece's
/// probe stopped at) and `unconverted` (a group not run in a probe, every piece unclean).
pub(crate) fn unclean<'f>(cx: &Cx<'f, '_>, pieces: &Pieces<'f>, gate: PieceGate) -> Vec<Piece, &'f Frame> {
    let list: &[Piece] = &pieces.list;
    let sweeping = tracing::info_span!(
        target: "blaze",
        "crate pieces sweep",
        group = gate.group.name(),
        pieces = list.len(),
        unclean = tracing::field::Empty,
        found = tracing::field::Empty,
        by_semantics = tracing::field::Empty,
        by_expansion = tracing::field::Empty,
        by_place = tracing::field::Empty,
        by_expect = tracing::field::Empty,
        by_other = tracing::field::Empty,
        unconverted = tracing::field::Empty,
    )
    .entered();
    let mut out = Vec::with_capacity_in(list.len(), cx.frame());
    if !gate.group.converted() {
        out.extend_from_slice(list);
        sweeping.record("unconverted", list.len());
        sweeping.record("unclean", list.len());
        return out;
    }
    let db = cx.db;
    let root = cx.krate.root_file(db);
    db.run_all(Tag::Diagnostics, list.len(), |db, at| {
        if probed(db, root, list[at]) {
            piece_clean(db, list[at], gate);
        }
    });
    let mut counts = Counts::default();
    for &piece in list {
        let declares = matches!(piece, Piece::Outline(file) if pieces.declarers.contains(&file));
        let verdict = match probed(db, root, piece) && !declares {
            true => piece_clean(db, piece, gate),
            false => Verdict::Beyond(Beyond::Other),
        };
        match verdict {
            Verdict::Clean => continue,
            Verdict::Found => counts.found += 1,
            Verdict::Beyond(Beyond::Semantics) => counts.semantics += 1,
            Verdict::Beyond(Beyond::Expansion) => counts.expansion += 1,
            Verdict::Beyond(Beyond::Place) => counts.place += 1,
            Verdict::Beyond(Beyond::Expect) => counts.expect += 1,
            Verdict::Beyond(Beyond::Other) => counts.other += 1,
        }
        out.push(piece);
    }
    sweeping.record("unclean", out.len());
    sweeping.record("found", counts.found);
    sweeping.record("by_semantics", counts.semantics);
    sweeping.record("by_expansion", counts.expansion);
    sweeping.record("by_place", counts.place);
    sweeping.record("by_expect", counts.expect);
    sweeping.record("by_other", counts.other);
    sweeping.record("unconverted", 0usize);
    out
}

/// Whether `piece` is probed: every piece but the outline of the crate root `root`.
fn probed(db: &Db, root: FileId, piece: Piece) -> bool {
    !matches!(piece, Piece::Outline(file) if file.file_id(db) == root)
}

/// The pieces a scoped context's walk visits ([`Cx::scoped`]).
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub(crate) pieces: &'a Pieces<'a>,
    pub(crate) unclean: &'a [Piece],
}

/// Of a file, what a scoped walk visits ([`FileScopes::walked`]).
pub(crate) enum Walked<'s> {
    /// Nothing: every piece of the file is clean.
    Not,
    /// The file, but the subtree at each of these ranges (each a clean body's cut, in order).
    Skipping(&'s [TextRange]),
}

/// [`Walked`] of each file of a [`Scope`].
pub(crate) struct FileScopes {
    /// Of each file with pieces: none when nothing of it is walked, else the clean bodies'
    /// ranges.
    files: blaze_hash::HashMap<EditionedFileId, Option<Vec<TextRange>>, Global>,
}

impl FileScopes {
    pub(crate) fn of(scope: Scope<'_>, db: &Db) -> FileScopes {
        let _ = db;
        #[cfg(test)]
        let planted = tests::planted(db) == Some(tests::Plant::CleanOutlineSkipsBodies);
        #[cfg(not(test))]
        let planted = false;
        // alloc: setup, once per scoped walk, one entry an unclean piece
        let unclean = blaze_hash::HashSet::from_iter_in(scope.unclean.iter().copied(), Global);
        let list = &scope.pieces.list;
        // alloc: setup, once per scoped walk, one entry a file
        let mut files = blaze_hash::HashMap::new_in(Global);
        let mut at = 0;
        while at < list.len() {
            let Piece::Outline(file) = list[at] else {
                unreachable!("{:?}: the pieces are each file's outline, then its bodies", list[at])
            };
            let mut end = at + 1;
            while end < list.len() && matches!(list[end], Piece::Body(_)) {
                end += 1;
            }
            let bodies = at + 1..end;
            let walked = unclean.contains(&list[at])
                || (!planted && bodies.clone().any(|it| unclean.contains(&list[it])));
            let skips = walked.then(|| {
                bodies
                    .filter(|&it| !unclean.contains(&list[it]))
                    .map(|it| scope.pieces.cuts[it].expect("a body's piece has its cut"))
                    // alloc: setup, the clean bodies of a walked file
                    .collect()
            });
            files.insert(file, skips);
            at = end;
        }
        FileScopes { files }
    }

    /// What the walk visits of `file`: a file with no pieces whole.
    pub(crate) fn walked(&self, file: EditionedFileId) -> Walked<'_> {
        match self.files.get(&file) {
            None => Walked::Skipping(&[]),
            Some(None) => Walked::Not,
            Some(Some(skips)) => Walked::Skipping(skips),
        }
    }
}

/// rustc's builtin attributes that expand to nothing, by their one name, and the lint levels:
/// an item with only these is no attribute macro's input. `expect` is one, read as
/// [`Beyond::Expect`] where its attribute is walked ([`probe_attr`]).
const INERT: &[&str] = &[
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "doc",
    "inline",
    "cold",
    "must_use",
    "deprecated",
    "repr",
    "non_exhaustive",
    "track_caller",
    "no_mangle",
    "export_name",
    "link_section",
    "link_name",
    "link",
    "used",
    "path",
    "macro_use",
    "macro_export",
    "automatically_derived",
    "feature",
    "no_std",
    "no_core",
    "no_main",
    "no_implicit_prelude",
    "crate_type",
    "crate_name",
    "recursion_limit",
    "type_length_limit",
    "windows_subsystem",
    "target_feature",
    "naked",
    "ignore",
    "should_panic",
];

/// Whether `attr` is a builtin attribute that expands to nothing ([`INERT`], `cfg`, a
/// `rustfmt::` or `clippy::` tool attribute): an item whose attributes all are is no
/// attribute macro's input, which a probe can tell without resolving a name. A `cfg_attr`
/// may hold a macro, so is not.
pub(crate) fn inert(attr: ast::Attr<'_>) -> bool {
    let Some(meta) = attr.meta() else { return false };
    if matches!(meta, ast::Meta::CfgMeta(_)) {
        return true;
    }
    let Some(path) = meta.path() else { return false };
    if let Some(name) = path.as_single_name_ref() {
        return INERT.contains(&name.text());
    }
    path.qualifier()
        .and_then(|it| it.as_single_name_ref())
        .is_some_and(|tool| matches!(tool.text(), "rustfmt" | "clippy"))
}

/// Of a piece's probe, an attribute the walk comes to: an `#[expect]` (one written in a
/// `cfg_attr` included) is beyond the key, and so is any `cfg` when the build's flags check
/// cfgs. Conservative, by the attribute's text.
pub(crate) fn probe_attr(cx: &Cx<'_, '_>, attr: ast::Node<'_>) {
    let text = attr.text();
    if text.contains("expect") {
        cx.note_beyond(Beyond::Expect);
    }
    if cx.probe_checks_cfg() && text.contains("cfg") {
        cx.note_beyond(Beyond::Other);
    }
}

/// Of a piece's probe, whether `node`, a `mod m;`, is declared in a block (a fn's body, a
/// const's): the module of a block's def map, whose file only the expansion of its block finds.
pub(crate) fn in_block(node: ast::Node<'_>, piece: Piece) -> bool {
    matches!(piece, Piece::Body(_))
        || std::iter::successors(node.parent(), |it| it.parent())
            .any(|it| it.kind() == SyntaxKind::BLOCK_EXPR)
}
