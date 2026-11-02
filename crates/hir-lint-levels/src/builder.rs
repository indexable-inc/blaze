//! rustc's `LintLevelsBuilder`: the level of each lint at each node, from the command line
//! down through the attributes of what is around the node, as the early lint pass has it
//! (`TopDown`: a `forbid` holds, and what overrules one is an error) or as the HIR has it
//! (`shallow_lint_levels_on`: the innermost attribute wins). What rustc's builder says of the
//! names in them, a builder that speaks hands to its [`Sink`] as [`Event`]s, and the HIR one
//! keeps the `#[expect]`s it finds.
//!
//! Cost: a node with no attribute costs the walk a check of its kind. A level costs one hash
//! of its node (or of each node out to the nearest one seen), and one bit test for a lint no
//! attribute nor flag names; else one bit test of each set of levels from there out and a scan
//! of the 2-byte lint ids of the one that has it. A level is a 12-byte [`Spec`].
//!
//! Follows rustc's `compiler/rustc_lint/src/levels.rs` (`add_command_line`, `insert_spec`,
//! `add`, `check_gated_lint`) and `compiler/rustc_middle/src/lint.rs`
//! (`reveal_actual_level_spec`, `LintExpectation`) (MIT or Apache-2.0).

use std::{hash::BuildHasher, num::NonZeroU32, ops::Range};

use base_db::{Crate, Db};
use blaze_hash::{FixedState, HashMap};
use blaze_lint_defs::{Checked, Feature, Level, Lint, LintId, LintOptions, LintSet, Store};
use cfg::CfgOptions;
use hashbrown::HashTable;
use hir_def::unstable_features::UnstableFeatures;
use hir_expand::{HirFileId, InFile};
use intern::Symbol;
use span::Edition;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use crate::attrs::{self, ItemKind, Read, Reads};

/// A lint's level where it is: rustc's `LevelSpec`, as 12 bytes. What rustc keeps in it of
/// where the level came from is in [`LevelCx::source`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub level: Level,
    /// For `expect`, and `--force-warn` over one: the expectation a lint at it fulfills.
    pub expect: Option<ExpectId>,
    pub source: SourceId,
}

const _: () = assert!(size_of::<Spec>() == 12);

/// An `#[expect]`'s name, by where it is: numbered from 1 as the builders of a crate come to
/// them, the same to both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExpectId(NonZeroU32);

impl ExpectId {
    /// `0..` the number of expectations.
    pub fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// An index into [`LevelCx`]'s sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceId(u32);

impl SourceId {
    /// rustc's `LintLevelSource::Default`, always the first.
    pub const DEFAULT: SourceId = SourceId(0);
}

/// Where a level came from: rustc's `LintLevelSource`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    /// An attribute: the name it sets the level of (the new one of a renamed lint, with its
    /// tool for a tool's), where the name is, and its `reason`.
    Node { name: Symbol, at: InFile<TextRange>, reason: Option<Symbol> },
    /// The command line: the name given, with `_` for `-`, and the level of its flag.
    CommandLine { name: Box<str>, level: Level },
}

/// What deciding levels in a crate reads, and what the builders of the crate find and share:
/// where each level came from and the `#[expect]`s by where they are.
pub struct LevelCx<'a, 'db> {
    pub db: &'db Db,
    pub krate: Crate,
    pub edition: Edition,
    pub cfg: &'db CfgOptions,
    /// The lint flags: rustc's `lint_opts` and `lint_cap`.
    pub opts: &'a LintOptions,
    /// The crate's root file.
    pub root: HirFileId,
    /// The files of the crate's `-Z crate-attr`s, the crate's attributes after its own.
    crate_attrs: Box<[HirFileId]>,
    pub store: Store,
    features: &'db UnstableFeatures,
    /// The level the first `-D warnings` or the like gives `warnings` on the command line.
    pub cmd_warnings: Option<Level>,
    /// Where each level came from, by [`SourceId`]: one per name in an attribute or on the
    /// command line, not one per lint or per node.
    sources: Vec<Source>,
    /// Each `#[expect]`'s name by where it is, hashed once as both builders come to it.
    expects: HashMap<(HirFileId, TextRange), ExpectId>,
}

impl<'a, 'db> LevelCx<'a, 'db> {
    /// The levels of `krate` with the lint flags `opts`.
    pub fn new(db: &'db Db, krate: Crate, opts: &'a LintOptions) -> LevelCx<'a, 'db> {
        let edition = krate.data(db).edition;
        let cfg = krate.cfg_options(db);
        let root = HirFileId::from(krate.root_file_id(db));
        // alloc: setup, once per crate's levels; empty but for `-Z crate-attr`
        let crate_attrs: Box<[HirFileId]> = base_db::applied_crate_attr_files(db, krate).iter().map(|&it| HirFileId::from(it)).collect();
        let root_attrs = ast::AnyHasAttrs::cast(root.root(db)).into_iter().flat_map(|it| ast::attrs_including_inner(&it));
        let cli_attrs = crate_attrs
            .iter()
            .filter_map(|it| ast::SourceFile::cast(it.root(db)))
            .flat_map(|it| it.attrs());
        let registered_tools = attrs::registered_tools(root_attrs.chain(cli_attrs), cfg, edition);
        LevelCx {
            db,
            krate,
            edition,
            cfg,
            opts,
            root,
            crate_attrs,
            store: Store { internal: false, registered_tools },
            features: UnstableFeatures::query(db, krate),
            cmd_warnings: opts.opts.iter().find(|(it, _)| &**it == "warnings").map(|&(_, it)| it),
            sources: vec![Source::Default], // alloc: once per crate's levels
            expects: HashMap::default(),
        }
    }

    /// Where the level of `id` came from.
    pub fn source(&self, id: SourceId) -> &Source {
        &self.sources[id.0 as usize]
    }

    fn add_source(&mut self, source: Source) -> SourceId {
        let id = SourceId(u32::try_from(self.sources.len()).expect("fewer than 2^32 lint names"));
        self.sources.push(source);
        id
    }

    /// The expectation of the `#[expect]` name at `range` of `file`, the same to both builders.
    fn expect_id(&mut self, file: HirFileId, range: TextRange) -> ExpectId {
        let next = self.expects.len();
        *self.expects.entry((file, range)).or_insert_with(|| {
            let id = u32::try_from(next + 1).ok().and_then(NonZeroU32::new).expect("fewer than 2^32 expectations");
            ExpectId(id)
        })
    }

    /// The node `at` points to, as a view of the database's tree.
    pub fn node(&self, at: InFile<SyntaxNodePtr>) -> InFile<ast::Node<'db>> {
        InFile::new(at.file_id, ast::to_node(at.value, at.file_id.root(self.db)))
    }
}

/// What is wrong with an element of a lint attribute's list that is not a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// `reason = ..` with a value that is not a string literal.
    ReasonNotString,
    /// A `name = value` that is not a reason, a `name(..)`, or a literal.
    BadArgument,
    /// `reason = ..` before the last element.
    ReasonNotLast,
}

/// What rustc's builder says of the command line and of the attributes, as facts: a builder
/// that speaks hands each to its [`Sink`] where rustc's builder says it, before the levels of
/// the name are set.
pub enum Event<'a, 'db> {
    /// `--force-warn warnings`, which rustc refuses (E0602).
    ForceWarnWarnings,
    /// A name on the command line (`tool::` and `name`, as given with `_` for `-`) that rustc's
    /// `check_lint_name` finds renamed, removed, unknown, a deprecated name of a tool's lint,
    /// or of an unknown tool.
    CommandLine { given: &'a str, tool: Option<&'a str>, name: &'a str, level: Level, checked: &'a Checked },
    /// An element of a lint attribute's list of `node` that is no name, at `at`. `pre`: of
    /// the crate's root file, written as itself, so rustc's pre-expansion lint pass says it
    /// too.
    Malformed { node: InFile<ast::Node<'db>>, at: InFile<TextRange>, what: Malformed, pre: bool },
    /// A name in a lint attribute of `node`, at `at`, that `check_lint_name` finds renamed,
    /// removed, unknown, a deprecated name of a tool's lint, or of an unknown tool: its tool
    /// with where that is, and the rest as written.
    Name {
        node: InFile<ast::Node<'db>>,
        at: InFile<TextRange>,
        tool: Option<(&'a str, TextRange)>,
        name: &'a str,
        checked: &'a Checked,
        pre: bool,
    },
    /// The early builder's `insert_spec`: an attribute's level `new` for `lint` where an
    /// earlier `forbid` (`old`) holds; `groups` when either names a group, which makes it the
    /// `forbidden_lint_groups` lint rather than E0453.
    Overruled { node: InFile<ast::Node<'db>>, lint: &'static Lint, new: Spec, old: Spec, groups: bool },
    /// `lint` is behind `feature`, which the crate lacks: named in an attribute of `node` at
    /// `at`, or on the command line.
    Gated { node: Option<InFile<ast::Node<'db>>>, lint: &'static Lint, feature: Feature, at: Option<InFile<TextRange>> },
    /// A level for a lint only the crate's own attributes set, set at `at` of `node`, below
    /// the crate.
    CrateLevelOnly { node: InFile<ast::Node<'db>>, level: Level, name: &'a Symbol, at: InFile<TextRange> },
}

/// What a builder that speaks hands what it finds to: given the builder and the context as
/// they are when it is found, and the set the name is being added to (for the level of the
/// lint that says it, which is the level there at that moment).
pub trait Sink<'db> {
    fn event(&mut self, levels: &Levels<'db>, cx: &LevelCx<'_, 'db>, set: SetId, event: Event<'_, 'db>);
}

/// The sink of a builder that says nothing.
impl<'db> Sink<'db> for () {
    #[inline]
    fn event(&mut self, _: &Levels<'db>, _: &LevelCx<'_, 'db>, _: SetId, _: Event<'_, 'db>) {}
}

/// An `#[expect]` of one lint, a group or a tool's lint, as the HIR has it.
pub struct Expectation<'db> {
    pub id: ExpectId,
    /// Where the name is in the attribute.
    pub key: InFile<TextRange>,
    pub reason: Option<Symbol>,
    /// `#[expect(unfulfilled_lint_expectations)]`, which nothing fulfills.
    pub is_unfulfilled: bool,
    /// The node the attribute is on: where the level of `unfulfilled_lint_expectations` is.
    pub node: InFile<ast::Node<'db>>,
}

/// A set of levels: the command line's, or one node's attributes'.
pub type SetId = u32;

/// The command line's set, around every other.
pub const COMMAND_LINE: SetId = 0;
const NO_SET: SetId = SetId::MAX;

/// The levels of a crate as one of rustc's builders has them.
///
/// The sets are CSR columns: set `s` gives the lints `ids[start[s]..start[s + 1]]` (to the
/// end for the last) the levels at the same indices of `specs`, inside set `parent[s]`, and
/// `has[s]` is the set of those lints as a bitset. That is 48 bytes a set and 14 a lint in
/// one, against rustc's `FxIndexMap` per set. Finding a lint's level is one bit test of
/// `anywhere` for a lint no set names (most of them), else one bit test of each set from the
/// node out and a scan of the 2-byte ids of the one that has it.
pub struct Levels<'db> {
    /// rustc's `lint_added_lints`: the early lint pass's builder, which says what is wrong with
    /// the attributes and holds to a `forbid`; the HIR's does neither and has no lint attributes
    /// on generic parameters.
    early: bool,
    /// Whether the walk of the crate is on: what is said of the attributes is said then, as
    /// rustc's walk comes to them, and once.
    walking: bool,
    start: Vec<u32>,
    parent: Vec<SetId>,
    /// By set: the lints it has a level for.
    has: Vec<LintSet>,
    ids: Vec<LintId>,
    specs: Vec<Spec>,
    /// The lints some set has a level for: any other is at its default level everywhere.
    anywhere: LintSet,
    /// The command line's lints at each level, by `Level as usize`: each lint it names is in
    /// the one of its level.
    cmd_at: [LintSet; LEVELS],
    /// The set in force at each node seen, by one hash of the node: 28 bytes a node.
    at: HashTable<(InFile<SyntaxNodePtr>, SetId)>,
    /// [`Levels::at`] as [`Levels::set_of_in_file`] has it, out to the root of each file.
    in_file: HashTable<(InFile<SyntaxNodePtr>, SetId)>,
    /// The nodes between one asked of and the nearest seen, kept for its capacity.
    chain: Vec<(u64, InFile<SyntaxNodePtr>, InFile<ast::Node<'db>>)>,
    /// The `#[expect]`s the HIR's builder came to on the walk, in its order.
    pub expectations: Vec<Expectation<'db>>,
}

const _: () = assert!(size_of::<(InFile<SyntaxNodePtr>, SetId)>() <= 28);

/// The number of [`Level`]s.
pub const LEVELS: usize = Level::Forbid as usize + 1;

impl<'db> Levels<'db> {
    /// The builder with the command line's levels (rustc's `add_command_line`), and, when it
    /// `speaks`, what it finds wrong with the names on it handed to `sink`; one that does not
    /// is for levels alone.
    pub fn new(cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, early: bool, speaks: bool) -> Levels<'db> {
        let mut levels = Levels::empty(early, speaks, cx.opts.opts.len());
        let opts = cx.opts;
        for (name, level) in &*opts.opts {
            levels.command_line(cx, sink, name, *level, speaks);
        }
        levels
    }

    /// The builder with an empty command line's set, room for `cmd` levels in it.
    fn empty(early: bool, walking: bool, cmd: usize) -> Levels<'db> {
        Levels {
            early,
            walking,
            // alloc: setup, once per builder (three per check)
            start: vec![0],
            // alloc: as above
            parent: vec![NO_SET],
            // alloc: as above
            has: vec![LintSet::EMPTY],
            ids: Vec::with_capacity(cmd),
            specs: Vec::with_capacity(cmd),
            anywhere: LintSet::EMPTY,
            cmd_at: [LintSet::EMPTY; LEVELS],
            at: HashTable::new(),
            in_file: HashTable::new(),
            // alloc: empty, allocates nothing
            chain: Vec::new(),
            // alloc: empty, allocates nothing
            expectations: Vec::new(),
        }
    }

    /// Whether it is the early lint pass's builder.
    pub fn is_early(&self) -> bool {
        self.early
    }

    /// The command line's lints at `level`.
    pub fn command_line_at(&self, level: Level) -> &LintSet {
        &self.cmd_at[level as usize]
    }

    fn command_line(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, given: &str, level: Level, speaks: bool) {
        let (tool, name) = match given.split_once("::") {
            Some((tool, name)) => (Some(tool), name),
            None => (None, given),
        };
        if speaks {
            if name == "warnings" && level == Level::ForceWarn {
                sink.event(self, cx, COMMAND_LINE, Event::ForceWarnWarnings);
            }
            let checked = cx.store.check(name, tool);
            if !matches!(checked, Checked::Ok(_) | Checked::Tool(_, None) | Checked::MissingTool) {
                sink.event(self, cx, COMMAND_LINE, Event::CommandLine { given, tool, name, level, checked: &checked });
            }
        }
        let Some(lints) = cx.store.find_lints(given) else { return };
        let source = cx.add_source(Source::CommandLine { name: given.into(), level });
        for &lint in lints {
            let id = lint.id();
            if self.command_line_at(Level::ForceWarn).contains(id) || self.command_line_at(Level::Forbid).contains(id) {
                continue;
            }
            if self.gated(cx, sink, COMMAND_LINE, lint, None, None) {
                self.insert(COMMAND_LINE, id, Spec { level, expect: None, source });
            }
        }
    }

    /// Every node of `nodes` with attributes, as rustc's walk of the crate comes to it: what is
    /// said of the attributes is said now, once. The caller gives the nodes [`attrs::owns`]
    /// lets through for this builder ([`Levels::is_early`] for its generic parameters).
    pub fn walk(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, nodes: impl IntoIterator<Item = InFile<ast::Node<'db>>>) {
        self.walking = true;
        for node in nodes {
            self.set_at(cx, sink, node);
        }
        self.walking = false;
    }

    /// [`Levels::set_at`] of the node `at` points to, with no tree walked when a node at it was
    /// seen.
    pub fn set_of(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, at: InFile<SyntaxNodePtr>) -> SetId {
        let hash = FixedState.hash_one(at);
        if let Some(&(_, set)) = self.at.find(hash, |(it, _)| *it == at) {
            return set;
        }
        let node = cx.node(at);
        self.set_at(cx, sink, node)
    }

    /// The set in force at `node`: of its attributes, else of the nearest node around it with
    /// any. Each node between it and the nearest one seen is hashed once.
    pub fn set_at(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, node: InFile<ast::Node<'db>>) -> SetId {
        self.walk_out(cx, sink, node, false)
    }

    /// The set in force at the node `at` points to as rustc's pre-expansion lint pass has it.
    /// That pass runs on each file it loads with a builder of its own (`pre_expansion_lint`),
    /// so the walk out from the node stops at the root of its file, inside the command line's
    /// set: a module file has neither its `mod` item's levels nor the crate's
    /// (rust-lang/rust#132218). A macro's expansion is walked out of to the file of its call.
    pub fn set_of_in_file(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, at: InFile<SyntaxNodePtr>) -> SetId {
        let hash = FixedState.hash_one(at);
        if let Some(&(_, set)) = self.in_file.find(hash, |(it, _)| *it == at) {
            return set;
        }
        let node = cx.node(at);
        self.walk_out(cx, sink, node, true)
    }

    /// [`Levels::set_at`], or [`Levels::set_of_in_file`] when `in_file`, each with its own
    /// table of the nodes seen.
    fn walk_out(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, node: InFile<ast::Node<'db>>, in_file: bool) -> SetId {
        let mut chain = std::mem::take(&mut self.chain);
        let mut next = Some(node);
        let mut set = COMMAND_LINE;
        while let Some(node) = next {
            let key = attrs::key(node);
            let hash = FixedState.hash_one(key);
            let seen = if in_file { &self.in_file } else { &self.at };
            if let Some(&(_, known)) = seen.find(hash, |(it, _)| *it == key) {
                set = known;
                break;
            }
            let file_root = node.value.kind() == SyntaxKind::SOURCE_FILE && !node.file_id.is_macro();
            next = if in_file && file_root { None } else { attrs::parent(cx.db, cx.krate, node) };
            chain.push((hash, key, node));
        }
        for (hash, key, node) in chain.drain(..).rev() {
            set = self.push_node(cx, sink, set, node);
            let seen = if in_file { &mut self.in_file } else { &mut self.at };
            seen.insert_unique(hash, (key, set), |(it, _)| FixedState.hash_one(it));
        }
        self.chain = chain;
        set
    }

    /// The set in force at an impl a builtin derive of `adt` makes with no tokens: rustc's
    /// `create_derived_impl` puts it beside the ADT with the ADT's `allow`, `warn`, `deny` and
    /// `forbid` copied onto it, not its `expect`s nor its other attributes. Made anew each time
    /// it is asked.
    pub fn set_of_derived(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, adt: InFile<ast::Node<'db>>) -> SetId {
        let parent = match attrs::parent(cx.db, cx.krate, adt) {
            Some(around) => self.set_at(cx, sink, around),
            None => COMMAND_LINE,
        };
        let Some(owner) = attrs::owner(adt.value, self.early) else { return parent };
        let mut reads = attrs::read(owner, cx.cfg, cx.edition);
        reads.retain(|it| !matches!(it, Read::Level(Level::Expect, ..)));
        if reads.is_empty() {
            return parent;
        }
        let set = self.open(parent);
        self.add(cx, sink, set, adt, adt.file_id, reads, false);
        self.close(set)
    }

    fn push_node(&mut self, cx: &mut LevelCx<'_, 'db>, sink: &mut impl Sink<'db>, parent: SetId, node: InFile<ast::Node<'db>>) -> SetId {
        let Some(owner) = attrs::owner(node.value, self.early) else { return parent };
        let is_crate = node.value.kind() == SyntaxKind::SOURCE_FILE && node.file_id == cx.root;
        let reads = attrs::read(owner, cx.cfg, cx.edition);
        if reads.is_empty() && !(is_crate && !cx.crate_attrs.is_empty()) {
            return parent;
        }
        let set = self.open(parent);
        self.add(cx, sink, set, node, node.file_id, reads, is_crate);
        if is_crate {
            for at in 0..cx.crate_attrs.len() {
                let file = cx.crate_attrs[at];
                let reads = attrs::read_crate_attr(file.root(cx.db), cx.cfg, cx.edition);
                self.add(cx, sink, set, node, file, reads, true);
            }
        }
        self.close(set)
    }

    /// A new set inside `parent`, the last, for [`Levels::insert`].
    fn open(&mut self, parent: SetId) -> SetId {
        let set = SetId::try_from(self.start.len()).expect("fewer than 2^32 sets");
        self.start.push(u32::try_from(self.ids.len()).expect("fewer than 2^32 levels"));
        self.parent.push(parent);
        self.has.push(LintSet::EMPTY);
        set
    }

    /// The set in force where `set`, the last, was opened: itself, or its parent when it has
    /// no level (and is dropped).
    fn close(&mut self, set: SetId) -> SetId {
        if !self.range(set).is_empty() {
            return set;
        }
        self.start.pop();
        self.has.pop();
        self.parent.pop().expect("an open set")
    }

    /// rustc's `add`: the attributes `reads` of `node`, written in `file` (the node's own, or
    /// a crate attribute's for the crate).
    fn add(
        &mut self,
        cx: &mut LevelCx<'_, 'db>,
        sink: &mut impl Sink<'db>,
        set: SetId,
        node: InFile<ast::Node<'db>>,
        file: HirFileId,
        reads: Reads<'db>,
        is_crate: bool,
    ) {
        let says = self.early && self.walking;
        let in_root = file == cx.root;
        for read in reads {
            let (level, mut items, pre) = match read {
                Read::Allows(id) => {
                    let spec = Spec { level: Level::Allow, expect: None, source: SourceId::DEFAULT };
                    self.insert(set, id, spec);
                    continue;
                }
                Read::Level(level, items, direct) => (level, items, in_root && direct),
            };
            let malformed = |range: TextRange, what: Malformed| Event::Malformed { node, at: InFile::new(file, range), what, pre };
            let tail = match items.last() {
                None => continue,
                Some(it) => match &it.kind {
                    ItemKind::Word(_) | ItemKind::Lit => Tail::Lint,
                    ItemKind::NameValue { string, value, .. } if it.kind.is_reason() => {
                        // clone: an interned symbol, a reference count
                        Tail::Reason(string.clone(), *value)
                    }
                    ItemKind::NameValue { .. } | ItemKind::List(_) => Tail::Bad(it.range),
                },
            };
            let mut reason: Option<Symbol> = None;
            match tail {
                Tail::Lint => {}
                Tail::Reason(string, value) => {
                    match string {
                        Some(string) => reason = Some(string),
                        None if says => sink.event(self, cx, set, malformed(value, Malformed::ReasonNotString)),
                        None => {}
                    }
                    items.pop();
                }
                Tail::Bad(range) => {
                    if says {
                        sink.event(self, cx, set, malformed(range, Malformed::BadArgument));
                    }
                }
            }
            for item in &items {
                let ItemKind::Word(path) = &item.kind else {
                    if says {
                        let what = match item.kind.is_reason() {
                            true => Malformed::ReasonNotLast,
                            false => Malformed::BadArgument,
                        };
                        sink.event(self, cx, set, malformed(item.range, what));
                    }
                    continue;
                };
                let at = InFile::new(file, item.range);
                let expect = (level == Level::Expect).then(|| cx.expect_id(file, item.range));
                let (tool, name) = match &path[..] {
                    [tool, rest @ ..] if !rest.is_empty() => (Some(tool), attrs::path_string(rest)),
                    _ => (None, attrs::path_string(path)),
                };
                let checked = cx.store.check(&name, tool.map(|it| it.name()));
                let named = match &checked {
                    Checked::Ok(_) => Some(Symbol::intern(path.last().map_or("", |it| it.name()))),
                    Checked::Tool(_, None) => Some(tool.map_or_else(|| Symbol::intern(&name), |tool| Symbol::intern_concat(&[tool.name(), "::", &name]))),
                    Checked::Tool(_, Some(new)) => Some(Symbol::intern(new)),
                    Checked::Renamed(new) => Some(Symbol::intern(new)),
                    Checked::MissingTool | Checked::NoTool | Checked::Removed(_) | Checked::NoLint { .. } => None,
                };
                let speaks_of = matches!(
                    checked,
                    Checked::Tool(_, Some(_)) | Checked::NoTool | Checked::Renamed(_) | Checked::Removed(_) | Checked::NoLint { .. }
                );
                if says && speaks_of {
                    let event = Event::Name { node, at, tool: tool.map(|it| (it.name(), it.range)), name: &name, checked: &checked, pre };
                    sink.event(self, cx, set, event);
                }
                let lints = match &checked {
                    Checked::Ok(lints) | Checked::Tool(lints, _) => *lints,
                    Checked::Renamed(new) => match cx.store.check(new, None) {
                        Checked::Ok(lints) => lints,
                        other => panic!("`{name}` is renamed to `{new}`, which is {other:?}"),
                    },
                    Checked::MissingTool | Checked::NoTool | Checked::Removed(_) | Checked::NoLint { .. } => continue,
                };
                let named = named.expect("a name that sets levels has a name for its source");
                // clone: an interned symbol, a reference count
                let source = cx.add_source(Source::Node { name: named, at, reason: reason.clone() });
                for &lint in lints {
                    if self.gated(cx, sink, set, lint, Some(node), Some(at)) {
                        self.insert_spec(cx, sink, set, node, lint, Spec { level, expect, source });
                    }
                }
                // An `#[expect]` only of lints another tool gives is that tool's to check.
                if let Some(id) = expect
                    && !self.early
                    && self.walking
                    && (lints.is_empty() || lints.iter().any(|it| !it.is_externally_loaded))
                {
                    let is_unfulfilled = matches!(lints, [only] if only.id() == LintId::UNFULFILLED_LINT_EXPECTATIONS);
                    // clone: an interned symbol, once per `#[expect]` name
                    let reason = reason.clone();
                    self.expectations.push(Expectation { id, key: at, reason, is_unfulfilled, node });
                }
            }
        }
        if says && !is_crate {
            let range = self.range(set);
            // clone: a `Range<usize>`, no allocation
            let ignored = self.ids[range.clone()].iter().zip(&self.specs[range]).find_map(|(id, spec)| {
                match cx.source(spec.source) {
                    // clone: an interned symbol, for a warning
                    Source::Node { name, at, .. } if id.lint().crate_level_only => Some((spec.level, name.clone(), *at)),
                    _ => None,
                }
            });
            if let Some((level, name, at)) = ignored {
                sink.event(self, cx, set, Event::CrateLevelOnly { node, level, name: &name, at });
            }
        }
    }

    /// rustc's `insert_spec`: `spec` of an attribute of `node` for `lint`, unless a `forbid`
    /// before it holds.
    fn insert_spec(
        &mut self,
        cx: &mut LevelCx<'_, 'db>,
        sink: &mut impl Sink<'db>,
        set: SetId,
        node: InFile<ast::Node<'db>>,
        lint: &'static Lint,
        spec: Spec,
    ) {
        let old = self.spec(cx, lint, set);
        if self.early && spec.level == Level::Deny && old.level == Level::Forbid {
            return;
        }
        if self.early && spec.level != Level::Forbid && old.level == Level::Forbid {
            let groups = match cx.source(old.source) {
                Source::Default => false,
                Source::Node { name, .. } => cx.store.is_lint_group(name.as_str()),
                Source::CommandLine { name, .. } => cx.store.is_lint_group(name),
            };
            if self.walking && matches!(cx.source(spec.source), Source::Node { .. }) {
                sink.event(self, cx, set, Event::Overruled { node, lint, new: spec, old, groups });
            }
            if !groups {
                return;
            }
        }
        if spec.level == Level::Expect && lint.id() == LintId::UNFULFILLED_LINT_EXPECTATIONS {
            return;
        }
        let spec = match (old.level, spec.level) {
            (Level::ForceWarn, Level::Expect) => Spec { level: Level::ForceWarn, expect: spec.expect, source: old.source },
            (Level::ForceWarn, _) => Spec { level: Level::ForceWarn, expect: None, source: old.source },
            _ => spec,
        };
        self.insert(set, lint.id(), spec);
    }

    /// rustc's `check_gated_lint`: whether `lint` is not behind a feature the crate lacks; the
    /// early builder on its walk says so of one that is.
    fn gated(
        &mut self,
        cx: &mut LevelCx<'_, 'db>,
        sink: &mut impl Sink<'db>,
        set: SetId,
        lint: &'static Lint,
        node: Option<InFile<ast::Node<'db>>>,
        at: Option<InFile<TextRange>>,
    ) -> bool {
        let Some(feature) = lint.feature_gate else { return true };
        if cx.features.is_enabled(&Symbol::intern(feature.name)) {
            return true;
        }
        if self.early && self.walking {
            sink.event(self, cx, set, Event::Gated { node, lint, feature, at });
        }
        false
    }

    /// The level of `lint` in `set`: rustc's `reveal_actual_level_spec`, with `warnings` and
    /// `--cap-lints`.
    pub fn spec(&self, cx: &LevelCx<'_, '_>, lint: &'static Lint, set: SetId) -> Spec {
        reveal(lint, self.raw(lint.id(), set), self.raw(LintId::WARNINGS, set), cx.edition, cx.opts.cap)
    }

    fn range(&self, set: SetId) -> Range<usize> {
        let set = set as usize;
        let end = self.start.get(set + 1).map_or(self.ids.len(), |&it| it as usize);
        self.start[set] as usize..end
    }

    /// The level of `id` set in `set` or a set around it, the innermost.
    pub fn raw(&self, id: LintId, set: SetId) -> Option<Spec> {
        if !self.anywhere.contains(id) {
            return None;
        }
        let mut at = set;
        while at != NO_SET {
            if self.has[at as usize].contains(id) {
                return self.own(at, id);
            }
            at = self.parent[at as usize];
        }
        None
    }

    fn own(&self, set: SetId, id: LintId) -> Option<Spec> {
        let range = self.range(set);
        let start = range.start;
        self.ids[range].iter().position(|&it| it == id).map(|it| self.specs[start + it])
    }

    /// Sets `id` at `spec` in `set`, which is the last set.
    fn insert(&mut self, set: SetId, id: LintId, spec: Spec) {
        debug_assert_eq!(set as usize, self.start.len() - 1, "levels are set in the last set only");
        let range = self.range(set);
        let start = range.start;
        match self.ids[range].iter().position(|&it| it == id) {
            Some(at) => {
                if set == COMMAND_LINE {
                    self.cmd_at[self.specs[start + at].level as usize].remove(id);
                }
                self.specs[start + at] = spec;
            }
            None => {
                self.ids.push(id);
                self.specs.push(spec);
                self.has[set as usize].insert(id);
                self.anywhere.insert(id);
            }
        }
        if set == COMMAND_LINE {
            self.cmd_at[spec.level as usize].insert(id);
        }
    }

    /// The levels in force in `set`, flattened: each lint some set from `set` out names, at
    /// the innermost one's level, in the order of their ids.
    pub fn flatten(&self, set: SetId) -> (LintSet, Vec<(LintId, Level)>) {
        let mut seen = LintSet::EMPTY;
        // alloc: once per flattening (a query's result), the few lints named from a node out
        let mut found = Vec::new();
        let mut at = set;
        while at != NO_SET {
            let Range { start, end } = self.range(at);
            for (&id, spec) in self.ids[start..end].iter().zip(&self.specs[start..end]) {
                if seen.insert(id) {
                    found.push((id, spec.level));
                }
            }
            at = self.parent[at as usize];
        }
        found.sort_unstable_by_key(|&(id, _)| id);
        (seen, found)
    }
}

/// rustc's `reveal_actual_level_spec`: the level of `lint` from `raw`, what the sets give it
/// (its default level in `edition` when none does), with `warnings`' level from the sets
/// (`warnings`) and the cap `--cap-lints` gives.
pub fn reveal(lint: &'static Lint, raw: Option<Spec>, warnings: Option<Spec>, edition: Edition, cap: Option<Level>) -> Spec {
    let mut spec = raw.unwrap_or(Spec { level: lint.default_level(edition), expect: None, source: SourceId::DEFAULT });
    if spec.level == Level::Warn
        && let Some(warnings) = warnings
    {
        let respect = match warnings.level {
            Level::Warn | Level::ForceWarn => false,
            Level::Deny | Level::Forbid => !lint.ignore_deny_warnings,
            Level::Allow | Level::Expect => true,
        };
        if respect {
            spec = warnings;
        }
    }
    // Only `--force-warn` gives `ForceWarn`, which keeps its source over an attribute's
    // level: a `ForceWarn` is a command-line one, which the cap leaves.
    if spec.level != Level::ForceWarn
        && let Some(cap) = cap
    {
        spec.level = spec.level.min(cap);
    }
    spec
}

/// The last element of a lint attribute's list, which may be its `reason`.
enum Tail {
    Lint,
    /// `reason = ..`: the string, if it is one, and where the value is.
    Reason(Option<Symbol>, TextRange),
    /// A `name = value` or `name(..)` that is not a reason.
    Bad(TextRange),
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    const ALL_LEVELS: [Level; LEVELS] =
        [Level::Allow, Level::Expect, Level::Warn, Level::ForceWarn, Level::Deny, Level::Forbid];

    /// [`Levels::raw`] as it was: a scan of the ids of each set from `set` out.
    fn raw_by_scan(levels: &Levels<'_>, id: LintId, set: SetId) -> Option<Spec> {
        let mut at = set;
        while at != NO_SET {
            let range = levels.range(at);
            let start = range.start;
            if let Some(found) = levels.ids[range].iter().position(|&it| it == id) {
                return Some(levels.specs[start + found]);
            }
            at = levels.parent[at as usize];
        }
        None
    }

    #[test]
    fn bitsets_find_what_the_scan_finds() {
        let mut rng = SplitMix64::new(0x6c65_7665_6c73);
        let ids: Vec<LintId> = blaze_lint_defs::all().iter().map(|it| it.id()).collect();
        // Few lints named, so that sets name the same ones and many are named nowhere.
        let pool: Vec<LintId> = (0..24).map(|_| ids[rng.index(ids.len())]).collect();
        let mut source = 0u32;
        let mut spec = |level: Level| {
            source += 1;
            Spec { level, expect: None, source: SourceId(source) }
        };
        let mut levels = Levels::empty(true, false, 0);
        for _ in 0..40 {
            let id = pool[rng.index(pool.len())];
            let level = ALL_LEVELS[rng.index(LEVELS)];
            levels.insert(COMMAND_LINE, id, spec(level));
        }
        for &id in &ids {
            for level in ALL_LEVELS {
                let scanned = levels.own(COMMAND_LINE, id).is_some_and(|it| it.level == level);
                assert_eq!(levels.command_line_at(level).contains(id), scanned, "{} at {level:?}", id.lint().name);
            }
        }
        let mut sets = vec![COMMAND_LINE];
        for _ in 0..300 {
            let parent = sets[rng.index(sets.len())];
            let set = levels.open(parent);
            for _ in 0..rng.below(5) {
                let id = pool[rng.index(pool.len())];
                let level = ALL_LEVELS[rng.index(LEVELS)];
                levels.insert(set, id, spec(level));
            }
            let set = levels.close(set);
            assert!(set == parent || set as usize == levels.start.len() - 1);
            sets.push(set);
        }
        assert_eq!(levels.has.len(), levels.start.len());
        assert_eq!(levels.parent.len(), levels.start.len());
        for &set in &sets {
            for &id in &ids {
                assert_eq!(levels.raw(id, set), raw_by_scan(&levels, id, set), "{} in set {set}", id.lint().name);
            }
            let (seen, flat) = levels.flatten(set);
            for &id in &ids {
                let flat_level = flat.iter().find(|(it, _)| *it == id).map(|&(_, level)| level);
                assert_eq!(flat_level, levels.raw(id, set).map(|it| it.level), "{} flattened in set {set}", id.lint().name);
                assert_eq!(seen.contains(id), flat_level.is_some());
            }
        }
        for &id in &ids {
            let named = (0..levels.start.len() as SetId).any(|set| levels.own(set, id).is_some());
            assert_eq!(levels.anywhere.contains(id), named, "{}", id.lint().name);
        }
    }
}
