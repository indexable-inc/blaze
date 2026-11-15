//! What a diagnostic is lowered with: the crate it is of, the names rustc gives files, spans
//! as rustc gives them (in the file a macro was called in, or in the macro's own definition
//! when it is the crate's), the text of what a span covers, rustc's printing of types and
//! paths, and the keys of the facts a diagnostic read. It also holds what every pass of one
//! check shares, each made the first time a pass asks: the walk of the crate's syntax, the
//! HIR lint levels, and the set of lints that may fire.

use std::alloc::Global;
#[expect(
    clippy::disallowed_types,
    reason = "OnceCell: memos of one check, dropped with it, not global state"
)]
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashMap,
    ops::ControlFlow,
};

use base_db::Db;
use blaze_items::{Fact, ItemId, ItemKey, NameHash, SimplifiedType};
use blaze_mem::Frame;
use hir::{DisplayTarget, HirFileId, InFile, MacroCallId};
use hir_expand::{MacroCallKind, MacroDefKind};

use smallvec::SmallVec;
use span::{Edition, FileId};
use syntax::{
    AstNode, AstPtr, NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self, AstNode as _, HasAttrs as _},
};

use crate::{
    lints::{
        Effective, ItemWalk, LevelMap,
        registry::{LintId, LintSet},
        syntax::Walk,
    },
    model::{Expansion, MacroKind, Span},
    pieces::{Piece, Scope},
};

/// What a probe was asked that its key does not cover, the first such ask kept
/// ([`Cx::beyond_why`]): what a by-piece check counts a piece unclean by
/// (`crate::pieces::unclean`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Beyond {
    /// [`Cx::vsema`], whose trees are the live files' (`hir::view::Semantics::asked`).
    Semantics,
    /// A macro call, an attribute macro, a derive or a module in a block: an expansion, which
    /// a piece names only through its file's position-bound ids.
    Expansion,
    /// A span, a place, a snippet, a file's text or name, a root or a view of the live tree.
    Place,
    /// An `#[expect]`: fulfilled by any lint under it, which only the whole check knows.
    Expect,
    /// Anything else of the build: the lint flags, a level, the crate's items or effective
    /// visibilities, the printing of a type or a path, a `cfg` checked by `--check-cfg`.
    Other,
}

/// What a probe of a piece ([`Cx::piece_probe`]) unwinds with when it is asked for something
/// its key does not cover, the reason kept in the probe ([`Cx::beyond_why`]): the probe's
/// query catches it and says the piece is not clean. No panic hook runs (`resume_unwind`).
#[derive(Debug)]
pub(crate) struct Unwound;

/// Which probe a context is, if any.
#[derive(Clone, Copy, Debug)]
enum Probe {
    /// A build's context.
    None,
    /// [`Cx::probe`]: a body's lints, which notes what is beyond its key and goes on with a
    /// stand-in.
    Body,
    /// [`Cx::piece_probe`]: one piece's passes, which stops at the first read beyond its key.
    Piece {
        piece: Piece,
        /// The build's lint flags check cfgs (`--check-cfg`), which the probe is not given:
        /// a `cfg` in the piece is beyond its key.
        check_cfg: bool,
    },
}

/// The names files are shown by: the front end's path of each file, and the root of the
/// workspace they are named from.
#[derive(Clone, Copy)]
pub struct Files<'a> {
    /// None for [`Files::unnamed`].
    paths: Option<&'a HashMap<FileId, String>>,
    root: &'a str,
}

impl<'a> Files<'a> {
    /// `paths` named from `root`; a `root` of `""` keeps each relative path whole, as the tests'
    /// paths, already names, are.
    pub fn new(paths: &'a HashMap<FileId, String>, root: &'a str) -> Files<'a> {
        Files { paths: Some(paths), root }
    }

    /// No names: each file is `<file N>`, one name a file, so a span still says which file it
    /// is in ([`Cx::file_of`]) and nothing more. What a query that has no build's names checks
    /// with ([`Cx::probe`]): nothing it gives is written.
    pub(crate) const fn unnamed() -> Files<'static> {
        Files { paths: None, root: "" }
    }

    /// The front end's path of `file`, if it has one.
    fn path(&self, file: FileId) -> Option<&'a String> {
        self.paths.and_then(|it| it.get(&file))
    }

    /// The name rustc gives `file` in a diagnostic, by [`Files::relative`]; a file the front end
    /// has no path for is `<file N>` (a crate attribute's is named by [`Cx::file_name`]).
    pub fn name(&self, file: FileId) -> String {
        match self.path(file) {
            // alloc: a span's file name, made only as a diagnostic is built
            Some(path) => Files::relative(path, self.root).to_owned(),
            // alloc: as above
            None => format!("<file {}>", file.index()),
        }
    }

    /// The name a span of `file` gives it, `name` the name of its path ([`Files::name`],
    /// [`Files::relative`]): `name` itself when the front end's text of `file` is the file at
    /// that path, else `name` with the content ID of the text ([`crate::emit::name_with_id`]),
    /// as for a crate root blaze appended a module to (a test harness, a wasm proc macro's
    /// glue), an alias of the file on disk shown by its path. The one place a span's file is
    /// named for every check and lint, so the text a span is placed in is the text it was made
    /// against, never the file on disk read by its name (keys-cover-every-input).
    pub fn of_text(db: &Db, file: FileId, name: String) -> String {
        if !db.has_file_text(file) {
            return name;
        }
        let text = db.file_text(file);
        match text.appended_root_module(db) {
            None => name,
            Some(_) => crate::emit::name_with_id(&name, text.content(db)),
        }
    }

    /// The name rustc gives `path`: from `root` when it is below it, as cargo runs rustc there,
    /// else the whole path, as `root` itself keeps it. A root of `""` is none, and keeps every
    /// path whole; under a root of `/` cargo gives rustc paths from `/`, without their `/`.
    pub fn relative<'p>(path: &'p str, root: &str) -> &'p str {
        if root.is_empty() {
            return path;
        }
        let root = root.trim_end_matches('/');
        match path.strip_prefix(root).and_then(|rest| rest.strip_prefix('/')) {
            Some(rest) if !rest.is_empty() => rest,
            _ => path,
        }
    }
}

/// A diagnostic of a crate being lowered.
#[expect(
    clippy::disallowed_types,
    reason = "OnceCell: memos of one check, dropped with it, not global state"
)]
pub struct Cx<'a, 'db> {
    /// The `Semantics` of views. Every view handed to it is of a tree it gave out: roots are
    /// taken through [`Cx::root`], never `HirFileId::root`.
    pub vsema: hir::view::Semantics<'db>,
    pub db: &'db Db,
    pub krate: hir::Crate,
    pub edition: Edition,
    pub display_target: DisplayTarget,
    files: Files<'a>,
    /// The check's task frame: what a pass builds for the crate (numberings, worklists,
    /// bitsets) is allocated here, `Vec<T, &Frame>`, and freed when the check returns.
    frame: &'a Frame,
    /// The lint flags the crate is checked with, for a lowering whose words depend on a
    /// lint's level; none is rustc's defaults.
    lints: Option<&'a crate::lints::CommandLine>,
    /// The file each name shown stands for, as spans were made.
    named: RefCell<blaze_hash::HashMap<String, FileId, Global>>,
    /// The paths types are printed by, made the first time one is printed.
    paths: OnceCell<crate::print::Paths<'a>>,
    /// The crate as rustc's early lint pass walks it, made the first time a pass asks.
    walk: OnceCell<Walk<'db>>,
    /// The crate's items as rustc's item passes visit them, made the first time a pass asks.
    items: OnceCell<ItemWalk<'db>>,
    /// The crate's effective visibilities, made the first time a pass asks.
    effective: OnceCell<Effective>,
    /// The HIR lint levels, made the first time a level is asked and filled in as levels are.
    levels: RefCell<Option<LevelMap<'a, 'db>>>,
    /// The lints that may fire anywhere in the crate, made the first time a pass asks.
    may_fire: OnceCell<LintSet>,
    /// Every trait, by the names of its items, made the first time an error asks which traits
    /// have an item of a name.
    traits: OnceCell<crate::lower::unresolved_method_call::TraitIndex>,
    /// Made by [`Cx::probe`] or [`Cx::piece_probe`]: a query's context, which has of the build
    /// only what the query's key says.
    probing: Probe,
    /// What a probe was first asked that its key does not cover ([`Cx::beyond`]).
    beyond: Cell<Option<Beyond>>,
    /// Made by [`Cx::scoped`]: the pieces the walk visits ([`Cx::walk`]).
    scope: Option<Scope<'a>>,
    /// The check's pieces ([`Cx::set_pieces`]), what a pass that folds the pieces' facts reads
    /// ([`Cx::pieces`]).
    pieces: OnceCell<&'a crate::pieces::Pieces<'a>>,
}

impl<'a, 'db> Cx<'a, 'db> {
    #[expect(
        clippy::disallowed_types,
        reason = "OnceCell: memos of one check, dropped with it, not global state"
    )]
    pub fn new(db: &'db Db, krate: hir::Crate, files: Files<'a>, frame: &'a Frame) -> Cx<'a, 'db> {
        Cx {
            vsema: hir::view::Semantics::new_within(db, within(krate)),
            db,
            krate,
            edition: krate.edition(db),
            display_target: krate.to_display_target(db),
            files,
            frame,
            lints: None,
            named: RefCell::new(blaze_hash::HashMap::new_in(Global)),
            paths: OnceCell::new(),
            walk: OnceCell::new(),
            items: OnceCell::new(),
            effective: OnceCell::new(),
            levels: RefCell::new(None),
            may_fire: OnceCell::new(),
            traits: OnceCell::new(),
            probing: Probe::None,
            beyond: Cell::new(None),
            scope: None,
            pieces: OnceCell::new(),
        }
    }

    /// The context a query checks one part of `krate` with (a body's lints,
    /// `lints::bodies::body_lints_clean`): it has no build's file names, lint flags or
    /// crate-wide tables, only `may_fire`, the lints that may fire in the crate, which the
    /// query's key holds. So what it is asked is the database's, which the engine tracks, or
    /// the key's; anything else it is asked (the lint flags, a level, the crate's walk, its
    /// items, its effective visibilities, the printing of a type or a path, which reads the
    /// names of the crate and its dependencies) is answered with a stand-in and noted
    /// ([`Cx::beyond`]), and the query then says nothing of what it found
    /// (keys-cover-every-input). Its files are [`Files::unnamed`].
    #[expect(
        clippy::disallowed_types,
        reason = "OnceCell: memos of one check, dropped with it, not global state"
    )]
    pub(crate) fn probe(
        db: &'db Db,
        krate: hir::Crate,
        frame: &'a Frame,
        may_fire: LintSet,
    ) -> Cx<'a, 'db> {
        Cx {
            may_fire: OnceCell::from(may_fire),
            probing: Probe::Body,
            ..Cx::new(db, krate, Files::unnamed(), frame)
        }
    }

    /// The context a piece's passes are run with (`crate::pieces::piece_clean`): as
    /// [`Cx::probe`], but its walk is the piece's own position-free tree ([`Cx::walk`],
    /// `Walk::of`), its [`Cx::vsema`] a probe (`hir::view::Semantics::new_probe`), and the
    /// first read beyond its key (a span or any read of the live tree, a macro's expansion,
    /// an `#[expect]`, the build's flags or tables) stops the run: it notes why and unwinds
    /// with [`Unwound`], so nothing stands in for what was asked. `check_cfg` is whether the
    /// build's flags check cfgs, which the probe is not given.
    #[expect(
        clippy::disallowed_types,
        reason = "OnceCell: memos of one check, dropped with it, not global state"
    )]
    pub(crate) fn piece_probe(
        db: &'db Db,
        krate: hir::Crate,
        frame: &'a Frame,
        may_fire: LintSet,
        piece: Piece,
        check_cfg: bool,
    ) -> Cx<'a, 'db> {
        Cx {
            vsema: hir::view::Semantics::new_probe(db, within(krate)),
            may_fire: OnceCell::from(may_fire),
            probing: Probe::Piece { piece, check_cfg },
            ..Cx::new(db, krate, Files::unnamed(), frame)
        }
    }

    /// The build's context whose walk ([`Cx::walk`]) visits only `unclean` of `pieces`: a file
    /// none of whose pieces is unclean is left out, and in a file that is walked the subtree of
    /// each clean body; a file with no pieces is walked whole. What else it is asked is the
    /// build's: its file names, its lint flags, and the lints that may fire, taken from this
    /// context's whole walk, never from the walk it visits.
    #[expect(
        clippy::disallowed_types,
        reason = "OnceCell: memos of one check, dropped with it, not global state"
    )]
    pub(crate) fn scoped<'s>(
        &'s self,
        pieces: &'s crate::pieces::Pieces<'_>,
        unclean: &'s [Piece],
    ) -> Cx<'s, 'db> {
        Cx {
            may_fire: OnceCell::from(self.may_fire_set()),
            lints: self.lints,
            scope: Some(Scope { pieces, unclean }),
            ..Cx::new(self.db, self.krate, self.files, self.frame)
        }
    }

    /// What the crate root's attributes say that a by-piece pass reads (the features, the
    /// channel, the registered tools, the root): the database's query, so a probe's key
    /// covers it.
    pub(crate) fn settings(&self) -> &'db crate::checks::settings::CrateSettings {
        crate::checks::settings::crate_settings(self.db, self.krate.base())
    }

    /// The piece this context is the probe of ([`Cx::piece_probe`]).
    pub(crate) fn probed_piece(&self) -> Option<Piece> {
        match self.probing {
            Probe::Piece { piece, .. } => Some(piece),
            Probe::None | Probe::Body => None,
        }
    }

    /// Of a piece's probe, whether the build's flags check cfgs ([`Cx::piece_probe`]).
    pub(crate) fn probe_checks_cfg(&self) -> bool {
        matches!(self.probing, Probe::Piece { check_cfg: true, .. })
    }

    /// Sets the check's pieces (`check::run`, once, right after they are made in the frame).
    pub(crate) fn set_pieces(&self, pieces: &'a crate::pieces::Pieces<'a>) {
        if self.pieces.set(pieces).is_err() {
            panic!("the pieces of the check of {} are set twice", self.crate_name(self.krate));
        }
    }

    /// The check's pieces, as `check::run` made them ([`Cx::set_pieces`]): a pass that folds
    /// the pieces' facts is never run without them.
    pub(crate) fn pieces(&self) -> &crate::pieces::Pieces<'a> {
        match self.pieces.get() {
            Some(pieces) => pieces,
            None => panic!(
                "the check of {} asked its pieces before `check::run` set them",
                self.crate_name(self.krate)
            ),
        }
    }

    /// The pieces this context's walk visits, of a context made by [`Cx::scoped`].
    pub(crate) fn scope(&self) -> Option<Scope<'a>> {
        self.scope
    }

    /// Whether a probe was asked for something its key does not cover: what it found, or did
    /// not find, may then differ from what the build's own context finds. Always false of a
    /// build's context.
    pub(crate) fn beyond(&self) -> bool {
        self.beyond.get().is_some() || self.vsema.asked()
    }

    /// What a probe was first asked that its key does not cover, if anything.
    pub(crate) fn beyond_why(&self) -> Option<Beyond> {
        self.beyond.get().or_else(|| self.vsema.asked().then_some(Beyond::Semantics))
    }

    /// Notes that `why`, something a probe's key does not cover, was asked: a body's probe
    /// keeps the first reason and goes on; a piece's keeps it and unwinds with [`Unwound`].
    /// Nothing of a build's context.
    #[inline]
    pub(crate) fn note_beyond(&self, why: Beyond) {
        match self.probing {
            Probe::None => {}
            Probe::Body => {
                if self.beyond.get().is_none() {
                    self.beyond.set(Some(why));
                }
            }
            Probe::Piece { .. } => {
                if self.beyond.get().is_none() {
                    self.beyond.set(Some(why));
                }
                std::panic::resume_unwind(Box::new(Unwound)); // alloc: an unwind's payload
            }
        }
    }

    /// Notes `why` of a piece's probe only: what a body's probe is given as the build's (a
    /// span, the live tree), which a piece's key does not cover.
    #[inline]
    fn piece_beyond(&self, why: Beyond) {
        if let Probe::Piece { .. } = self.probing {
            self.note_beyond(why);
        }
    }

    /// Notes that something a probe's key does not cover was asked. Nothing of a build's
    /// context.
    #[inline]
    fn asked_beyond(&self) {
        self.note_beyond(Beyond::Other);
    }

    /// The lints that may fire anywhere in the crate ([`Cx::lint_may_fire`]): what a probe of
    /// the crate is made with.
    pub(crate) fn may_fire_set(&self) -> LintSet {
        *self.lints_that_may_fire()
    }

    /// The check's task frame, for the crate-sized tables a pass builds.
    pub fn frame(&self) -> &'a Frame {
        self.frame
    }

    /// The names the check's files are shown by.
    pub fn files(&self) -> Files<'a> {
        self.files
    }

    /// What rustc's printing of paths and the import suggestions read: the crate's visible
    /// names and its declarations by name, built in the check's frame when first read.
    pub(crate) fn paths(&self) -> &crate::print::Paths<'a> {
        self.asked_beyond();
        self.paths.get_or_init(|| crate::print::Paths::new(self))
    }

    /// The context checking with the lint flags `lints`, before anything was asked of it.
    pub fn with_lints(self, lints: &'a crate::lints::CommandLine) -> Cx<'a, 'db> {
        assert!(
            self.walk.get().is_none()
                && self.levels.borrow().is_none()
                && self.may_fire.get().is_none(),
            "the lint flags are given before a level is asked"
        );
        Cx { lints: Some(lints), ..self }
    }

    /// The lint flags the crate is checked with: rustc's defaults when none were given.
    pub fn lint_flags(&self) -> &'a crate::lints::CommandLine {
        static NONE: crate::lints::CommandLine = crate::lints::CommandLine::NONE;
        self.asked_beyond();
        self.lints.unwrap_or(&NONE)
    }

    /// The crate as rustc's early lint pass walks it: made once per check, for every pass that
    /// walks the crate's syntax.
    /// Of a piece's probe it is the piece's own tree, which its key covers; of a context made
    /// by [`Cx::scoped`], the pieces it was made with.
    pub(crate) fn walk(&self) -> &Walk<'db> {
        if !matches!(self.probing, Probe::Piece { .. }) {
            self.asked_beyond();
        }
        self.walk.get_or_init(|| Walk::of(self))
    }

    /// The crate's items in the order rustc's item passes visit them: made once per check, for
    /// the item lints before the abort and after it.
    pub(crate) fn items(&self) -> &ItemWalk<'db> {
        self.asked_beyond();
        self.items.get_or_init(|| ItemWalk::of(self))
    }

    /// The crate's effective visibilities (rustc's `effective_visibilities`): made once per
    /// check, for the stability pass, the dead-code pass and the item lints after the abort.
    pub(crate) fn effective(&self) -> &Effective {
        self.asked_beyond();
        self.effective.get_or_init(|| Effective::of(self))
    }

    /// Whether the `cfg`s of `node`, an attribute owner, keep it in the crate, its `cfg_attr`s
    /// expanded. A node's attributes and doc comments are its first children, so one without
    /// either first has no attribute and costs one step; a doc comment is a node of its own.
    pub(crate) fn cfg_enabled(&self, node: view::Node<'_>) -> bool {
        if node.first_child().is_none_or(|it| !view::AnyAttr::can_cast(it.kind())) {
            return true;
        }
        let Some(owner) = view::AnyHasAttrs::cast(node) else { return true };
        let cfg = self.krate.cfg(self.db);
        let off = hir_expand::attrs::expand_cfg_attr_view(
            owner.attrs(),
            || cfg,
            |meta, _| {
                if let view::Meta::CfgMeta(meta) = meta
                    && let Some(predicate) = meta.cfg_predicate()
                    && cfg.check(&::cfg::CfgExpr::parse_from_view(predicate)) == Some(false)
                {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            },
        );
        off.is_none()
    }

    /// The level `lint` has at `at`, from the attributes there and around it and the lint
    /// flags, as rustc's HIR levels decide it (`unsafe_op_in_unsafe_fn`'s level changes E0133's
    /// words). The levels are rustc's `ShallowLintLevelMap`, kept for the check: asked again at
    /// a node, or at one whose set is known, it is one hash probe, with nothing allocated.
    pub fn lint_level(&self, lint: LintId, at: InFile<SyntaxNodePtr>) -> crate::lints::Level {
        let cmd = self.lint_flags();
        let mut levels = self.levels.borrow_mut();
        let levels = levels.get_or_insert_with(|| LevelMap::new(self, &cmd.lints));
        levels.level(lint, at)
    }

    /// Whether a diagnostic of `lint` may be shown anywhere in the crate: false when the lint
    /// flags and every lint attribute leave it allowed (rustc's `lints_that_dont_need_to_run`),
    /// so a pass that gives nothing but it can be skipped. The set is made once per check from
    /// one scan of the walk's attributes; after that this is one bit test.
    pub fn lint_may_fire(&self, lint: LintId) -> bool {
        self.lints_that_may_fire().contains(lint)
    }

    /// Whether a diagnostic of some lint of `lints` may be shown: [`Cx::lint_may_fire`] of a
    /// pass's lints at once, one AND per word.
    pub fn any_may_fire(&self, lints: &LintSet) -> bool {
        self.lints_that_may_fire().intersects(lints)
    }

    /// Every trait of every crate with, for each name, the traits that have an item of it:
    /// walked once a check, the first time an error asks.
    pub(crate) fn traits(&self) -> &crate::lower::unresolved_method_call::TraitIndex {
        self.piece_beyond(Beyond::Other);
        self.traits.get_or_init(|| crate::lower::unresolved_method_call::TraitIndex::new(self))
    }

    fn lints_that_may_fire(&self) -> &LintSet {
        self.may_fire.get_or_init(|| crate::lints::may_fire(self, self.lint_flags(), self.walk()))
    }

    /// Whether cargo runs the compile (rustc's `was_invoked_from_cargo`: `CARGO_CRATE_NAME` is
    /// set), as it is under blaze: some help then names `cargo add` and `Cargo.toml`.
    pub fn cargo(&self) -> bool {
        self.asked_beyond();
        self.lints.is_some_and(|it| it.check_cfg.cargo)
    }

    /// The name rustc shows `file` by. A crate attribute's file has no path: it is
    /// [`base_db::CRATE_ATTR_FILE_NAME`] with the file's text, which tells the attributes apart
    /// and is their source to the emitter ([`crate::emit::name_with_text`]). A file `include!`
    /// read from outside every source root has no path of the front end's: it is named by its
    /// path there ([`base_db::Files::included_source_path`]).
    pub fn file_name(&self, file: FileId) -> String {
        self.piece_beyond(Beyond::Place);
        let outside = || self.db.files().included_source_path(file);
        let name = match self.files.path(file).is_some() {
            true => Files::of_text(self.db, file, self.files.name(file)),
            // alloc: a span's file name, made only as a diagnostic is built
            false => match outside() {
                Some(path) => Files::relative(path.as_str(), self.files.root).to_owned(),
                None if base_db::crate_attr_owner(self.db, file).is_none() => self.files.name(file),
                None => {
                    crate::emit::name_with_text(base_db::CRATE_ATTR_FILE_NAME, self.file_text(file))
                }
            },
        };
        let mut named = self.named.borrow_mut();
        if !named.contains_key(name.as_str()) {
            named.insert(name.clone(), file); // alloc: once per file named
        }
        name
    }

    /// The file a span names, once a span of it was made.
    pub fn file_of(&self, span: &Span) -> Option<FileId> {
        self.named.borrow().get(&span.file).copied()
    }

    pub fn file_text(&self, file: FileId) -> &'db str {
        self.piece_beyond(Beyond::Place);
        self.db.file_text(file).text(self.db)
    }

    /// rustc's `tcx.crate_name`: the crate's name (`--crate-name`, [`base_db::Crate::rust_name`]),
    /// else, as rustc names a crate built with none, its root file's stem with `-` as `_`. Not
    /// [`base_db::Crate::named`]: a diagnostic says what rustc says of the file it was given.
    pub(crate) fn crate_name(&self, krate: hir::Crate) -> String {
        match krate.base().rust_name(self.db) {
            Ok(name) => name.as_str().to_owned(), // alloc: the error's text, for an error hir found
            Err(_) => {
                let path = self.file_name(krate.root_file(self.db));
                let stem = path.rsplit('/').next().unwrap_or(&path);
                stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
            }
        }
    }

    /// `range` of `file`, as a span with no expansion.
    pub fn span_in_file(&self, file: FileId, range: TextRange) -> Span {
        Span::new(&self.file_name(file), range)
    }

    /// The source text a span covers.
    pub fn snippet(&self, span: &Span) -> Option<&'db str> {
        self.piece_beyond(Beyond::Place);
        let file = self.file_of(span)?;
        self.file_text(file).get(span.lo as usize..span.hi as usize)
    }

    /// The span rustc gives `range`: where it is written when it is in a file or in what a
    /// macro call was handed; where the crate's own macro's definition writes it, inside that
    /// macro's expansion; else the call of the outermost macro of another crate it came from,
    /// inside that macro's expansion.
    pub fn span(&self, range: InFile<TextRange>) -> Span {
        self.piece_beyond(Beyond::Place);
        let Some(call) = range.file_id.macro_file() else {
            let file = range.file_id.file_id().expect("a file that is not a macro's");
            return self.span_in_file(file.file_id(self.db), range.value);
        };
        let mapped = range.original_node_file_range_opt(self.db);
        if let Some((at, ctxt)) = mapped
            && ctxt.is_root()
            && !self.written_by_derive(call, at)
        {
            return self.span_in_file(at.file_id.file_id(self.db), at.range);
        }
        let expansion = self.expansion_of(call).map(Box::new);
        let loc = call.loc(self.db);
        let local = hir::Crate::from(loc.def.krate) == self.krate;
        let at = match mapped {
            Some((at, _)) if local => {
                Span::new(&self.file_name(at.file_id.file_id(self.db)), at.range)
            }
            _ => match &expansion {
                // clone: the call site's span, on the path of a diagnostic
                Some(it) => Span { expansion: None, ..it.call_site.clone() },
                None => {
                    let at = loc.kind.original_call_range(self.db, loc.krate);
                    Span::new(&self.file_name(at.file_id.file_id(self.db)), at.range)
                }
            },
        };
        Span { expansion, ..at }
    }

    /// The file and range [`Cx::span`] gives `range`, without the span: no file name copied and
    /// no expansion made, for a scan that orders places and writes nothing. Takes `span`'s cases
    /// in its order.
    pub fn place(&self, range: InFile<TextRange>) -> (FileId, TextRange) {
        self.piece_beyond(Beyond::Place);
        let Some(call) = range.file_id.macro_file() else {
            let file = range.file_id.file_id().expect("a file that is not a macro's");
            return (file.file_id(self.db), range.value);
        };
        let mapped = range.original_node_file_range_opt(self.db);
        let loc = call.loc(self.db);
        match mapped {
            Some((at, ctxt))
                if (ctxt.is_root() && !self.written_by_derive(call, at))
                    || hir::Crate::from(loc.def.krate) == self.krate =>
            {
                (at.file_id.file_id(self.db), at.range)
            }
            _ => match self.call_of(call, false) {
                Some((_, _, at)) => self.place(at),
                None => {
                    let at = loc.kind.original_call_range(self.db, loc.krate);
                    (at.file_id.file_id(self.db), at.range)
                }
            },
        }
    }

    /// Whether `at`, where a range of `call`'s expansion is written with the root context, is
    /// the text of a derive's own `derive(..)`: what the derive wrote, not the item it was
    /// handed. blaze's derives give what they write that span with no context of their own;
    /// rustc gives it the call site in the expansion's context (`Span::in_external_macro`).
    fn written_by_derive(&self, call: MacroCallId, at: hir_expand::FileRange) -> bool {
        let loc = call.loc(self.db);
        if !matches!(loc.kind, MacroCallKind::Derive { .. })
            || loc.kind.file_id().file_id().is_none()
        {
            return false;
        }
        let written = loc.kind.original_call_range(self.db, loc.krate);
        written.file_id == at.file_id && written.range.contains_range(at.range)
    }

    /// [`Cx::span`] of a node.
    pub fn span_of(&self, node: InFile<SyntaxNodePtr>) -> Span {
        self.span(node.map(|it| it.text_range()))
    }

    /// [`Cx::span`] of a typed node.
    pub fn span_of_ptr<N: AstNode>(&self, ptr: InFile<AstPtr<N>>) -> Span {
        self.span(ptr.map(|it| it.text_range()))
    }

    /// Whether `range` of `file` is in the module the compiler appended to the crate root
    /// `file` is (a test harness, a wasm proc macro's glue: `FileText::appended_root_module`):
    /// the root's last item, a module of that name. It is the compiler's own code, as rustc's
    /// injected harness and proc-macro declarations are: no lint is given in it, whatever the
    /// crate's levels (a `#![forbid(unsafe_code)]` included), and an unstable feature it uses
    /// is allowed.
    pub(crate) fn in_appended(&self, file: HirFileId, range: TextRange) -> bool {
        self.piece_beyond(Beyond::Place);
        use syntax::view::{HasModuleItem as _, HasName as _};
        let db = self.db;
        let Some(real) = file.file_id() else { return false };
        let Some(name) = db.file_text(real.file_id(db)).appended_root_module(db) else {
            return false;
        };
        let Some(root) = view::SourceFile::cast(self.root(file)) else { return false };
        let Some(view::Item::Module(module)) = root.items().last() else { return false };
        module.name().is_some_and(|it| it.text() == name)
            && module.syntax().text_range().contains_range(range)
    }

    /// The root of `file`, parsed or expanded, as a view of a tree [`Cx::vsema`] gave out.
    pub fn root(&self, file: HirFileId) -> view::Node<'db> {
        self.piece_beyond(Beyond::Place);
        self.vsema.parse_or_expand(file)
    }

    /// The node `ptr` points to, as a view: [`Cx::node`] with no handle.
    pub fn node_view<N: AstNode + view::Typed>(
        &self,
        ptr: InFile<AstPtr<N>>,
    ) -> InFile<N::Of<'db>> {
        self.piece_beyond(Beyond::Place);
        InFile::new(ptr.file_id, self.vsema.to_view(ptr))
    }

    /// The syntax node `ptr` points to, as a view.
    pub fn syntax_view(&self, ptr: InFile<SyntaxNodePtr>) -> InFile<view::Node<'db>> {
        self.piece_beyond(Beyond::Place);
        InFile::new(ptr.file_id, self.vsema.to_view_syntax(ptr))
    }

    /// What rustc says of the expansion of the macro call `call`: where it was called (itself
    /// in an expansion when it was), what it is and where it is defined.
    pub fn expansion_of(&self, call: MacroCallId) -> Option<Expansion> {
        self.piece_beyond(Beyond::Expansion);
        let db = self.db;
        let loc = call.loc(db);
        let (kind, name, at) = self.call_of(call, true)?;
        let call_site = self.span(at);
        let def_site = match loc.def.kind {
            // Its declaration where it is written: in a file, or in what a macro of its crate
            // wrote (synstructure's `decl_derive!`, which rustc_type_ir_macros and vir_macros
            // declare their derives with).
            MacroDefKind::ProcMacro(id, ..) => {
                Some(self.span(id.with_value(signature_range_view(id.to_view(db)))))
            }
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => {
                Some(self.span(id.with_value(item_range_view(id.to_view(db).syntax()))))
            }
        };
        let opaque =
            !matches!(loc.def.kind, MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..));
        Some(Expansion { call_site, kind, name, def_site, opaque })
    }

    /// What the macro call `call` is, its name (empty unless `named`) and the range its call
    /// site is shown at; none exactly where [`Cx::expansion_of`] tells nothing of it.
    fn call_of(
        &self,
        call: MacroCallId,
        named: bool,
    ) -> Option<(MacroKind, String, InFile<TextRange>)> {
        let db = self.db;
        let loc = call.loc(db);
        let node = loc.to_node_view(db);
        // alloc: the macro's name, kept in the diagnostic; none unless `named`
        let owned = |text: &str| if named { text.to_owned() } else { String::new() };
        let (kind, name, range) = match &loc.kind {
            MacroCallKind::FnLike { .. } => {
                let call = view::MacroCall::cast(node.value)?;
                let path = call.path()?;
                let name = if named { fast_print_path(path)? } else { String::new() };
                // rustc's `MacCall::span`: the path to the closing delimiter, without the
                // call's attributes or the `;` of one in item or statement position.
                let whole = node.value.text_range();
                let end = call.token_tree().map_or(whole, |it| it.syntax().text_range()).end();
                let range = TextRange::new(path.syntax().text_range().start(), end);
                (MacroKind::Bang, name, range)
            }
            MacroCallKind::Derive { derive_index, .. } => {
                // A derive's node is its `derive(..)` meta (`MacroCallLoc::to_node_view`).
                let meta = view::Meta::cast(node.value)?;
                if named {
                    let (range, path) =
                        derive_paths_view(meta).into_iter().nth(*derive_index as usize)?;
                    (MacroKind::Derive, owned(path.rsplit("::").next()?), range)
                } else {
                    let mut at = None;
                    let mut k = 0u32;
                    derive_path_tokens_view(meta, |range, _| {
                        if k == *derive_index {
                            at = Some(range);
                        }
                        k += 1;
                    });
                    // alloc: none, an empty name
                    (MacroKind::Derive, String::new(), at?)
                }
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                // The attribute that invoked the macro, not the item's first: a `#[cfg(test)]`
                // before `#[global_allocator]` is a `cfg` meta, with no path to name a macro by.
                let (attr, meta) =
                    censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, *ast_id);
                let path = meta.path()?;
                let name = if named { fast_print_path(path)? } else { String::new() };
                // rustc's span of the attribute: all of it as written, or what a `cfg_attr`
                // holds of it.
                let written = attr.meta().is_some_and(|it| it.syntax() == meta.syntax());
                let range =
                    if written { attr.syntax().text_range() } else { meta.syntax().text_range() };
                (MacroKind::Attr, name, range)
            }
        };
        Some((kind, name, node.with_value(range)))
    }

    /// rustc's printing of `ty`: trimmed paths, `{integer}` for an integer variable, closures
    /// by where they are written.
    pub fn ty(&self, ty: &hir::Type<'db>) -> String {
        if !matches!(self.probing, Probe::None) {
            // A probe prints nothing: the names a type is printed by are the crate's and its
            // dependencies', which its key does not hold.
            self.note_beyond(Beyond::Other);
            return "_".to_owned(); // alloc: a stand-in, only of a body a lint is found in
        }
        crate::print::ty(self, ty)
    }

    /// rustc's `def_path_str` of `def`: the shortest path that names it, trimmed to its name
    /// when that name is unique.
    pub fn def_path(&self, def: hir::ModuleDef) -> String {
        if !matches!(self.probing, Probe::None) {
            // As `Cx::ty`.
            self.note_beyond(Beyond::Other);
            return "_".to_owned(); // alloc: a stand-in, only of a body a lint is found in
        }
        crate::print::def_path(self, def)
    }

    /// The key of an item, for a fact.
    pub fn key(&self, item: ItemId) -> ItemKey {
        blaze_items::item_key(self.db, item)
    }

    /// The key of a module, a scope names resolve in.
    pub fn module_key(&self, module: hir::Module) -> ItemKey {
        blaze_items::key::module_key(self.db, module.into())
    }

    pub fn name_hash(&self, name: &str) -> NameHash {
        blaze_items::key::name_hash(name)
    }

    /// The simplified type impls and methods of `ty` are found under, if it has one.
    pub fn simplified(&self, ty: &hir::Type<'db>) -> Option<SimplifiedType> {
        let simplified = ty.fingerprint_for_trait_impl(self.db)?;
        Some(blaze_items::key::simplified(self.db, &simplified))
    }

    /// `Fact::Method` of `name` on `ty`.
    pub fn method_fact(&self, ty: &hir::Type<'db>, name: &str) -> Option<Fact> {
        Some(Fact::Method { self_ty: self.simplified(ty)?, name: self.name_hash(name) })
    }

    /// `Fact::Implements` of `trait_` for `ty`.
    pub fn implements_fact(&self, trait_: hir::Trait, ty: &hir::Type<'db>) -> Option<Fact> {
        let trait_ = self.key(ItemId::Trait(trait_.into()));
        Some(Fact::Implements { trait_, self_ty: self.simplified(ty)? })
    }

    /// `Fact::Resolves` of `name` in `module`.
    pub fn resolves_fact(&self, module: hir::Module, name: &str) -> Fact {
        Fact::Resolves { scope: self.module_key(module), name: self.name_hash(name) }
    }
}

/// The crates a file of `krate` is taken as a module of: `krate` only, as a file of two crates
/// (a crate built for the host and the target) is a module of each and its syntax must map to
/// this crate's items, not the first crate's.
fn within(krate: hir::Crate) -> std::sync::Arc<blaze_hash::HashSet<base_db::Crate, Global>> {
    // alloc: once a check, one crate
    std::sync::Arc::new(blaze_hash::HashSet::from_iter_in([krate.base()], Global))
}

/// rustc's `fast_print_path`: a macro's path as its call writes it, the segments joined by
/// `::` (a global path's starting with it), the name rustc gives the expansion.
fn fast_print_path(path: view::Path<'_>) -> Option<String> {
    let mut segments: SmallVec<[view::PathSegment<'_>; 2]> = SmallVec::new();
    let mut at = Some(path);
    while let Some(path) = at {
        segments.push(path.segment()?);
        at = path.qualifier();
    }
    // alloc: the macro's name, kept in the diagnostic
    let mut out = String::new();
    for (i, segment) in segments.iter().rev().enumerate() {
        if i != 0 || segment.coloncolon_token().is_some() {
            out.push_str("::");
        }
        out.push_str(segment.name_ref()?.text());
    }
    Some(out)
}

/// The paths of a `#[derive(...)]`, in order, with where each is, as text: for a diagnostic
/// naming a derive. A scan takes [`derive_path_tokens_view`].
pub fn derive_paths_view(meta: view::Meta<'_>) -> Vec<(TextRange, String)> {
    // alloc: the paths as text, for a diagnostic's name of a derive
    let mut paths = Vec::new();
    derive_path_tokens_view(meta, |range, tokens| {
        // alloc: as above
        paths.push((range, tokens.iter().map(|it| it.text()).collect()));
    });
    paths
}

/// Each path of a `derive(...)` meta, the attribute's own or one in a `cfg_attr`, in order:
/// where it is and its tokens (names, `::`). No allocation for a path of up to eight tokens.
pub fn derive_path_tokens_view<'t>(
    meta: view::Meta<'t>,
    mut each: impl FnMut(TextRange, &[view::Token<'t>]),
) {
    let Some((_, tt)) = meta.as_simple_call() else { return };
    let mut current: SmallVec<[view::Token<'t>; 8]> = SmallVec::new();
    let mut flush = |current: &mut SmallVec<[view::Token<'t>; 8]>| {
        if let (Some(first), Some(last)) = (current.first(), current.last()) {
            each(TextRange::new(first.text_range().start(), last.text_range().end()), current);
        }
        current.clear();
    };
    for token in tt.syntax().children_with_tokens().filter_map(NodeOrToken::into_token) {
        match token.kind() {
            SyntaxKind::IDENT | SyntaxKind::COLON2 | SyntaxKind::COLON => current.push(token),
            SyntaxKind::COMMA | SyntaxKind::R_PAREN => flush(&mut current),
            _ => {}
        }
    }
    flush(&mut current);
}

/// Where rustc's span of an item starts: after its attributes, doc comments and the comments
/// before it.
pub use syntax::view::written_range as item_range_view;

/// rustc's `def_span` of a closure (`fn_decl_span`): from after its attributes to the end of
/// its return type, else of its parameters; [`item_range_view`] of a node that is not a closure.
pub fn closure_decl_range_view(node: view::Node<'_>) -> TextRange {
    let range = item_range_view(node);
    let end = view::ClosureExpr::cast(node)
        .and_then(|it| {
            it.ret_type()
                .map(|it| it.syntax().text_range().end())
                .or_else(|| it.param_list().map(|it| it.syntax().text_range().end()))
        })
        .unwrap_or(range.end());
    TextRange::new(range.start(), end.max(range.start()))
}

/// The span rustc gives a function's definition (`def_span`): its signature, without its body.
pub use syntax::view::signature_range as signature_range_view;

/// The span rustc gives an item's definition (`def_span`): a function's signature; of a type,
/// a trait or an impl, from its first keyword to the end of its header, before `{` or `;`;
/// else the whole item. The header's text is sliced from the text the tree lends, so nothing
/// is copied.
pub fn def_range_view(node: view::Node<'_>) -> TextRange {
    if let Some(item) = view::Fn::cast(node) {
        return signature_range_view(item);
    }
    let range = item_range_view(node);
    let header_end = node
        .children_with_tokens()
        .filter(|it| it.text_range().start() >= range.start())
        .find(|it| match it {
            NodeOrToken::Node(node) => matches!(
                node.kind(),
                SyntaxKind::RECORD_FIELD_LIST
                    | SyntaxKind::VARIANT_LIST
                    | SyntaxKind::ASSOC_ITEM_LIST
                    | SyntaxKind::ITEM_LIST
            ),
            NodeOrToken::Token(token) => token.kind() == SyntaxKind::SEMICOLON,
        })
        .map(|it| it.text_range().start());
    let Some(end) = header_end else { return range };
    let header = &node.text()[TextRange::new(range.start(), end) - node.text_range().start()];
    TextRange::at(range.start(), TextSize::of(header.trim_end()))
}

#[cfg(test)]
mod tests {
    use syntax::{SourceFile, view::AstNode as _};

    use super::*;

    fn parse(text: &str) -> syntax::Parse<SourceFile> {
        SourceFile::parse(text, Edition::CURRENT)
    }

    fn first_view<'t, N: view::AstNode<'t>>(parse: &'t syntax::Parse<SourceFile>) -> N {
        parse.root().descendants().find_map(N::cast).unwrap()
    }

    #[test]
    fn a_function_is_spanned_by_its_signature() {
        let text = "/// doc\n#[inline]\npub fn f(x: u32) -> u32 { x }";
        let parse = parse(text);
        let range = signature_range_view(first_view::<view::Fn<'_>>(&parse));
        assert_eq!(&text[range], "pub fn f(x: u32) -> u32");
    }

    #[test]
    fn a_type_is_spanned_by_its_header() {
        let text = "#[derive(Debug)]\npub struct S<T> where T: Copy {\n    a: T,\n}";
        let parsed = parse(text);
        let node = first_view::<view::Struct<'_>>(&parsed);
        assert_eq!(&text[def_range_view(node.syntax())], "pub struct S<T> where T: Copy");
        let text = "struct U;";
        let parsed = parse(text);
        assert_eq!(
            &text[def_range_view(first_view::<view::Struct<'_>>(&parsed).syntax())],
            "struct U"
        );
    }

    /// A closure is spanned from after its attributes to its return type, else its parameters;
    /// anything else by its item range, at every node.
    #[test]
    fn a_closure_is_spanned_by_its_declaration() {
        let text = "fn f() { #[a] move |x| -> u8 { 0 }; |x| x; }";
        let parse = parse(text);
        let spans: Vec<_> = parse
            .root()
            .descendants()
            .filter_map(view::ClosureExpr::cast)
            .map(|it| &text[closure_decl_range_view(it.syntax())])
            .collect();
        assert_eq!(spans, ["move |x| -> u8", "|x|"]);
        let mut compared = 0;
        for node in parse.root().descendants().filter(|it| it.kind() != SyntaxKind::CLOSURE_EXPR) {
            assert_eq!(closure_decl_range_view(node), item_range_view(node));
            compared += 1;
        }
        assert!(compared > 10, "the source has many nodes: {compared}");
    }

    /// Each item's range and definition range, and each attribute's derive paths, as rustc
    /// spans them, in the order of the source.
    #[test]
    fn items_and_derives_are_spanned_as_rustc_spans_them() {
        let text = "/// doc\n#[inline]\npub fn f(x: u32) -> u32 { x }\n\
                    #[derive(Debug)]\npub struct S<T> where T: Copy {\n    a: T,\n}\n\
                    struct U;\n// c\nimpl<T> Tr for S<T> { fn g(); }\n\
                    #[derive(Debug, serde :: Serialize, ,)]\nenum E { A }\n";
        let parse = parse(text);
        let items: Vec<_> = parse
            .root()
            .descendants()
            .filter(|it| view::Item::can_cast(it.kind()))
            .map(|it| (&text[item_range_view(it)], &text[def_range_view(it)]))
            .collect();
        assert_eq!(
            items,
            [
                ("pub fn f(x: u32) -> u32 { x }", "pub fn f(x: u32) -> u32"),
                ("pub struct S<T> where T: Copy {\n    a: T,\n}", "pub struct S<T> where T: Copy"),
                ("struct U;", "struct U"),
                ("impl<T> Tr for S<T> { fn g(); }", "impl<T> Tr for S<T>"),
                ("fn g();", "fn g();"),
                ("enum E { A }", "enum E"),
            ]
        );
        let derives: Vec<Vec<_>> = parse
            .root()
            .descendants()
            .filter_map(view::Attr::cast)
            .map(|attr| {
                let paths = attr.meta().map(derive_paths_view).unwrap_or_default();
                paths.into_iter().map(|(range, it)| (&text[range], it)).collect()
            })
            .collect();
        assert_eq!(
            derives,
            [
                vec![],
                vec![("Debug", "Debug".to_owned())],
                vec![
                    ("Debug", "Debug".to_owned()),
                    ("serde :: Serialize", "serde::Serialize".to_owned())
                ],
            ]
        );
        let f: view::Fn<'_> = first_view(&parse);
        assert_eq!(&text[signature_range_view(f)], "pub fn f(x: u32) -> u32");
    }

    #[test]
    fn derive_paths_are_each_named() {
        let text = "#[derive(Debug, serde::Serialize)]\nstruct S;";
        let parse = parse(text);
        let meta = first_view::<view::Attr<'_>>(&parse).meta().unwrap();
        let paths: Vec<_> =
            derive_paths_view(meta).into_iter().map(|(range, it)| (&text[range], it)).collect();
        assert_eq!(
            paths,
            [("Debug", "Debug".to_owned()), ("serde::Serialize", "serde::Serialize".to_owned())]
        );
    }

    #[test]
    fn files_are_named_from_the_root_below_it() {
        assert_eq!(Files::relative("/ws/src/lib.rs", "/ws"), "src/lib.rs");
        assert_eq!(Files::relative("/ws/src/lib.rs", "/ws/"), "src/lib.rs");
        assert_eq!(Files::relative("/ws", "/ws"), "/ws");
        assert_eq!(Files::relative("/ws/", "/ws"), "/ws/");
        assert_eq!(Files::relative("/wsx/lib.rs", "/ws"), "/wsx/lib.rs");
        assert_eq!(Files::relative("/std/lib.rs", "/ws"), "/std/lib.rs");
        assert_eq!(Files::relative("src/lib.rs", ""), "src/lib.rs");
        assert_eq!(Files::relative("/srv/probe.rs", ""), "/srv/probe.rs");
        assert_eq!(Files::relative("/ws/src/lib.rs", "/"), "ws/src/lib.rs");
        assert_eq!(Files::relative("$DIR/a.rs", ""), "$DIR/a.rs");

        let paths = HashMap::from([(FileId::from_raw(0), "/ws/src/main.rs".to_owned())]);
        let files = Files::new(&paths, "/ws");
        assert_eq!(files.name(FileId::from_raw(0)), "src/main.rs");
        assert_eq!(files.name(FileId::from_raw(7)), "<file 7>");
    }
}
