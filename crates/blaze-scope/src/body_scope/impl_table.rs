//! D189 step 4's whole-crate differential (proof 3): a crate's impl table as rows of (the impl,
//! where it is written, where the bucket index files it: its trait and its self type's head,
//! resolved), made once under `BlockScopes::Modules` (every block a module of the crate solve)
//! and once under `Bodies` (the crate's module-level impls, then each body's impls from its
//! `BodyScope`, resolved through [`super::BodyWorld`], super::merged `body_rows`). The two are compared by impl key: block module
//! keys and every key under them are made by the one `make_blocks` from the one text under both
//! (crate::blocks), so a row is the same row under both, byte for byte.
//!
//! Every path is resolved through the crate's OWN names ([`OwnNames`], [`super::BodyWorld`]): a path
//! into another crate is filed `TraitNotRead`/`SelfNotRead` under both strategies alike, so the
//! differential needs no dependency's names and runs where the command resolves one crate
//! against its dependencies' records (crate_scope.rs `block_scopes_differential`).

#![blaze::sans_io]

use core::fmt;

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Bump};

use super::{BodyScope, BodyScopeRefusal};
use crate::items_record::buckets::{Filing, KEY, OwnNames};
use crate::items_record::{ItemsRecordError, impl_filing};
use crate::resolve::CrateNames;

/// One impl of a crate's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImplRow {
    pub key: ItemKey,
    /// The module it is written in (a block's module for an impl in a body).
    pub module: ItemKey,
    /// Its trait and self type's head as the bucket index files them.
    pub filing: Filing,
}

/// The impl table of crate `c` as its solve wrote it (under `Modules`: every impl of every
/// block too), sorted by impl key.
pub fn modules_impl_table<'o>(c: &CrateNames<'_>, alloc: Alloc<'o>) -> Result<AVec<'o, ImplRow>, ItemsRecordError> {
    let own = OwnNames(c);
    let mut rows: AVec<'o, ImplRow> = AVec::with_capacity_in(c.tables.impls.len(), alloc);
    for i in c.tables.impls.iter() {
        rows.push(ImplRow { key: i.key, module: i.module, filing: impl_filing(&own, c.key, c.edition, i, alloc)? });
    }
    rows.sort_unstable_by_key(|r| r.key);
    Ok(rows)
}

/// What the differential reads of the `Modules` solve, copied out of that solve's frame
/// ([`ModulesSide::of`]), so the `Bodies` solve runs in a frame of its own after the first is
/// gone: on tempo `core` panicked with 4,411,162,624 bytes committed of its 4 GiB frame with
/// both whole solves (and every held body's) in the one frame. Its size is the crate's impl
/// count: each impl's row and header, and the chain of each module an impl is written in.
#[derive(Debug)]
pub struct ModulesSide<'o> {
    /// The impl table under `Modules` ([`modules_impl_table`]), sorted by impl key.
    pub rows: AVec<'o, ImplRow>,
    /// Each module a row is written in, with its chain (itself first, the root last), sorted
    /// by module: what tells a row written under a refused body's block.
    chains: AVec<'o, (ItemKey, AVec<'o, ItemKey>)>,
    /// Each impl's header, sorted by impl key ([`ImplHeaders`]).
    headers: AVec<'o, (ItemKey, &'o str)>,
}

impl<'o> ModulesSide<'o> {
    /// The side of crate `c`, solved under `Modules`, copied into `out`; the rows are made in
    /// `scratch` (the solve's frame: what filing an impl reads is left there).
    pub fn of(c: &CrateNames<'_>, scratch: Alloc<'_>, out: Bump<'o>) -> Result<ModulesSide<'o>, ItemsRecordError> {
        let out_alloc = out.alloc();
        let made = modules_impl_table(c, scratch)?;
        let mut rows: AVec<'o, ImplRow> = AVec::with_capacity_in(made.len(), out_alloc);
        rows.extend_from_slice(&made);
        let mut modules: AVec<'o, ItemKey> = AVec::from_iter_in(rows.iter().map(|r| r.module), out_alloc);
        modules.sort_unstable();
        modules.dedup();
        let mut chains: AVec<'o, (ItemKey, AVec<'o, ItemKey>)> = AVec::with_capacity_in(modules.len(), out_alloc);
        for &m in modules.iter() {
            let mut chain: AVec<'o, ItemKey> = AVec::new_in(out_alloc);
            if let Some(held) = c.chains.get(&m) {
                chain.extend_from_slice(held);
            }
            chains.push((m, chain));
        }
        let mut headers: AVec<'o, (ItemKey, &'o str)> = AVec::with_capacity_in(c.tables.impls.len(), out_alloc);
        for i in c.tables.impls.iter() {
            let kept: &'o str = out.alloc_str(i.header);
            headers.push((i.key, kept));
        }
        headers.sort_unstable_by_key(|r| r.0);
        headers.dedup_by_key(|r| r.0);
        Ok(ModulesSide { rows, chains, headers })
    }

    /// The chain of `module` (a module a row is written in), itself first.
    pub fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.chains.binary_search_by_key(&module, |c| c.0).ok().map(|i| &self.chains[i].1[..])
    }
}

/// The impl table of crate `c` solved under `Bodies`: its own impls (no block's), then the
/// impls of each body of `bodies`, each resolved through the body's blocks; sorted by impl key.
pub fn bodies_impl_table<'r, 'a, 'o>(c: &'r CrateNames<'a>, bodies: &'r [BodyScope<'a>], alloc: Alloc<'o>) -> Result<AVec<'o, ImplRow>, ItemsRecordError> {
    let mut rows = modules_impl_table(c, alloc)?;
    for body in bodies {
        rows.extend_from_slice(&super::merged::body_rows(c, body, alloc)?);
    }
    rows.sort_unstable_by_key(|r| r.key);
    Ok(rows)
}

/// Two impl tables compared by impl key.
#[derive(Debug)]
pub struct ImplDiff<'o> {
    /// Rows equal under both.
    pub equal: usize,
    /// Rows only the `Modules` table holds.
    pub only_modules: AVec<'o, ImplRow>,
    /// Rows only the `Bodies` table holds.
    pub only_bodies: AVec<'o, ImplRow>,
    /// One impl filed or placed differently: (`Modules`'s row, `Bodies`'s row).
    pub differ: AVec<'o, (ImplRow, ImplRow)>,
}

impl ImplDiff<'_> {
    /// No row differs.
    pub fn is_empty(&self) -> bool {
        self.only_modules.is_empty() && self.only_bodies.is_empty() && self.differ.is_empty()
    }
}

/// `modules` against `bodies`, each sorted by impl key.
pub fn impl_diff<'o>(modules: &[ImplRow], bodies: &[ImplRow], alloc: Alloc<'o>) -> ImplDiff<'o> {
    let mut out = ImplDiff { equal: 0, only_modules: AVec::new_in(alloc), only_bodies: AVec::new_in(alloc), differ: AVec::new_in(alloc) };
    let (mut i, mut j) = (0usize, 0usize);
    while i < modules.len() || j < bodies.len() {
        match (modules.get(i), bodies.get(j)) {
            (Some(m), Some(b)) if m.key == b.key => {
                if m == b {
                    out.equal += 1;
                } else {
                    out.differ.push((*m, *b));
                }
                i += 1;
                j += 1;
            }
            (Some(m), Some(b)) if m.key < b.key => {
                out.only_modules.push(*m);
                i += 1;
            }
            (Some(_), Some(b)) => {
                out.only_bodies.push(*b);
                j += 1;
            }
            (Some(m), None) => {
                out.only_modules.push(*m);
                i += 1;
            }
            (None, Some(b)) => {
                out.only_bodies.push(*b);
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

/// Whether `key` is an item of crate `c` (solved under `Modules`) declared under a block's
/// module: its module, or one around it, is a block's.
fn in_a_block(c: &CrateNames<'_>, key: ItemKey) -> bool {
    let Some((text, _)) = c.tables.texts.get(&key) else { return false };
    let Some(chain) = c.chains.get(&text.module) else { return false };
    chain.iter().any(|m| c.anonymous(*m))
}

/// Whether the impl `row` of crate `c` (solved under `Modules`) can be seen only inside the
/// body that writes it: it is filed (its paths resolved here) under a self head that is a type
/// declared in a block, and, for a trait impl, a trait declared in a block. Anything else
/// (a trait or type of the crate or of another crate, a head with no constructor, an impl not
/// filed) may be seen outside: a superset (rule 3).
pub fn inside_only(c: &CrateNames<'_>, row: &ImplRow) -> bool {
    let Filing::At(bucket) = row.filing else { return false };
    let bytes = bucket.bytes();
    if bytes.len() != KEY || bytes[33] != 0 {
        return false;
    }
    let key = |at: usize| {
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes[at..at + 32]);
        ItemKey(blaze_id::ContentId(k))
    };
    if !in_a_block(c, key(36)) {
        return false;
    }
    bytes[0] == 0 || in_a_block(c, key(1))
}

/// Whether the impl `row` of crate `c` (solved under `Modules`, the truth) is written inside a
/// body: the module it is written in, or one around it, is a block's. The rows a `Bodies` table
/// must take from `BodyScope` records, never from the crate's own solve.
pub fn written_in_a_body(c: &CrateNames<'_>, row: &ImplRow) -> bool {
    c.anonymous(row.module) || c.chains.get(&row.module).is_some_and(|chain| chain.iter().any(|m| c.anonymous(*m)))
}

/// Each impl's header text by impl key, sorted: what a verdict names an impl by (its trait and
/// self type as written, rule 19), never its key's bytes.
#[derive(Debug)]
pub struct ImplHeaders<'a> {
    rows: AVec<'a, (ItemKey, &'a str)>,
}

impl<'a> ImplHeaders<'a> {
    /// The headers of the impls of `c` and of every body record of `bodies`.
    pub fn of(c: &CrateNames<'a>, bodies: &[BodyScope<'a>], alloc: Alloc<'a>) -> ImplHeaders<'a> {
        let mut rows: AVec<'a, (ItemKey, &'a str)> = AVec::with_capacity_in(c.tables.impls.len(), alloc);
        rows.extend(c.tables.impls.iter().map(|i| (i.key, i.header)));
        for b in bodies {
            rows.extend(b.impls.iter().map(|i| (i.key, i.header)));
        }
        rows.sort_unstable_by_key(|r| r.0);
        rows.dedup_by_key(|r| r.0);
        ImplHeaders { rows }
    }

    /// The headers of `more` added (another solve's impls of the same crate).
    pub fn add(&mut self, more: &CrateNames<'a>) {
        self.rows.extend(more.tables.impls.iter().map(|i| (i.key, i.header)));
        self.rows.sort_unstable_by_key(|r| r.0);
        self.rows.dedup_by_key(|r| r.0);
    }

    /// The headers of the `Modules` side added ([`ModulesSide`], copied out of its frame).
    pub fn add_side<'o: 'a>(&mut self, side: &ModulesSide<'o>) {
        self.rows.extend(side.headers.iter().map(|&(k, h)| -> (ItemKey, &'a str) { (k, h) }));
        self.rows.sort_unstable_by_key(|r| r.0);
        self.rows.dedup_by_key(|r| r.0);
    }

    /// `key`'s header, written as one line (`impl<T> Tr for X<T>`).
    pub fn of_key(&self, key: ItemKey) -> Option<OneLine<'a>> {
        self.rows.binary_search_by_key(&key, |r| r.0).ok().map(|i| OneLine(self.rows[i].1))
    }
}

/// A header's text with its runs of white space as one space.
#[derive(Clone, Copy, Debug)]
pub struct OneLine<'a>(pub &'a str);

impl fmt::Display for OneLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for word in self.0.split_whitespace() {
            if !first {
                f.write_str(" ")?;
            }
            f.write_str(word)?;
            first = false;
        }
        Ok(())
    }
}

/// An impl named by its header (`impl Show for A`), or, with no header held, as not in either
/// table's texts.
struct Named<'h, 'a> {
    headers: &'h ImplHeaders<'a>,
    key: ItemKey,
}

impl fmt::Display for Named<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.headers.of_key(self.key) {
            Some(h) => write!(f, "`{h}`"),
            None => f.write_str("an impl whose header neither table holds"),
        }
    }
}

/// Where an impl is filed, in words: under its trait and self head, or why not.
struct FilingWords(Filing);

impl fmt::Display for FilingWords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Filing::At(_) => f.write_str("filed under its trait and self head"),
            Filing::Not(why) => write!(f, "not filed ({why:?})"),
        }
    }
}

/// Which part of two filings of one impl differ: the trait, the self head, or whether it is
/// filed at all.
fn filing_change(m: Filing, b: Filing) -> &'static str {
    match (m, b) {
        (Filing::At(m), Filing::At(b)) => {
            let (m, b) = (m.bytes(), b.bytes());
            let trait_differs = m.get(..33) != b.get(..33);
            let head_differs = m.get(33..) != b.get(33..);
            match (trait_differs, head_differs) {
                (true, true) => "its trait and its self head resolve otherwise",
                (true, false) => "its trait resolves otherwise",
                (false, _) => "its self head resolves otherwise",
            }
        }
        (Filing::At(_), Filing::Not(_)) | (Filing::Not(_), Filing::At(_)) => "filed under one strategy only",
        (Filing::Not(_), Filing::Not(_)) => "not filed under either, for different reasons",
    }
}

/// What the differential of one crate found ([`crate::crate_scope::block_scopes_differential`]).
#[derive(Debug)]
pub struct BlockScopesVerdict<'a> {
    pub krate: &'a str,
    /// The bodies the `Bodies` solve held, and how many of them `body_scope` solved.
    pub held: usize,
    pub solved: usize,
    /// The crate's modules the solved bodies' solves made outside their own blocks, summed
    /// (resolve.rs `BodySolved::made_outside`): "modules made for body solves". Each body's
    /// share is its chain and what it reads, not its crate's modules.
    pub modules_made: usize,
    /// Each held body `body_scope` refused, named.
    pub refused: AVec<'a, BodyScopeRefusal<'a>>,
    pub modules_rows: usize,
    pub bodies_rows: usize,
    pub diff: ImplDiff<'a>,
    /// Of `diff.only_modules`, the rows written inside a body `body_scope` refused: missing
    /// because of the refusal, not because the `Bodies` path lost a solved body's impl.
    pub only_modules_refused: usize,
    /// What the verdict names each impl by.
    pub headers: ImplHeaders<'a>,
}

impl BlockScopesVerdict<'_> {
    /// The two tables are equal and every held body was solved.
    pub fn agrees(&self) -> bool {
        self.diff.is_empty() && self.refused.is_empty()
    }
}

/// The most rows of each kind a verdict prints.
const LISTED: usize = 8;

impl fmt::Display for BlockScopesVerdict<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.diff;
        let h = &self.headers;
        write!(
            f,
            "block scopes differential: crate `{}`: {}: impls modules {} bodies {} equal {} only-modules {} (of refused bodies {}) only-bodies {} differ {}; bodies held {} solved {} refused {}; modules made for body solves: {}; bodies: {}",
            self.krate,
            if self.agrees() { "AGREE" } else { "DIFFER" },
            self.modules_rows,
            self.bodies_rows,
            d.equal,
            d.only_modules.len(),
            self.only_modules_refused,
            d.only_bodies.len(),
            d.differ.len(),
            self.held,
            self.solved,
            self.refused.len(),
            self.modules_made,
            self.solved
        )?;
        for r in d.only_modules.iter().take(LISTED) {
            write!(f, "\n  only under modules: {}: {}", Named { headers: h, key: r.key }, FilingWords(r.filing))?;
        }
        for r in d.only_bodies.iter().take(LISTED) {
            write!(f, "\n  only under bodies: {}: {}", Named { headers: h, key: r.key }, FilingWords(r.filing))?;
        }
        for (m, b) in d.differ.iter().take(LISTED) {
            let place = if m.module == b.module { "" } else { "; written in another module" };
            write!(f, "\n  differ: {}: {} (modules: {}; bodies: {}){place}", Named { headers: h, key: m.key }, filing_change(m.filing, b.filing), FilingWords(m.filing), FilingWords(b.filing))?;
        }
        for r in self.refused.iter().take(LISTED) {
            write!(f, "\n  refused: {r}")?;
        }
        Ok(())
    }
}

/// The differential's sums over a run's crates: the one count line `--block-scopes=bodies`
/// prints whatever its verdicts' lines are capped at (blaze-cli engine_check.rs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlocksTally {
    /// Crates compared, and of them those that differ.
    pub crates: u64,
    pub differ: u64,
    /// Crates whose differential did not run ([`DifferentialError`], or its frame refused).
    pub failed: u64,
    pub held: u64,
    pub solved: u64,
    pub refused: u64,
    pub modules_rows: u64,
    pub bodies_rows: u64,
    pub only_modules: u64,
    pub only_modules_refused: u64,
    pub only_bodies: u64,
    pub differ_rows: u64,
    /// "Modules made for body solves" ([`BlockScopesVerdict::modules_made`]), over `solved`
    /// bodies.
    pub modules_made: u64,
}

impl BlocksTally {
    /// One crate's verdict as a tally.
    pub fn of(v: &BlockScopesVerdict<'_>) -> BlocksTally {
        BlocksTally {
            crates: 1,
            differ: u64::from(!v.agrees()),
            failed: 0,
            held: v.held as u64,
            solved: v.solved as u64,
            refused: v.refused.len() as u64,
            modules_rows: v.modules_rows as u64,
            bodies_rows: v.bodies_rows as u64,
            only_modules: v.diff.only_modules.len() as u64,
            only_modules_refused: v.only_modules_refused as u64,
            only_bodies: v.diff.only_bodies.len() as u64,
            differ_rows: v.diff.differ.len() as u64,
            modules_made: v.modules_made as u64,
        }
    }

    /// One crate whose differential did not run.
    pub fn failed() -> BlocksTally {
        BlocksTally { failed: 1, ..BlocksTally::default() }
    }

    pub fn add(&mut self, o: BlocksTally) {
        self.crates += o.crates;
        self.differ += o.differ;
        self.failed += o.failed;
        self.held += o.held;
        self.solved += o.solved;
        self.refused += o.refused;
        self.modules_rows += o.modules_rows;
        self.bodies_rows += o.bodies_rows;
        self.only_modules += o.only_modules;
        self.only_modules_refused += o.only_modules_refused;
        self.only_bodies += o.only_bodies;
        self.differ_rows += o.differ_rows;
        self.modules_made += o.modules_made;
    }

    /// Whether every crate ran and agreed.
    pub fn agrees(&self) -> bool {
        self.differ == 0 && self.failed == 0
    }
}

impl fmt::Display for BlocksTally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block scopes differential: {} of {} crate(s) differ, {} did not run; bodies held {}, solved {}, refused {}; impls modules {} bodies {}, only-modules {} (of refused bodies {}), only-bodies {}, differ {}; modules made for body solves: {}; bodies: {}",
            self.differ,
            self.crates + self.failed,
            self.failed,
            self.held,
            self.solved,
            self.refused,
            self.modules_rows,
            self.bodies_rows,
            self.only_modules,
            self.only_modules_refused,
            self.only_bodies,
            self.differ_rows,
            self.modules_made,
            self.solved
        )
    }
}

/// Why a crate's differential did not run, naming the crate.
#[derive(Debug)]
pub enum DifferentialError<'a> {
    /// One of the two solves failed.
    Names(crate::crate_scope::CrateScopeError<'a>),
    /// An impl's row could not be made.
    Rows { krate: &'a str, error: ItemsRecordError },
}

impl fmt::Display for DifferentialError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DifferentialError::Names(e) => write!(f, "block scopes differential: a solve failed: {e:?}"),
            DifferentialError::Rows { krate, error } => write!(f, "block scopes differential: crate `{krate}`: an impl row: {error:?}"),
        }
    }
}
