//! D116's equality harness (drive.md "Names at module grain", point 7): the per-crate scope
//! stays until names at module grain give equal answers. After one run of the cell core, every
//! name a module's written scope answers (declared, imported, glob-imported, macro-made, the
//! root's published `#[macro_export]`s, the textual `macro_rules!` scope) is asked twice:
//!
//! - OLD: the module's written scope (write.rs `scope`, what CrateScope's `exports` and `inner`
//!   hold), its own binding, else blaze-surface's `through` over the glob edges of the written
//!   scopes (the old reader, `Names::lookup`);
//! - NEW: [`lookup`] (and [`lookup_textual`]) over the module's records at module grain
//!   (cells/made.rs), every hop a logged cell read.
//!
//! Agreement: the same definition, the same visibility and the same ambiguity, or absent in
//! both. A lookup the tables leave to the core ([`Unfollowed`]) agrees only for the kinds the
//! core keeps by design ([`Unfollowed::core_only`]), counted apart. The first disagreement stops
//! the check with the module, the name, both answers and the new lookup's reads.

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc};
use blaze_surface::{
    BindingState, Globs, ImportsView, LinksView, Node, Passed, ScopeBuilder, Sources, Table, TableRefused, TargetView, Through,
    ThroughScratch, through_in,
};

use super::{CellError, Cells, FragId, TablesError};
use crate::cell_key::{CellKey, cell_key};
use crate::links::{CellRead, Hop, LinkTables, LookupError, ModuleRecords, TextualHop, Unfollowed, lookup, lookup_textual, write_tables};
use crate::{Def, DefKind, Namespace, Visibility};

/// The most glob sources whose names are asked through one module's globs, and the most such
/// names a module (rule 18): past either, the rest are not asked, and the count says so.
const MAX_GLOB_SOURCES: usize = 64;
const MAX_GLOB_NAMES: usize = 4096;

/// What the check of one crate came to.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Agreed {
    /// Modules compared (those whose text was read).
    pub modules: u64,
    /// Lookups compared: (module, namespace, name) and textual (module, name).
    pub names: u64,
    /// Lookups with equal answers.
    pub agreed: u64,
    /// Lookups the tables left to the core in a core-only case, by [`Unfollowed::index`].
    pub unfollowed: [u64; Unfollowed::ALL.len()],
    /// Glob-reached names not asked past [`MAX_GLOB_SOURCES`] or [`MAX_GLOB_NAMES`].
    pub capped: u64,
    /// The written scopes' bytes (`ScopeBuilder::write`, CrateScope's `exports` and `inner`).
    pub old_bytes: u64,
    /// The records at module grain: outline, made, imports, textual, exported.
    pub new_bytes: u64,
}

impl Agreed {
    pub fn unfollowed_total(&self) -> u64 {
        self.unfollowed.iter().sum()
    }

    /// Another crate's totals added.
    pub fn add(&mut self, other: &Agreed) {
        self.modules += other.modules;
        self.names += other.names;
        self.agreed += other.agreed;
        for (a, b) in self.unfollowed.iter_mut().zip(other.unfollowed.iter()) {
            *a += b;
        }
        self.capped += other.capped;
        self.old_bytes += other.old_bytes;
        self.new_bytes += other.new_bytes;
    }
}

impl fmt::Display for Agreed {
    /// `modules M, names N, agreed A, unfollowed U, old bytes B1, new bytes B2`, then each
    /// unfollowed kind met.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "modules {}, names {}, agreed {}, unfollowed {}, old bytes {}, new bytes {}",
            self.modules,
            self.names,
            self.agreed,
            self.unfollowed_total(),
            self.old_bytes,
            self.new_bytes
        )?;
        for k in Unfollowed::ALL {
            if self.unfollowed[k.index()] > 0 {
                write!(f, " [{} {}]", k.text(), self.unfollowed[k.index()])?;
            }
        }
        if self.capped > 0 {
            write!(f, " [capped {}]", self.capped)?;
        }
        Ok(())
    }
}

/// One side's answer for a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Found { def: Def, vis: Visibility, ambiguous: bool },
    Absent,
    /// The old reader reached what the written scopes do not hold (another crate's module, an
    /// enum's variants): outside.
    Outside,
    /// The old reader's walk did not settle in its bound.
    Unbounded,
    Unfollowed(Unfollowed),
}

impl Answer {
    fn of_passed(p: Passed) -> Answer {
        Answer::Found { def: p.def, vis: p.vis, ambiguous: p.state != BindingState::Determined }
    }
}

/// Which lookup disagreed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    Path(Namespace),
    Textual,
}

/// The first lookup whose two answers differ.
#[derive(Debug)]
pub struct AnswersDiffer<'a, N> {
    pub module: ItemKey,
    /// The module's path from the crate root, by name (empty at the root).
    pub path: AVec<'a, N>,
    pub asked: Asked,
    pub name: N,
    pub old: Answer,
    pub new: Answer,
    /// The new lookup's cell reads, in order.
    pub log: AVec<'a, CellRead>,
}

/// Why the check did not agree, naming the thing.
#[derive(Debug)]
pub enum Disagreement<'a, N> {
    Answers(AnswersDiffer<'a, N>),
    /// A definition row of a table no name of the old scope has.
    ExtraRow { module: ItemKey, table: Table, cell: CellKey },
    /// The crate's names were not solved.
    Unsolved(CellError),
    /// The run left `count` imports or calls of `module` unknown (a dependency whose names
    /// were not given, say): its written scope is no answer to compare with, as the command
    /// refuses it (`Unwritten::Unknown`).
    Unknown { module: ItemKey, count: u32 },
    /// A module's records (or its written scope) were not written.
    Unwritten(TablesError),
    /// A record written here did not open.
    Unread { module: ItemKey, why: TableRefused },
    Lookup { module: ItemKey, error: LookupError },
}

impl<N: crate::Name> fmt::Display for Disagreement<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Disagreement::Answers(d) => {
                write!(f, "module {:x} `crate", d.module)?;
                for seg in d.path.iter() {
                    write!(f, "::{}", seg.as_ref())?;
                }
                write!(f, "` {:?} `{}`: old {:?}, new {:?}; reads:", d.asked, d.name.as_ref(), d.old, d.new)?;
                for r in d.log.iter() {
                    write!(f, " ({:x} {:?} {:?})", r.module, r.record, r.seen)?;
                }
                Ok(())
            }
            Disagreement::ExtraRow { module, table, cell } => {
                write!(f, "module {module:x}: a definition row of its {table:?} table, cell {:02x?}, that its written scope does not bind", cell.0)
            }
            Disagreement::Unsolved(e) => write!(f, "the crate's names were not solved: {e}"),
            Disagreement::Unknown { module, count } => write!(f, "module {module:x}: the run left {count} imports or calls unknown"),
            Disagreement::Unwritten(e) => write!(f, "module {:x}: its names records were not written: {}", e.module, e.error),
            Disagreement::Unread { module, why } => write!(f, "module {module:x}: {why}"),
            Disagreement::Lookup { module, error } => write!(f, "module {module:x}: the lookup over the tables stopped: {error:?}"),
        }
    }
}

/// A module's records, opened and checked once.
struct Opened<'h> {
    outline: LinksView<'h>,
    made: Option<LinksView<'h>>,
    textual: Option<LinksView<'h>>,
    exported: Option<LinksView<'h>>,
    imports: ImportsView<'h>,
    chain: &'h [ItemKey],
}

struct Held<'h, 'a> {
    krate: CrateItemKey,
    opened: ABTreeMap<'a, ItemKey, Opened<'h>>,
}

impl<'h> LinkTables<'h> for Held<'h, '_> {
    fn table(&self, module: ItemKey, table: Table) -> Option<LinksView<'h>> {
        let o = self.opened.get(&module)?;
        match table {
            Table::Outline => Some(o.outline),
            Table::Made => o.made,
            Table::Textual => o.textual,
            Table::Exported => o.exported,
        }
    }

    fn imports(&self, module: ItemKey) -> Option<ImportsView<'h>> {
        self.opened.get(&module).map(|o| o.imports)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.opened.get(&module).map(|o| o.chain)
    }

    fn krate(&self) -> CrateItemKey {
        self.krate
    }
}

fn open<'h>(module: ItemKey, bytes: &'h [u8]) -> Result<LinksView<'h>, TableRefused> {
    let v = LinksView::open(bytes)?;
    v.check()?;
    if v.module() != module {
        return Err(TableRefused { what: "a record of another module" });
    }
    Ok(v)
}

fn open_opt<'h>(module: ItemKey, bytes: &'h Option<blaze_surface::RecordBytes<'_>>) -> Result<Option<LinksView<'h>>, TableRefused> {
    bytes.as_deref().map(|b| open(module, b)).transpose()
}

/// The old reader's sources: the written scopes, one name at a time. A source that is not a
/// written module of this crate (another crate's, an enum) stops the walk: `Outside`.
struct OldScopes<'h, 'a, N> {
    cells: &'h Cells<'a, N>,
    old: &'h ABTreeMap<'a, ItemKey, ScopeBuilder<'a, N>>,
    ns: Namespace,
    name: N,
}

impl<'h, 'a, N: crate::Name> Sources<'h> for OldScopes<'h, 'a, N> {
    type Stop = ();

    fn node(&mut self, source: Def) -> Result<Node<'h>, ()> {
        let (cells, old): (&'h Cells<'a, N>, &'h ABTreeMap<'a, ItemKey, ScopeBuilder<'a, N>>) = (self.cells, self.old);
        // An enum source (`use self::Location::*;` in a block, aquamarine attrs.rs:295; `use
        // self::Cursor::*;`, http header/map.rs:1109) is read by its variants, as the core
        // reads it ([`super::EnumNames`]): a name that is no variant is not there, and the walk
        // goes on to the other globs (the block's lexical `use super::*;`). Stopping here made
        // the old reader answer `Outside` for every name such a block reaches through its
        // other globs, a disagreement of this harness alone (tempo, 21 Nov 04:10: 11 of the 20
        // listed). An enum the crate does not hold, or a variant undecided or ambiguous there,
        // stays outside.
        if source.kind == DefKind::Enum {
            let held = cells.enums.filter(|e| e.holds(source.key)).ok_or(())?;
            return match held.variant(source.key, self.ns, self.name) {
                super::DepAnswer::NotFound => Ok(Node::Value(None)),
                super::DepAnswer::Found(def, vis) => Ok(Node::Value(Some(Passed { def, vis, state: BindingState::Determined }))),
                super::DepAnswer::Ambiguous(..) | super::DepAnswer::Unknown => Err(()),
            };
        }
        if source.kind != DefKind::Module {
            return Err(());
        }
        let Some(s) = old.get(&source.key) else { return Err(()) };
        if let Some(b) = s.get(self.ns, self.name) {
            return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
        }
        let &m = cells.by_key.get(&source.key).ok_or(())?;
        let chain: &'h [ItemKey] = &cells.modules[m as usize].chain;
        Ok(Node::Edges { edges: Globs::of(&s.globs), krate: cells.krate, chain })
    }
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// D116's equality check of this crate after the run (see the module docs). The check's
    /// work is in `alloc`; a disagreement is made in `report`, which may outlive the core (a
    /// solve's `Cells` borrow its locals, as `decide::check`'s report does).
    pub(crate) fn links_agree<'o>(&self, alloc: Alloc<'a>, report: Alloc<'o>) -> Result<Agreed, Disagreement<'o, N>> {
        let mut totals = Agreed::default();
        let ix = self.write_index(self.alloc);
        let mut old: ABTreeMap<'a, ItemKey, ScopeBuilder<'a, N>> = ABTreeMap::new_in(alloc);
        for (&key, &m) in self.by_key.iter() {
            let md = &self.modules[m as usize];
            if md.foreign || md.unread || md.text == FragId::MAX {
                continue;
            }
            let Some((scope, unknown)) = self.scope(&ix, key, alloc) else { continue };
            if unknown > 0 {
                return Err(Disagreement::Unknown { module: key, count: unknown });
            }
            let bytes = scope.write(alloc).map_err(|error| Disagreement::Unwritten(TablesError { module: key, error }))?;
            totals.old_bytes += bytes.len() as u64;
            old.insert(key, scope);
        }
        let mut new: ABTreeMap<'a, ItemKey, ModuleRecords<'a>> = ABTreeMap::new_in(alloc);
        self.tables(alloc, &mut |t| {
            let r = write_tables(t, alloc)?;
            new.insert(r.module, r);
            Ok(())
        })
        .map_err(Disagreement::Unwritten)?;
        let mut held = Held { krate: self.krate, opened: ABTreeMap::new_in(alloc) };
        for (&key, r) in new.iter() {
            totals.new_bytes += r.bytes() as u64;
            let unread = |why| Disagreement::Unread { module: key, why };
            let imports = ImportsView::open(&r.imports).map_err(unread)?;
            imports.check().map_err(unread)?;
            let &m = self.by_key.get(&key).ok_or(Disagreement::Unread { module: key, why: TableRefused { what: "a record of no module of the crate" } })?;
            let opened = Opened {
                outline: open(key, &r.outline).map_err(unread)?,
                made: open_opt(key, &r.made).map_err(unread)?,
                textual: open_opt(key, &r.textual).map_err(unread)?,
                exported: open_opt(key, &r.exported).map_err(unread)?,
                imports,
                chain: &self.modules[m as usize].chain,
            };
            held.opened.insert(key, opened);
        }
        // The root's published `#[macro_export]`s (resolve.rs `publish_expanded_exports`): what
        // another crate reads at the root, after the root's own bindings. Its two sources, as
        // resolve.rs gives them: each written scope's expansion-written exports, and the exports
        // in the own text of a module an expansion or a block made (`in_made`: commonware's
        // `stability_scope!(.. { pub mod hostname; })`, nix's `feature! { pub mod socket; }`;
        // without them the old answer at the root was absent where every other crate finds
        // the macro, tempo 21 Nov 05:33). Of one name exported twice, the least key, as there.
        let root_key = self.modules[self.root as usize].key;
        let mut published: ABTreeMap<'a, N, Def> = ABTreeMap::new_in(alloc);
        let mut publish = |name: N, def: Def| {
            let kept = published.entry(name).or_insert(def);
            if def.key < kept.key {
                *kept = def;
            }
        };
        for s in old.values() {
            for t in s.macro_exports.iter() {
                publish(t.name, Def { key: t.def, kind: DefKind::Macro(t.shapes) });
            }
        }
        for md in self.modules[self.walked as usize..].iter() {
            if md.foreign || md.unread || md.text == FragId::MAX {
                continue;
            }
            for (_, slot) in self.frags[md.text as usize].slots.iter() {
                if let super::Slot::Rules(name, def, true) = *slot {
                    publish(name, def);
                }
            }
        }
        let mut scratch = ThroughScratch::new_in(alloc);
        let mut log: AVec<'a, CellRead> = AVec::new_in(alloc);
        let mut stack: AVec<'a, (ItemKey, CellKey)> = AVec::new_in(alloc);
        for (&key, s) in old.iter() {
            if !held.opened.contains_key(&key) {
                continue;
            }
            let m = self.by_key[&key];
            let md = &self.modules[m as usize];
            totals.modules += 1;
            // The names asked: the scope's own bindings, every name the module's text or calls
            // write (an import of a namespace its target lacks too), and the names its globs reach.
            let mut asked: ABTreeSet<'a, (Namespace, N)> = ABTreeSet::new_in(alloc);
            for e in s.bindings.iter() {
                asked.insert((e.ns, e.name));
            }
            for &(ns, name) in md.explicit.keys() {
                asked.insert((ns, name));
            }
            for (&name, ids) in md.named.iter() {
                for &j in ids.iter() {
                    for &ns in self.import_namespaces(j) {
                        asked.insert((ns, name));
                    }
                }
            }
            if key == root_key {
                for &name in published.keys() {
                    asked.insert((Namespace::Macros, name));
                }
            }
            totals.capped += self.glob_names(s, &old, &mut asked, alloc);
            // Every definition row is one of the names asked.
            let mut cells: ABTreeSet<'a, [u8; 16]> = ABTreeSet::new_in(alloc);
            for &(ns, name) in asked.iter() {
                cells.insert(cell_key(ns, name.as_ref()).0);
            }
            let o = &held.opened[&key];
            for (table, view) in [(Table::Outline, Some(o.outline)), (Table::Made, o.made), (Table::Exported, o.exported)] {
                let Some(view) = view else { continue };
                for i in 0..view.names() {
                    let row = view.row(i).map_err(|why| Disagreement::Unread { module: key, why })?;
                    let cell = cell_key(row.ns, row.name).0;
                    if matches!(row.to, TargetView::Def(_)) && !cells.contains(&cell) {
                        return Err(Disagreement::ExtraRow { module: key, table, cell: CellKey(cell) });
                    }
                }
            }
            for &(ns, name) in asked.iter() {
                let old_answer = match s.get(ns, name) {
                    Some(b) => Answer::of_passed(Passed { def: b.def, vis: b.vis, state: b.state }),
                    None => match published.get(&name) {
                        Some(&def) if key == root_key && ns == Namespace::Macros => Answer::Found { def, vis: Visibility::Public, ambiguous: false },
                        _ => {
                            let mut sources = OldScopes { cells: self, old: &old, ns, name };
                            match through_in(&mut sources, Def::module(key), &mut scratch) {
                                Ok(Some(p)) => Answer::of_passed(p),
                                Ok(None) => Answer::Absent,
                                Err(Through::Stop(())) => Answer::Outside,
                                Err(Through::Unbounded) => Answer::Unbounded,
                            }
                        }
                    },
                };
                log.clear();
                stack.clear();
                let hop = lookup(&held, key, ns, name.as_ref(), &mut log, &mut stack).map_err(|error| Disagreement::Lookup { module: key, error })?;
                let new_answer = match hop {
                    Hop::Found(p) => Answer::of_passed(p),
                    Hop::NotInTables => Answer::Absent,
                    Hop::Unfollowed(_, why) => Answer::Unfollowed(why),
                };
                self.judge(&mut totals, key, Asked::Path(ns), name, old_answer, new_answer, &log, report)?;
            }
            // The textual scope: every name of this module's sequence and its ancestors'.
            let mut textual: ABTreeSet<'a, N> = ABTreeSet::new_in(alloc);
            for &k in md.chain.iter() {
                if let Some(a) = old.get(&k) {
                    for t in a.textual.iter() {
                        textual.insert(t.name);
                    }
                }
            }
            for &name in textual.iter() {
                let old_answer = old_textual(&old, &md.chain, name);
                log.clear();
                let hop = lookup_textual(&held, key, name.as_ref(), &mut log).map_err(|error| Disagreement::Lookup { module: key, error })?;
                let new_answer = match hop {
                    TextualHop::Found(def, _) => Answer::Found { def, vis: Visibility::Public, ambiguous: false },
                    TextualHop::NotInTables => Answer::Absent,
                    // The ancestor `at` has no mark for the child the walk came up from because
                    // that child is a block's module (no `mod` line): core-only on both sides
                    // ([`Unfollowed::BlockTextualCut`]). Any other child's missing mark stays a gap.
                    TextualHop::Unfollowed(at, Unfollowed::TextualCut) if self.block_below(&md.chain, at) => Answer::Unfollowed(Unfollowed::BlockTextualCut),
                    TextualHop::Unfollowed(_, why) => Answer::Unfollowed(why),
                };
                self.judge(&mut totals, key, Asked::Textual, name, old_answer, new_answer, &log, report)?;
            }
        }
        Ok(totals)
    }

    /// Whether the module just below `at` on `chain` (a module first, its root last: the child
    /// a textual walk came up from) is a block's module ([`crate::blocks::is_block_module`]).
    fn block_below(&self, chain: &[ItemKey], at: ItemKey) -> bool {
        let Some(i) = chain.iter().position(|&k| k == at) else { return false };
        let Some(&child) = i.checked_sub(1).and_then(|c| chain.get(c)) else { return false };
        self.by_key.get(&child).is_some_and(|&m| self.modules[m as usize].name.is_some_and(|n| crate::blocks::is_block_module(n.as_ref())))
    }

    /// Counts one lookup, or stops with the disagreement.
    #[expect(clippy::too_many_arguments, reason = "one lookup's facts, each named at the call")]
    fn judge<'o>(
        &self,
        totals: &mut Agreed,
        module: ItemKey,
        asked: Asked,
        name: N,
        old: Answer,
        new: Answer,
        log: &[CellRead],
        report: Alloc<'o>,
    ) -> Result<(), Disagreement<'o, N>> {
        totals.names += 1;
        match new {
            Answer::Unfollowed(why) if why.core_only() => {
                totals.unfollowed[why.index()] += 1;
                return Ok(());
            }
            _ if new == old => {
                totals.agreed += 1;
                return Ok(());
            }
            _ => {}
        }
        let mut path: AVec<'o, N> = AVec::new_in(report);
        let mut at = self.by_key.get(&module).copied();
        while let Some(m) = at {
            let md = &self.modules[m as usize];
            if let Some(n) = md.name {
                path.push(n);
            }
            at = md.parent;
        }
        path.reverse();
        let mut kept: AVec<'o, CellRead> = AVec::with_capacity_in(log.len(), report);
        kept.extend_from_slice(log);
        Err(Disagreement::Answers(AnswersDiffer { module, path, asked, name, old, new, log: kept }))
    }

    /// Adds to `asked` the names bound by the in-crate modules `scope`'s globs reach, breadth
    /// first; returns how many were left out past the caps.
    fn glob_names(
        &self,
        scope: &ScopeBuilder<'_, N>,
        old: &ABTreeMap<'a, ItemKey, ScopeBuilder<'a, N>>,
        asked: &mut ABTreeSet<'a, (Namespace, N)>,
        alloc: Alloc<'a>,
    ) -> u64 {
        let mut seen: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
        let mut queue: AVec<'a, ItemKey> = AVec::new_in(alloc);
        for g in scope.globs.iter() {
            if g.source.kind == DefKind::Module && seen.insert(g.source.key) {
                queue.push(g.source.key);
            }
        }
        let (mut at, mut added, mut capped) = (0usize, 0usize, 0u64);
        while at < queue.len() {
            let k = queue[at];
            at += 1;
            let Some(s) = old.get(&k) else { continue };
            if at > MAX_GLOB_SOURCES {
                capped += s.bindings.len() as u64;
                continue;
            }
            for e in s.bindings.iter() {
                if added >= MAX_GLOB_NAMES {
                    capped += 1;
                } else if asked.insert((e.ns, e.name)) {
                    added += 1;
                }
            }
            for g in s.globs.iter() {
                if g.source.kind == DefKind::Module && seen.insert(g.source.key) {
                    queue.push(g.source.key);
                }
            }
        }
        capped
    }
}

/// The old textual answer: the last `macro_rules!` of `name` in the module's own sequence,
/// else in each ancestor's sequence cut at the child's mark (write.rs's `textual` and `marks`).
fn old_textual<N: crate::Name>(old: &ABTreeMap<'_, ItemKey, ScopeBuilder<'_, N>>, chain: &[ItemKey], name: N) -> Answer {
    for (i, &m) in chain.iter().enumerate() {
        let Some(s) = old.get(&m) else { continue };
        let len = match i {
            0 => s.textual.len(),
            _ => match s.mark(chain[i - 1]) {
                Some(len) => len as usize,
                None if s.textual.is_empty() => continue,
                None => return Answer::Outside,
            },
        };
        if let Some(t) = s.textual[..len.min(s.textual.len())].iter().rev().find(|t| t.name == name) {
            return Answer::Found { def: Def { key: t.def, kind: DefKind::Macro(t.shapes) }, vis: Visibility::Public, ambiguous: false };
        }
    }
    Answer::Absent
}
