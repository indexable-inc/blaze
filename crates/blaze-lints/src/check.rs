//! Runs the lints over a crate. Each body is linted by a query of its own
//! ([`query::body_lints`]) over the tree of the anchor it is written in, and what of a file is
//! in no body by one per file ([`query::item_lints`]); each walks its tree as rustc's lint
//! passes walk the HIR, past what `cfg` turns off, into what attribute macros and macro calls
//! are handed and what derives expand to. The driver ([`check`]) moves what they found into
//! the files, gives each the level the attributes around it, the command line and the
//! `warnings` group give it, with the notes rustc adds of where that level came from, and says
//! what `#[expect]` was set for and not found, as rustc says it.

use std::{
    borrow::Cow,
    cell::{OnceCell, RefCell},
};

use base_db::{Db, EditionedFileId};
use blaze_diagnostics::model;
use blaze_hash::{HashMap, HashSet};
use cfg::{CfgAtom, CfgDiff, CfgExpr, CfgOptions};
use hir::{HasAttrs as _, HirFileId, InFile};
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, HasModule as _, ImplId, Lookup as _, MacroId, ModuleDefId,
    expr_store::{Body, body::body_syntax},
    item_scope::ItemScope,
    nameres::crate_def_map,
};
use hir_expand::{AstId, MacroCallId, MacroCallKind, MacroDefKind};
use span::{AstIdMap, Edition};
use syntax::{
    AstPtr, FlatTree, NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, WalkEvent,
    view::{
        self as ast, AstNode as _, AstToken as _, HasAttrs as _, HasName as _, HasVisibility as _,
    },
};

use crate::{
    Level, blaze,
    conf::Conf,
    diag::{self, Code, Diag, Expansion, MacroKind, MultiSpan, Span, Sublevel},
    famous::{self, Famous},
    level::{CommandLine, Levels, Lint, Set, Source, Spec, Target},
    lints::{self, Implemented, On},
    query::{
        self, BodyAt, Expect, FileNames, Finding, Found as FoundLint, LintSet, Lints, ModuleFile,
        Named, Place, Plan,
    },
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
    pub diagnostic: Diag,
    pub test_only: bool,
}

/// Names files as rustc names them in diagnostics: from the root of the workspace when they
/// are below it, else by their whole path.
// A generic would copy the whole driver per caller.
// dyn: asked once per file of the crate and once per diagnostic span that leaves its file.
pub type NameOf<'a> = &'a dyn Fn(base_db::FileId) -> Option<String>;

/// The diagnostics of the lints of `krate`, in the order rustc would emit them. A file of the
/// crate, or of a span a lint gives, that `name_of` does not name is the error: its lints are
/// never left out.
pub fn check(db: &Db, krate: hir::Crate, setup: &Setup, name_of: NameOf<'_>) -> Result<Vec<Found>, diag::Unnamed> {
    hir::attach_db(db, || {
        let levels = Levels::new(&setup.command_line);
        let mut driver = Driver::new(db, krate, setup, name_of, lints::command_line_on(&levels));
        driver.run()?;
        Ok(driver.finish(&levels))
    })
}

/// A file of the crate, as the driver reads it.
struct File<'q> {
    id: EditionedFileId,
    name: String,
    /// The file and range of the `mod` item declaring it.
    declared: Option<(usize, TextRange)>,
    /// Where rustc's walks come to the `mod` item declaring it, in the file that has that:
    /// the last of its [`Driver::nesting`], which `declared`'s file's nesting comes before.
    place: Option<(u32, u32)>,
    /// The lint names its text writes, with the range of what the attribute naming each is on.
    names: &'q FileNames,
    /// The lints named around the file's module, in the files declaring it.
    around: On,
}

/// A lint found, before its level is known.
struct Pending {
    lint: FoundLint,
    file: usize,
    /// Where its level is asked for.
    at: TextRange,
    diagnostic: Diag,
    /// The order rustc emits it in: by pass (pre-expansion over the root file, early, late,
    /// after late), then by where the node it was found at is in the walk over the crate,
    /// outer nodes first.
    order: (u8, Vec<(u32, u32)>),
}

/// An `#[expect]` of a lint, as a walk met it.
struct Expectation {
    /// The lint's name in the attribute.
    span: Span,
    reason: Option<String>,
    file: usize,
    at: TextRange,
}

struct Driver<'q> {
    db: &'q Db,
    krate: hir::Crate,
    conf: &'q Conf,
    cfg: &'q CfgOptions,
    /// The crate's cfg without `test`, when it has `test`.
    without_test: Option<CfgOptions>,
    name_of: NameOf<'q>,
    command_line: On,
    files: Vec<File<'q>>,
    by_id: HashMap<EditionedFileId, usize>,
    sets: HashMap<On, LintSet>,
    pending: Vec<Pending>,
    expectations: Vec<Expectation>,
}

impl<'q> Driver<'q> {
    fn new(
        db: &'q Db,
        krate: hir::Crate,
        setup: &'q Setup,
        name_of: NameOf<'q>,
        command_line: On,
    ) -> Driver<'q> {
        let cfg = krate.cfg(db);
        let test = CfgAtom::Flag(intern::sym::test);
        let without_test = cfg.check_atom(&test).then(|| {
            // clone: the crate's cfg once per crate, to take `test` out of
            let mut it = cfg.clone();
            it.apply_diff(CfgDiff::new(Vec::new(), vec![test]));
            it
        });
        Driver {
            db,
            krate,
            conf: &setup.conf,
            cfg,
            without_test,
            name_of,
            command_line,
            files: Vec::new(),
            by_id: HashMap::default(),
            sets: HashMap::default(),
            pending: Vec::new(),
            expectations: Vec::new(),
        }
    }

    fn run(&mut self) -> Result<(), diag::Unnamed> {
        let db = self.db;
        let base = self.krate.base();
        let plan = query::lint_plan(db, base);
        for &ModuleFile { file, declared } in plan.files.iter() {
            let name = (self.name_of)(file.file_id(db)).ok_or(diag::Unnamed { file: file.file_id(db) })?;
            let mut place = None;
            let mut parent_of = None;
            if let Some((parent_file, range)) = declared {
                parent_of = self.by_id.get(&parent_file).map(|&parent| (parent, range));
                place = Some((u32::from(range.start()), u32::MAX - u32::from(range.end())));
            }
            let names = query::lint_names(db, file);
            let around = match parent_of {
                Some((parent, range)) => {
                    let parent = &self.files[parent];
                    parent.around
                        | parent
                            .names
                            .names
                            .iter()
                            .filter(|it| it.owner.contains_range(range))
                            .fold(0, |on, it| on | it.lints)
                }
                None => 0,
            };
            self.by_id.insert(file, self.files.len());
            self.files.push(File { id: file, name, declared: parent_of, place, names, around });
        }
        for at in 0..self.files.len() {
            let file = &self.files[at];
            let id = file.id;
            let on = self.command_line | file.around | file.names.on;
            if on == 0 {
                continue;
            }
            let set = self.set(on);
            let lints = query::item_lints(db, id, base, set);
            self.take(at, 0, lints)?;
        }
        for &body in plan.bodies.iter() {
            self.lint_body(body)?;
        }
        Ok(())
    }

    fn lint_body(&mut self, BodyAt { def, file, offset, item }: BodyAt) -> Result<(), diag::Unnamed> {
        let db = self.db;
        let Some(&at) = self.by_id.get(&file) else { return Ok(()) };
        let named = &self.files[at];
        let mut on = self.command_line | named.around;
        // A name can only add lints when it names one not on already.
        if named.names.on & !on != 0 {
            on |= named
                .names
                .names
                .iter()
                .filter(|it| it.owner.contains_range(item) || item.contains_range(it.at))
                .fold(0, |on, it| on | it.lints);
        }
        if on == 0 {
            return Ok(());
        }
        let set = self.set(on);
        let lints = query::body_lints(db, def, set);
        self.take(at, u32::from(offset), lints)
    }

    /// Where rustc's walks over the crate come to file `at`: at the `mod` item declaring it,
    /// in the file that has that, and so on up to the root; outermost first, into `out`.
    fn nesting(&self, at: usize, out: &mut Vec<(u32, u32)>) {
        let file = &self.files[at];
        if let Some((parent, _)) = file.declared {
            self.nesting(parent, out);
        }
        out.extend(file.place);
    }

    /// The interned set of `on` with the crate's configuration, made once per set.
    fn set(&mut self, on: On) -> LintSet {
        let (db, conf) = (self.db, self.conf);
        // clone: the configuration, once per distinct set of a crate
        *self.sets.entry(on).or_insert_with(|| LintSet::new(db, on, conf.clone()))
    }

    /// Takes what a query found in the file `at`, moving its ranges by `offset`.
    fn take(&mut self, at: usize, offset: u32, lints: &Lints) -> Result<(), diag::Unnamed> {
        let file = &self.files[at];
        let moved = |range: TextRange| range + TextSize::new(offset);
        for Finding { lint, at: found_at, place, diagnostic } in &lints.findings {
            // clone: a finding's diagnostic, out of the query's value, to be given its level
            let mut diagnostic = diagnostic.clone();
            diag::place_diag(&mut diagnostic, offset, &file.name, self.name_of)?;
            let place = match *place {
                Place::Node(range) => {
                    let range = moved(range);
                    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
                }
                Place::At { at, then } => (at + offset, then),
            };
            let phase = match lint {
                FoundLint::Clippy(lint) => lints::phase(*lint, at == 0),
                FoundLint::Blaze(_) => 2,
            };
            // What clippy gathers to say after the late pass it says in the order of the
            // source map, which has the crate's files in the order rustc read them.
            let key = match phase {
                // alloc: a finding's order key, once per finding
                3 => vec![(u32::try_from(at).unwrap_or(u32::MAX), 0), place],
                _ => {
                    // alloc: a finding's order key, once per finding
                    let mut key = Vec::new();
                    self.nesting(at, &mut key);
                    key.push(place);
                    key
                }
            };
            self.pending.push(Pending {
                lint: *lint,
                file: at,
                at: moved(*found_at),
                diagnostic,
                order: (phase, key),
            });
        }
        for Expect { name, reason, owner } in &lints.expects {
            self.expectations.push(Expectation {
                span: Span::new(&file.name, moved(*name)),
                reason: reason.clone(),
                file: at,
                at: moved(*owner),
            });
        }
        Ok(())
    }

    /// The level sets around `at` of `file`, outermost first, and whether `cfg(test)` alone
    /// keeps it.
    fn sets_at(&self, file: usize, at: TextRange) -> (Vec<Set>, bool) {
        let db = self.db;
        let mut owners: Vec<(usize, ast::AnyHasAttrs<'q>)> = Vec::new();
        let mut place = Some((file, at));
        while let Some((file, at)) = place.take() {
            let root = self.files[file].id.parse(db).root();
            if root.text_range().contains_range(at) {
                let start = match root.covering_element(at) {
                    NodeOrToken::Node(it) => Some(it),
                    NodeOrToken::Token(it) => it.parent(),
                };
                for node in start.into_iter().flat_map(|it| it.ancestors()) {
                    if let Some(owner) = ast::AnyHasAttrs::cast(node) {
                        owners.push((file, owner));
                    }
                }
            }
            place = self.files[file].declared;
        }
        let mut sets = Vec::new();
        let mut test_only = false;
        for &(file, owner) in owners.iter().rev() {
            if let Some(without_test) = &self.without_test {
                for_each_meta(self.cfg, owner.attrs(), |meta| {
                    if let ast::Meta::CfgMeta(meta) = meta
                        && let Some(predicate) = meta.cfg_predicate()
                    {
                        let cfg = CfgExpr::parse_from_view(predicate);
                        test_only |= self.cfg.check(&cfg) != Some(false)
                            && without_test.check(&cfg) == Some(false);
                    }
                });
            }
            let name = &self.files[file].name;
            for LintAttr { level, target, name: attr_name, range, reason } in lint_attrs(self.cfg, owner)
            {
                let source = Source::Node { name: attr_name, span: Span::new(name, range), reason };
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
        let mut fulfilled: HashSet<Span> = HashSet::default();
        let mut sets_of: HashMap<(usize, TextRange), (Vec<Set>, bool)> = HashMap::default();
        for Pending { lint, file, at, mut diagnostic, .. } in pending {
            let (sets, test_only) =
                sets_of.entry((file, at)).or_insert_with(|| self.sets_at(file, at)).clone();
            // blaze's own lints add their link after their own help, as clippy's order is.
            let (asked, docs) = match lint {
                FoundLint::Clippy(lint) => (Lint::Clippy(lint.info()), Some(lint.info().docs_url())),
                FoundLint::Blaze(lint) => (Lint::Blaze(lint), None),
            };
            let spec = levels.of(asked, &sets);
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
            diagnostic.level = written_at(spec.level);
            if let Some(docs) = docs {
                diagnostic.help(format!("for further information visit {docs}"));
            }
            crate::level::explain(&mut diagnostic, asked, &spec);
            if diagnostic
                .span
                .primary
                .iter()
                .any(|it| it.expansion.as_ref().is_some_and(|it| it.kind != MacroKind::Bang))
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
            let (sets, test_only) = self.sets_at(expectation.file, expectation.at);
            let spec = levels.of(unfulfilled, &sets);
            if matches!(spec.level, Level::Allow | Level::Expect) {
                continue;
            }
            let code = Code::Lint("unfulfilled_lint_expectations".to_owned());
            let mut diagnostic = Diag::new(written_at(spec.level), Some(code), "this lint expectation is unfulfilled");
            diagnostic.span = MultiSpan::of(expectation.span);
            if let Some(reason) = expectation.reason {
                diagnostic.sub(Sublevel::Note, reason, MultiSpan::default(), false);
            }
            crate::level::explain(&mut diagnostic, unfulfilled, &spec);
            found.push(Found { diagnostic, test_only });
        }
        found
    }
}

/// The lint names `file` writes, each with what the attribute it is in is on:
/// [`query::lint_names`].
pub(crate) fn names_of(db: &Db, file: EditionedFileId) -> FileNames {
    let text = db.file_text(file.file_id(db)).text(db);
    // alloc: the query's value, once per edit of the file
    let mut names = Vec::new();
    let mut on = 0;
    let mut root = None;
    lints::names_in(text, |at, lints| {
        let root = *root.get_or_insert_with(|| file.parse(db).root());
        let owner = attr_owner(root, at).unwrap_or(at);
        names.push(Named { at, owner, lints });
        on |= lints;
    });
    FileNames { names: names.into_boxed_slice(), on }
}

/// The files and bodies the driver walks of `krate`: [`query::lint_plan`].
pub(crate) fn plan_of(db: &Db, krate: base_db::Crate) -> Plan {
    // alloc: the query's value and its worklist, once per crate and edit
    let mut files: Vec<ModuleFile> = Vec::new();
    let mut seen: HashSet<EditionedFileId> = HashSet::default();
    for module in hir::Crate::from(krate).modules(db) {
        let Some(file) = module.definition_source_file_id(db).file_id() else { continue };
        if !seen.insert(file) {
            continue;
        }
        let declared = module.declaration_source_range(db).map(|range| {
            let range = range.original_node_file_range_rooted(db);
            (range.file_id, range.range)
        });
        files.push(ModuleFile { file, declared });
    }
    // alloc: the bodies' worklist, once per crate and edit
    let mut defs = Vec::new();
    let def_map = crate_def_map(db, krate);
    for (_, module) in def_map.modules(db) {
        bodies_of(db, module.scope(db), &mut defs);
    }
    let mut bodies = Vec::with_capacity(defs.len());
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        for (_, block) in Body::of(db, def).blocks(db) {
            for (_, module) in block.modules(db) {
                bodies_of(db, module.scope(db), &mut defs);
            }
        }
        bodies.extend(body_at(db, def));
    }
    Plan { files: files.into_boxed_slice(), bodies: bodies.into_boxed_slice() }
}

/// Where `def`'s body is in the file the driver names it by.
fn body_at(db: &Db, def: DefWithBodyId) -> Option<BodyAt> {
    let (file, offset, item) = match body_syntax(db, def) {
        Some(syntax) => {
            let file = syntax.file_id.file_id()?;
            let offset = Body::anchor_offset(db, def)?;
            (file, offset, syntax.item.text_range() + offset)
        }
        None => {
            let file_id = def.erased_ast_id(db).file_id;
            let call = file_id.macro_file()?;
            if matches!(call.loc(db).kind, MacroCallKind::Derive { .. }) {
                return None;
            }
            let outer = file_id.original_call_node_view(db)?;
            (outer.file_id, TextSize::new(0), outer.value.text_range())
        }
    };
    Some(BodyAt { def, file, offset, item })
}

/// The range of what the attribute holding `at` is on, if `at` is in one.
fn attr_owner(root: ast::Node<'_>, at: TextRange) -> Option<TextRange> {
    let start = match root.covering_element(at) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => it.parent()?,
    };
    let attr = start.ancestors().find(|it| it.kind() == SyntaxKind::ATTR)?;
    let mut owner = attr.parent()?;
    while matches!(
        owner.kind(),
        SyntaxKind::STMT_LIST
            | SyntaxKind::BLOCK_EXPR
            | SyntaxKind::ITEM_LIST
            | SyntaxKind::ASSOC_ITEM_LIST
            | SyntaxKind::EXTERN_ITEM_LIST
    ) && let Some(parent) = owner.parent()
    {
        owner = parent;
    }
    Some(owner.text_range())
}

/// The bodies of the items of `scope`: its functions, consts, statics and enum discriminants,
/// and those of its traits and impls.
fn bodies_of(db: &Db, scope: &ItemScope, out: &mut Vec<DefWithBodyId>) {
    let assoc = |out: &mut Vec<DefWithBodyId>, items: &[(hir::Name, AssocItemId)]| {
        for &(_, item) in items {
            match item {
                AssocItemId::FunctionId(it) => out.push(it.into()),
                AssocItemId::ConstId(it) => out.push(it.into()),
                AssocItemId::TypeAliasId(_) => {}
            }
        }
    };
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => out.push(it.into()),
            ModuleDefId::ConstId(it) => out.push(it.into()),
            ModuleDefId::StaticId(it) => out.push(it.into()),
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                let variants = it.enum_variants(db);
                for &(variant, _) in variants.variants.values() {
                    if variants.has_explicit_discriminant(variant) {
                        out.push(variant.into());
                    }
                }
            }
            ModuleDefId::TraitId(it) => assoc(out, &it.trait_items(db).items),
            _ => {}
        }
    }
    for it in scope.unnamed_consts() {
        out.push(it.into());
    }
    for it in scope.impls() {
        assoc(out, &it.impl_items(db).items);
    }
}

fn erased<N>(id: AstId<N>) -> (HirFileId, span::ErasedFileAstId) {
    (id.file_id, id.value.erase())
}

/// The lints of `on` over the body of `def`: [`query::body_lints`].
pub(crate) fn lint_body(db: &Db, def: DefWithBodyId, on: On, conf: &Conf) -> Lints {
    let krate = def.krate(db);
    match body_syntax(db, def) {
        Some(syntax) => {
            let Some(file) = syntax.file_id.file_id() else { return Lints::default() };
            let tree = syntax.tree(db);
            let anchor = ast::Node::root(&tree);
            let Some(owner) = ast::try_to_node(syntax.item, anchor) else {
                return Lints::default();
            };
            let Some(root) = body_root(def, owner) else { return Lints::default() };
            let base = Base {
                file,
                tree: Some(anchor.file()),
                root: anchor,
                text: syntax.text(),
                ast_id_map: &syntax.ast_id_map,
                relative: true,
            };
            let mut cx = Cx::new(db, krate, conf, on, base, root, Some(def));
            cx.walk(root);
            cx.done()
        }
        None => {
            let InFile { file_id, value: ast_id } = def.erased_ast_id(db);
            let HirFileId::MacroFile(call) = file_id else { return Lints::default() };
            let file = file_id.original_file(db);
            let file_root = file.parse(db).root();
            let base = Base {
                file,
                tree: Some(file_root.file()),
                root: file_root,
                text: db.file_text(file.file_id(db)).text(db),
                ast_id_map: HirFileId::from(file).ast_id_map(db),
                relative: false,
            };
            let mut cx = Cx::new(db, krate, conf, on, base, file_root, Some(def));
            let expansion = cx.file_root(file_id);
            let Some(ptr) = file_id.ast_id_map(db).try_get_erased(ast_id) else {
                return Lints::default();
            };
            let Some(owner) = ast::try_to_node(ptr, expansion) else { return Lints::default() };
            let Some(root) = body_root(def, owner) else { return Lints::default() };
            let Some((call_at, input, in_bang)) = cx.outermost_call(call) else {
                return Lints::default();
            };
            cx.root = root;
            cx.walking = Walking { input: Some(input), call_at: Some(call_at), in_bang, ..Walking::default() };
            cx.walk(root);
            cx.done()
        }
    }
}

/// The lints of `on` over what of `file` is in no body: [`query::item_lints`].
pub(crate) fn lint_items(
    db: &Db,
    file: EditionedFileId,
    krate: base_db::Crate,
    on: On,
    conf: &Conf,
) -> Lints {
    let root = file.parse(db).root();
    let base = Base {
        file,
        tree: Some(root.file()),
        root,
        text: db.file_text(file.file_id(db)).text(db),
        ast_id_map: HirFileId::from(file).ast_id_map(db),
        relative: false,
    };
    let mut cx = Cx::new(db, krate, conf, on, base, root, None);
    cx.walk(root);
    cx.done()
}

/// Where the walk of `def`'s body starts: its item, or for an enum variant, its discriminant.
fn body_root(def: DefWithBodyId, owner: ast::Node<'_>) -> Option<ast::Node<'_>> {
    match def {
        DefWithBodyId::VariantId(_) => Some(ast::Variant::cast(owner)?.const_arg()?.syntax()),
        _ => Some(owner),
    }
}

/// The tree a query's findings are of: a body's anchor, or the file.
struct Base<'q, 't> {
    file: EditionedFileId,
    /// The tree [`Base::root`] is the root of.
    tree: Option<&'t FlatTree>,
    root: ast::Node<'t>,
    text: &'q str,
    /// The ids of the nodes of [`Base::root`]'s tree.
    ast_id_map: &'q AstIdMap,
    /// Whether the tree is a body's anchor alone, whose ranges begin at the anchor.
    relative: bool,
}

/// Where the walk is.
#[derive(Default)]
struct Walking {
    /// The macro the walk is in the expansion of, if it is in one.
    expansion: Option<Expansion>,
    /// When the walk is in what a macro call of the file expands to, the arguments of the
    /// call: what of the expansion is written there is linted as code of the file, as rustc
    /// lints what a macro is handed.
    input: Option<TextRange>,
    /// The outermost macro call the walk is in, as written in the file.
    call_at: Option<TextRange>,
    /// How many macro calls deep the walk is.
    depth: u32,
    /// Whether the walk is in what a function-like macro call expands to, not only in what an
    /// attribute macro makes of an item.
    in_bang: bool,
}

/// What the items a query walks are, by where they are written.
#[derive(Default)]
struct Items {
    defs: HashMap<Key, ModuleDefId>,
    impls: HashMap<Key, ImplId>,
    /// The macro calls of items and statements, and the items an attribute macro is on.
    calls: HashMap<Key, MacroCallId>,
    /// The calls of each `#[derive]` of an ADT, in the order of the attributes.
    derives: HashMap<Key, Vec<Box<[Option<MacroCallId>]>>>,
}

type Key = (HirFileId, SyntaxNodePtr);

/// The context a lint is run in: what is linted, the tree walked and where the walk is.
pub(crate) struct Cx<'q: 't, 't> {
    db: &'q Db,
    krate: base_db::Crate,
    conf: &'q Conf,
    on: On,
    cfg: &'q CfgOptions,
    edition: Edition,
    base: Base<'q, 't>,
    /// Where the walk starts: a body's item, or the file.
    root: ast::Node<'t>,
    def: Option<DefWithBodyId>,
    analysis: OnceCell<Option<hir::BodyAnalysis<'q>>>,
    sema: OnceCell<hir::view::Semantics<'q>>,
    items: OnceCell<Items>,
    /// The crate's [`query::famous_defs`], read once per query.
    famous_defs: OnceCell<&'q [Option<hir::ModuleDef>]>,
    /// The crate's [`lints::disallowed::disallowed_defs`], read once per query.
    disallowed: OnceCell<&'q lints::disallowed::Resolved>,
    /// The expansions read, with the call of each.
    trees: RefCell<Vec<(&'q FlatTree, MacroCallId)>>,
    walking: Walking,
    found: Vec<Finding>,
    expects: Vec<Expect>,
}

impl<'q: 't, 't> Cx<'q, 't> {
    fn new(
        db: &'q Db,
        krate: base_db::Crate,
        conf: &'q Conf,
        on: On,
        base: Base<'q, 't>,
        root: ast::Node<'t>,
        def: Option<DefWithBodyId>,
    ) -> Cx<'q, 't> {
        Cx {
            db,
            krate,
            conf,
            on,
            cfg: krate.cfg_options(db),
            edition: base.file.edition(db),
            base,
            root,
            def,
            analysis: OnceCell::new(),
            sema: OnceCell::new(),
            items: OnceCell::new(),
            famous_defs: OnceCell::new(),
            disallowed: OnceCell::new(),
            trees: RefCell::new(Vec::new()),
            walking: Walking::default(),
            found: Vec::new(),
            expects: Vec::new(),
        }
    }

    fn done(self) -> Lints {
        Lints { findings: self.found, expects: self.expects }
    }

    pub(crate) fn db(&self) -> &'q Db {
        self.db
    }

    pub(crate) fn conf(&self) -> &'q Conf {
        self.conf
    }

    pub(crate) fn krate(&self) -> hir::Crate {
        hir::Crate::from(self.krate)
    }

    pub(crate) fn edition(&self) -> Edition {
        self.edition
    }

    /// Whether `lint` can be at a level that is not `allow` where the query looks: a lint
    /// that cannot is not run.
    #[inline]
    pub(crate) fn on(&self, lint: Implemented) -> bool {
        self.on & lint.bit() != 0
    }

    /// Whether some lint of `lints` is [`Cx::on`].
    #[inline]
    pub(crate) fn on_any(&self, lints: On) -> bool {
        self.on & lints != 0
    }

    #[inline]
    pub(crate) fn on_blaze(&self, lint: blaze::Lint) -> bool {
        self.on & lint.bit() != 0
    }

    /// Where the walk starts: the item of the body linted, or the file.
    pub(crate) fn root(&self) -> ast::Node<'t> {
        self.root
    }

    /// The text the ranges of [`Cx::range`] index: the anchor's or the file's.
    pub(crate) fn text(&self) -> &'q str {
        self.base.text
    }

    /// The source text at `range` of [`Cx::text`].
    pub(crate) fn snippet(&self, range: TextRange) -> &'q str {
        self.base.text.get(std::ops::Range::<usize>::from(range)).unwrap_or_default()
    }

    /// Where `node`, of the tree walked or of an expansion, is written in the tree of
    /// [`Cx::text`]; `None` for what a macro made up.
    pub(crate) fn range(&self, node: ast::Node<'_>) -> Option<TextRange> {
        match self.call_of(node) {
            None => Some(node.text_range()),
            Some(call) => self.upmap(call, node.text_range()),
        }
    }

    pub(crate) fn original_range(&self, node: ast::Node<'_>) -> Option<TextRange> {
        self.range(node)
    }

    /// The source of `node` as written in the file, or its text in the expansion it is in.
    pub(crate) fn source_of(&self, node: ast::Node<'t>) -> Cow<'t, str> {
        match self.range(node).and_then(|it| self.base.text.get(std::ops::Range::<usize>::from(it))) {
            Some(text) => Cow::Borrowed(text),
            None => Cow::Borrowed(node.text()),
        }
    }

    /// Whether the walk is in the file's own tree, not in an expansion of a macro.
    pub(crate) fn in_file(&self) -> bool {
        self.walking.expansion.is_none() && self.walking.input.is_none()
    }

    /// The outermost macro call of the file the walk is in the expansion of, as written there.
    pub(crate) fn call_at(&self) -> Option<TextRange> {
        self.walking.call_at
    }

    /// Whether the walk is in the arguments of a function-like macro call, as `vec!` is in
    /// `assert!(f(vec![]))`.
    pub(crate) fn in_bang_call(&self) -> bool {
        self.walking.in_bang
    }

    /// Whether `node` is code of the file: outside macro calls, or handed to the call the walk
    /// is in the expansion of, all of it.
    pub(crate) fn is_written(&self, node: ast::Node<'_>) -> bool {
        match self.walking.input {
            None => true,
            Some(input) => self.range(node).is_some_and(|it| input.contains_range(it)),
        }
    }

    /// The macro whose expansion the walk is in, when it is in what a derive expands to.
    pub(crate) fn expansion(&self) -> Option<&Expansion> {
        self.walking.expansion.as_ref()
    }

    /// The macro `call` calls, by name, if it is one of `core`'s, `alloc`'s or `std`'s, as
    /// clippy's diagnostic items name them.
    pub(crate) fn std_macro(&self, call: ast::MacroCall<'t>) -> Option<intern::Symbol> {
        let db = self.db;
        let def = &self.call_id(call)?.loc(db).def;
        if !def.krate.data(db).origin.is_lang() {
            return None;
        }
        let name = match def.ast_id() {
            either::Either::Left(id) => ast::Macro::cast(self.node_of(id)?)?.name()?,
            either::Either::Right(id) => self.view_of(id)?.name()?,
        };
        Some(intern::Symbol::intern(name.text()))
    }

    /// The root of what `call` expands to.
    pub(crate) fn expand(&self, call: ast::MacroCall<'t>) -> Option<ast::Node<'q>> {
        let id = self.call_id(call)?;
        Some(self.file_root(HirFileId::MacroFile(id)))
    }

    /// The expressions written in `call`'s arguments, as its expansion has them, in order:
    /// each the outermost there whose source is in the arguments. `out` is cleared first.
    pub(crate) fn macro_args(
        &self,
        call: ast::MacroCall<'t>,
        out: &mut Vec<(TextRange, ast::Expr<'t>)>,
    ) {
        out.clear();
        let Some(tt) = call.token_tree() else { return };
        let Some(range) = self.range(tt.syntax()) else { return };
        if range.len() < TextSize::new(2) {
            return;
        }
        let inner = TextRange::new(range.start() + TextSize::new(1), range.end() - TextSize::new(1));
        let Some(expansion) = self.expand(call) else { return };
        self.args_in(expansion, inner, out, 0);
        out.sort_unstable_by_key(|(range, _)| (range.start(), range.end()));
        out.dedup_by_key(|(range, _)| *range);
    }

    fn args_in(
        &self,
        root: ast::Node<'t>,
        inner: TextRange,
        found: &mut Vec<(TextRange, ast::Expr<'t>)>,
        depth: u32,
    ) {
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if let Some(expr) = ast::Expr::cast(node)
                && let Some(range) = self.range(node)
                && inner.contains_range(range)
                && !found.iter().any(|(it, _)| it.contains_range(range))
            {
                found.push((range, expr));
                events.skip_subtree();
                continue;
            }
            if let Some(call) = ast::MacroCall::cast(node) {
                if depth < 4
                    && let Some(expansion) = self.expand(call)
                {
                    self.args_in(expansion, inner, found, depth + 1);
                }
                events.skip_subtree();
            }
        }
    }

    /// The body's types and resolutions; `None` outside a body.
    pub(crate) fn analysis(&self) -> Option<&hir::BodyAnalysis<'q>> {
        self.analysis
            .get_or_init(|| {
                let def = self.def?;
                Some(hir::BodyAnalysis::new(self.db, hir::DefWithBody::from(def)))
            })
            .as_ref()
    }

    fn ptr<N: syntax::AstNode + syntax::view::Typed>(&self, node: N::Of<'t>) -> InFile<AstPtr<N>> {
        InFile::new(self.file_of(node.syntax()), AstPtr::<N>::of(node))
    }

    /// The type of `expr`, before adjustments.
    pub(crate) fn ty(&self, expr: ast::Expr<'t>) -> Option<hir::Type<'q>> {
        let ptr = self.ptr::<syntax::ast::Expr>(expr);
        Some(self.analysis()?.type_of_expr(ptr)?.original)
    }

    /// The type of `expr`, after adjustments.
    pub(crate) fn adjusted_ty(&self, expr: ast::Expr<'t>) -> Option<hir::Type<'q>> {
        let ptr = self.ptr::<syntax::ast::Expr>(expr);
        Some(self.analysis()?.type_of_expr(ptr)?.adjusted())
    }

    pub(crate) fn adjustments(&self, expr: ast::Expr<'t>) -> Option<Vec<hir::Adjustment<'q>>> {
        self.analysis()?.expr_adjustments(self.ptr::<syntax::ast::Expr>(expr))
    }

    pub(crate) fn method(&self, call: ast::MethodCallExpr<'t>) -> Option<hir::Function> {
        self.analysis()?.resolve_method_call(self.ptr::<syntax::ast::MethodCallExpr>(call))
    }

    /// What `path` resolves to: in the body through its analysis, elsewhere (a signature, a
    /// field, an item's type) in the scope of the item it is written in.
    pub(crate) fn path(&self, path: ast::Path<'t>) -> Option<hir::PathResolution<'q>> {
        if self.def.is_some() {
            return self.analysis()?.resolve_path(self.ptr::<syntax::ast::Path>(path));
        }
        let path = ast::Path::cast(self.in_sema(path.syntax())?)?;
        self.sema().resolve_path(path)
    }

    pub(crate) fn pat_ty(&self, pat: ast::Pat<'t>) -> Option<hir::Type<'q>> {
        Some(self.analysis()?.type_of_pat(self.ptr::<syntax::ast::Pat>(pat))?.original)
    }

    pub(crate) fn local(&self, pat: ast::IdentPat<'t>) -> Option<hir::Local<'q>> {
        self.analysis()?.binding(self.ptr::<syntax::ast::IdentPat>(pat))
    }

    /// A written type lowered in the scope of what it is written in.
    pub(crate) fn resolve_type(&self, ty: ast::Type<'t>) -> Option<hir::Type<'q>> {
        if self.def.is_some() {
            return self.analysis()?.resolve_type(self.ptr::<syntax::ast::Type>(ty));
        }
        let ty = ast::Type::cast(self.in_sema(ty.syntax())?)?;
        self.sema().resolve_type(ty)
    }

    /// The macro `call` calls.
    pub(crate) fn macro_def(&self, call: ast::MacroCall<'t>) -> Option<hir::Macro> {
        if self.def.is_some() {
            return self.analysis()?.resolve_macro_call(self.ptr::<syntax::ast::MacroCall>(call));
        }
        let call = ast::MacroCall::cast(self.in_sema(call.syntax())?)?;
        self.sema().resolve_macro_call(call)
    }

    /// The `macro_rules!` or `macro` item defining `mac`; `None` for a proc macro.
    pub(crate) fn macro_source(&self, mac: hir::Macro) -> Option<ast::Macro<'q>> {
        match MacroId::from(mac) {
            MacroId::MacroRulesId(it) => self.view_of(it.lookup(self.db).id).map(ast::Macro::MacroRules),
            MacroId::Macro2Id(it) => self.view_of(it.lookup(self.db).id).map(ast::Macro::MacroDef),
            MacroId::ProcMacroId(_) => None,
        }
    }

    /// `name` resolved as a one-segment path in the scope at `at`.
    pub(crate) fn resolve_name(&self, at: ast::Node<'t>, name: &str) -> Option<hir::PathResolution<'q>> {
        if self.def.is_some() {
            let name = hir::Name::new_symbol_root(intern::Symbol::intern(name));
            let at = InFile::new(self.file_of(at), ast::ptr(at));
            return self.analysis()?.resolve_name(at, &name);
        }
        let at = self.in_sema(at)?;
        self.sema().scope(at)?.speculative_resolve_mod_path(&one_segment_path(name))
    }

    pub(crate) fn famous(&self) -> Famous<'q> {
        Famous::new(self.db, self.krate())
    }

    pub(crate) fn lang_trait(&self, item: hir::LangItem) -> Option<hir::Trait> {
        hir::Trait::lang(self.db, self.krate(), item)
    }

    /// The trait at `path` in the crate `path[0]` (`core`, `alloc`, `std`).
    pub(crate) fn std_trait(&self, path: &[&str]) -> Option<hir::Trait> {
        match self.std_def(path)? {
            hir::ModuleDef::Trait(it) => Some(it),
            _ => None,
        }
    }

    /// The item at `path` in the crate `path[0]` (`core`, `alloc`, `std`): one perfect hash of
    /// the path into what the crate resolved of [`famous::KNOWN`].
    /// The `disallowed-*` entries of the crate's configuration, resolved.
    pub(crate) fn disallowed(&self) -> &'q lints::disallowed::Resolved {
        self.disallowed.get_or_init(|| {
            // clone: the configuration, once per query that reads the entries
            let key = lints::disallowed::ConfKey::new(self.db, self.conf.clone());
            lints::disallowed::disallowed_defs(self.db, self.krate, key)
        })
    }

    pub(crate) fn std_def(&self, path: &[&str]) -> Option<hir::ModuleDef> {
        let krate = path.first()?;
        if !matches!(*krate, "core" | "alloc" | "std") {
            return None;
        }
        let Some(at) = famous::known(path) else {
            // alloc: the message of a bug, a lint looking up a path left out of the table
            panic!("`{}` is not in `famous::KNOWN`", path.join(":"));
        };
        let defs = self.famous_defs.get_or_init(|| &**query::famous_defs(self.db, self.krate));
        defs[at]
    }

    /// The type `trait_`'s associated type `name` is for `ty`.
    pub(crate) fn assoc_ty(
        &self,
        ty: &hir::Type<'q>,
        trait_: hir::Trait,
        name: &str,
    ) -> Option<hir::Type<'q>> {
        let db = self.db;
        let alias = trait_.items(db).into_iter().find_map(|it| match it {
            hir::AssocItem::TypeAlias(alias) if alias.name(db).as_str() == name => Some(alias),
            _ => None,
        })?;
        ty.normalize_trait_assoc_type(db, &[], alias)
    }

    /// The item of this crate `node` is.
    pub(crate) fn def(&self, node: ast::Node<'t>) -> Option<hir::ModuleDef> {
        let key = (self.file_of(node), ast::ptr(node));
        self.items().defs.get(&key).map(|&it| hir::ModuleDef::from(it))
    }

    pub(crate) fn fn_def(&self, f: ast::Fn<'t>) -> Option<hir::Function> {
        match self.def(f.syntax())? {
            hir::ModuleDef::Function(it) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn adt_def(&self, adt: ast::Adt<'t>) -> Option<hir::Adt> {
        match self.def(adt.syntax())? {
            hir::ModuleDef::Adt(it) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn impl_def(&self, imp: ast::Impl<'t>) -> Option<hir::Impl> {
        let key = (self.file_of(imp.syntax()), ast::ptr(imp.syntax()));
        self.items().impls.get(&key).map(|&it| hir::Impl::from(it))
    }

    /// The view of where `adt` is written, in whichever file.
    pub(crate) fn adt_source(&self, adt: hir::Adt) -> Option<ast::Adt<'q>> {
        let db = self.db;
        match AdtId::from(adt) {
            AdtId::StructId(it) => self.view_of(it.lookup(db).id).map(ast::Adt::Struct),
            AdtId::UnionId(it) => self.view_of(it.lookup(db).id).map(ast::Adt::Union),
            AdtId::EnumId(it) => self.view_of(it.lookup(db).id).map(ast::Adt::Enum),
        }
    }

    /// rustc's `hir_span_if_local`: where `adt` is written, from after its attributes, when it
    /// is of the crate being linted and written in a file.
    pub(crate) fn def_span(&self, adt: hir::Adt) -> Option<Span> {
        let db = self.db;
        if adt.module(db).krate(db) != self.krate() {
            return None;
        }
        let file = match AdtId::from(adt) {
            AdtId::StructId(it) => it.lookup(db).id.file_id,
            AdtId::UnionId(it) => it.lookup(db).id.file_id,
            AdtId::EnumId(it) => it.lookup(db).id.file_id,
        };
        let file = file.file_id()?;
        let source = self.adt_source(adt)?;
        Some(diag::of_file(file.file_id(db), item_range(source.syntax())))
    }

    /// rustc's `effective_visibilities.is_exported` of the item, associated item or field at
    /// `node`: another crate can name it.
    pub(crate) fn is_exported(&self, node: ast::Node<'t>) -> bool {
        let db = self.db;
        let exported = query::exported(db, self.krate);
        let public = match (ast::RecordField::cast(node), ast::TupleField::cast(node)) {
            (Some(field), _) => Some(field.visibility()),
            (_, Some(field)) => Some(field.visibility()),
            _ => None,
        };
        if let Some(visibility) = public {
            // A variant's fields are as visible as its enum.
            let in_variant = node.ancestors().skip(1).any(|it| it.kind() == SyntaxKind::VARIANT);
            return (in_variant || visibility.is_some_and(|it| it.syntax().text() == "pub"))
                && node.ancestors().skip(1).find_map(ast::Adt::cast).is_some_and(|adt| {
                    self.adt_def(adt).is_some_and(|it| exported.has(db, ModuleDefId::from(AdtId::from(it))))
                });
        }
        let key = (self.file_of(node), ast::ptr(node));
        let items = self.items();
        if let Some(&imp) = items.impls.get(&key) {
            return exported.has_impl(db, imp);
        }
        items.defs.get(&key).is_some_and(|&def| exported.has(db, def))
    }

    /// clippy's `is_in_test`: inside a `#[test]` function, or under an item with
    /// `#[cfg(test)]`, the modules the walk is in included.
    pub(crate) fn is_in_test(&self, node: ast::Node<'t>) -> bool {
        let mut node = Some(node);
        while let Some(start) = node.take() {
            if start.ancestors().filter_map(ast::AnyHasAttrs::cast).any(|it| self.is_test(it)) {
                return true;
            }
            let top = start.ancestors().last().unwrap_or(start);
            node = self.call_of(top).and_then(|call| self.call_node(call));
        }
        let db = self.db;
        let module = match self.def {
            Some(def) => def.module(db),
            None => return false,
        };
        // The modules' attributes from their attribute queries, not their files' trees, so that
        // an edit elsewhere in those files leaves a body's query as it is.
        let mut module = Some(hir::Module::from(module));
        while let Some(it) = module.take() {
            if it.attrs(db).cfgs(db).is_some_and(names_test) {
                return true;
            }
            module = it.parent(db);
        }
        false
    }

    fn is_test(&self, owner: ast::AnyHasAttrs<'_>) -> bool {
        let mut test = false;
        for_each_meta(self.cfg, owner.attrs(), |meta| match meta {
            ast::Meta::CfgMeta(meta) => {
                test |= meta.cfg_predicate().is_some_and(|it| names_test(&CfgExpr::parse_from_view(it)))
            }
            _ => test |= meta.simple_name() == Some("test") && ast::Fn::can_cast(owner.syntax().kind()),
        });
        test
    }

    /// The span of `range` of [`Cx::text`], with the expansion the walk is in.
    pub(crate) fn span(&self, range: TextRange) -> Span {
        let mut span = diag::in_query(range);
        // clone: the expansion a span is in, once per span of a lint found in one
        span.expansion = self.walking.expansion.clone().map(Box::new);
        span
    }

    /// The span rustc gives an item: from after its attributes and doc comments.
    pub(crate) fn item_span(&self, node: ast::Node<'t>) -> Option<Span> {
        let range = self.range(node)?;
        if self.walking.expansion.is_some() {
            return Some(self.span(range));
        }
        match self.call_of(node) {
            None => Some(self.span(item_range(node))),
            Some(call) => Some(self.span(self.upmap(call, item_range(node))?)),
        }
    }

    /// Reports `lint` at `span`, its level asked for at `at`; what it returns is decorated as
    /// clippy's lint decorates it.
    pub(crate) fn lint(
        &mut self,
        lint: Implemented,
        at: ast::Node<'t>,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diag {
        self.lint_when(lint, at, at, span, message)
    }

    /// Reports `lint` at `span`, its level asked for at `at`, in the order rustc would come to
    /// `when` in its walk: for a lint run on a node that it reports of a node inside it.
    pub(crate) fn lint_when(
        &mut self,
        lint: Implemented,
        at: ast::Node<'t>,
        when: ast::Node<'t>,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diag {
        let span: MultiSpan = span.into();
        // rustc's passes run a lint as they come to a node, outer nodes before inner ones.
        let place = match self.original_range(when) {
            Some(range) => Place::Node(range),
            None => Place::At { at: span.primary_span().map_or(0, |it| it.lo), then: 0 },
        };
        self.report(FoundLint::Clippy(lint), at, place, span, message)
    }

    /// Reports `lint` at `span`, its level asked for at `at`, as rustc would once its walk has
    /// left `after`: for what clippy says when a body has been walked.
    pub(crate) fn lint_after(
        &mut self,
        lint: Implemented,
        at: ast::Node<'t>,
        after: ast::Node<'t>,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diag {
        let span: MultiSpan = span.into();
        let place = match self.original_range(after) {
            Some(range) => Place::At { at: u32::from(range.end()).saturating_sub(1), then: u32::MAX },
            None => Place::At { at: span.primary_span().map_or(0, |it| it.hi), then: u32::MAX },
        };
        self.report(FoundLint::Clippy(lint), at, place, span, message)
    }

    /// Reports blaze's own `lint` at `span`, its level asked for at `at`.
    pub(crate) fn blaze(
        &mut self,
        lint: blaze::Lint,
        at: ast::Node<'t>,
        span: impl Into<MultiSpan>,
        message: impl Into<String>,
    ) -> &mut Diag {
        let span: MultiSpan = span.into();
        let place = match self.original_range(at) {
            Some(range) => Place::Node(range),
            None => Place::At { at: span.primary_span().map_or(0, |it| it.lo), then: 0 },
        };
        self.report(FoundLint::Blaze(lint), at, place, span, message)
    }

    fn report(
        &mut self,
        lint: FoundLint,
        at: ast::Node<'t>,
        place: Place,
        span: MultiSpan,
        message: impl Into<String>,
    ) -> &mut Diag {
        let at = self.range(at).or(self.walking.call_at).unwrap_or(self.root.text_range());
        let name = match lint {
            FoundLint::Clippy(lint) => format!("clippy::{}", lint.name()),
            FoundLint::Blaze(lint) => format!("blaze::{}", lint.name()),
        };
        self.found.push(Finding {
            lint,
            at,
            place,
            diagnostic: {
                let mut diag = Diag::new(model::Level::Warning, Some(Code::Lint(name)), message);
                diag.span = span;
                diag
            },
        });
        &mut self.found.last_mut().expect("just pushed").diagnostic
    }

    /// Walks `root` and what it expands to, running the lints on each node; a body inside is
    /// left to its own query.
    fn walk(&mut self, root: ast::Node<'t>) {
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if node != root && is_body_owner(node) {
                events.skip_subtree();
                continue;
            }
            // One scan of the children for attributes: every item and ADT can have them, so
            // a node without is neither `cfg`'d out nor expanded by an attribute or derive.
            let mut has_attrs = false;
            if let Some(owner) = ast::AnyHasAttrs::cast(node)
                && owner.attrs().next().is_some()
            {
                has_attrs = true;
                if !self.enabled(owner) {
                    events.skip_subtree();
                    continue;
                }
                if self.in_file() {
                    self.expectations_of(owner);
                }
            }
            // What an attribute macro such as `#[test]` makes of an item is what rustc lints,
            // the code written in it keeping its place.
            if has_attrs
                && node != root
                && let Some(item) = ast::Item::cast(node)
                && self.walk_attr_expansion(item)
            {
                events.skip_subtree();
                continue;
            }
            if self.is_written(node) {
                lints::check_node(self, node);
            }
            blaze::check_node(self, node);
            if has_attrs && let Some(adt) = ast::Adt::cast(node) {
                self.walk_derives(adt);
            }
            if let Some(call) = ast::MacroCall::cast(node) {
                self.walk_call(call);
                events.skip_subtree();
            }
        }
    }

    /// Walks what an attribute macro on `item`, which has attributes, expands it to, if one
    /// does; says whether one did.
    fn walk_attr_expansion(&mut self, item: ast::Item<'t>) -> bool {
        const DEEPEST: u32 = 8;
        if self.walking.depth >= DEEPEST || self.walking.expansion.is_some() {
            return false;
        }
        let key = (self.file_of(item.syntax()), ast::ptr(item.syntax()));
        let Some(&call) = self.items().calls.get(&key) else { return false };
        if !matches!(call.loc(self.db).kind, MacroCallKind::Attr { .. }) {
            return false;
        }
        let Some(input) = self.range(item.syntax()).or(self.walking.input) else { return true };
        let expanded = self.file_root(HirFileId::MacroFile(call));
        let outer_input = self.walking.input.replace(input);
        let outer_call = self.walking.call_at;
        self.walking.call_at = outer_call.or(Some(input));
        self.walking.depth += 1;
        self.walk(expanded);
        self.walking.depth -= 1;
        self.walking.call_at = outer_call;
        self.walking.input = outer_input;
        true
    }

    /// Walks what the macro call `call` expands to, for the code handed to it.
    fn walk_call(&mut self, call: ast::MacroCall<'t>) {
        const DEEPEST: u32 = 8;
        if self.walking.depth >= DEEPEST
            || self.walking.expansion.is_some()
            || !lints::in_calls(self)
        {
            return;
        }
        let Some(tt) = call.token_tree() else { return };
        // A call the expansion makes of its own hands on what it was handed.
        let Some(input) = self.range(tt.syntax()).or(self.walking.input) else { return };
        let Some(expanded) = self.expand(call) else { return };
        let outer_input = self.walking.input.replace(input);
        let outer_call = self.walking.call_at;
        self.walking.call_at = outer_call.or_else(|| self.range(call.syntax()));
        let outer_bang = std::mem::replace(&mut self.walking.in_bang, true);
        self.walking.depth += 1;
        self.walk(expanded);
        self.walking.depth -= 1;
        self.walking.in_bang = outer_bang;
        self.walking.call_at = outer_call;
        self.walking.input = outer_input;
    }

    /// Walks what the derives of `adt`, which has attributes, expand to.
    fn walk_derives(&mut self, adt: ast::Adt<'t>) {
        if !lints::in_derives(self) {
            return;
        }
        let key = (self.file_of(adt.syntax()), ast::ptr(adt.syntax()));
        let Some(derives) = self.items().derives.get(&key) else { return };
        let attrs = adt.attrs().filter(|it| it.simple_name() == Some("derive"));
        let mut walks: Vec<(MacroCallId, Expansion)> = Vec::new();
        for (attr, calls) in attrs.zip(derives) {
            let paths = derive_paths(attr);
            for (call, (range, path)) in calls.iter().zip(paths) {
                let Some(call) = *call else { continue };
                let Some(expansion) = self.expansion_of(call, MacroKind::Derive, &path, range) else {
                    continue;
                };
                walks.push((call, expansion));
            }
        }
        for (call, expansion) in walks {
            let root = self.file_root(HirFileId::MacroFile(call));
            let outer = self.walking.expansion.replace(expansion);
            self.walk(root);
            self.walking.expansion = outer;
        }
    }

    /// What rustc says of the expansion of the macro call `call`, made at `call_site` and
    /// named `name`: where it was called, what it is and where it is defined.
    fn expansion_of(
        &self,
        call: MacroCallId,
        kind: MacroKind,
        name: &str,
        call_site: TextRange,
    ) -> Option<Expansion> {
        let call_site = self.range_in_base(call_site)?;
        let db = self.db;
        let named = |file: HirFileId, range: TextRange| Some(diag::of_file(file.file_id()?.file_id(db), range));
        let def_site = match call.loc(db).def.kind {
            MacroDefKind::ProcMacro(id, ..) => Some(named(id.file_id, signature_range(self.view_of(id)?))?),
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => {
                self.node_of(id).and_then(|node| named(id.file_id, item_range(node)))
            }
        };
        let name = name.rsplit("::").next()?.trim().to_owned();
        let opaque = !matches!(call.loc(db).def.kind, MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..));
        Some(Expansion { call_site: diag::in_query(call_site), kind, name, def_site, opaque })
    }

    /// `range` of the tree walked, when that is the tree of [`Cx::text`].
    fn range_in_base(&self, range: TextRange) -> Option<TextRange> {
        self.base.tree.is_some_and(|tree| std::ptr::eq(tree, self.root.file())).then_some(range)
    }

    /// Records the `#[expect]`s of the lints run on `owner`.
    fn expectations_of(&mut self, owner: ast::AnyHasAttrs<'t>) {
        // An `expect` level, written plainly or in a `cfg_attr`, is an `expect` identifier.
        let has_expect = owner.attrs().any(|it| {
            it.syntax().descendants_with_tokens().any(|it| {
                it.as_token().is_some_and(|it| it.kind() == SyntaxKind::IDENT && it.text() == "expect")
            })
        });
        if !has_expect {
            return;
        }
        for LintAttr { level, target, range, reason, .. } in lint_attrs(self.cfg, owner) {
            if level != Level::Expect {
                continue;
            }
            let ours = match target {
                Target::Lint(lint) => Implemented::of(lint).is_some(),
                Target::Blaze(_) => true,
                _ => false,
            };
            if ours {
                self.expects.push(Expect { name: range, reason, owner: owner.syntax().text_range() });
            }
        }
    }

    /// Whether the `cfg`s of `owner` keep it.
    fn enabled(&self, owner: ast::AnyHasAttrs<'_>) -> bool {
        let mut enabled = true;
        for_each_meta(self.cfg, owner.attrs(), |meta| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
            {
                enabled &= self.cfg.check(&CfgExpr::parse_from_view(predicate)) != Some(false);
            }
        });
        enabled
    }

    /// The file `node` is of: the base tree's, or the macro file of the expansion it is in.
    fn file_of(&self, node: ast::Node<'_>) -> HirFileId {
        match self.call_of(node) {
            Some(call) => HirFileId::MacroFile(call),
            None => HirFileId::FileId(self.base.file),
        }
    }

    /// The macro call whose expansion `node` is in; `None` for the base tree.
    fn call_of(&self, node: ast::Node<'_>) -> Option<MacroCallId> {
        let tree = node.file();
        if self.base.tree.is_some_and(|it| std::ptr::eq(it, tree)) {
            return None;
        }
        self.trees.borrow().iter().find(|(it, _)| std::ptr::eq(*it, tree)).map(|&(_, call)| call)
    }

    /// The root of `file`'s tree, its expansion noted for [`Cx::call_of`].
    fn file_root(&self, file: HirFileId) -> ast::Node<'q> {
        match file {
            HirFileId::FileId(file) => file.parse(self.db).root(),
            HirFileId::MacroFile(call) => {
                let root = call.parse_macro_expansion(self.db).value.0.root();
                let mut trees = self.trees.borrow_mut();
                if !trees.iter().any(|(it, _)| std::ptr::eq(*it, root.file())) {
                    trees.push((root.file(), call));
                }
                root
            }
        }
    }

    /// Where `range` of the expansion of `call` is written in the base tree: the spans of its
    /// tokens are of one place in the file, as written there, not made by a macro.
    fn upmap(&self, call: MacroCallId, range: TextRange) -> Option<TextRange> {
        let map = &call.parse_macro_expansion(self.db).value.1;
        let mut spans = map.spans_for_range(range);
        let first = spans.next()?;
        let (mut start, mut end) = (first.range.start(), first.range.end());
        for span in spans {
            if span.anchor != first.anchor || span.ctx != first.ctx {
                return None;
            }
            start = start.min(span.range.start());
            end = end.max(span.range.end());
        }
        if !first.ctx.is_root() || first.anchor.file_id != self.base.file.span_file_id(self.db) {
            return None;
        }
        let anchor = self.base.ast_id_map.try_get_erased(first.anchor.ast_id)?;
        Some(TextRange::new(start, end) + anchor.text_range().start())
    }

    /// The view of what `id` names, in whichever file.
    fn view_of<N>(&self, id: AstId<N>) -> Option<N::Of<'q>>
    where
        N: syntax::AstNode + syntax::view::Typed + span::AstIdNode,
    {
        <N::Of<'q> as ast::AstNode<'q>>::cast(self.node_of(id)?)
    }

    /// The node `id` names, in whichever file.
    fn node_of<N: span::AstIdNode>(&self, id: AstId<N>) -> Option<ast::Node<'q>> {
        let root = self.file_root(id.file_id);
        let ptr = id.file_id.ast_id_map(self.db).try_get_erased(id.value.erase())?;
        ast::try_to_node(ptr, root)
    }

    /// The call node of `call`, when it is in a tree the walk has read.
    fn call_node(&self, call: MacroCallId) -> Option<ast::Node<'t>> {
        let (file, ptr) = match call.loc(self.db).kind {
            MacroCallKind::FnLike { ast_id, .. } => erased(ast_id),
            MacroCallKind::Derive { ast_id, .. } => erased(ast_id),
            MacroCallKind::Attr { ast_id, .. } => erased(ast_id),
        };
        let root = match file {
            HirFileId::FileId(it) if it == self.base.file => self.base.root,
            HirFileId::FileId(_) => return None,
            HirFileId::MacroFile(_) => self.file_root(file),
        };
        let map = match file {
            HirFileId::FileId(_) => self.base.ast_id_map,
            HirFileId::MacroFile(_) => file.ast_id_map(self.db),
        };
        ast::try_to_node(map.try_get_erased(ptr)?, root)
    }

    /// The outermost call of the macros that wrote `call`'s expansion, as written in the
    /// file: where it is, its arguments (or the item an attribute macro is on), and whether
    /// it is a function-like call.
    fn outermost_call(&self, call: MacroCallId) -> Option<(TextRange, TextRange, bool)> {
        let db = self.db;
        let mut call = call;
        loop {
            match call.loc(db).kind.file_id() {
                HirFileId::FileId(_) => break,
                HirFileId::MacroFile(it) => call = it,
            }
        }
        let node = self.call_node(call)?;
        match call.loc(db).kind {
            MacroCallKind::FnLike { .. } => {
                let tt = ast::MacroCall::cast(node)?.token_tree()?;
                Some((node.text_range(), tt.syntax().text_range(), true))
            }
            MacroCallKind::Attr { .. } => Some((node.text_range(), node.text_range(), false)),
            MacroCallKind::Derive { .. } => None,
        }
    }

    /// The macro call `call` is, from the items' calls or the body's.
    fn call_id(&self, call: ast::MacroCall<'t>) -> Option<MacroCallId> {
        let file = self.file_of(call.syntax());
        if let Some(&id) = self.items().calls.get(&(file, ast::ptr(call.syntax()))) {
            return Some(id);
        }
        self.analysis()?.expansion(InFile::new(file, AstPtr::<syntax::ast::MacroCall>::of(call)))
    }

    /// The node of the file's whole tree, or of the expansion it is in, that `node` is, in the
    /// tree the view `Semantics` hands out, for what `hir` answers only of those: in
    /// [`query::item_lints`], which reads the whole file.
    fn in_sema(&self, node: ast::Node<'_>) -> Option<ast::Node<'q>> {
        debug_assert!(!self.base.relative, "a body's query reads its own tree, not the file's");
        let root = self.sema().parse_or_expand(self.file_of(node));
        let range = node.text_range();
        if !root.text_range().contains_range(range) {
            return None;
        }
        root.descendant_at_range(range, node.kind())
    }

    fn sema(&self) -> &hir::view::Semantics<'q> {
        self.sema.get_or_init(|| hir::view::Semantics::new(self.db))
    }

    /// What the items the query walks are: of the file's modules for the items of a file, of
    /// the body's blocks for a body.
    fn items(&self) -> &Items {
        self.items.get_or_init(|| {
            let db = self.db;
            let mut items = Items::default();
            match self.def {
                Some(def) => {
                    let InFile { file_id: file, value: id } = def.erased_ast_id(db);
                    let defined = match def {
                        DefWithBodyId::FunctionId(it) => ModuleDefId::FunctionId(it),
                        DefWithBodyId::ConstId(it) => ModuleDefId::ConstId(it),
                        DefWithBodyId::StaticId(it) => ModuleDefId::StaticId(it),
                        DefWithBodyId::VariantId(it) => ModuleDefId::EnumVariantId(it),
                    };
                    if let Some(key) = self.key(file, id) {
                        items.defs.insert(key, defined);
                    }
                    for (_, block) in Body::of(db, def).blocks(db) {
                        for (_, module) in block.modules(db) {
                            self.index(&mut items, module.scope(db));
                        }
                    }
                }
                None => {
                    let def_map = crate_def_map(db, self.krate);
                    for (id, module) in def_map.modules(db) {
                        let defined = hir::Module::from(id).definition_source_file_id(db);
                        if defined.original_file(db) == self.base.file {
                            self.index(&mut items, module.scope(db));
                        }
                    }
                }
            }
            items
        })
    }

    /// Where `id` of `file` is in a tree the query reads.
    fn key(&self, file: HirFileId, id: span::ErasedFileAstId) -> Option<Key> {
        let map = match file {
            HirFileId::FileId(it) if it == self.base.file => self.base.ast_id_map,
            HirFileId::FileId(_) => return None,
            HirFileId::MacroFile(_) => file.ast_id_map(self.db),
        };
        Some((file, map.try_get_erased(id)?))
    }

    fn index(&self, items: &mut Items, scope: &ItemScope) {
        let db = self.db;
        let mut def = |items: &mut Items, (file, id), def: ModuleDefId| {
            if let Some(key) = self.key(file, id) {
                items.defs.insert(key, def);
            }
        };
        let assoc = |items: &mut Items,
                     list: &[(hir::Name, AssocItemId)],
                     calls: &[(AstId<syntax::ast::Item>, MacroCallId)]| {
            for &(_, item) in list {
                let (at, defined) = match item {
                    AssocItemId::FunctionId(it) => (erased(it.lookup(db).id), ModuleDefId::FunctionId(it)),
                    AssocItemId::ConstId(it) => (erased(it.lookup(db).id), ModuleDefId::ConstId(it)),
                    AssocItemId::TypeAliasId(it) => (erased(it.lookup(db).id), ModuleDefId::TypeAliasId(it)),
                };
                if let Some(key) = self.key(at.0, at.1) {
                    items.defs.insert(key, defined);
                }
            }
            for &(at, call) in calls {
                if let Some(key) = self.key(at.file_id, at.value.erase()) {
                    items.calls.insert(key, call);
                }
            }
        };
        for it in scope.declarations() {
            let at = match it {
                ModuleDefId::FunctionId(f) => erased(f.lookup(db).id),
                ModuleDefId::AdtId(AdtId::StructId(s)) => erased(s.lookup(db).id),
                ModuleDefId::AdtId(AdtId::UnionId(u)) => erased(u.lookup(db).id),
                ModuleDefId::AdtId(AdtId::EnumId(e)) => {
                    for &(variant, _) in e.enum_variants(db).variants.values() {
                        def(items, erased(variant.lookup(db).id), ModuleDefId::EnumVariantId(variant));
                    }
                    erased(e.lookup(db).id)
                }
                ModuleDefId::ConstId(c) => erased(c.lookup(db).id),
                ModuleDefId::StaticId(s) => erased(s.lookup(db).id),
                ModuleDefId::TraitId(t) => {
                    let trait_items = t.trait_items(db);
                    assoc(items, &trait_items.items, &trait_items.macro_calls);
                    erased(t.lookup(db).id)
                }
                ModuleDefId::TypeAliasId(t) => erased(t.lookup(db).id),
                _ => continue,
            };
            def(items, at, it);
        }
        for it in scope.unnamed_consts() {
            def(items, erased(it.lookup(db).id), ModuleDefId::ConstId(it));
        }
        for it in scope.impls() {
            let impl_items = it.impl_items(db);
            assoc(items, &impl_items.items, &impl_items.macro_calls);
            let (file, id) = erased(it.lookup(db).id);
            if let Some(key) = self.key(file, id) {
                items.impls.insert(key, it);
            }
        }
        for (at, &call) in scope.iter_macro_invoc() {
            if let Some(key) = self.key(at.file_id, at.value.erase()) {
                items.calls.insert(key, call);
            }
        }
        for (at, call) in scope.attr_macro_invocs() {
            if let Some(key) = self.key(at.file_id, at.value.erase()) {
                items.calls.insert(key, call);
            }
        }
        for (at, derives) in scope.derive_macro_invocs() {
            let Some(key) = self.key(at.file_id, at.value.erase()) else { continue };
            let calls = items.derives.entry(key).or_default();
            for (_, _, each) in derives {
                // alloc: the calls of one `#[derive]`, once per query that walks into derives
                calls.push(each.iter().map(|it| it.and_then(|it| it.left())).collect());
            }
        }
    }
}

/// Whether `node` is what a body of its own is written in: a `fn`, `const` or `static`, or an
/// enum variant's discriminant.
fn is_body_owner(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => true,
        SyntaxKind::CONST_ARG => node.parent().is_some_and(|it| it.kind() == SyntaxKind::VARIANT),
        _ => false,
    }
}

/// clippy's `is_cfg_test`: the `cfg`'s list has the word `test`, as `cfg(test)` and two `cfg`s
/// one of which is `cfg(test)` do.
fn names_test(cfg: &CfgExpr) -> bool {
    let is_test = |it: &CfgExpr| matches!(it, CfgExpr::Atom(CfgAtom::Flag(it)) if *it == intern::sym::test);
    match cfg {
        CfgExpr::All(items) => items.iter().any(is_test),
        it => is_test(it),
    }
}

/// Each meta of `attrs`, their `cfg_attr`s expanded where `cfg` keeps them.
fn for_each_meta<'t>(
    cfg: &CfgOptions,
    attrs: impl Iterator<Item = ast::Attr<'t>>,
    mut f: impl FnMut(ast::Meta<'t>),
) {
    hir_expand::attrs::cfg_attrs_view(attrs, cfg).for_each(|(meta, _)| f(meta));
}

/// The level a diagnostic of a lint at `level` is written at.
fn written_at(level: Level) -> model::Level {
    match level.is_error() {
        true => model::Level::Error,
        false => model::Level::Warning,
    }
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

/// The lint attributes on `owner`, its `cfg_attr`s expanded, in their order: the level,
/// what it sets, the name as written and where it is, and the reason given.
fn lint_attrs(cfg: &CfgOptions, owner: ast::AnyHasAttrs<'_>) -> Vec<LintAttr> {
    let mut found = Vec::new();
    for_each_meta(cfg, ast::attrs_including_inner(&owner), |meta| {
        let ast::Meta::TokenTreeMeta(meta) = meta else { return };
        let Some(level) = meta
            .path()
            .and_then(|it| it.as_single_name_ref())
            .and_then(|it| Level::of_attr(it.text()))
        else {
            return;
        };
        let Some(tt) = meta.token_tree() else { return };
        let (names, reason) = lint_list(tt);
        for (name, range) in names {
            if let Some(target) = Target::of(&name) {
                let name = canonical(&name);
                found.push(LintAttr { level, target, name, range, reason: reason.clone() });
            }
        }
    });
    found
}

/// A lint name as rustc keeps it: without spaces, with `_` for `-`.
fn canonical(name: &str) -> String {
    name.chars().filter(|it| !it.is_whitespace()).collect::<String>().replace('-', "_")
}

/// The names in a lint attribute's list, with where each is, and its `reason`.
fn lint_list(tt: ast::TokenTree<'_>) -> (Vec<(String, TextRange)>, Option<String>) {
    let mut names = Vec::new();
    let mut reason = None;
    let mut element: Vec<ast::Token<'_>> = Vec::new();
    let mut flush = |element: &mut Vec<ast::Token<'_>>| {
        let (Some(first), Some(last)) = (element.first().copied(), element.last().copied()) else {
            return;
        };
        if first.text() == "reason" && element.get(1).is_some_and(|it| it.kind() == SyntaxKind::EQ) {
            reason = element
                .get(2)
                .and_then(|it| ast::String::cast(*it))
                .and_then(|it| it.value().ok().map(|it| it.into_owned()));
            element.clear();
            return;
        }
        let name: String = element.iter().map(|it| it.text()).collect();
        names.push((name, TextRange::new(first.text_range().start(), last.text_range().end())));
        element.clear();
    };
    let children: Vec<_> = tt.syntax().children_with_tokens().collect();
    let inner = children.get(1..children.len().saturating_sub(1)).unwrap_or_default();
    for child in inner {
        match *child {
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => flush(&mut element),
            NodeOrToken::Token(token) if token.kind().is_trivia() => {}
            NodeOrToken::Token(token) => element.push(token),
            NodeOrToken::Node(node) => {
                element.extend(node.descendants_with_tokens().filter_map(|it| it.into_token()))
            }
        }
    }
    flush(&mut element);
    (names, reason)
}

/// The paths of a `#[derive(...)]`, in order, with where each is.
fn derive_paths(attr: ast::Attr<'_>) -> Vec<(TextRange, String)> {
    let Some(tt) = attr.meta().and_then(|it| match it {
        ast::Meta::TokenTreeMeta(it) => it.token_tree(),
        _ => None,
    }) else {
        return Vec::new();
    };
    lint_list(tt).0.into_iter().map(|(name, range)| (range, name)).collect()
}

/// Where rustc's span of an item starts: after its attributes, doc comments and the comments
/// before it.
pub(crate) use syntax::view::written_range as item_range;

/// The path `name` is when written alone, as `SemanticsScope::speculative_resolve` reads a
/// path of one segment.
fn one_segment_path(name: &str) -> hir::ModPath {
    use hir::PathKind;
    let (kind, segment) = match name {
        "self" => (PathKind::Super(0), None),
        "super" => (PathKind::Super(1), None),
        "crate" => (PathKind::Crate, None),
        "Self" => (PathKind::Plain, Some(hir::Name::new_symbol_root(intern::sym::Self_))),
        _ => (PathKind::Plain, Some(hir::Name::new_root(name))),
    };
    hir::ModPath::from_segments(kind, segment)
}

use syntax::view::signature_range;

#[cfg(test)]
mod for_each_meta_tests {
    use cfg::{CfgExpr, CfgOptions};
    use intern::Symbol;
    use span::Edition;
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _, HasAttrs as _},
    };

    /// `for_each_meta` before it called `hir_expand::attrs::cfg_attrs_view`.
    fn reference<'t>(
        cfg: &CfgOptions,
        attrs: impl Iterator<Item = ast::Attr<'t>>,
        mut f: impl FnMut(ast::Meta<'t>),
    ) {
        let mut nested: Vec<ast::Meta<'t>> = Vec::new();
        for attr in attrs {
            let Some(meta) = attr.meta() else { continue };
            let mut next = Some(meta);
            while let Some(mut meta) = next.take().or_else(|| nested.pop()) {
                while let ast::Meta::UnsafeMeta(it) = meta {
                    match it.meta() {
                        Some(inner) => meta = inner,
                        None => break,
                    }
                }
                match meta {
                    ast::Meta::UnsafeMeta(_) => {}
                    ast::Meta::CfgAttrMeta(meta) => {
                        let Some(predicate) = meta.cfg_predicate() else { continue };
                        if cfg.check(&CfgExpr::parse_from_view(predicate)) != Some(false) {
                            let at = nested.len();
                            nested.extend(meta.metas());
                            nested[at..].reverse();
                        }
                    }
                    meta => f(meta),
                }
            }
        }
    }

    #[test]
    fn the_metas_are_the_ones_cfg_attrs_view_leaves() {
        let text = r#"
#[cfg_attr(a, inline, cfg_attr(b, cold), must_use)]
#[cfg_attr(not(a), deprecated)]
#[unsafe(no_mangle)]
#[cfg(test)]
#[cfg_attr(a, unsafe(export_name = "x"), cfg_attr(not(b), doc = "y"))]
#[cfg_attr()]
fn f() {}
#[cfg_attr(b, allow(dead_code))]
mod m { #![cfg_attr(a, cfg(test))] }
"#;
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let mut cfg = CfgOptions::default();
        cfg.insert_atom(Symbol::intern("a"));
        let mut owners = 0;
        for owner in parse.root().descendants().filter_map(ast::AnyHasAttrs::cast) {
            for attrs in [true, false] {
                let (mut old, mut new) = (Vec::new(), Vec::new());
                if attrs {
                    reference(&cfg, owner.attrs(), |it| old.push(it.syntax().text_range()));
                    super::for_each_meta(&cfg, owner.attrs(), |it| new.push(it.syntax().text_range()));
                } else {
                    let inner = || ast::attrs_including_inner(&owner);
                    reference(&cfg, inner(), |it| old.push(it.syntax().text_range()));
                    super::for_each_meta(&cfg, inner(), |it| new.push(it.syntax().text_range()));
                }
                assert_eq!(old, new, "{owner:?}");
            }
            owners += 1;
        }
        assert!(owners >= 2);
        let f = parse.root().descendants().find_map(ast::Fn::cast).unwrap();
        let mut names = Vec::new();
        super::for_each_meta(&cfg, f.attrs(), |it| names.push(it.syntax().to_string()));
        assert_eq!(names[..3], ["inline", "must_use", "no_mangle"]);
    }
}
