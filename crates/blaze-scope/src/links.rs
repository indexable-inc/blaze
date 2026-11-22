//! Names at module grain (docs/v2/plan/design/drive.md, "Names at module grain", D116): the
//! records a crate's names are made of once nothing of one module is copied into another, and
//! the reads that walk their links one cell at a time.
//!
//! - [`module_records`]: each module's OUTLINE (`ModuleLinks`, table `Outline`), its
//!   `ModuleScope` (`ModuleLinks`, table `Made`, only for a module with item-position macro
//!   calls) and its `Imports`, written from the cell core's run (cells/made.rs).
//! - [`module_scope_cell`] and [`imports_cell`]: the kinds' part reads for the drive
//!   (`blaze_drive::CellOf` once adapted to its `Cell` and `Seen`; the adapter is blaze-cli's).
//! - [`lookup`]: one name looked up through the tables by its links, every hop a cell read
//!   logged in a [`CellRead`]: the outline (a declaration answers; a `use` link is followed
//!   segment by segment), then what the calls made, then the glob links, each followed as seen
//!   from this module, equal definitions merged and different ones `Ambiguous`; a lookup already
//!   on the walk's stack gives nothing (a glob cycle's least fixed point). What the tables cannot
//!   decide (a path through the extern prelude or `$crate`, an enum's variants, the preludes, a
//!   plain first segment a call made or a glob brought, which an outer name may contest) is
//!   [`Hop::Unfollowed`] for the cell core, which keeps the rules' one home (rule 30): this is the
//!   hop over records the core's `decide` reads through, not a second resolver.
//!
//! A glob passes a name on only where the importing module may name it, narrowed to the glob's
//! own visibility (`pub(crate) use m::*` re-exports `m`'s `pub` names as `pub(crate)`), and one
//! item by two globs is one binding as visible as the wider: blaze-surface's `through` rules
//! (`SeenFrom`, `merge`), not a copy of them. A `use` is as visible as the narrower of itself and
//! its target. [`lookup_textual`] reads the textual `macro_rules!` scope (`Table::Textual`), and
//! a crate root's `#[macro_export]` macros are its `Table::Exported`, read after its outline and
//! made table.
//!
//! Not in the tables (named so no reader takes their absence as an answer): the solve's findings
//! of the made table; the preludes, `$crate`, a `use` of an extern crate and an enum's variants
//! (each an [`Unfollowed`] kind the core keeps).

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{
    Asker, BindingState, CellSeen, DECLARED_VIS, ImportsView, LinksView, Passed, RecordBytes, RowVis, Segments, SeenFrom,
    Table, TableRefused, TargetView, imports_part, links_part, mark_cell, merge,
};

use crate::cell_key::{CellKey, cell_key, textual_cut_key};
use crate::cells::{CellError, Cells, TablesError};
pub use crate::cells::{Agreed, AnswersDiffer, Answer, Asked, Disagreement};
use crate::facts::ScopeFacts;
use crate::{Def, DefKind, Namespace, PathKind, Visibility};

/// The `ModuleScope` kind's (and the outline's) part read: a cell's digest in a `ModuleLinks`
/// record, or `Absent` (the glob links' digest when the table has globs).
pub fn module_scope_cell(payload: &[u8], cell: CellKey) -> Result<CellSeen, TableRefused> {
    links_part(payload, cell.0)
}

/// The `Imports` kind's part read; `blaze_surface::TRAITS_CELL` is its TRAITS column.
pub fn imports_cell(payload: &[u8], cell: CellKey) -> Result<CellSeen, TableRefused> {
    imports_part(payload, cell.0)
}

/// One module's records at module grain.
pub struct ModuleRecords<'r> {
    pub module: ItemKey,
    pub outline: RecordBytes<'r>,
    /// The module's `ModuleScope`: `None` for a module with no item-position macro call.
    pub made: Option<RecordBytes<'r>>,
    pub imports: RecordBytes<'r>,
    /// Its textual `macro_rules!` scope: `None` when the sequence is empty.
    pub textual: Option<RecordBytes<'r>>,
    /// The crate's `#[macro_export]` macros: `Some` at the crate root alone.
    pub exported: Option<RecordBytes<'r>>,
}

impl ModuleRecords<'_> {
    /// Every byte of the module's records: what the names at module grain cost.
    pub fn bytes(&self) -> usize {
        let opt = |b: &Option<RecordBytes<'_>>| b.as_ref().map_or(0, |b| b.len());
        self.outline.len() + opt(&self.made) + self.imports.len() + opt(&self.textual) + opt(&self.exported)
    }
}

/// Why [`module_records`] stopped, naming the thing.
#[derive(Clone, Debug)]
pub enum RecordsError {
    Cells(CellError),
    Tables(TablesError),
}

impl core::fmt::Display for RecordsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordsError::Cells(e) => write!(f, "the names of the crate were not solved: {e}"),
            RecordsError::Tables(e) => write!(f, "the names records of module {:x} were not written: {}", e.module, e.error),
        }
    }
}

/// The cell core on one crate's module tree under `root` (`expand` makes a call's expansion,
/// which `facts.expansion` then hands back, as for [`crate::cells_count`]), then every module's
/// records lent to `visit`, in module key order.
pub fn module_records<'a, F: ScopeFacts>(
    facts: &F,
    root: ItemKey,
    alloc: Alloc<'a>,
    expand: &dyn Fn(ItemKey, Def),
    visit: &mut dyn FnMut(ModuleRecords<'a>),
) -> Result<(), RecordsError> {
    let mut c = Cells::load(facts, root, alloc).map_err(RecordsError::Cells)?;
    c.run(facts, expand, &[]).map_err(RecordsError::Cells)?;
    c.tables(alloc, &mut |t| {
        visit(write_tables(t, alloc)?);
        Ok(())
    })
    .map_err(RecordsError::Tables)
}

/// One module's tables written as its records.
pub(crate) fn write_tables<'a>(mut t: crate::cells::Tables<'_, 'a>, alloc: Alloc<'a>) -> Result<ModuleRecords<'a>, blaze_surface::BuildError> {
    let opt = |b: &Option<blaze_surface::LinksBuilder<'_, 'a>>| match b {
        Some(b) => b.write(alloc).map(Some),
        None => Ok(None),
    };
    Ok(ModuleRecords {
        module: t.module,
        outline: t.outline.write(alloc)?,
        made: opt(&t.made)?,
        imports: t.imports.write(alloc)?,
        textual: opt(&t.textual)?,
        exported: opt(&t.exported)?,
    })
}

/// D116's equality harness on one crate: the cell core loaded and run on `root` (as
/// [`module_records`] does), then every name its written scopes answer compared with
/// [`lookup`] over its records ([`Cells::links_agree`], cells/agree.rs).
pub fn links_agree<'a, F: ScopeFacts>(
    facts: &F,
    root: ItemKey,
    alloc: Alloc<'a>,
    expand: &dyn Fn(ItemKey, Def),
) -> Result<Agreed, Disagreement<'a, F::Name>>
where
    F::Name: 'a,
{
    let mut c = Cells::load(facts, root, alloc).map_err(Disagreement::Unsolved)?;
    c.run(facts, expand, &[]).map_err(Disagreement::Unsolved)?;
    c.links_agree(alloc, alloc)
}

/// The tables a [`lookup`] reads, all of one crate: each module's records, mapped, and its
/// chain.
pub trait LinkTables<'b> {
    /// `module`'s table: `Outline` for every module of the crate whose text was read; `Made`
    /// only for one with item-position calls; `Textual` for one with a textual sequence;
    /// `Exported` at the root alone.
    fn table(&self, module: ItemKey, table: Table) -> Option<LinksView<'b>>;
    /// `module`'s `Imports`.
    fn imports(&self, module: ItemKey) -> Option<ImportsView<'b>>;
    /// `module`, then its ancestors up to its crate's root (the module's identity, not a read).
    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]>;
    /// The crate the tables are of: who a `pub(crate)` lets name a binding.
    fn krate(&self) -> CrateItemKey;
}

/// Which record of a module a [`CellRead`] read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Record {
    Links(Table),
    Imports,
}

/// One part read a lookup made: of `module`'s `record`, the cell, and what it held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRead {
    pub module: ItemKey,
    pub record: Record,
    pub cell: CellKey,
    pub seen: CellSeen,
}

/// Why a lookup left a name to the cell core: the links the tables do not follow.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Unfollowed {
    /// A path through a prelude: `::a`, or a plain first segment no table here binds (the
    /// extern prelude, the standard prelude).
    Prelude,
    /// A path that starts at `$crate`.
    DollarCrate,
    /// `use x;` of a name no table here binds: an extern crate.
    ExternCrateUse,
    /// A path segment or glob source that is an enum (its variants are the core's) or no module.
    NotAModule,
    /// A module with no tables here: another crate's, or one whose text was not read.
    Unheld,
    /// A path with no segment.
    EmptyPath,
    /// An ancestor whose `Textual` table has no mark row for the child the walk came up from.
    /// (A name defined both before and after the child's `mod` line is no longer one: the
    /// table's cut row holds the definition the child sees, `cell_key::textual_cut_key`.)
    TextualCut,
    /// A definition's written `pub(in path)` with no decided row in `Imports`.
    VisUndecided,
    /// The ORACLE's reading of a [`Unfollowed::TextualCut`] whose child is a block's module
    /// (cells/agree.rs, never [`lookup_textual`]): a block has no `mod` line, so no mark row, and
    /// the old scope's walk passes it as outside. Neither side decides the textual scope seen
    /// from inside a body's block (D189: the body's own solve does), so it is core-only.
    BlockTextualCut,
}

impl Unfollowed {
    pub const ALL: [Unfollowed; 9] = [
        Unfollowed::Prelude,
        Unfollowed::DollarCrate,
        Unfollowed::ExternCrateUse,
        Unfollowed::NotAModule,
        Unfollowed::Unheld,
        Unfollowed::EmptyPath,
        Unfollowed::TextualCut,
        Unfollowed::VisUndecided,
        Unfollowed::BlockTextualCut,
    ];

    /// Its place in [`Unfollowed::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Unfollowed::Prelude => 0,
            Unfollowed::DollarCrate => 1,
            Unfollowed::ExternCrateUse => 2,
            Unfollowed::NotAModule => 3,
            Unfollowed::Unheld => 4,
            Unfollowed::EmptyPath => 5,
            Unfollowed::TextualCut => 6,
            Unfollowed::VisUndecided => 7,
            Unfollowed::BlockTextualCut => 8,
        }
    }

    /// Whether the case is the cell core's by design (the preludes, `$crate`, extern crates,
    /// variants, other crates): such a lookup agrees with any answer of the old scope, counted
    /// apart. The others are gaps of the tables, and a disagreement.
    pub const fn core_only(self) -> bool {
        matches!(
            self,
            Unfollowed::Prelude | Unfollowed::DollarCrate | Unfollowed::ExternCrateUse | Unfollowed::NotAModule | Unfollowed::Unheld | Unfollowed::BlockTextualCut
        )
    }

    pub const fn text(self) -> &'static str {
        match self {
            Unfollowed::Prelude => "prelude",
            Unfollowed::DollarCrate => "$crate",
            Unfollowed::ExternCrateUse => "extern crate use",
            Unfollowed::NotAModule => "not a module",
            Unfollowed::Unheld => "unheld module",
            Unfollowed::EmptyPath => "empty path",
            Unfollowed::TextualCut => "textual cut",
            Unfollowed::VisUndecided => "pub(in path) undecided",
            Unfollowed::BlockTextualCut => "textual cut at a block (no mod line)",
        }
    }
}

/// What a lookup through the tables came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hop {
    /// The binding: its definition, who may name it from the module asked, and its state (two
    /// different definitions through globs are `Ambiguous(GlobVsGlob)` under the smaller key,
    /// an error only where used, as blaze-surface's `merge` keeps them).
    Found(Passed),
    /// Not in the tables the walk reached (the preludes are the cell core's).
    NotInTables,
    /// A link the tables do not follow, met in this module: the cell core decides it.
    Unfollowed(ItemKey, Unfollowed),
}

/// What a textual `macro_rules!` lookup through the tables came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextualHop {
    /// The macro and the expansion depth it was defined at.
    Found(Def, u16),
    NotInTables,
    Unfollowed(ItemKey, Unfollowed),
}

/// Why a lookup stopped, naming the module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookupError {
    Refused { module: ItemKey, why: TableRefused },
    /// A module the walk reached has no outline or no chain among the tables given.
    NoTable { module: ItemKey },
    /// The walk passed [`MAX_HOPS`] lookups deep (rule 18).
    TooDeep { module: ItemKey },
}

/// How many lookups deep one walk may go.
pub const MAX_HOPS: usize = 256;

/// `name` in `ns` as seen from `module`, every hop logged in `log`; `stack` is the walk's
/// (empty at the first call).
pub fn lookup<'b>(
    tables: &dyn LinkTables<'b>,
    module: ItemKey,
    ns: Namespace,
    name: &str,
    log: &mut AVec<'_, CellRead>,
    stack: &mut AVec<'_, (ItemKey, CellKey)>,
) -> Result<Hop, LookupError> {
    Ok(lookup_name(tables, module, ns, cell_key(ns, name), log, stack)?.0)
}

/// Where a [`Hop::Found`] binding stands in the module asked: a row of its outline (its own
/// text, a declaration or a `use` not written by a macro), or later (a row a call made, or a
/// glob). A path's first segment found later may lose to an outer candidate the tables do not
/// hold (an extern crate, a prelude name: R6's macro-expanded name or a glob against an outer
/// one), which the cell core decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Via {
    Text,
    Later,
}

fn lookup_in<'b>(
    tables: &dyn LinkTables<'b>,
    module: ItemKey,
    ns: Namespace,
    cell: CellKey,
    log: &mut AVec<'_, CellRead>,
    stack: &mut AVec<'_, (ItemKey, CellKey)>,
) -> Result<(Hop, Via), LookupError> {
    let Some(outline) = tables.table(module, Table::Outline) else {
        return Ok((Hop::Unfollowed(module, Unfollowed::Unheld), Via::Text));
    };
    let chain = tables.chain(module).ok_or(LookupError::NoTable { module })?;
    let asker = Asker { krate: tables.krate(), chain };
    let made = tables.table(module, Table::Made);
    // The root's `#[macro_export]` macros, after what its text and calls bind.
    let exported = if ns == Namespace::Macros && chain.len() == 1 { tables.table(module, Table::Exported) } else { None };
    let refused = |why| LookupError::Refused { module, why };
    for view in [Some(outline), made, exported].into_iter().flatten() {
        let row = view.get(cell.0).map_err(refused)?;
        let seen = match row {
            Some(r) => CellSeen::Is(r.digest),
            None => view.part(cell.0),
        };
        log.push(CellRead { module, record: Record::Links(view.table()), cell, seen });
        let Some(r) = row else { continue };
        let via = if view.table() == Table::Outline { Via::Text } else { Via::Later };
        let vis = match r.vis {
            RowVis::Is(vis) => vis,
            RowVis::Written { .. } => match declared_vis(tables, module, cell, log)? {
                Some(vis) => vis,
                None => return Ok((Hop::Unfollowed(module, Unfollowed::VisUndecided), via)),
            },
        };
        match r.to {
            TargetView::Def(def) => return Ok((Hop::Found(Passed { def, vis, state: BindingState::Determined }), via)),
            TargetView::Path { kind, segments } => match follow(tables, module, kind, segments, ns, log, stack)? {
                // A `use` is as visible as the narrower of itself and its target (E0364, E0365).
                Hop::Found(p) => return Ok((Hop::Found(Passed { vis: Visibility::narrower(vis, p.vis, asker), ..p }), via)),
                // A `use` that names nothing in this namespace binds nothing in it: the globs may.
                Hop::NotInTables => break,
                h @ Hop::Unfollowed(..) => return Ok((h, via)),
            },
        }
    }
    let mut acc: Option<Passed> = None;
    for view in [Some(outline), made].into_iter().flatten() {
        for i in 0..view.globs() {
            let glob = view.glob(i).map_err(refused)?;
            let source = match glob.to {
                TargetView::Def(def) if def.kind == DefKind::Module => def.key,
                TargetView::Def(_) => return Ok((Hop::Unfollowed(module, Unfollowed::NotAModule), Via::Later)),
                TargetView::Path { kind, segments } => match walk(tables, module, kind, segments, segments.len(), log, stack)? {
                    Walk::At(m) => m,
                    // The core refused the glob (a finding): it brings nothing.
                    Walk::Absent => continue,
                    Walk::Unfollowed(why) => return Ok((Hop::Unfollowed(module, why), Via::Later)),
                },
            };
            match lookup_name(tables, source, ns, cell, log, stack)?.0 {
                Hop::NotInTables => {}
                // Kept only where this module may name it, narrowed by the glob (through.rs).
                Hop::Found(p) if p.vis.visible_from(asker) => {
                    let p = Passed { vis: Visibility::narrower(glob.vis, p.vis, asker), ..p };
                    acc = Some(merge(acc, p, asker));
                }
                Hop::Found(_) => {}
                h @ Hop::Unfollowed(..) => return Ok((h, Via::Later)),
            }
        }
    }
    Ok((acc.map_or(Hop::NotInTables, Hop::Found), Via::Later))
}

/// The decided visibility of a definition whose row holds its `pub(in path)` as written: the
/// module's `Imports` row of the cell with index [`DECLARED_VIS`], one cell read.
fn declared_vis<'b>(tables: &dyn LinkTables<'b>, module: ItemKey, cell: CellKey, log: &mut AVec<'_, CellRead>) -> Result<Option<Visibility>, LookupError> {
    let Some(view) = tables.imports(module) else { return Ok(None) };
    let row = view.get(cell.0).map_err(|why| LookupError::Refused { module, why })?;
    log.push(CellRead { module, record: Record::Imports, cell, seen: view.part(cell.0) });
    Ok(row.filter(|r| r.index == DECLARED_VIS).map(|r| r.vis))
}

/// `name`'s `macro_rules!` in the textual scope at the end of `module`'s text: its own
/// sequence's last definition of the name, else each ancestor's sequence cut at the `mod` line
/// of the child the walk came up from (its mark row), every table read one cell, and one more
/// (the cut row) where the name is defined on both sides of that line.
pub fn lookup_textual<'b>(tables: &dyn LinkTables<'b>, module: ItemKey, name: &str, log: &mut AVec<'_, CellRead>) -> Result<TextualHop, LookupError> {
    let cell = cell_key(Namespace::Macros, name);
    let chain = tables.chain(module).ok_or(LookupError::NoTable { module })?;
    for (i, &m) in chain.iter().enumerate() {
        let Some(view) = tables.table(m, Table::Textual) else { continue };
        let refused = |why| LookupError::Refused { module: m, why };
        let cut = match i {
            0 => None,
            _ => {
                let child = chain[i - 1];
                let mark = CellKey(mark_cell(child));
                let row = view.get(mark.0).map_err(refused)?;
                log.push(CellRead { module: m, record: Record::Links(Table::Textual), cell: mark, seen: view.part(mark.0) });
                match row {
                    Some(r) => Some((child, r.order)),
                    None => return Ok(TextualHop::Unfollowed(m, Unfollowed::TextualCut)),
                }
            }
        };
        let row = view.get(cell.0).map_err(refused)?;
        log.push(CellRead { module: m, record: Record::Links(Table::Textual), cell, seen: view.part(cell.0) });
        let Some(mut r) = row else { continue };
        match cut {
            // Every definition of the name is after the child's `mod` line: none is seen.
            Some((_, len)) if r.first >= len => continue,
            // The last is after it and the first before: the one seen is the cut row's.
            Some((child, len)) if r.order >= len => {
                let at = textual_cut_key(child, name);
                let row = view.get(at.0).map_err(refused)?;
                log.push(CellRead { module: m, record: Record::Links(Table::Textual), cell: at, seen: view.part(at.0) });
                r = row.ok_or(LookupError::Refused { module: m, why: TableRefused { what: "a textual row cut by a child's `mod` line with no cut row" } })?;
            }
            _ => {}
        }
        let TargetView::Def(def) = r.to else { return Err(LookupError::Refused { module: m, why: TableRefused { what: "a textual row that is no definition" } }) };
        return Ok(TextualHop::Found(def, r.expansion));
    }
    Ok(TextualHop::NotInTables)
}

/// [`lookup`] by a cell already keyed: the glob's source is asked the same cell.
fn lookup_name<'b>(
    tables: &dyn LinkTables<'b>,
    module: ItemKey,
    ns: Namespace,
    cell: CellKey,
    log: &mut AVec<'_, CellRead>,
    stack: &mut AVec<'_, (ItemKey, CellKey)>,
) -> Result<(Hop, Via), LookupError> {
    if stack.iter().any(|&(m, c)| m == module && c == cell) {
        return Ok((Hop::NotInTables, Via::Later));
    }
    if stack.len() >= MAX_HOPS {
        return Err(LookupError::TooDeep { module });
    }
    stack.push((module, cell));
    let out = lookup_in(tables, module, ns, cell, log, stack);
    stack.pop();
    out
}

/// A `use` link of `from` followed: its path's modules, then its last segment in `ns`.
fn follow<'b>(
    tables: &dyn LinkTables<'b>,
    from: ItemKey,
    kind: PathKind,
    segments: Segments<'b>,
    ns: Namespace,
    log: &mut AVec<'_, CellRead>,
    stack: &mut AVec<'_, (ItemKey, CellKey)>,
) -> Result<Hop, LookupError> {
    let n = segments.len();
    if n == 0 {
        return Ok(match kind {
            // `use crate as name;`: the crate root, a module, in the type namespace alone (rustc
            // refuses the unnamed `use crate;`: "crate root imports need to be explicitly named:
            // `use crate as name;`"). icu's baked data writes `pub mod icu { pub use crate as
            // normalizer; }` in a `const _: () = { .. }` block (icu_normalizer, icu_properties;
            // tempo, 21 Nov 05:33): the row was left to the core (`EmptyPath`), the core found it.
            // The caller narrows by the `use`'s own visibility; the root itself is public.
            PathKind::Crate if ns == Namespace::Types => {
                let chain = tables.chain(from).ok_or(LookupError::NoTable { module: from })?;
                let root = *chain.last().ok_or(LookupError::NoTable { module: from })?;
                Hop::Found(Passed { def: Def::module(root), vis: Visibility::Public, state: BindingState::Determined })
            }
            PathKind::Crate => Hop::NotInTables,
            _ => Hop::Unfollowed(from, Unfollowed::EmptyPath),
        });
    }
    let m = match walk(tables, from, kind, segments, n - 1, log, stack)? {
        Walk::At(m) => m,
        Walk::Absent => return Ok(Hop::NotInTables),
        Walk::Unfollowed(why) => return Ok(Hop::Unfollowed(from, why)),
    };
    let last = segments.get(n - 1).map_err(|why| LookupError::Refused { module: from, why })?;
    let chain = tables.chain(from).ok_or(LookupError::NoTable { module: from })?;
    let asker = Asker { krate: tables.krate(), chain };
    let lexical = kind == PathKind::Plain && n == 1;
    match lookup_name(tables, m, ns, cell_key(ns, last), log, stack)? {
        // `use x;` of a plain name not declared here: an extern crate, the core's.
        (Hop::NotInTables, _) if lexical => Ok(Hop::Unfollowed(from, Unfollowed::ExternCrateUse)),
        // `use x;` of a name a call made or a glob brought: an outer `x` may make it ambiguous.
        (Hop::Found(_), Via::Later) if lexical => Ok(Hop::Unfollowed(from, Unfollowed::Prelude)),
        // A target the importer may not name is the core's `Private` finding: no binding.
        (Hop::Found(p), _) if !p.vis.visible_from(asker) => Ok(Hop::NotInTables),
        (h, _) => Ok(h),
    }
}

/// Where the module part of a path leads.
enum Walk {
    At(ItemKey),
    /// Nowhere: a segment no table binds, binds as a module the importer may not name, or binds
    /// ambiguously (the core's finding), or `super` above the root.
    Absent,
    Unfollowed(Unfollowed),
}

/// The module the first `upto` segments of a path written in `from` name.
fn walk<'b>(
    tables: &dyn LinkTables<'b>,
    from: ItemKey,
    kind: PathKind,
    segments: Segments<'b>,
    upto: usize,
    log: &mut AVec<'_, CellRead>,
    stack: &mut AVec<'_, (ItemKey, CellKey)>,
) -> Result<Walk, LookupError> {
    let chain = tables.chain(from).ok_or(LookupError::NoTable { module: from })?;
    let asker = Asker { krate: tables.krate(), chain };
    let mut at = match kind {
        PathKind::Plain | PathKind::SelfMod => from,
        PathKind::Super(n) => match chain.get(usize::from(n)) {
            Some(&m) => m,
            None => return Ok(Walk::Absent),
        },
        PathKind::Crate => *chain.last().ok_or(LookupError::NoTable { module: from })?,
        PathKind::Abs => return Ok(Walk::Unfollowed(Unfollowed::Prelude)),
        PathKind::DollarCrate(_) => return Ok(Walk::Unfollowed(Unfollowed::DollarCrate)),
    };
    for i in 0..upto {
        let seg = segments.get(i).map_err(|why| LookupError::Refused { module: from, why })?;
        let (hop, via) = lookup_name(tables, at, Namespace::Types, cell_key(Namespace::Types, seg), log, stack)?;
        match hop {
            // A plain first segment a call made or a glob brought: an outer candidate (the
            // extern prelude, a prelude module) may make it ambiguous or have been taken first
            // (R6), which the core decides.
            Hop::Found(_) if i == 0 && kind == PathKind::Plain && via == Via::Later => return Ok(Walk::Unfollowed(Unfollowed::Prelude)),
            Hop::Found(p) if p.state != BindingState::Determined || !p.vis.visible_from(asker) => return Ok(Walk::Absent),
            Hop::Found(p) if p.def.kind == DefKind::Module => at = p.def.key,
            Hop::Found(_) => return Ok(Walk::Unfollowed(Unfollowed::NotAModule)),
            Hop::NotInTables if i == 0 && kind == PathKind::Plain => return Ok(Walk::Unfollowed(Unfollowed::Prelude)),
            Hop::NotInTables => return Ok(Walk::Absent),
            Hop::Unfollowed(_, why) => return Ok(Walk::Unfollowed(why)),
        }
    }
    Ok(Walk::At(at))
}
