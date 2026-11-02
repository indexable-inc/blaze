//! What a diagnostic is lowered with: the crate it is of, the names rustc gives files, spans
//! as rustc gives them (in the file a macro was called in, or in the macro's own definition
//! when it is the crate's), the text of what a span covers, rustc's printing of types and
//! paths, and the keys of the facts a diagnostic read. It also holds what every pass of one
//! check shares, each made the first time a pass asks: the walk of the crate's syntax, the
//! HIR lint levels, and the set of lints that may fire.

use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    ops::ControlFlow,
    sync::LazyLock,
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
};

/// The names files are shown by: the front end's path of each file, and the root of the
/// workspace they are named from.
#[derive(Clone, Copy)]
pub struct Files<'a> {
    paths: &'a HashMap<FileId, String>,
    root: &'a str,
}

impl<'a> Files<'a> {
    /// `paths` named from `root`; a `root` of `""` keeps each relative path whole, as the tests'
    /// paths, already names, are.
    pub fn new(paths: &'a HashMap<FileId, String>, root: &'a str) -> Files<'a> {
        Files { paths, root }
    }

    /// The name rustc gives `file` in a diagnostic, by [`Files::relative`]; a file the front end
    /// has no path for is `<file N>` (a crate attribute's is named by [`Cx::file_name`]).
    pub fn name(&self, file: FileId) -> String {
        match self.paths.get(&file) {
            // alloc: a span's file name, made only as a diagnostic is built
            Some(path) => Files::relative(path, self.root).to_owned(),
            // alloc: as above
            None => format!("<file {}>", file.index()),
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
    named: RefCell<blaze_hash::HashMap<String, FileId>>,
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
}

impl<'a, 'db> Cx<'a, 'db> {
    pub fn new(
        db: &'db Db,
        krate: hir::Crate,
        files: Files<'a>,
        frame: &'a Frame,
    ) -> Cx<'a, 'db> {
        // A file of two crates (a crate built for the host and the target) is a module of each:
        // its syntax must map to this crate's items, not the first crate's.
        let within = std::sync::Arc::new(blaze_hash::HashSet::from_iter([krate.base()])); // alloc: once a check, one crate
        Cx {
            vsema: hir::view::Semantics::new_within(db, within),
            db,
            krate,
            edition: krate.edition(db),
            display_target: krate.to_display_target(db),
            files,
            frame,
            lints: None,
            named: RefCell::new(blaze_hash::HashMap::default()),
            paths: OnceCell::new(),
            walk: OnceCell::new(),
            items: OnceCell::new(),
            effective: OnceCell::new(),
            levels: RefCell::new(None),
            may_fire: OnceCell::new(),
        }
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
        self.paths.get_or_init(|| crate::print::Paths::new(self))
    }

    /// The context checking with the lint flags `lints`, before anything was asked of it.
    pub fn with_lints(self, lints: &'a crate::lints::CommandLine) -> Cx<'a, 'db> {
        assert!(
            self.walk.get().is_none() && self.levels.borrow().is_none() && self.may_fire.get().is_none(),
            "the lint flags are given before a level is asked"
        );
        Cx { lints: Some(lints), ..self }
    }

    /// The lint flags the crate is checked with: rustc's defaults when none were given.
    pub fn lint_flags(&self) -> &'a crate::lints::CommandLine {
        static DEFAULT: LazyLock<crate::lints::CommandLine> = LazyLock::new(crate::lints::CommandLine::default);
        self.lints.unwrap_or(&*DEFAULT)
    }

    /// The crate as rustc's early lint pass walks it: made once per check, for every pass that
    /// walks the crate's syntax.
    pub(crate) fn walk(&self) -> &Walk<'db> {
        self.walk.get_or_init(|| Walk::of(self))
    }

    /// The crate's items in the order rustc's item passes visit them: made once per check, for
    /// the item lints before the abort and after it.
    pub(crate) fn items(&self) -> &ItemWalk<'db> {
        self.items.get_or_init(|| ItemWalk::of(self))
    }

    /// The crate's effective visibilities (rustc's `effective_visibilities`): made once per
    /// check, for the stability pass, the dead-code pass and the item lints after the abort.
    pub(crate) fn effective(&self) -> &Effective {
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

    fn lints_that_may_fire(&self) -> &LintSet {
        self.may_fire.get_or_init(|| crate::lints::may_fire(self, self.lint_flags(), self.walk()))
    }

    /// Whether cargo runs the compile (rustc's `was_invoked_from_cargo`: `CARGO_CRATE_NAME` is
    /// set), as it is under blaze: some help then names `cargo add` and `Cargo.toml`.
    pub fn cargo(&self) -> bool {
        self.lints.is_some_and(|it| it.check_cfg.cargo)
    }

    /// The name rustc shows `file` by. A crate attribute's file has no path: it is
    /// [`base_db::CRATE_ATTR_FILE_NAME`] with the file's text, which tells the attributes apart
    /// and is their source to the emitter ([`crate::emit::name_with_text`]).
    pub fn file_name(&self, file: FileId) -> String {
        let name = match self.files.paths.contains_key(&file) || base_db::crate_attr_owner(self.db, file).is_none() {
            true => self.files.name(file),
            false => crate::emit::name_with_text(base_db::CRATE_ATTR_FILE_NAME, self.file_text(file)),
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
        self.db.file_text(file).text(self.db)
    }

    /// `range` of `file`, as a span with no expansion.
    pub fn span_in_file(&self, file: FileId, range: TextRange) -> Span {
        Span::new(&self.file_name(file), range)
    }

    /// The source text a span covers.
    pub fn snippet(&self, span: &Span) -> Option<&'db str> {
        let file = self.file_of(span)?;
        self.file_text(file).get(span.lo as usize..span.hi as usize)
    }

    /// The span rustc gives `range`: where it is written when it is in a file or in what a
    /// macro call was handed; where the crate's own macro's definition writes it, inside that
    /// macro's expansion; else the call of the outermost macro of another crate it came from,
    /// inside that macro's expansion.
    pub fn span(&self, range: InFile<TextRange>) -> Span {
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
            Some((at, _)) if local => Span::new(&self.file_name(at.file_id.file_id(self.db)), at.range),
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

    /// The root of `file`, parsed or expanded, as a view of a tree [`Cx::vsema`] gave out.
    pub fn root(&self, file: HirFileId) -> view::Node<'db> {
        self.vsema.parse_or_expand(file)
    }

    /// The node `ptr` points to, as a view: [`Cx::node`] with no handle.
    pub fn node_view<N: AstNode + view::Typed>(&self, ptr: InFile<AstPtr<N>>) -> InFile<N::Of<'db>> {
        InFile::new(ptr.file_id, self.vsema.to_view(ptr))
    }

    /// The syntax node `ptr` points to, as a view.
    pub fn syntax_view(&self, ptr: InFile<SyntaxNodePtr>) -> InFile<view::Node<'db>> {
        InFile::new(ptr.file_id, self.vsema.to_view_syntax(ptr))
    }

    /// What rustc says of the expansion of the macro call `call`: where it was called (itself
    /// in an expansion when it was), what it is and where it is defined.
    pub fn expansion_of(&self, call: MacroCallId) -> Option<Expansion> {
        let db = self.db;
        let loc = call.loc(db);
        let (kind, name, at) = self.call_of(call, true)?;
        let call_site = self.span(at);
        let def_site = match loc.def.kind {
            MacroDefKind::ProcMacro(id, ..) => {
                let file = id.file_id.file_id()?.file_id(db);
                Some(self.span_in_file(file, signature_range_view(id.to_view(db))))
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
        let opaque = !matches!(loc.def.kind, MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..));
        Some(Expansion { call_site, kind, name, def_site, opaque })
    }

    /// What the macro call `call` is, its name (empty unless `named`) and the range its call
    /// site is shown at; none exactly where [`Cx::expansion_of`] tells nothing of it.
    fn call_of(&self, call: MacroCallId, named: bool) -> Option<(MacroKind, String, InFile<TextRange>)> {
        let db = self.db;
        let loc = call.loc(db);
        if let MacroDefKind::ProcMacro(id, ..) = loc.def.kind {
            id.file_id.file_id()?;
        }
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
                    let (range, path) = derive_paths_view(meta).into_iter().nth(*derive_index as usize)?;
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
            MacroCallKind::Attr { .. } => {
                let item = view::Item::cast(node.value)?;
                let attr = item.attrs().next()?;
                let path = attr.path()?;
                let name = if named { fast_print_path(path)? } else { String::new() };
                (MacroKind::Attr, name, attr.syntax().text_range())
            }
        };
        Some((kind, name, node.with_value(range)))
    }

    /// rustc's printing of `ty`: trimmed paths, `{integer}` for an integer variable, closures
    /// by where they are written.
    pub fn ty(&self, ty: &hir::Type<'db>) -> String {
        crate::print::ty(self, ty)
    }

    /// rustc's `def_path_str` of `def`: the shortest path that names it, trimmed to its name
    /// when that name is unique.
    pub fn def_path(&self, def: hir::ModuleDef) -> String {
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
pub fn derive_path_tokens_view<'t>(meta: view::Meta<'t>, mut each: impl FnMut(TextRange, &[view::Token<'t>])) {
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
            it.ret_type().map(|it| it.syntax().text_range().end()).or_else(|| it.param_list().map(|it| it.syntax().text_range().end()))
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
        assert_eq!(&text[def_range_view(first_view::<view::Struct<'_>>(&parsed).syntax())], "struct U");
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
                vec![("Debug", "Debug".to_owned()), ("serde :: Serialize", "serde::Serialize".to_owned())],
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
        let paths: Vec<_> = derive_paths_view(meta).into_iter().map(|(range, it)| (&text[range], it)).collect();
        assert_eq!(paths, [("Debug", "Debug".to_owned()), ("serde::Serialize", "serde::Serialize".to_owned())]);
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
        assert_eq!(Files::relative("/tmp/probe.rs", ""), "/tmp/probe.rs");
        assert_eq!(Files::relative("/ws/src/lib.rs", "/"), "ws/src/lib.rs");
        assert_eq!(Files::relative("$DIR/a.rs", ""), "$DIR/a.rs");

        let paths = HashMap::from([(FileId::from_raw(0), "/ws/src/main.rs".to_owned())]);
        let files = Files::new(&paths, "/ws");
        assert_eq!(files.name(FileId::from_raw(0)), "src/main.rs");
        assert_eq!(files.name(FileId::from_raw(7)), "<file 7>");
    }
}
