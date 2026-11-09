//! blaze: what one configuration of a package (one unit's crate: its lib, a test of it, a bin,
//! under one set of features and one platform) says of its definitions, for the lints over
//! every configuration at once (`crate::reach`, docs/v2/plan/design/multiconfig-check.md): each
//! definition with where it is, whether this configuration uses it (rustc's liveness, the same
//! pass `dead_code` runs), whether `dead_code` would report it here, and at what levels; what
//! the code this configuration does not use names (so a `#[cfg]` fix can be shown sound); and
//! the code a `#[cfg]` leaves out of it (so a fix is never called sound for code no checked
//! configuration compiles).
//!
//! Nothing here decides a finding: one configuration can't tell a dead item from one only
//! another configuration uses. [`crate::reach::merge`] does, over all of them.

use std::{alloc::Global, ops::ControlFlow};

use blaze_hash::HashMap;
use cfg::{CfgAtom, CfgExpr};
use hir::InFile;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::{
    defs::{DefIdx, Defs, Kind, Parent},
    level::{LevelPlus, Levels},
    mark, reach, report,
    seed::{self, WorkItem},
};
use super::super::effective::Effective;
use crate::{
    cx::Cx,
    lints::registry::{self, Level, LintId},
    reach::{Atom, Inactive, Item, Key, Lvl, Observation},
};

/// What `cx`'s crate, as this configuration builds it, says of its definitions.
pub(crate) fn observe(cx: &Cx<'_, '_>, effective: &Effective) -> Observation {
    let mut observation = Observation {
        atoms: atoms(cx),
        files: files(cx),
        inactive: inactive(cx),
        ..Observation::default()
    };
    let defs = Defs::of(cx);
    if defs.unported {
        // alloc: once per crate observed, on the path that stops
        observation.unported = Some(
            "a `global_asm!` names an item, or a `pub macro` makes private items reachable, and what is live then is \
             not ported (as rustc's `dead_code` is not given here)"
                .to_owned(),
        );
        return observation;
    }
    let levels = Levels::new();
    let reach = reach::Reach::of(cx, &defs, effective);
    let k = seed::Crate::new(cx, &defs, &levels, &reach);
    let seeds = seed::seeds(&k);
    let (liveness, _) = mark::live(&k, seeds);
    let reported = report::reported(&k, &liveness);
    let mut level_of: Vec<Option<LevelPlus>> = vec![None; defs.len()]; // alloc: a column, once per crate observed
    for &(def, level) in &reported {
        level_of[def as usize] = Some(level);
    }
    // alloc: the observation's items, one per definition, once per crate observed
    observation.items.reserve_exact(defs.len());
    let mut nth = Nth::default();
    for def in defs.all() {
        let item = item(cx, &defs, def, liveness.last.contains(def), level_of[def as usize], &mut nth);
        observation.items.push(item);
    }
    // An impl of a type or trait goes with it: a `#[cfg]` on one needs the same on the other.
    // What a builtin derive makes has no node, and what a derive or attribute macro on the
    // type writes is in its expansion: both go with the type as if in it.
    for of in defs.impls() {
        let Some(ptr) = defs[of.def].node else { continue };
        for owner in [of.self_adt, of.trait_].into_iter().flatten() {
            if !matches!(defs[owner].kind, Kind::Struct | Kind::Union | Kind::Enum | Kind::Trait) {
                continue;
            }
            if ptr.file_id.macro_file().is_some() && written_on(cx, &defs, owner, ptr) {
                if of.self_adt == Some(owner) {
                    observation.items[of.def as usize].parent = Some(owner);
                }
                continue;
            }
            observation.items[owner as usize].companions.push(of.def);
        }
    }
    let unused: Vec<WorkItem> = defs
        .all()
        .filter(|&def| !liveness.last.contains(def))
        .map(|def| WorkItem::new(def, false))
        .collect(); // alloc: the worklist of the walk of what is unused, once per crate observed
    observation.refs = mark::references(&k, unused);
    observation
}

/// Whether the macro call `ptr` is in the expansion of is written on `owner` (a derive in its
/// attributes, an attribute macro on it): its span, as diagnostics place it, is inside the
/// owner's.
fn written_on(cx: &Cx<'_, '_>, defs: &Defs, owner: DefIdx, ptr: InFile<syntax::SyntaxNodePtr>) -> bool {
    let Some(at) = defs[owner].node else { return false };
    if at.file_id.macro_file().is_some() {
        return false;
    }
    let outer = cx.span(InFile::new(at.file_id, cx.syntax_view(at).value.text_range()));
    let inner = cx.span(InFile::new(ptr.file_id, cx.syntax_view(ptr).value.text_range()));
    inner.file == outer.file && outer.lo <= inner.lo && inner.hi <= outer.hi
}

/// The crate's cfg atoms, sorted: what tells this configuration from the others.
fn atoms(cx: &Cx<'_, '_>) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = (cx.krate.cfg(cx.db).atoms())
        .map(|it| match it {
            CfgAtom::Flag(key) => Atom { key: key.as_str().to_owned(), value: None },
            CfgAtom::KeyValue { key, value } => Atom { key: key.as_str().to_owned(), value: Some(value.as_str().to_owned()) },
        })
        .collect(); // alloc: the crate's atoms, once per crate observed
    atoms.sort_unstable();
    atoms
}

/// The files of the crate's modules, by the names diagnostics show them by
/// ([`crate::emit::shown`]), sorted: a file another configuration has and this one does not
/// is no code this one leaves out.
fn files(cx: &Cx<'_, '_>) -> Vec<String> {
    let mut files: Vec<String> = crate::checks::modules(cx)
        .into_iter()
        .filter_map(|module| {
            let source = module.definition_source_view(cx.db);
            source.file_id.macro_file().is_none().then(|| {
                let span = cx.span(InFile::new(source.file_id, TextRange::empty(TextSize::new(0))));
                crate::emit::shown(&span.file).to_owned() // alloc: a file's name, once per file of the crate
            })
        })
        .collect(); // alloc: as above
    files.sort_unstable();
    files.dedup();
    files
}

/// The code a `#[cfg]` leaves out of this configuration: each outermost item, field, variant,
/// statement or expression whose `cfg` (its `cfg_attr`s expanded) is false, with the names in
/// it. Only files the crate has, as written, are read.
fn inactive(cx: &Cx<'_, '_>) -> Vec<Inactive> {
    let mut out = Vec::new(); // alloc: the crate's code left out, once per crate observed
    let cfg = cx.krate.cfg(cx.db);
    for module in crate::checks::modules(cx) {
        let source = module.definition_source_view(cx.db);
        if source.file_id.macro_file().is_some() || !matches!(source.value, hir::ModuleSourceView::SourceFile(_)) {
            continue;
        }
        let root = cx.vsema.parse_or_expand(source.file_id);
        let mut past = TextSize::new(0);
        for node in root.descendants() {
            let range = node.text_range();
            if range.start() < past {
                continue;
            }
            let Some(owner) = ast::AnyHasAttrs::cast(node) else { continue };
            let off = hir_expand::attrs::expand_cfg_attr_view(
                owner.attrs(),
                || cfg,
                |meta, _| {
                    if let ast::Meta::CfgMeta(meta) = meta
                        && let Some(predicate) = meta.cfg_predicate()
                        && cfg.check(&CfgExpr::parse_from_view(predicate)) == Some(false)
                    {
                        // alloc: a predicate's text, once per piece of code left out
                        return ControlFlow::Break(predicate.syntax().text().split_whitespace().collect::<String>());
                    }
                    ControlFlow::Continue(())
                },
            );
            let Some(predicate) = off else { continue };
            past = range.end();
            let span = cx.span(InFile::new(source.file_id, range));
            let mut names: Vec<String> = (node.descendants_with_tokens())
                .filter_map(NodeOrToken::into_token)
                .filter(|it| it.kind() == SyntaxKind::IDENT)
                .map(|it| it.text().to_owned()) // alloc: a name in code left out, once per name
                .collect(); // alloc: as above
            names.sort_unstable();
            names.dedup();
            out.push(Inactive {
                file: crate::emit::shown(&span.file).to_owned(), // alloc: once per piece of code left out
                lo: span.lo,
                hi: span.hi,
                predicate,
                names,
            });
        }
    }
    out.sort_unstable_by(|a, b| (&a.file, a.lo, a.hi).cmp(&(&b.file, b.lo, b.hi)));
    out
}

/// How many definitions before each had its key's place, kind and name: two a macro makes at
/// one call are told apart by their order.
struct Nth {
    seen: HashMap<(String, u32, u32, &'static str, String), u32, Global>,
}

impl Default for Nth {
    fn default() -> Self {
        Nth { seen: HashMap::new_in(Global) }
    }
}

impl Nth {
    fn next(&mut self, key: (String, u32, u32, &'static str, String)) -> u32 {
        let n = self.seen.entry(key).or_insert(0);
        let it = *n;
        *n += 1;
        it
    }
}

/// One definition as this configuration sees it.
fn item(cx: &Cx<'_, '_>, defs: &Defs, def: DefIdx, used: bool, reported: Option<LevelPlus>, nth: &mut Nth) -> Item {
    let d = &defs[def];
    // What a builtin derive makes has no node and goes with its type, as if in it: a `#[cfg]`
    // on the type leaves the derive out too.
    let parent = match (d.parent, d.node, defs.impl_of(def).and_then(|it| it.self_adt)) {
        (_, None, Some(adt)) if matches!(d.kind, Kind::Impl { .. }) => Some(adt),
        (Parent::Def(it), _, _) => Some(it),
        (Parent::Module | Parent::Anon(_), _, _) => None,
    };
    // Whose finding says this one's: an inherent impl's item is its self type's; an item of a
    // trait, the trait's; a variant, its enum's; a field, its struct's or variant's.
    let owner = parent.map(|parent| match defs[parent].kind {
        Kind::Impl { of_trait: false } => defs.impl_of(parent).and_then(|it| it.self_adt).unwrap_or(parent),
        _ => parent,
    });
    let descr = d.kind.descr();
    let name = d.name.as_ref().map_or("", |it| it.as_str()).to_owned(); // alloc: once per definition observed
    let mut item = Item {
        key: None,
        span: None,
        insert: None,
        descr: descr.to_owned(), // alloc: once per definition observed
        name,
        used,
        reported: reported.is_some(),
        levels: None,
        parent,
        owner,
        companions: Vec::new(), // alloc: empty until an impl is of it
        fixable: false,
    };
    let Some(ptr) = d.node else { return item };
    let node = cx.syntax_view(ptr);
    let in_file = ptr.file_id.macro_file().is_none();
    let range = match ast::AnyHasName::cast(node.value).and_then(|it| it.name()) {
        Some(name) => name.syntax().text_range(),
        None => match d.kind {
            Kind::Variant | Kind::Field => crate::cx::item_range_view(node.value),
            _ => crate::cx::def_range_view(node.value),
        },
    };
    let span = cx.span(InFile::new(ptr.file_id, range));
    let key = (crate::emit::shown(&span.file).to_owned(), span.lo, span.hi, descr, item.name.clone()); // alloc: clone: the key, once per definition observed
    let n = nth.next(key.clone()); // clone: the key, once per definition observed
    item.key = Some(Key {
        file: key.0,
        lo: key.1,
        hi: key.2,
        in_expansion: !in_file || span.expansion.is_some(),
        kind: descr.to_owned(), // alloc: once per definition observed
        name: key.4,
        nth: n,
    });
    item.span = Some(span);
    if let Some(level) = reported {
        let blaze = |name: &str| {
            let lint = registry::lint(name).expect("blaze's reachability lints are in the registry's blaze table");
            lvl(cx.lint_level(LintId::of(lint), ptr))
        };
        item.levels = Some(crate::reach::Levels {
            dead_code: lvl(level.level),
            dead: blaze("blaze::dead"),
            test_only: blaze("blaze::test_only"),
            cfg_only: blaze("blaze::cfg_only"),
        });
    }
    let inherent_item = matches!(d.kind, Kind::AssocFn { .. } | Kind::AssocConst | Kind::AssocTy)
        && parent.is_some_and(|it| defs[it].kind == (Kind::Impl { of_trait: false }));
    let free = matches!(
        d.kind,
        Kind::Fn | Kind::Const | Kind::Static | Kind::Struct | Kind::Union | Kind::Enum | Kind::Trait | Kind::TyAlias
    ) && d.parent == Parent::Module
        && !d.foreign;
    item.fixable = in_file && (free || inherent_item || d.kind == (Kind::Impl { of_trait: false }) || d.kind == (Kind::Impl { of_trait: true }));
    if in_file {
        let start = node.value.text_range().start();
        let indent = node
            .value
            .first_token()
            .and_then(|it| it.prev_token())
            .filter(|it| it.kind() == SyntaxKind::WHITESPACE)
            .map_or("", |it| it.text().rsplit_once('\n').map_or("", |(_, after)| after));
        let at = cx.span(InFile::new(ptr.file_id, TextRange::empty(start)));
        item.insert = Some((at, indent.to_owned())); // alloc: once per definition observed
    }
    item
}

/// A level as the reachability lints keep it.
fn lvl(level: Level) -> Lvl {
    match level {
        Level::Allow => Lvl::Allow,
        Level::Expect => Lvl::Expect,
        Level::Warn | Level::ForceWarn => Lvl::Warn,
        Level::Deny | Level::Forbid => Lvl::Deny,
    }
}
