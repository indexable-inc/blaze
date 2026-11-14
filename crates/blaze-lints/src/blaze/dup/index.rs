//! The copies of a crate, as queries: one shape per body ([`body_shape`], again only when the
//! body's anchor changes), one pass per crate that groups them by hash ([`dup_index`]), and per
//! body the findings it holds ([`body_findings`]), which an edit of another body leaves equal
//! unless it adds or removes a copy of this one. Types are read the same way per file
//! ([`crate_types`], [`type_findings`]). Hashes only: a body's tokens are read again from its
//! anchor for the few pairs a hash names, never kept.
//!
//! Whole functions, constants and types are compared with the crate's local dependencies too
//! (each crate's sorted hashes, [`crate_wholes`], searched per hash), so what a crate is told
//! depends only on it and what it depends on, never on which other crates are in the graph.
//! Blocks, near-duplicates and paste mismatches are compared within the crate.
//!
//! Every list is in one order: the dependencies first, in the order a walk from the crate
//! reaches them, each by the item's path, then the crate itself, by the file's place among its
//! modules, then by offset. A copy in a dependency is the original, and the crate's copies of
//! it are reported, each once.

use std::alloc::Global;

use base_db::{Db, EditionedFileId, TargetKind};
use blaze_hash::HashMap;
use blaze_items::ItemId;
use hir::HasAttrs as _;
use hir_def::{DefWithBodyId, ItemContainerId, Lookup as _, expr_store::body::body_syntax};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::{
    near, paste,
    shape::{self, By, Class, Piece, Run, Tok},
    unify,
};
use crate::{
    blaze::{Lint, test_code},
    query,
};

/// The fewest tokens a function's body has, its braces left out, to be compared whole: below it
/// a body is a delegation or an accessor (`self.0.len()`), which a shared helper would not make
/// shorter.
pub(crate) const FN_FLOOR: u32 = 12;
/// The fewest tokens a block or row of statements has to be reported as a copy: about four
/// lines of Rust (a line is 15 to 20 tokens), where a function taking its free variables starts
/// to be shorter than the copies it replaces. PMD's CPD defaults to 100 for Java's longer
/// lines; Simian to six lines.
pub(crate) const BLOCK_FLOOR: u32 = 64;
/// The fewest tokens of a constant's value for two equal ones to be one constant written twice:
/// `const MAX: u32 = 8;` twice is two facts that are equal today, `magic_number`'s question.
pub(crate) const CONST_FLOOR: u32 = 8;
/// The fewest characters between a string constant's quotes for two equal ones to be one.
pub(crate) const STRING_FLOOR: usize = 12;

/// What a body is to the comparisons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Role {
    /// Not compared: an enum discriminant, a static, an associated const, a body in an `extern`
    /// block, test code.
    #[default]
    Other,
    /// A function of a module.
    Free,
    /// A method of an inherent impl.
    Inherent,
    /// A method of a trait's impl.
    TraitImpl,
    /// A trait's default method.
    TraitDefault,
    /// A constant of a module.
    Const,
    /// A `#[test]` function: compared with other tests by its literals alone.
    Test,
}

impl Role {
    fn is_trait(self) -> bool {
        matches!(self, Role::TraitImpl | Role::TraitDefault)
    }
}

/// A body's shapes: what the crate's pass groups by.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) role: Role,
    /// The shape of the whole function past [`FN_FLOOR`], or of the constant past its floors.
    pub(crate) whole: Option<u64>,
    /// Its blocks and rows of statements past [`BLOCK_FLOOR`], by shape.
    pub(crate) pieces: Box<[Piece]>,
    /// Its blocks and rows past [`paste::PASTE_FLOOR`], by structure.
    pub(crate) skeletons: Box<[Piece]>,
    /// The MinHash sketch of a body past [`near::NEAR_FLOOR`].
    pub(crate) sketch: Option<Box<[u32]>>,
    /// A test's shape with its literals erased ([`shape::erased`]): tests of one table share it.
    pub(crate) erased: Option<u64>,
    /// The structure of a function past [`FN_FLOOR`] ([`shape::structure`]): functions that
    /// may generalise to one ([`super::unify`]) share it.
    pub(crate) outline: Option<u64>,
}

/// The shapes of `def`'s body, from its anchor: [`shape_of`] of its item, or nothing for a body
/// a macro writes, test code other than a `#[test]` function, and what [`Role::Other`] is.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_shape(db: &Db, def: DefWithBodyId) -> Shape {
    let role = role_of(db, def);
    if role == Role::Other {
        return Shape::default();
    }
    let Some(syntax) = body_syntax(db, def).as_ref() else { return Shape::default() };
    let tree = syntax.tree(db);
    let anchor = ast::Node::root(&tree);
    let Some(owner) = ast::try_to_node(syntax.item, anchor) else { return Shape::default() };
    if role == Role::Free && is_test_fn(owner) {
        return shape_of(owner, Role::Test);
    }
    if module_is_test(db, hir::DefWithBody::from(def).module(db))
        || std::iter::once(owner).chain(owner.ancestors()).any(test_attr)
    {
        return Shape::default();
    }
    shape_of(owner, role)
}

/// Whether `node` is a function marked `#[test]` (or `#[something::test]`).
pub(crate) fn is_test_fn(node: ast::Node<'_>) -> bool {
    node.kind() == SyntaxKind::FN
        && node.children().filter(|it| it.kind() == SyntaxKind::ATTR).any(|attr| {
            let text = compact(attr);
            text == "#[test]" || text.ends_with("::test]")
        })
}

/// The shapes of `owner`, a function or constant item, as `role`.
pub(crate) fn shape_of(owner: ast::Node<'_>, role: Role) -> Shape {
    let toks = shape::tokens(owner);
    let mut scratch = shape::Renumber::default();
    let empty = || Vec::new_in(Global).into_boxed_slice();
    let only = |whole: Option<u64>, erased: Option<u64>| Shape {
        role,
        whole,
        pieces: empty(),
        skeletons: empty(),
        sketch: None,
        erased,
        outline: None,
    };
    if role == Role::Const {
        let whole = const_key(owner, &toks, &mut scratch);
        return only(whole, None);
    }
    let Some(body) = owner.children().find(|it| it.kind() == SyntaxKind::BLOCK_EXPR) else {
        return only(None, None);
    };
    let body_toks = shape::within(&toks, body.text_range());
    let inner = u32::try_from(body_toks.len().saturating_sub(2)).unwrap_or(u32::MAX);
    if role == Role::Test {
        return only(None, (inner >= FN_FLOOR).then(|| shape::erased(&toks, &mut scratch)));
    }
    let whole = (inner >= FN_FLOOR).then(|| shape::alpha(&toks, &mut scratch));
    let outline = whole.map(|_| shape::structure(&toks));
    let pieces = shape::pieces(owner, Some(body), &toks, BLOCK_FLOOR, By::Shape);
    let skeletons = shape::pieces(owner, Some(body), &toks, paste::PASTE_FLOOR, By::Structure);
    let sketch = (inner >= near::NEAR_FLOOR)
        .then(|| near::sketch(&shape::shingles(body_toks, near::WIDTH)));
    Shape {
        role,
        whole,
        pieces: pieces.into_boxed_slice(),
        skeletons: skeletons.into_boxed_slice(),
        sketch,
        erased: None,
        outline,
    }
}

/// The key of a constant whose value is past the floors: its shape (type and value, not name).
fn const_key(owner: ast::Node<'_>, toks: &[Tok], scratch: &mut shape::Renumber) -> Option<u64> {
    let fixed = |tok: &Tok, kind: SyntaxKind| tok.class == Class::Fixed && tok.key == u64::from(kind as u16);
    let eq = toks.iter().position(|it| fixed(it, SyntaxKind::EQ))?;
    let mut value = toks.get(eq + 1..)?;
    if let Some((last, rest)) = value.split_last()
        && fixed(last, SyntaxKind::SEMICOLON)
    {
        value = rest;
    }
    let long_string = match value {
        [only] if only.class == Class::Literal => {
            let text = text_at(owner, only.range);
            text.ends_with('"') && text.len() >= STRING_FLOOR + 2
        }
        _ => false,
    };
    let weight = u32::try_from(value.len()).unwrap_or(u32::MAX);
    (long_string || weight >= CONST_FLOOR).then(|| shape::alpha(toks, scratch))
}

/// The text of `range` of `node`'s tree.
pub(crate) fn text_at<'t>(node: ast::Node<'t>, range: TextRange) -> &'t str {
    let start = node.text_range().start();
    if range.start() < start {
        return "";
    }
    node.text().get(std::ops::Range::<usize>::from(range - start)).unwrap_or_default()
}

/// The text of `range` of a tree whose text is `text`.
fn slice(text: &str, range: TextRange) -> &str {
    text.get(std::ops::Range::<usize>::from(range)).unwrap_or_default()
}

/// What `def` is to the comparisons, by where it is declared.
fn role_of(db: &Db, def: DefWithBodyId) -> Role {
    match def {
        DefWithBodyId::FunctionId(it) => match it.lookup(db).container {
            ItemContainerId::ImplId(imp) if hir::Impl::from(imp).trait_(db).is_some() => {
                Role::TraitImpl
            }
            ItemContainerId::ImplId(_) => Role::Inherent,
            ItemContainerId::TraitId(_) => Role::TraitDefault,
            ItemContainerId::ModuleId(_) => Role::Free,
            ItemContainerId::ExternBlockId(_) => Role::Other,
        },
        DefWithBodyId::ConstId(it) => match it.lookup(db).container {
            ItemContainerId::ModuleId(_) => Role::Const,
            _ => Role::Other,
        },
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => Role::Other,
    }
}

/// Whether `module` or a module it is in says `cfg(test)`.
fn module_is_test(db: &Db, module: hir::Module) -> bool {
    let mut module = Some(module);
    while let Some(it) = module.take() {
        if it.attrs(db).cfgs(db).is_some_and(test_code::names_test) {
            return true;
        }
        module = it.parent(db);
    }
    false
}

/// Whether `node`'s own attributes make it test code: `#[test]`, `#[bench]`, a
/// `#[something::test]`, or a `cfg` that names `test`.
pub(crate) fn test_attr(node: ast::Node<'_>) -> bool {
    node.children().filter(|it| it.kind() == SyntaxKind::ATTR).any(|attr| {
        let text = compact(attr);
        text == "#[test]"
            || text == "#[bench]"
            || text.ends_with("::test]")
            || (text.starts_with("#[cfg(") && text.contains("test"))
    })
}

/// The text of `node` without its trivia.
fn compact(node: ast::Node<'_>) -> String {
    let mut out = String::new(); // alloc: an attribute's text, compared and dropped
    for element in node.descendants_with_tokens() {
        if let NodeOrToken::Token(token) = element
            && !token.kind().is_trivia()
        {
            out.push_str(token.text());
        }
    }
    out
}

/// Whether `krate` is a test, bench or example target, which is all test code.
pub(crate) fn is_test_target(db: &Db, krate: base_db::Crate) -> bool {
    matches!(krate.data(db).target_kind, TargetKind::Test | TargetKind::Bench | TargetKind::Example)
}

/// The local crates `krate` depends on, directly or not, in the order a walk reaches them; the
/// one list a lint compares a crate with its dependencies over (`magic_string` too).
pub(crate) fn local_deps(db: &Db, krate: base_db::Crate) -> Vec<base_db::Crate, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(
        krate
            .transitive_loaded_deps(db)
            .iter()
            .copied()
            .filter(|&it| it != krate && it.data(db).origin.is_local() && !is_test_target(db, it)),
    );
    out
}

/// One row of [`crate_wholes`]: a whole function's or constant's shape, and the item.
pub(crate) type WholeRow = (u64, DefWithBodyId);

/// The whole functions and constants of a crate past their floors, by their shape, sorted:
/// what a dependent crate searches for its own, and what [`merge_siblings`] merges.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_wholes(db: &Db, krate: base_db::Crate) -> Box<[WholeRow]> {
    let mut out: Vec<WholeRow, Global> = Vec::new_in(Global);
    if !is_test_target(db, krate) {
        for body in query::lint_plan(db, krate).bodies.iter() {
            if let Some(whole) = body_shape(db, body.def).whole {
                out.push((whole, body.def));
            }
        }
    }
    out.sort_unstable();
    out.into_boxed_slice()
}

/// [`Order::krate`] of the crate checked: after every dependency.
const OWN: u32 = u32::MAX;

/// Where a copy is, in the order every list is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Order {
    /// A dependency's place in the walk, or [`OWN`].
    krate: u32,
    /// The file's place among the crate's module files.
    file: u32,
    /// The offset in the file.
    at: u32,
}

/// One copy, where a finding names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) place: Place,
    /// How the message names it: "`name`", or "`path`" for another crate's.
    pub(crate) label: String,
    /// Whether it is the copy the finding is at.
    pub(crate) here: bool,
}

/// Where a copy is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// In a body of the crate checked, by a range of the body's anchor (its first token or its
    /// name, for a short note).
    Body { def: DefWithBodyId, range: TextRange },
    /// In another crate, named by the label alone.
    Crate,
}

/// What else a finding says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    /// A function or constant: the copy it forwards to when its fix can, a function of the same
    /// module whose name is `name`.
    Whole { forward: Option<Forward> },
    /// A block or row: the free locals of its first copy, as the parameters a function made of
    /// it takes, and its tokens.
    Block { free: Box<[String]>, weight: u32 },
    /// A near-duplicate: how alike, in percent, and the lines that differ.
    Near { percent: u32, diff: Option<String> },
    /// A rename not finished: `from` became `to` in `renamed` places of the copy and stayed
    /// `from` in `kept`.
    Paste { from: String, to: String, renamed: u32, kept: u32 },
    /// Two functions one function with parameters ([`super::unify`]): the holes, `a` in
    /// `first` (the earlier copy) and `b` in the body reported, and how alike the two are
    /// when `near_duplicate` paired them, which it says instead when inference cannot give
    /// each hole one type.
    Extract { first: DefWithBodyId, holes: Box<[HoleAt]>, near: Option<u32> },
    /// Tests that are one test over a table: `rows` tests, of `values` literals each.
    Table { rows: u32, values: u32 },
    /// Two functions that differ in types alone: the holes.
    ByType { holes: Box<[HoleAt]> },
}

/// A parameter of a generalisation, as a finding names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoleAt {
    pub(crate) class: unify::HoleClass,
    /// Its range in the earlier copy and in the copy reported.
    pub(crate) a: TextRange,
    pub(crate) b: TextRange,
    /// The places of each copy it is used at.
    pub(crate) uses: u32,
    pub(crate) text_a: String,
    pub(crate) text_b: String,
}

/// What a same function's fix forwards to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forward {
    pub(crate) name: String,
}

/// A finding of a body, before its level is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) lint: Lint,
    /// Where it is reported, in the body's anchor.
    pub(crate) at: TextRange,
    /// Every copy, this one among them, in order.
    pub(crate) sites: Box<[Site]>,
    pub(crate) note: Note,
}

/// Every finding of a crate's bodies, by body.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Index {
    findings: Box<[(DefWithBodyId, Found)]>,
}

/// The findings of `def`'s body: its slice of its crate's [`dup_index`].
#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_findings(db: &Db, def: DefWithBodyId) -> Box<[Found]> {
    use hir_def::HasModule as _;
    let index = dup_index(db, def.krate(db));
    let start = index.findings.partition_point(|(it, _)| *it < def);
    let end = index.findings.partition_point(|(it, _)| *it <= def);
    // alloc: the body's findings, few
    // clone: a finding moves out of the crate's index into the body's value
    index.findings.get(start..end).unwrap_or_default().iter().map(|(_, it)| it.clone()).collect()
}

/// Every finding of `krate`'s bodies: one pass over their shapes.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn dup_index(db: &Db, krate: base_db::Crate) -> Index {
    let mut ix = Indexer::new(db, krate);
    // A test target is all test code: only its tests are compared, with each other.
    if !is_test_target(db, krate) {
        ix.wholes();
        ix.blocks();
        ix.near();
        ix.outlines();
        ix.pastes();
    }
    ix.tests();
    let mut findings = ix.out;
    findings.sort_by(|(a, x), (b, y)| {
        (a, x.at.start(), x.at.end(), x.lint.name()).cmp(&(b, y.at.start(), y.at.end(), y.lint.name()))
    });
    findings.dedup_by(|(a, x), (b, y)| a == b && x.at == y.at && x.lint == y.lint);
    Index { findings: findings.into_boxed_slice() }
}

/// What the pass reads of a body's anchor again.
struct Read<'db> {
    toks: Vec<Tok, Global>,
    text: &'db str,
    item: TextRange,
    name: Option<TextRange>,
    body: Option<TextRange>,
}

fn read(db: &Db, def: DefWithBodyId) -> Option<Read<'_>> {
    let syntax = body_syntax(db, def).as_ref()?;
    let tree = syntax.tree(db);
    let anchor = ast::Node::root(&tree);
    let owner = ast::try_to_node(syntax.item, anchor)?;
    let child = |kind: SyntaxKind| owner.children().find(|it| it.kind() == kind);
    Some(Read {
        toks: shape::tokens(owner),
        text: syntax.text(),
        item: owner.text_range(),
        name: child(SyntaxKind::NAME).map(|it| it.text_range()),
        body: child(SyntaxKind::BLOCK_EXPR).map(|it| it.text_range()),
    })
}

/// One copy in the pass.
#[derive(Clone, Copy, Debug)]
struct Member {
    def: DefWithBodyId,
    order: Order,
    range: TextRange,
    run: Run,
}

/// The pass over a crate's shapes.
struct Indexer<'db> {
    db: &'db Db,
    krate: base_db::Crate,
    /// The crate's bodies, with where each is.
    bodies: Vec<(DefWithBodyId, &'db Shape, u32, syntax::TextSize), Global>,
    reads: HashMap<DefWithBodyId, Option<Read<'db>>, Global>,
    /// What a reported group covers, by body: a smaller copy inside is not reported again.
    covered: Vec<(DefWithBodyId, TextRange), Global>,
    /// The pairs of bodies, by their index, a pass has judged: each is judged once.
    paired: Vec<(usize, usize), Global>,
    out: Vec<(DefWithBodyId, Found), Global>,
}

impl<'db> Indexer<'db> {
    fn new(db: &'db Db, krate: base_db::Crate) -> Indexer<'db> {
        let plan = query::lint_plan(db, krate);
        let mut files: HashMap<EditionedFileId, u32, Global> = HashMap::new_in(Global);
        for (index, file) in plan.files.iter().enumerate() {
            files.entry(file.file).or_insert(u32::try_from(index).unwrap_or(u32::MAX));
        }
        let mut bodies = Vec::new_in(Global);
        for body in plan.bodies.iter() {
            let file = files.get(&body.file).copied().unwrap_or(u32::MAX);
            bodies.push((body.def, body_shape(db, body.def), file, body.offset));
        }
        Indexer {
            db,
            krate,
            bodies,
            reads: HashMap::new_in(Global),
            covered: Vec::new_in(Global),
            paired: Vec::new_in(Global),
            out: Vec::new_in(Global),
        }
    }

    /// The order of `range` of the crate's `body`th body.
    fn order(&self, body: usize, range: TextRange) -> Order {
        let (_, _, file, offset) = self.bodies[body];
        Order { krate: OWN, file, at: u32::from(offset).saturating_add(u32::from(range.start())) }
    }

    fn load(&mut self, def: DefWithBodyId) {
        let db = self.db;
        self.reads.entry(def).or_insert_with(|| read(db, def));
    }

    fn peek(&self, def: DefWithBodyId) -> Option<&Read<'db>> {
        self.reads.get(&def).and_then(Option::as_ref)
    }

    /// The name of the crate's body `def`, or its first token's range when it has none.
    fn head(&mut self, def: DefWithBodyId, range: TextRange) -> (TextRange, String) {
        self.load(def);
        let Some(read) = self.peek(def) else { return (range, String::new()) };
        let head = shape::within(&read.toks, range).first().map_or(range, |it| it.range);
        let name = read.name.map_or("", |it| slice(read.text, it));
        // alloc: a label of the message, per copy reported
        (head, format!("`{name}`"))
    }

    /// Same functions and constants, in the crate and against its dependencies.
    fn wholes(&mut self) {
        let db = self.db;
        let mut own: Vec<(u64, Order, DefWithBodyId), Global> = Vec::new_in(Global);
        for (body, &(def, shape, _, _)) in self.bodies.iter().enumerate() {
            if let Some(whole) = shape.whole {
                let item = body_syntax(db, def)
                    .as_ref()
                    .map_or_else(TextRange::default, |it| it.item.text_range());
                own.push((whole, self.order(body, item), def));
            }
        }
        own.sort_unstable();
        let deps = local_deps(db, self.krate);
        let mut start = 0;
        while start < own.len() {
            let hash = own[start].0;
            let end = start + own[start..].iter().take_while(|it| it.0 == hash).count();
            let mut members: Vec<(Order, DefWithBodyId, Option<String>), Global> = Vec::new_in(Global);
            members.extend(own[start..end].iter().map(|&(_, order, def)| (order, def, None)));
            start = end;
            for (index, &dep) in deps.iter().enumerate() {
                let theirs = crate_wholes(db, dep);
                let from = theirs.partition_point(|it| it.0 < hash);
                for &(_, def) in theirs[from..].iter().take_while(|it| it.0 == hash) {
                    let path = blaze_items::key::path(db, ItemId::of_body(db, def));
                    let order = Order { krate: u32::try_from(index).unwrap_or(OWN - 1), file: 0, at: 0 };
                    members.push((order, def, Some(path)));
                }
            }
            if members.len() < 2 {
                continue;
            }
            members.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
            for class in split_by_code(db, members) {
                self.whole_group(class);
            }
        }
    }

    /// Reports one group of same functions or constants, in order.
    fn whole_group(&mut self, members: Vec<(Order, DefWithBodyId, Option<String>), Global>) {
        let db = self.db;
        if members.len() < 2 || !members.iter().any(|it| it.0.krate == OWN) {
            return;
        }
        let roles: Vec<Role, Global> = {
            let mut out = Vec::new_in(Global);
            out.extend(members.iter().map(|it| body_shape(db, it.1).role));
            out
        };
        let lint = if roles.iter().all(|it| *it == Role::Const) {
            Lint::SameConst
        } else if roles.iter().all(|it| it.is_trait()) {
            Lint::SameImplFn
        } else {
            Lint::SameFn
        };
        let mut sites: Vec<Site, Global> = Vec::new_in(Global);
        let mut heads: Vec<TextRange, Global> = Vec::new_in(Global);
        for (_, def, path) in &members {
            match path {
                // alloc: the label of another crate's copy, once per site
                Some(path) => {
                    sites.push(Site { place: Place::Crate, label: format!("`{path}`"), here: false });
                    heads.push(TextRange::default());
                }
                None => {
                    let item = self.peek_item(*def);
                    let (head, label) = self.head(*def, item);
                    let head = self.peek(*def).and_then(|it| it.name).unwrap_or(head);
                    sites.push(Site { place: Place::Body { def: *def, range: head }, label, here: false });
                    heads.push(head);
                    self.covered.push((*def, item));
                }
            }
        }
        let first = &members[0];
        let forward = (first.0.krate == OWN).then(|| {
            let read = self.peek(first.1)?;
            let name = slice(read.text, read.name?).to_owned(); // alloc: the fix's name
            Some(Forward { name })
        });
        let forward = forward.flatten();
        let first_module = hir::DefWithBody::from(first.1).module(db);
        let reported: Vec<usize, Global> = {
            let mut out = Vec::new_in(Global);
            if lint == Lint::SameImplFn {
                out.extend(members.iter().position(|it| it.0.krate == OWN));
            } else {
                out.extend((1..members.len()).filter(|&i| members[i].0.krate == OWN));
            }
            out
        };
        for index in reported {
            let def = members[index].1;
            let mut here = sites.clone(); // clone: each finding names every copy
            here[index].here = true;
            let same_module = hir::DefWithBody::from(def).module(db) == first_module;
            let forward = match (&forward, roles[0], roles[index]) {
                (Some(it), Role::Free, Role::Free) if same_module && lint == Lint::SameFn => {
                    Some(it.clone()) // clone: the fix's name, per finding
                }
                _ => None,
            };
            let found = Found {
                lint,
                at: heads[index],
                sites: here.into_boxed_slice(),
                note: Note::Whole { forward },
            };
            self.out.push((def, found));
        }
    }

    /// The item range of the crate's body `def`.
    fn peek_item(&mut self, def: DefWithBodyId) -> TextRange {
        self.load(def);
        self.peek(def).map_or_else(TextRange::default, |it| it.item)
    }

    /// Same blocks and rows of statements in the crate.
    fn blocks(&mut self) {
        let mut all: Vec<(u64, Order, usize, usize), Global> = Vec::new_in(Global);
        for (body, &(_, shape, _, _)) in self.bodies.iter().enumerate() {
            for (index, piece) in shape.pieces.iter().enumerate() {
                all.push((piece.hash, self.order(body, piece.range), body, index));
            }
        }
        all.sort_unstable();
        let mut groups: Vec<Vec<Member, Global>, Global> = Vec::new_in(Global);
        let mut start = 0;
        while start < all.len() {
            let hash = all[start].0;
            let end = start + all[start..].iter().take_while(|it| it.0 == hash).count();
            if end - start >= 2 {
                let mut group = Vec::new_in(Global);
                for &(_, order, body, index) in &all[start..end] {
                    let (def, shape, _, _) = self.bodies[body];
                    let piece = shape.pieces[index];
                    group.push(Member { def, order, range: piece.range, run: piece.run });
                }
                groups.push(group);
            }
            start = end;
        }
        let mut runs = chain(groups);
        for group in &mut runs {
            drop_overlaps(group);
        }
        runs.retain(|it| it.len() >= 2);
        runs.sort_by(|a, b| {
            (std::cmp::Reverse(a[0].range.len()), a[0].order)
                .cmp(&(std::cmp::Reverse(b[0].range.len()), b[0].order))
        });
        for group in runs {
            let covered = group.iter().all(|member| {
                self.covered.iter().any(|(def, range)| *def == member.def && range.contains_range(member.range))
            });
            if covered {
                continue;
            }
            self.block_group(group);
        }
    }

    /// Reports one group of same blocks at its first copy.
    fn block_group(&mut self, group: Vec<Member, Global>) {
        let mut sites: Vec<Site, Global> = Vec::new_in(Global);
        for member in &group {
            let (head, name) = self.head(member.def, member.range);
            // alloc: a label of the message, per copy
            let label = format!("in {name}");
            sites.push(Site { place: Place::Body { def: member.def, range: head }, label, here: false });
            self.covered.push((member.def, member.range));
        }
        let first = group[0];
        sites[0].here = true;
        let (free, weight) = match self.peek(first.def) {
            Some(read) => {
                let run = shape::within(&read.toks, first.range);
                let mut free = Vec::new_in(Global);
                // alloc: the names of the free locals, per finding
                free.extend(shape::free(run).into_iter().map(|it| slice(read.text, it).to_owned()));
                (free.into_boxed_slice(), u32::try_from(run.len()).unwrap_or(u32::MAX))
            }
            None => (Vec::new_in(Global).into_boxed_slice(), 0),
        };
        let found = Found {
            lint: Lint::SameBlock,
            at: first.range,
            sites: sites.into_boxed_slice(),
            note: Note::Block { free, weight },
        };
        self.out.push((first.def, found));
    }

    /// Near-duplicate functions in the crate.
    fn near(&mut self) {
        let mut keys: Vec<(u64, Order, usize), Global> = Vec::new_in(Global);
        for (body, &(_, shape, _, _)) in self.bodies.iter().enumerate() {
            let Some(sketch) = &shape.sketch else { continue };
            let order = self.order(body, TextRange::default());
            for band in 0..near::BANDS {
                keys.push((near::band_key(sketch, band), order, body));
            }
        }
        keys.sort_unstable();
        let mut pairs: Vec<(Order, usize, Order, usize), Global> = Vec::new_in(Global);
        let mut start = 0;
        while start < keys.len() {
            let key = keys[start].0;
            let end = start + keys[start..].iter().take_while(|it| it.0 == key).count();
            if end - start <= near::BUCKET_CAP {
                for a in start..end {
                    for b in a + 1..end {
                        pairs.push((keys[a].1, keys[a].2, keys[b].1, keys[b].2));
                    }
                }
            }
            start = end;
        }
        pairs.sort_unstable();
        pairs.dedup();
        // The best earlier match of each later body.
        let mut best: Vec<(usize, usize, u32), Global> = Vec::new_in(Global);
        for (_, a, _, b) in pairs {
            let (da, sa, _, _) = self.bodies[a];
            let (db_, sb, _, _) = self.bodies[b];
            if a == b || (sa.role.is_trait() && sb.role.is_trait()) {
                continue;
            }
            if sa.whole.is_some() && sa.whole == sb.whole {
                continue;
            }
            let (Some(ka), Some(kb)) = (&sa.sketch, &sb.sketch) else { continue };
            if near::agreement(ka, kb) < near::SKETCH_FLOOR {
                continue;
            }
            self.load(da);
            self.load(db_);
            let shingles = |read: Option<&Read<'_>>| match read.and_then(|it| Some((it, it.body?))) {
                Some((read, body)) => shape::shingles(shape::within(&read.toks, body), near::WIDTH),
                None => Vec::new_in(Global),
            };
            let percent = near::jaccard(&shingles(self.peek(da)), &shingles(self.peek(db_)));
            if percent < near::JACCARD_FLOOR {
                continue;
            }
            match best.iter_mut().find(|it| it.1 == b) {
                Some(it) if it.2 >= percent => {}
                Some(it) => *it = (a, b, percent),
                None => best.push((a, b, percent)),
            }
        }
        for (a, b, percent) in best {
            self.paired.push((a, b));
            // The stronger finding wins: a pair that generalises is said as that, once.
            if let Some(note) = self.judged(a, b, Some(percent)) {
                self.pair_finding(a, b, note);
                continue;
            }
            let (da, _, _, _) = self.bodies[a];
            let (db_, _, _, _) = self.bodies[b];
            let diff = match (self.peek(da), self.peek(db_)) {
                (Some(x), Some(y)) => near::diff(slice(x.text, x.item), slice(y.text, y.item)),
                _ => None,
            };
            self.pair_finding(a, b, Note::Near { percent, diff });
        }
    }

    /// The two copies of a pair as a finding names them, the later one here, and its range.
    fn pair_sites(&mut self, a: usize, b: usize) -> (TextRange, Box<[Site]>) {
        let (da, _, _, _) = self.bodies[a];
        let (db_, _, _, _) = self.bodies[b];
        let item_a = self.peek_item(da);
        let item_b = self.peek_item(db_);
        let (head_a, label_a) = self.head(da, item_a);
        let (head_b, label_b) = self.head(db_, item_b);
        let head_a = self.peek(da).and_then(|it| it.name).unwrap_or(head_a);
        let head_b = self.peek(db_).and_then(|it| it.name).unwrap_or(head_b);
        let mut sites = Vec::new_in(Global);
        sites.push(Site { place: Place::Body { def: da, range: head_a }, label: label_a, here: false });
        sites.push(Site { place: Place::Body { def: db_, range: head_b }, label: label_b, here: true });
        (head_b, sites.into_boxed_slice())
    }

    /// Reports the pair `a`, `b` at `b`, as `note` says.
    fn pair_finding(&mut self, a: usize, b: usize, note: Note) {
        let lint = match note {
            Note::Extract { .. } => Lint::ExtractableDuplicate,
            Note::ByType { .. } => Lint::ImplsDifferByType,
            _ => Lint::NearDuplicate,
        };
        let (at, sites) = self.pair_sites(a, b);
        let (def, _, _, _) = self.bodies[b];
        self.out.push((def, Found { lint, at, sites, note }));
    }

    /// The generalisation of the crate's bodies `a` and `b` ([`unify::unify`]), each hole
    /// named by its two texts; `None` past its bounds.
    fn generalise(&mut self, a: usize, b: usize) -> Option<Box<[HoleAt]>> {
        let db = self.db;
        let (da, _, _, _) = self.bodies[a];
        let (db_, _, _, _) = self.bodies[b];
        self.load(da);
        self.load(db_);
        let (p, q) = (self.peek(da)?, self.peek(db_)?);
        let (sa, sb) = (body_syntax(db, da).as_ref()?, body_syntax(db, db_).as_ref()?);
        let (ta, tb) = (sa.tree(db), sb.tree(db));
        let x = ast::try_to_node(sa.item, ast::Node::root(&ta))?;
        let y = ast::try_to_node(sb.item, ast::Node::root(&tb))?;
        let general = unify::unify(x, y, &p.toks, &q.toks)?;
        // What the two share must be worth a function of its own.
        if general.common_tokens < FN_FLOOR {
            return None;
        }
        let mut out = Vec::new_in(Global);
        for hole in general.holes {
            out.push(HoleAt {
                class: hole.class,
                a: hole.a,
                b: hole.b,
                uses: hole.uses,
                text_a: slice(p.text, hole.a).to_owned(), // alloc: the message's text
                text_b: slice(q.text, hole.b).to_owned(), // alloc: the message's text
            });
        }
        Some(out.into_boxed_slice())
    }

    /// What the pair `a`, `b` generalises to: one function with parameters, one generic
    /// function, or neither (no holes, past the bounds, or a hole of a method or statements).
    fn judged(&mut self, a: usize, b: usize, near: Option<u32>) -> Option<Note> {
        use unify::HoleClass as C;
        let holes = self.generalise(a, b)?;
        if holes.is_empty() {
            return None;
        }
        let all = |ok: &[C]| holes.iter().all(|it| ok.contains(&it.class));
        if all(&[C::Type]) {
            return Some(Note::ByType { holes });
        }
        if all(&[C::Literal, C::Path, C::Field, C::Expr]) {
            let (first, _, _, _) = self.bodies[a];
            return Some(Note::Extract { first, holes, near });
        }
        None
    }

    /// Functions of one structure that `near` did not pair (shorter than its floor, or
    /// differing in more of their names than its shingles hold), generalised against the
    /// first of their structure: each later one is reported once.
    fn outlines(&mut self) {
        let mut all: Vec<(u64, Order, usize), Global> = Vec::new_in(Global);
        for (body, &(_, shape, _, _)) in self.bodies.iter().enumerate() {
            if let Some(outline) = shape.outline
                && !shape.role.is_trait()
            {
                all.push((outline, self.order(body, TextRange::default()), body));
            }
        }
        all.sort_unstable();
        let mut start = 0;
        while start < all.len() {
            let key = all[start].0;
            let end = start + all[start..].iter().take_while(|it| it.0 == key).count();
            let group = start..end;
            start = end;
            if !(2..=paste::PASTE_BUCKET_CAP).contains(&group.len()) {
                continue;
            }
            let first = all[group.start].2;
            for &(_, _, b) in &all[group.start + 1..group.end] {
                let (sa, sb) = (self.bodies[first].1, self.bodies[b].1);
                if sa.whole == sb.whole || self.paired.iter().any(|it| it.1 == b) {
                    continue;
                }
                let (db_, _, _, _) = self.bodies[b];
                let item = self.peek_item(db_);
                if self.covered.iter().any(|(def, range)| *def == db_ && range.contains_range(item)) {
                    continue;
                }
                self.paired.push((first, b));
                if let Some(note) = self.judged(first, b, None) {
                    self.pair_finding(first, b, note);
                }
            }
        }
    }

    /// Tests of one shape but their literals: one finding at the first of each group, naming
    /// the rest, and how many literals tell them apart.
    fn tests(&mut self) {
        let mut all: Vec<(u64, Order, usize), Global> = Vec::new_in(Global);
        for (body, &(_, shape, _, _)) in self.bodies.iter().enumerate() {
            if let Some(erased) = shape.erased {
                all.push((erased, self.order(body, TextRange::default()), body));
            }
        }
        all.sort_unstable();
        let mut start = 0;
        while start < all.len() {
            let key = all[start].0;
            let end = start + all[start..].iter().take_while(|it| it.0 == key).count();
            let group = start..end;
            start = end;
            if group.len() < 2 {
                continue;
            }
            let mut members: Vec<usize, Global> = Vec::new_in(Global);
            members.extend(all[group].iter().map(|it| it.2));
            self.table(&members);
        }
    }

    /// Reports one group of tests that differ in literals alone, at its first.
    fn table(&mut self, members: &[usize]) {
        let first = members[0];
        let (first_def, _, _, _) = self.bodies[first];
        // The literals that differ: the most holes of the first against any other.
        let mut values = 0;
        for &other in &members[1..] {
            if let Some(holes) = self.generalise(first, other) {
                values = values.max(holes.len());
            }
        }
        let mut sites: Vec<Site, Global> = Vec::new_in(Global);
        let mut at = TextRange::default();
        for (index, &body) in members.iter().enumerate() {
            let (def, _, _, _) = self.bodies[body];
            let item = self.peek_item(def);
            let (head, label) = self.head(def, item);
            let head = self.peek(def).and_then(|it| it.name).unwrap_or(head);
            if index == 0 {
                at = head;
            }
            sites.push(Site { place: Place::Body { def, range: head }, label, here: index == 0 });
        }
        let found = Found {
            lint: Lint::TestsDifferByLiteral,
            at,
            sites: sites.into_boxed_slice(),
            note: Note::Table {
                rows: u32::try_from(members.len()).unwrap_or(u32::MAX),
                values: u32::try_from(values).unwrap_or(u32::MAX),
            },
        };
        self.out.push((first_def, found));
    }

    /// Renames a copy did not finish, between pieces of one structure in the crate.
    fn pastes(&mut self) {
        let mut all: Vec<(u64, Order, usize, usize), Global> = Vec::new_in(Global);
        for (body, &(_, shape, _, _)) in self.bodies.iter().enumerate() {
            for (index, piece) in shape.skeletons.iter().enumerate() {
                all.push((piece.hash, self.order(body, piece.range), body, index));
            }
        }
        all.sort_unstable();
        let mut start = 0;
        while start < all.len() {
            let hash = all[start].0;
            let end = start + all[start..].iter().take_while(|it| it.0 == hash).count();
            if (2..=paste::PASTE_BUCKET_CAP).contains(&(end - start)) {
                for a in start..end {
                    for b in a + 1..end {
                        let p = self.member(all[a].2, all[a].3, all[a].1);
                        let q = self.member(all[b].2, all[b].3, all[b].1);
                        if p.def == q.def && p.range.intersect(q.range).is_some() {
                            continue;
                        }
                        self.paste_pair(p, q);
                        self.paste_pair(q, p);
                    }
                }
            }
            start = end;
        }
    }

    fn member(&self, body: usize, index: usize, order: Order) -> Member {
        let (def, shape, _, _) = self.bodies[body];
        let piece = shape.skeletons[index];
        Member { def, order, range: piece.range, run: piece.run }
    }

    /// Reports the unfinished rename of `original` in `copy`, if there is one.
    fn paste_pair(&mut self, original: Member, copy: Member) {
        self.load(original.def);
        self.load(copy.def);
        let (Some(p), Some(q)) = (self.peek(original.def), self.peek(copy.def)) else { return };
        let tp = shape::within(&p.toks, original.range);
        let tq = shape::within(&q.toks, copy.range);
        let Some(found) = paste::mismatch(tp, tq) else { return };
        let (Some(at), Some(renamed)) = (tq.get(found.at), tq.get(found.renamed_at)) else { return };
        let at = at.range;
        let from = slice(q.text, at).to_owned(); // alloc: the message's name
        let to = slice(q.text, renamed.range).to_owned(); // alloc: the message's name
        let (head, label) = self.head(original.def, original.range);
        let mut sites = Vec::new_in(Global);
        // alloc: a label of the message
        sites.push(Site { place: Place::Body { def: original.def, range: head }, label: format!("the copy in {label}"), here: false });
        // alloc: a label of the message
        sites.push(Site { place: Place::Body { def: copy.def, range: at }, label: String::new(), here: true });
        let found = Found {
            lint: Lint::PasteMismatch,
            at,
            sites: sites.into_boxed_slice(),
            note: Note::Paste {
                from,
                to,
                renamed: u32::try_from(found.renamed).unwrap_or(u32::MAX),
                kept: u32::try_from(found.kept).unwrap_or(u32::MAX),
            },
        };
        self.out.push((copy.def, found));
    }
}

/// `members` split by their code identity ([`blaze_items::code_hash`]): two copies whose text
/// is the same but whose paths reach different items (`self.len()` of two types) are two
/// functions. A group with a copy that has no code hash stays as its shape made it.
fn split_by_code(
    db: &Db,
    members: Vec<(Order, DefWithBodyId, Option<String>), Global>,
) -> Vec<Vec<(Order, DefWithBodyId, Option<String>), Global>, Global> {
    let mut hashes = Vec::new_in(Global);
    for member in &members {
        match blaze_items::code_hash(db, ItemId::of_body(db, member.1)) {
            Ok(Some(hash)) => hashes.push(hash),
            _ => break,
        }
    }
    if hashes.len() < members.len() {
        let mut whole = Vec::new_in(Global);
        whole.push(members);
        return whole;
    }
    let mut classes: Vec<(blaze_items::CodeHash, Vec<(Order, DefWithBodyId, Option<String>), Global>), Global> =
        Vec::new_in(Global);
    for (member, hash) in members.into_iter().zip(hashes) {
        match classes.iter_mut().find(|it| it.0 == hash) {
            Some(class) => class.1.push(member),
            None => {
                let mut class = Vec::new_in(Global);
                class.push(member);
                classes.push((hash, class));
            }
        }
    }
    let mut out = Vec::new_in(Global);
    out.extend(classes.into_iter().map(|it| it.1));
    out
}

/// The groups of rows of statements joined where each copy's next row is a copy too, in one
/// group of the same size: a run of `n` rows is `n - 1` seeds, chained here into one copy.
fn chain(groups: Vec<Vec<Member, Global>, Global>) -> Vec<Vec<Member, Global>, Global> {
    let mut seed_at: HashMap<(DefWithBodyId, u32, u32), usize, Global> = HashMap::new_in(Global);
    for (index, group) in groups.iter().enumerate() {
        for member in group {
            if let Run::Stmts { list, first, .. } = member.run {
                seed_at.insert((member.def, list, first), index);
            }
        }
    }
    let successor = |index: usize| -> Option<usize> {
        let group = &groups[index];
        let mut next = None;
        for member in group {
            let Run::Stmts { list, first, .. } = member.run else { return None };
            let at = *seed_at.get(&(member.def, list, first + 1))?;
            if next.is_some_and(|it| it != at) || at == index {
                return None;
            }
            next = Some(at);
        }
        next.filter(|&it| groups[it].len() == group.len())
    };
    let mut next: Vec<Option<usize>, Global> = Vec::new_in(Global);
    next.extend((0..groups.len()).map(successor));
    let mut has_prev: Vec<bool, Global> = Vec::new_in(Global);
    has_prev.resize(groups.len(), false);
    for it in next.iter().flatten() {
        has_prev[*it] = true;
    }
    let mut out = Vec::new_in(Global);
    for (index, group) in groups.iter().enumerate() {
        if has_prev[index] {
            continue;
        }
        let mut members = group.clone(); // clone: the run's copies, grown below
        let mut at = index;
        while let Some(then) = next[at] {
            // Each copy's `first` is the row its last seed starts at, moved one on per step.
            for member in &mut members {
                let Run::Stmts { list, first, last } = member.run else { continue };
                let Some(more) = groups[then].iter().find(|it| {
                    it.def == member.def
                        && matches!(it.run, Run::Stmts { list: l, first: f, .. } if l == list && f == first + 1)
                }) else {
                    continue;
                };
                let Run::Stmts { last: more_last, .. } = more.run else { continue };
                member.range = member.range.cover(more.range);
                member.run = Run::Stmts { list, first: first + 1, last: more_last.max(last) };
            }
            at = then;
        }
        out.push(members);
    }
    out
}

/// Drops each copy that overlaps an earlier one of its group in the same body.
fn drop_overlaps(group: &mut Vec<Member, Global>) {
    group.sort_by_key(|it| it.order);
    let mut kept: Vec<Member, Global> = Vec::new_in(Global);
    for member in group.drain(..) {
        let overlaps = kept.iter().any(|it| it.def == member.def && it.range.intersect(member.range).is_some_and(|r| !r.is_empty()));
        if !overlaps {
            kept.push(member);
        }
    }
    *group = kept;
}

/// The duplicate types of a crate's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TypeEntry {
    /// The type's fields or variants as a set.
    key: u64,
    /// As a list, in order.
    in_order: u64,
    file: EditionedFileId,
    order: (u32, u32),
    /// Its name's range in its file.
    name_at: TextRange,
    name: String,
    is_enum: bool,
}

/// The structs and enums of a crate past their floors, sorted by key and place.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Types(Box<[TypeEntry]>);

/// The fewest fields of a struct, or variants of an enum with data, two copies of which are one
/// type written twice: two fields alike is a pair (`start`, `end`), not a type.
pub(crate) const FIELDS_FLOOR: usize = 3;
/// The fewest variants of an enum without data two copies of which are one type.
pub(crate) const UNIT_VARIANTS_FLOOR: usize = 4;

/// The types of `krate`'s module files, outside test code.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_types(db: &Db, krate: base_db::Crate) -> Types {
    let mut out: Vec<TypeEntry, Global> = Vec::new_in(Global);
    if is_test_target(db, krate) {
        return Types::default();
    }
    let plan = query::lint_plan(db, krate);
    let mut test_files: Vec<bool, Global> = Vec::new_in(Global);
    for (index, module) in plan.files.iter().enumerate() {
        let test = match module.declared {
            Some((file, range)) => {
                let parent = plan.files.iter().position(|it| it.file == file);
                parent.and_then(|it| test_files.get(it).copied()).unwrap_or(false)
                    || declared_in_test(db, file, range)
            }
            None => false,
        };
        test_files.push(test);
        if test {
            continue;
        }
        let root = module.file.parse(db).root();
        let mut walk = root.preorder();
        while let Some(event) = walk.next() {
            let WalkEvent::Enter(node) = event else { continue };
            let kind = node.kind();
            if test_attr(node) || matches!(kind, SyntaxKind::FN | SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_CALL) {
                walk.skip_subtree();
                continue;
            }
            let Some((key, in_order, is_enum)) = fingerprint(node) else { continue };
            let Some(name) = node.children().find(|it| it.kind() == SyntaxKind::NAME) else { continue };
            let file_index = u32::try_from(index).unwrap_or(u32::MAX);
            out.push(TypeEntry {
                key,
                in_order,
                file: module.file,
                order: (file_index, u32::from(name.text_range().start())),
                name_at: name.text_range(),
                name: name.text().to_owned(), // alloc: the type's name, per type past the floors
                is_enum,
            });
        }
    }
    out.sort_by(|a, b| (a.key, a.order).cmp(&(b.key, b.order)));
    Types(out.into_boxed_slice())
}

/// Whether the `mod` item at `range` of `file` is test code.
fn declared_in_test(db: &Db, file: EditionedFileId, range: TextRange) -> bool {
    let root = file.parse(db).root();
    let start = match root.covering_element(range) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => match it.parent() {
            Some(it) => it,
            None => return false,
        },
    };
    std::iter::once(start).chain(start.ancestors()).any(test_attr)
}

/// A struct's or enum's fields or variants, as a set and as a list, when they are past the
/// floors: each by its text without trivia, attributes or visibility, with the type's generic
/// parameters. A tuple struct's are a list either way.
pub(crate) fn fingerprint(node: ast::Node<'_>) -> Option<(u64, u64, bool)> {
    use std::hash::{Hash as _, Hasher as _};
    let is_enum = match node.kind() {
        SyntaxKind::STRUCT => false,
        SyntaxKind::ENUM => true,
        _ => return None,
    };
    let child = |kind: SyntaxKind| node.children().find(|it| it.kind() == kind);
    let generics = child(SyntaxKind::GENERIC_PARAM_LIST).map_or(0, member_hash);
    let list = if is_enum {
        child(SyntaxKind::VARIANT_LIST)?
    } else {
        node.children().find(|it| {
            matches!(it.kind(), SyntaxKind::RECORD_FIELD_LIST | SyntaxKind::TUPLE_FIELD_LIST)
        })?
    };
    let tuple = list.kind() == SyntaxKind::TUPLE_FIELD_LIST;
    let mut members: Vec<u64, Global> = Vec::new_in(Global);
    members.extend(
        list.children()
            .filter(|it| matches!(it.kind(), SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD | SyntaxKind::VARIANT))
            .map(member_hash),
    );
    let with_data = is_enum
        && list.children().any(|variant| {
            variant.children().any(|it| {
                matches!(it.kind(), SyntaxKind::RECORD_FIELD_LIST | SyntaxKind::TUPLE_FIELD_LIST)
            })
        });
    let enough = if is_enum && !with_data {
        members.len() >= UNIT_VARIANTS_FLOOR
    } else {
        members.len() >= FIELDS_FLOOR
    };
    if !enough {
        return None;
    }
    let hash = |members: &[u64]| {
        let mut hasher = blaze_hash::FixedHasher::with_seed(4);
        (is_enum, tuple, generics).hash(&mut hasher);
        members.hash(&mut hasher);
        hasher.finish()
    };
    let in_order = hash(members.as_slice());
    if !tuple {
        members.sort_unstable();
    }
    Some((hash(members.as_slice()), in_order, is_enum))
}

/// The hash of a field's, variant's or parameter list's tokens but trivia, attributes and
/// visibility.
fn member_hash(node: ast::Node<'_>) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = blaze_hash::FixedHasher::with_seed(5);
    let mut walk = node.preorder_with_tokens();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(element) = event else { continue };
        match element {
            NodeOrToken::Node(it) => {
                if matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::VISIBILITY) {
                    walk.skip_subtree();
                }
            }
            NodeOrToken::Token(it) => {
                if !it.kind().is_trivia() {
                    it.text().hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

/// A duplicate type of a file, before its level is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TypeFound {
    /// The type's name, in its file.
    pub(crate) at: TextRange,
    /// Every copy, this one among them, in order: a file and the name's range there, or
    /// another crate's, by its name alone.
    pub(crate) sites: Box<[(Option<(EditionedFileId, TextRange)>, String, bool)]>,
    /// Whether every copy lists its fields in one order.
    pub(crate) same_order: bool,
    pub(crate) is_enum: bool,
}

/// The duplicate types of `krate`'s `file`: each type past the floors whose fields or variants
/// an earlier type of the crate, or one of its local dependencies, has too.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn type_findings(db: &Db, krate: base_db::Crate, file: EditionedFileId) -> Box<[TypeFound]> {
    let own = &crate_types(db, krate).0;
    let deps = local_deps(db, krate);
    let mut out: Vec<TypeFound, Global> = Vec::new_in(Global);
    let mut start = 0;
    while start < own.len() {
        let key = own[start].key;
        let end = start + own[start..].iter().take_while(|it| it.key == key).count();
        let ours = &own[start..end];
        start = end;
        if !ours.iter().any(|it| it.file == file) {
            continue;
        }
        let mut sites: Vec<(Option<(EditionedFileId, TextRange)>, String, bool), Global> =
            Vec::new_in(Global);
        let mut in_order: Vec<u64, Global> = Vec::new_in(Global);
        for &dep in &deps {
            let theirs = &crate_types(db, dep).0;
            let from = theirs.partition_point(|it| it.key < key);
            let name = hir::Crate::from(dep)
                .display_name(db)
                .map(|it| it.to_string())
                .unwrap_or_default();
            for entry in theirs[from..].iter().take_while(|it| it.key == key) {
                // alloc: a label of the message, per copy
                sites.push((None, format!("`{}` in crate `{name}`", entry.name), false));
                in_order.push(entry.in_order);
            }
        }
        let first_own = sites.len();
        for entry in ours {
            // alloc: a label of the message, per copy
            sites.push((Some((entry.file, entry.name_at)), format!("`{}`", entry.name), false));
            in_order.push(entry.in_order);
        }
        if sites.len() < 2 {
            continue;
        }
        let same_order = in_order.iter().all(|it| *it == in_order[0]);
        for (index, entry) in ours.iter().enumerate() {
            let index = first_own + index;
            if index == 0 || entry.file != file {
                continue;
            }
            let mut here = sites.clone(); // clone: each finding names every copy
            here[index].2 = true;
            out.push(TypeFound {
                at: entry.name_at,
                sites: here.into_boxed_slice(),
                same_order,
                is_enum: entry.is_enum,
            });
        }
    }
    out.sort_by_key(|it| (it.at.start(), it.at.end()));
    out.into_boxed_slice()
}

/// One shape written in two or more of the crates merged.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "made by `merge_siblings`, which blaze-findings' workspace pass calls")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiblingCopy<C, T> {
    pub(crate) hash: u64,
    /// Every copy, by crate in the order the crates were given, then by row.
    pub(crate) items: Vec<(C, T), Global>,
    /// The copies' home, their nearest common dependency: the caller fills it.
    pub(crate) common_dep: Option<C>,
}

/// The shapes two or more crates each hold, from each crate's sorted rows ([`crate_wholes`],
/// a [`WholeRow`] each): a k-way merge, one heap entry per crate, so `n` rows over `k` crates
/// cost `n log k`. Pure; a crate's own findings never depend on it (they read its
/// dependencies alone), so the workspace's pass is the only reader.
///
/// Caller: blaze-findings calls it with the workspace's crates (each its name and its
/// [`crate_wholes`], an item named by its path and range) and reports
/// `duplicate_across_crates` for each copy whose crates do not depend on one another, after
/// filling [`SiblingCopy::common_dep`].
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "blaze-findings' workspace pass calls it; until it is wired, the tests do")
)]
pub(crate) fn merge_siblings<C: Copy, T: Copy>(
    crates: &[(C, &[(u64, T)])],
) -> Vec<SiblingCopy<C, T>, Global> {
    use std::{cmp::Reverse, collections::BinaryHeap};
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>, Global> =
        BinaryHeap::with_capacity_in(crates.len(), Global);
    for (index, (_, rows)) in crates.iter().enumerate() {
        if let Some(row) = rows.first() {
            heap.push(Reverse((row.0, index, 0)));
        }
    }
    let mut out = Vec::new_in(Global);
    let mut group: Vec<(usize, usize), Global> = Vec::new_in(Global);
    while let Some(&Reverse((hash, _, _))) = heap.peek() {
        group.clear();
        while let Some(&Reverse((next, krate, row))) = heap.peek() {
            if next != hash {
                break;
            }
            heap.pop();
            group.push((krate, row));
            if let Some(after) = crates[krate].1.get(row + 1) {
                heap.push(Reverse((after.0, krate, row + 1)));
            }
        }
        group.sort_unstable();
        let first_crate = group[0].0;
        if group.iter().all(|it| it.0 == first_crate) {
            continue;
        }
        let mut items = Vec::with_capacity_in(group.len(), Global);
        items.extend(group.iter().map(|&(krate, row)| (crates[krate].0, crates[krate].1[row].1)));
        out.push(SiblingCopy { hash, items, common_dep: None });
    }
    out
}
