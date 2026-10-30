//! Runs the lints over a crate: each file of its modules is walked as rustc's lint passes walk
//! its HIR, past what `cfg` turns off, into what derives expand to; each lint found is given
//! the level the attributes around it, the command line and the `warnings` group give it, and
//! the notes rustc adds of where that level came from. What `#[expect]` was set for and was
//! not found is said as rustc says it.

use std::ops::ControlFlow;

use cfg::{CfgAtom, CfgDiff, CfgExpr, CfgOptions};
use hir::{HirFileId, InFile, Semantics};
use hir_expand::{MacroCallKind, MacroDefKind};
use ide_db::{EditionedFileId, RootDatabase, base_db::SourceDatabase as _};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize,
    WalkEvent,
    ast::{self, HasAttrs},
};

use crate::{
    Level,
    conf::Conf,
    diag::{Diagnostic, Expansion, MacroKind, MultiSpan, Span, Sublevel},
    level::{CommandLine, Levels, Lint, Set, Source, Spec, Target},
    lints,
    registry::{self, Info},
};

/// What a crate's lints are run with.
#[derive(Clone, Debug, Default)]
pub struct Setup {
    /// The lint flags rustc would be given: the package's `[lints]`, `RUSTFLAGS`, then what
    /// `cargo clippy --` was given.
    pub command_line: CommandLine,
    pub conf: Conf,
}

/// A diagnostic of a crate, and whether only code `cfg(test)` keeps has it, which a build of
/// the crate without its tests does not see.
#[derive(Clone, Debug)]
pub struct Found {
    pub diagnostic: Diagnostic,
    pub test_only: bool,
}

/// Names files as rustc names them in diagnostics: from the root of the workspace when they
/// are below it, else by their whole path.
pub type NameOf<'a> = &'a dyn Fn(ide_db::FileId) -> Option<String>;

/// The diagnostics of the lints of `krate`, in the order rustc would emit them.
pub fn check(
    db: &RootDatabase,
    krate: hir::Crate,
    setup: &Setup,
    name_of: NameOf<'_>,
) -> Vec<Found> {
    hir::attach_db(db, || {
        // A file of several crates (a library and its test build) is read as `krate`'s.
        let sema =
            Semantics::new_within(db, std::sync::Arc::new(std::iter::once(krate.base()).collect()));
        let cfg = krate.cfg(db).clone();
        let test = CfgAtom::Flag(intern::sym::test);
        let without_test = cfg.check_atom(&test).then(|| {
            let mut it = cfg.clone();
            it.apply_diff(CfgDiff::new(Vec::new(), vec![test]));
            it
        });
        let mut files: Vec<EditionedFileId> = Vec::new();
        // Where rustc's walks over the crate come to each file: at the `mod` item declaring it,
        // in the file that has that, and so on up to the root.
        let mut nesting: FxHashMap<EditionedFileId, Vec<(u32, u32)>> = FxHashMap::default();
        for module in krate.modules(db) {
            let Some(file) = module.definition_source_file_id(db).file_id() else { continue };
            if nesting.contains_key(&file) {
                continue;
            }
            let mut place = Vec::new();
            if let Some(declared) = module.declaration_source_range(db) {
                let declared = declared.original_node_file_range_rooted(db);
                place = nesting.get(&declared.file_id).cloned().unwrap_or_default();
                place.push((
                    u32::from(declared.range.start()),
                    u32::MAX - u32::from(declared.range.end()),
                ));
            }
            nesting.insert(file, place);
            files.push(file);
        }
        let levels = Levels::new(&setup.command_line);
        let texts: Vec<&str> =
            files.iter().map(|it| &**db.file_text(it.file_id(db)).text(db)).collect();
        let on = lints::on(&levels, &texts);
        let mut state = State {
            sema: &sema,
            db,
            krate,
            conf: &setup.conf,
            cfg,
            without_test,
            name_of,
            on,
            pending: Vec::new(),
            expectations: Vec::new(),
        };
        for (order, file) in files.iter().enumerate() {
            let Some(name) = name_of(file.file_id(db)) else { continue };
            let source = sema.parse(*file);
            let nesting = nesting.remove(file).unwrap_or_default();
            let mut cx = Cx {
                state: &mut state,
                file: *file,
                name,
                order,
                nesting,
                expansion: None,
                input: None,
                depth: 0,
                in_bang: false,
            };
            cx.walk(source.syntax());
        }
        state.finish(&levels)
    })
}

/// A lint found, before its level is known.
struct Pending {
    lint: &'static Info,
    /// Where its level is asked for.
    at: InFile<SyntaxNode>,
    diagnostic: Diagnostic,
    /// The order rustc emits it in: by pass (pre-expansion over the root file, early, late,
    /// after late), then by where the node it was found at is in the walk over the crate,
    /// outer nodes first.
    order: (u8, Vec<(u32, u32)>),
}

/// An `#[expect]` of a lint, as the walk met it.
struct Expectation {
    /// The lint's name in the attribute.
    span: Span,
    reason: Option<String>,
    at: InFile<SyntaxNode>,
}

pub(crate) struct State<'a, 'db> {
    pub(crate) sema: &'a Semantics<'db, RootDatabase>,
    pub(crate) db: &'db RootDatabase,
    pub(crate) krate: hir::Crate,
    pub(crate) conf: &'a Conf,
    cfg: CfgOptions,
    /// The crate's cfg without `test`, when it has `test`.
    without_test: Option<CfgOptions>,
    name_of: NameOf<'a>,
    /// The lints that can be at a level other than `allow` somewhere in the crate.
    on: FxHashSet<&'static str>,
    pending: Vec<Pending>,
    expectations: Vec<Expectation>,
}

/// The context a lint is run in: the crate, and the file being walked.
pub(crate) struct Cx<'s, 'a, 'db> {
    pub(crate) state: &'s mut State<'a, 'db>,
    pub(crate) file: EditionedFileId,
    /// The file's name, as diagnostics name it.
    pub(crate) name: String,
    order: usize,
    /// Where the walk over the crate comes to the file, as [`Pending::order`] has it.
    nesting: Vec<(u32, u32)>,
    /// The macro the walk is in the expansion of, if it is in one.
    pub(crate) expansion: Option<Expansion>,
    /// When the walk is in what a macro call of the file expands to, the arguments of the
    /// call: what of the expansion is written there is linted as code of the file, as rustc
    /// lints what a macro is handed.
    input: Option<TextRange>,
    /// How many macro calls deep the walk is.
    depth: u32,
    /// Whether the walk is in what a function-like macro call expands to, not only in what an
    /// attribute macro makes of an item.
    in_bang: bool,
}

impl<'a, 'db> Cx<'_, 'a, 'db> {
    pub(crate) fn sema(&self) -> &'a Semantics<'db, RootDatabase> {
        self.state.sema
    }

    pub(crate) fn db(&self) -> &'db RootDatabase {
        self.state.db
    }

    pub(crate) fn conf(&self) -> &'a Conf {
        self.state.conf
    }

    pub(crate) fn krate(&self) -> hir::Crate {
        self.state.krate
    }

    pub(crate) fn edition(&self) -> span::Edition {
        self.file.edition(self.db())
    }

    /// Whether `lint` can be at a level that is not `allow` in the crate: a lint that cannot is
    /// not run.
    pub(crate) fn on(&self, lint: &str) -> bool {
        self.state.on.contains(lint)
    }

    /// The text of the file being walked.
    pub(crate) fn text(&self) -> &'db str {
        let db = self.db();
        db.file_text(self.file.file_id(db)).text(db)
    }

    /// The source text at `range` of the file being walked.
    pub(crate) fn snippet(&self, range: TextRange) -> String {
        self.text().get(std::ops::Range::<usize>::from(range)).unwrap_or_default().to_owned()
    }

    /// Where `node` is in the file being walked, if it is written there.
    pub(crate) fn range(&self, node: &SyntaxNode) -> Option<TextRange> {
        match (&self.expansion, self.input) {
            (None, None) => Some(node.text_range()),
            _ => {
                let range = self.sema().original_range_opt(node)?;
                (range.file_id == self.file).then_some(range.range)
            }
        }
    }

    /// Whether the walk is in the file's own tree, not in an expansion of a macro.
    pub(crate) fn in_file(&self) -> bool {
        self.expansion.is_none() && self.input.is_none()
    }

    /// Whether the walk is in the arguments of a function-like macro call, as `vec!` is in
    /// `assert!(f(vec![]))`.
    pub(crate) fn in_bang_call(&self) -> bool {
        self.in_bang
    }

    /// Where `node`, of the file or of an expansion, is written in the file being walked, if
    /// it is.
    pub(crate) fn original_range(&self, node: &SyntaxNode) -> Option<TextRange> {
        let range = self.sema().original_range_opt(node)?;
        (range.file_id == self.file).then_some(range.range)
    }

    /// `range` of the file being walked as a span, with the expansion the walk is in.
    pub(crate) fn span(&self, range: TextRange) -> Span {
        let mut span = Span::new(&self.name, range);
        span.expansion = self.expansion.clone().map(Box::new);
        span
    }

    /// The span rustc gives an item: from after its attributes and doc comments.
    pub(crate) fn item_span(&self, node: &SyntaxNode) -> Option<Span> {
        if let Some(expansion) = &self.expansion {
            // An item made of the macro's own tokens and of the input's maps to no one range of
            // the file; rustc has it where the macro was called, as `Span::call_site()` is
            // (serde's `enum __Field {}` for a struct with no fields).
            let call_site = &expansion.call_site;
            let range = self
                .range(node)
                .unwrap_or_else(|| TextRange::new(call_site.lo.into(), call_site.hi.into()));
            return Some(self.span(range));
        }
        self.range(node)?;
        Some(self.span(item_range(node)))
    }

    /// The macro `call` calls, by name, if it is one of `core`'s, `alloc`'s or `std`'s, as
    /// clippy's diagnostic items name them.
    pub(crate) fn std_macro(&self, call: &ast::MacroCall) -> Option<String> {
        let db = self.db();
        let mac = self.sema().resolve_macro_call(call)?;
        if !mac.module(db).krate(db).origin(db).is_lang() {
            return None;
        }
        Some(mac.name(db).as_str().to_owned())
    }

    /// The expressions written in `call`'s arguments, as its expansion has them, in order:
    /// each the outermost there whose source is in the arguments.
    pub(crate) fn macro_args(&self, call: &ast::MacroCall) -> Vec<(TextRange, ast::Expr)> {
        let Some(tt) = call.token_tree() else { return Vec::new() };
        let Some(range) = self.range(tt.syntax()) else { return Vec::new() };
        if range.len() < TextSize::from(2) {
            return Vec::new();
        }
        let inner =
            TextRange::new(range.start() + TextSize::from(1), range.end() - TextSize::from(1));
        let Some(expansion) = self.sema().expand_macro_call(call) else { return Vec::new() };
        let mut found = Vec::new();
        self.args_in(&expansion.value, inner, &mut found, 0);
        found.sort_by_key(|(range, _)| range.start());
        found.dedup_by_key(|(range, _)| *range);
        found
    }

    fn args_in(
        &self,
        root: &SyntaxNode,
        inner: TextRange,
        found: &mut Vec<(TextRange, ast::Expr)>,
        depth: u32,
    ) {
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if let Some(expr) = ast::Expr::cast(node.clone())
                && let Some(range) = self.sema().original_range_opt(&node)
                && range.file_id == self.file
                && inner.contains_range(range.range)
                && !found.iter().any(|(it, _)| it.contains_range(range.range))
            {
                found.push((range.range, expr));
                events.skip_subtree();
                continue;
            }
            if let Some(call) = ast::MacroCall::cast(node.clone()) {
                if depth < 4
                    && let Some(expansion) = self.sema().expand_macro_call(&call)
                {
                    self.args_in(&expansion.value, inner, found, depth + 1);
                }
                events.skip_subtree();
            }
        }
    }

    /// The type of `expr`, before adjustments.
    pub(crate) fn ty(&self, expr: &ast::Expr) -> Option<hir::Type<'db>> {
        Some(self.sema().type_of_expr(expr)?.original)
    }

    /// The type of `expr`, after adjustments.
    pub(crate) fn adjusted_ty(&self, expr: &ast::Expr) -> Option<hir::Type<'db>> {
        Some(self.sema().type_of_expr(expr)?.adjusted())
    }

    pub(crate) fn lang_trait(&self, item: hir::LangItem) -> Option<hir::Trait> {
        hir::Trait::lang(self.db(), self.krate(), item)
    }

    /// The trait at `path` in the crate `path[0]` (`core`, `alloc`, `std`).
    pub(crate) fn std_trait(&self, path: &[&str]) -> Option<hir::Trait> {
        match self.std_def(path)? {
            hir::ModuleDef::Trait(it) => Some(it),
            _ => None,
        }
    }

    /// The item at `path` in the crate `path[0]` (`core`, `alloc`, `std`).
    pub(crate) fn std_def(&self, path: &[&str]) -> Option<hir::ModuleDef> {
        let db = self.db();
        let (krate, rest) = path.split_first()?;
        let famous = ide_db::famous_defs::FamousDefs(self.sema(), self.krate());
        let krate = match *krate {
            "core" => famous.core()?,
            "alloc" => famous.alloc()?,
            "std" => famous.std()?,
            _ => return None,
        };
        let mut module = krate.root_module(db);
        let (last, modules) = rest.split_last()?;
        for name in modules {
            module = module
                .children(db)
                .find(|it| it.name(db).is_some_and(|it| it.as_str() == *name))?;
        }
        module.scope(db, None).into_iter().find_map(|(name, def)| match def {
            hir::ScopeDef::ModuleDef(def) if name.as_str() == *last => Some(def),
            _ => None,
        })
    }

    /// The type `trait_`'s associated type `name` is for `ty`.
    pub(crate) fn assoc_ty(
        &self,
        ty: &hir::Type<'db>,
        trait_: hir::Trait,
        name: &str,
    ) -> Option<hir::Type<'db>> {
        let db = self.db();
        let alias = trait_.items(db).into_iter().find_map(|it| match it {
            hir::AssocItem::TypeAlias(alias) if alias.name(db).as_str() == name => Some(alias),
            _ => None,
        })?;
        ty.normalize_trait_assoc_type(db, &[], alias)
    }

    /// The source of `node` of an expansion, as written in the file walked.
    pub(crate) fn source_of(&self, node: &SyntaxNode) -> String {
        match self.sema().original_range_opt(node) {
            Some(range) if range.file_id == self.file => self.snippet(range.range),
            _ => node.text().to_string(),
        }
    }

    /// Reports `lint` at `span`, its level asked for at `at`; what it returns is decorated as
    /// clippy's lint decorates it.
    pub(crate) fn lint(
        &mut self,
        lint: &'static str,
        at: &SyntaxNode,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.lint_when(lint, at, at, span, message)
    }

    /// Reports `lint` at `span`, its level asked for at `at`, in the order rustc would come to
    /// `when` in its walk: for a lint run on a node that it reports of a node inside it.
    pub(crate) fn lint_when(
        &mut self,
        lint: &'static str,
        at: &SyntaxNode,
        when: &SyntaxNode,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let span: MultiSpan = span.into();
        // rustc's passes run a lint as they come to a node, outer nodes before inner ones.
        let place = match self.original_range(when) {
            Some(range) => (u32::from(range.start()), u32::MAX - u32::from(range.end())),
            None => (span.primary_span().map_or(0, |it| it.lo), 0),
        };
        self.lint_placed(lint, at, place, span, message)
    }

    /// Reports `lint` at `span`, its level asked for at `at`, as rustc would once its walk has
    /// left `after`: for what clippy says when a body has been walked.
    pub(crate) fn lint_after(
        &mut self,
        lint: &'static str,
        at: &SyntaxNode,
        after: &SyntaxNode,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let span: MultiSpan = span.into();
        let place = match self.original_range(after) {
            Some(range) => (u32::from(range.end()).saturating_sub(1), u32::MAX),
            None => (span.primary_span().map_or(0, |it| it.hi), u32::MAX),
        };
        self.lint_placed(lint, at, place, span, message)
    }

    fn lint_placed(
        &mut self,
        lint: &'static str,
        at: &SyntaxNode,
        place: (u32, u32),
        span: MultiSpan,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        let info = registry::lint(lint).unwrap_or_else(|| panic!("`{lint}` is not clippy's"));
        let phase = lints::phase(lint, self.order == 0);
        // What clippy gathers to say after the late pass it says in the order of the source map,
        // which has the crate's files in the order rustc read them.
        let at_place = match phase {
            3 => vec![(u32::try_from(self.order).unwrap_or(u32::MAX), 0), place],
            _ => self.nesting.iter().copied().chain([place]).collect(),
        };
        let file = self.sema().hir_file_for(at);
        self.state.pending.push(Pending {
            lint: info,
            at: InFile::new(file, at.clone()),
            diagnostic: Diagnostic {
                lint: format!("clippy::{lint}"),
                level: Level::Warn,
                message: message.into(),
                span,
                children: Vec::new(),
                suggestions: Vec::new(),
                suggestions_disabled: false,
            },
            order: (phase, at_place),
        });
        &mut self.state.pending.last_mut().expect("just pushed").diagnostic
    }

    /// Walks `root` and what it expands to, running the lints on each node.
    pub(crate) fn walk(&mut self, root: &SyntaxNode) {
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) {
                if !self.state.enabled(&owner) {
                    events.skip_subtree();
                    continue;
                }
                if self.expansion.is_none() && self.input.is_none() {
                    self.expectations_of(&owner);
                }
            }
            // What an attribute macro such as `#[test]` makes of an item is what rustc lints,
            // the code written in it keeping its place.
            if let Some(item) = ast::Item::cast(node.clone())
                && self.walk_attr_expansion(&item)
            {
                events.skip_subtree();
                continue;
            }
            if self.is_written(&node) {
                lints::check_node(self, &node);
            }
            if let Some(adt) = ast::Adt::cast(node.clone()) {
                self.walk_derives(&adt);
            }
            if let Some(call) = ast::MacroCall::cast(node.clone()) {
                self.walk_call(&call);
                events.skip_subtree();
            }
        }
    }

    /// Whether `node` is code of the file: outside macro calls, or handed to the call the walk
    /// is in the expansion of, all of it.
    pub(crate) fn is_written(&self, node: &SyntaxNode) -> bool {
        match self.input {
            None => true,
            Some(input) => self.range(node).is_some_and(|it| input.contains_range(it)),
        }
    }

    /// Walks what an attribute macro on `item` expands it to, if one does; says whether one
    /// did.
    fn walk_attr_expansion(&mut self, item: &ast::Item) -> bool {
        const DEEPEST: u32 = 8;
        if self.depth >= DEEPEST || self.expansion.is_some() || item.attrs().next().is_none() {
            return false;
        }
        let Some(expanded) = self.sema().expand_attr_macro(item) else { return false };
        let Some(input) = self.range(item.syntax()).or(self.input) else { return true };
        let outer = self.input.replace(input);
        self.depth += 1;
        self.walk(&expanded.value.value);
        self.depth -= 1;
        self.input = outer;
        true
    }

    /// Walks what the macro call `call` expands to, for the code handed to it.
    fn walk_call(&mut self, call: &ast::MacroCall) {
        const DEEPEST: u32 = 8;
        if self.depth >= DEEPEST || self.expansion.is_some() || !lints::in_calls(self) {
            return;
        }
        let Some(tt) = call.token_tree() else { return };
        // A call the expansion makes of its own hands on what it was handed.
        let Some(input) = self.range(tt.syntax()).or(self.input) else { return };
        let Some(expanded) = self.sema().expand_macro_call(call) else { return };
        let outer = self.input.replace(input);
        let outer_bang = std::mem::replace(&mut self.in_bang, true);
        self.depth += 1;
        self.walk(&expanded.value);
        self.depth -= 1;
        self.in_bang = outer_bang;
        self.input = outer;
    }

    /// Walks what the derives of `adt` expand to.
    fn walk_derives(&mut self, adt: &ast::Adt) {
        if !lints::in_derives(self) {
            return;
        }
        for attr in adt.attrs() {
            let Some(meta) = attr.meta() else { continue };
            if attr.simple_name().as_deref() != Some("derive") {
                continue;
            }
            let Some(expanded) = self.sema().expand_derive_macro(&meta) else { continue };
            for root in expanded.into_iter().flatten() {
                let root = root.value;
                let Some(call) = self.sema().hir_file_for(&root).macro_file() else { continue };
                let Some(expansion) = self.expansion_of(call) else { continue };
                let outer = self.expansion.replace(expansion);
                self.walk(&root);
                self.expansion = outer;
            }
        }
    }

    /// What rustc says of the expansion of the macro call `call`: where it was called, what
    /// it is and where it is defined.
    fn expansion_of(&self, call: hir_expand::MacroCallId) -> Option<Expansion> {
        let db = self.db();
        let loc = call.loc(db);
        let node = loc.to_node(db);
        if node.file_id.file_id() != Some(self.file) {
            return None;
        }
        let (kind, name, call_site) = match &loc.kind {
            MacroCallKind::FnLike { .. } => {
                let call = ast::MacroCall::cast(node.value.clone())?;
                let name = call.path()?.segment()?.name_ref()?.text().to_owned();
                (MacroKind::Bang, name, node.value.text_range())
            }
            MacroCallKind::Derive { derive_index, .. } => {
                // The `derive(..)` itself, which in `#[cfg_attr(.., derive(..))]` is not the
                // attribute's own meta.
                let meta = node.value.ancestors().find_map(ast::Meta::cast)?;
                let path = derive_paths(&meta).into_iter().nth(*derive_index as usize)?;
                let name = path.1.rsplit("::").next()?.to_owned();
                (MacroKind::Derive, name, path.0)
            }
            MacroCallKind::Attr { .. } => {
                let item = ast::Item::cast(node.value.clone())?;
                let attr = item.attrs().next()?;
                let name = attr.simple_name()?.to_string();
                (MacroKind::Attr, name, attr.syntax().text_range())
            }
        };
        let def_site = match loc.def.kind {
            MacroDefKind::ProcMacro(id, ..) => {
                let file = id.file_id.file_id()?;
                let name = (self.state.name_of)(file.file_id(db))?;
                let item = id.to_node(db);
                let range = signature_range(&item);
                Some(Span::new(&name, range))
            }
            _ => None,
        };
        Some(Expansion { call_site: Span::new(&self.name, call_site), kind, name, def_site })
    }

    /// Records the `#[expect]`s of lints of clippy's on `owner`.
    fn expectations_of(&mut self, owner: &ast::AnyHasAttrs) {
        let has_expect = owner
            .attrs()
            .any(|it| it.syntax().text().contains_char('e') && it.to_string().contains("expect"));
        if !has_expect {
            return;
        }
        let file = self.sema().hir_file_for(owner.syntax());
        for LintAttr { level, target, range, reason, .. } in self.state.lint_attrs(owner) {
            if level != Level::Expect {
                continue;
            }
            let Target::Lint(name) = target else { continue };
            let Some(lint) = registry::lint(&name) else { continue };
            if !lints::implemented(lint.name) {
                continue;
            }
            self.state.expectations.push(Expectation {
                span: Span::new(&self.name, range),
                reason,
                at: InFile::new(file, owner.syntax().clone()),
            });
        }
    }
}

impl State<'_, '_> {
    /// Whether the `cfg`s of `owner` keep it.
    fn enabled(&self, owner: &ast::AnyHasAttrs) -> bool {
        self.cfgs(owner).iter().all(|it| self.cfg.check(it) != Some(false))
    }

    /// The `cfg` predicates on `owner`, with its `cfg_attr`s expanded.
    fn cfgs(&self, owner: &ast::AnyHasAttrs) -> Vec<CfgExpr> {
        let mut cfgs = Vec::new();
        if owner.attrs().next().is_none() {
            return cfgs;
        }
        hir_expand::attrs::expand_cfg_attr::<()>(
            owner.attrs(),
            || &self.cfg,
            |meta, _| {
                if let ast::Meta::CfgMeta(meta) = meta
                    && let Some(predicate) = meta.cfg_predicate()
                {
                    cfgs.push(CfgExpr::parse_from_ast(predicate));
                }
                ControlFlow::Continue(())
            },
        );
        cfgs
    }

    /// The lint attributes on `owner`, its `cfg_attr`s expanded, in their order: the level,
    /// what it sets, the name as written and where it is, and the reason given.
    fn lint_attrs(&self, owner: &ast::AnyHasAttrs) -> Vec<LintAttr> {
        let mut found = Vec::new();
        hir_expand::attrs::expand_cfg_attr::<()>(
            ast::attrs_including_inner(owner),
            || &self.cfg,
            |meta, _| {
                let ast::Meta::TokenTreeMeta(meta) = meta else { return ControlFlow::Continue(()) };
                let Some(level) = meta
                    .path()
                    .and_then(|it| it.as_single_name_ref())
                    .and_then(|it| Level::of_attr(it.text()))
                else {
                    return ControlFlow::Continue(());
                };
                let Some(tt) = meta.token_tree() else { return ControlFlow::Continue(()) };
                let (names, reason) = lint_list(&tt);
                for (name, range) in names {
                    if let Some(target) = Target::of(&name) {
                        let name = canonical(&name);
                        found.push(LintAttr { level, target, name, range, reason: reason.clone() });
                    }
                }
                ControlFlow::Continue(())
            },
        );
        found
    }

    /// The level sets around `at`, outermost first, and whether `cfg(test)` alone keeps it.
    fn sets_at(&self, at: &InFile<SyntaxNode>, name_of: NameOf<'_>) -> (Vec<Set>, bool) {
        let mut owners: Vec<(HirFileId, ast::AnyHasAttrs)> = Vec::new();
        let mut node = Some(at.clone());
        while let Some(start) = node.take() {
            let mut last = start.clone();
            for ancestor in self.sema.ancestors_with_macros_file(start) {
                if let Some(owner) = ast::AnyHasAttrs::cast(ancestor.value.clone()) {
                    owners.push((ancestor.file_id, owner));
                }
                last = ancestor;
            }
            if let HirFileId::FileId(file) = last.file_id {
                node = self.sema.find_parent_file(HirFileId::FileId(file));
            }
        }
        let mut sets = Vec::new();
        let mut test_only = false;
        for (file, owner) in owners.iter().rev() {
            if let Some(without_test) = &self.without_test {
                test_only |= self.cfgs(owner).iter().any(|it| {
                    self.cfg.check(it) != Some(false) && without_test.check(it) == Some(false)
                });
            }
            let Some(name) = file.file_id().and_then(|it| name_of(it.file_id(self.db))) else {
                continue;
            };
            for LintAttr { level, target, name: attr_name, range, reason } in self.lint_attrs(owner)
            {
                let source =
                    Source::Node { name: attr_name, span: Span::new(&name, range), reason };
                sets.push(Set { target, spec: Spec { level, source } });
            }
        }
        (sets, test_only)
    }

    /// The diagnostics of the lints found, at their levels, with what `#[expect]` was set for
    /// and not found.
    fn finish(mut self, levels: &Levels) -> Vec<Found> {
        let mut pending = std::mem::take(&mut self.pending);
        pending.sort_by(|a, b| a.order.cmp(&b.order));
        let mut found = Vec::new();
        let mut fulfilled: FxHashSet<Span> = FxHashSet::default();
        let mut sets_of: FxHashMap<InFile<SyntaxNode>, (Vec<Set>, bool)> = FxHashMap::default();
        for Pending { lint, at, mut diagnostic, .. } in pending {
            let (sets, test_only) = sets_of
                .entry(at.clone())
                .or_insert_with(|| self.sets_at(&at, self.name_of))
                .clone();
            let spec = levels.of(Lint::Clippy(lint), &sets);
            match spec.level {
                Level::Allow => continue,
                Level::Expect => {
                    if let Source::Node { span, .. } = &spec.source {
                        fulfilled.insert(span.clone());
                    }
                    continue;
                }
                _ => {}
            }
            diagnostic.level = spec.level;
            diagnostic.help(format!("for further information visit {}", lint.docs_url()));
            crate::level::explain(&mut diagnostic, Lint::Clippy(lint), &spec);
            originates_in(&mut diagnostic);
            if diagnostic
                .span
                .primary
                .iter()
                .any(|it| it.expansion.as_ref().is_some_and(|it| it.def_site.is_some()))
            {
                diagnostic.suggestions_disabled = true;
            }
            found.push(Found { diagnostic, test_only });
        }
        let unfulfilled = Lint::Rustc("unfulfilled_lint_expectations");
        for expectation in std::mem::take(&mut self.expectations) {
            if fulfilled.contains(&expectation.span) {
                continue;
            }
            let (sets, test_only) = self.sets_at(&expectation.at, self.name_of);
            let spec = levels.of(unfulfilled, &sets);
            if matches!(spec.level, Level::Allow | Level::Expect) {
                continue;
            }
            let mut diagnostic = Diagnostic {
                lint: "unfulfilled_lint_expectations".to_owned(),
                level: spec.level,
                message: "this lint expectation is unfulfilled".to_owned(),
                span: MultiSpan::of(expectation.span),
                children: Vec::new(),
                suggestions: Vec::new(),
                suggestions_disabled: false,
            };
            if let Some(reason) = expectation.reason {
                diagnostic.sub(Sublevel::Note, reason, MultiSpan::default(), false);
            }
            crate::level::explain(&mut diagnostic, unfulfilled, &spec);
            found.push(Found { diagnostic, test_only });
        }
        found
    }
}

/// rustc's note on a diagnostic in a macro's expansion: which macro it came from.
fn originates_in(diagnostic: &mut Diagnostic) {
    let level = if diagnostic.is_error() { "error" } else { "warning" };
    let spans = diagnostic
        .span
        .primary
        .iter()
        .chain(diagnostic.children.iter().flat_map(|it| &it.span.primary));
    let backtrace: Vec<&Expansion> = spans
        .flat_map(|span| {
            std::iter::successors(span.expansion.as_deref(), |it| it.call_site.expansion.as_deref())
        })
        .collect();
    let (Some(first), Some(last)) = (backtrace.first(), backtrace.last()) else { return };
    let and_then = match last.name != first.name {
        true => {
            format!(" which comes from the expansion of the {} `{}`", last.kind.descr(), last.name)
        }
        false => String::new(),
    };
    let note = format!(
        "this {level} originates in the {} `{}`{and_then} (in Nightly builds, run with -Z macro-backtrace for more info)",
        first.kind.descr(),
        first.name
    );
    diagnostic.note(note);
}

/// A lint attribute's level set on one name.
struct LintAttr {
    level: Level,
    target: Target,
    /// The name as written, as rustc keeps it.
    name: String,
    range: TextRange,
    reason: Option<String>,
}

/// A lint name as rustc keeps it: without spaces, with `_` for `-`.
fn canonical(name: &str) -> String {
    name.chars().filter(|it| !it.is_whitespace()).collect::<String>().replace('-', "_")
}

/// The names in a lint attribute's list, with where each is, and its `reason`.
fn lint_list(tt: &ast::TokenTree) -> (Vec<(String, TextRange)>, Option<String>) {
    let mut names = Vec::new();
    let mut reason = None;
    let mut element: Vec<SyntaxToken> = Vec::new();
    let mut flush = |element: &mut Vec<SyntaxToken>| {
        let tokens: Vec<_> = std::mem::take(element);
        let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else { return };
        if first.text() == "reason" && tokens.get(1).is_some_and(|it| it.kind() == SyntaxKind::EQ) {
            reason = tokens
                .get(2)
                .and_then(|it| ast::String::cast(it.clone()))
                .and_then(|it| it.value().ok().map(|it| it.into_owned()));
            return;
        }
        let name: String = tokens.iter().map(|it| it.text()).collect();
        let range = TextRange::new(first.text_range().start(), last.text_range().end());
        names.push((name, range));
    };
    let children: Vec<_> = tt.syntax().children_with_tokens().collect();
    let inner = children.get(1..children.len().saturating_sub(1)).unwrap_or_default();
    for child in inner {
        match child {
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => flush(&mut element),
            NodeOrToken::Token(token) if token.kind().is_trivia() => {}
            NodeOrToken::Token(token) => element.push(token.clone()),
            NodeOrToken::Node(node) => {
                element.extend(node.descendants_with_tokens().filter_map(|it| it.into_token()))
            }
        }
    }
    flush(&mut element);
    (names, reason)
}

/// The paths of a `#[derive(...)]`, in order, with where each is.
fn derive_paths(meta: &ast::Meta) -> Vec<(TextRange, String)> {
    let ast::Meta::TokenTreeMeta(meta) = meta else { return Vec::new() };
    let Some(tt) = meta.token_tree() else { return Vec::new() };
    lint_list(&tt).0.into_iter().map(|(name, range)| (range, name)).collect()
}

/// Where rustc's span of an item starts: after its attributes, doc comments and the comments
/// before it.
pub(crate) fn item_range(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| match it {
            NodeOrToken::Node(node) => {
                !matches!(node.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
            }
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// The span rustc gives a function's definition: its signature, without its body.
fn signature_range(item: &ast::Fn) -> TextRange {
    let range = item_range(item.syntax());
    let end: TextSize = match item.body() {
        Some(body) => {
            let before = body.syntax().prev_sibling_or_token();
            let mut end = body.syntax().text_range().start();
            let mut at = before;
            while let Some(it) = at {
                if !it.kind().is_trivia() {
                    end = it.text_range().end();
                    break;
                }
                at = it.prev_sibling_or_token();
            }
            end
        }
        None => range.end(),
    };
    TextRange::new(range.start(), end.max(range.start()))
}
