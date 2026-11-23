//! THE HEAD FACTS OF ADTS THE BORROW CHECK TAKES, computed per strongly connected group (D190
//! point 2): the variance of every parameter (adt_variance.rs) and the outlives requirements,
//! declared and inferred (adt_outlives.rs, RFC 2093). An ADT's facts are a function of its own
//! head (its fields' types and region columns, its declared outlives bounds, its lang item)
//! and of the facts of the ADTs its fields name: a fixed point over each group of ADTs that
//! name each other, the groups solved in dependency order (Tarjan's order: every group after
//! the groups it names). The same table answers a fn's implied bounds (RFC 1214):
//! [`AdtFactsTable::implied`].
//!
//! THE KEY (rules 21, 65). Each ADT's input digest hashes what the computation reads of its
//! head: its key, lang item, parameter counts, each field's laid nodes and column, each
//! declared bound. A group's key hashes its members' input digests (in key order) and the
//! result digest of every ADT outside the group a member names. A member's result digest
//! hashes the group key, its key and its answers. An edit to an unrelated item moves no
//! digest; an edit to a field type of an ADT moves its input digest, so its group's key, so
//! every group naming it. The body reads each ADT the computation read through a `BorrowAdt`
//! row of detail [`ADT_FACTS_DETAIL`] answered by its input digest (borrows.rs
//! `gather_facts_planted`).
//!
//! The core is sans-IO: [`AdtFactsInput`] is filled by the driver (borrows.rs
//! `adt_facts_of`), then [`AdtFactsInput::solve`] makes the [`AdtFactsTable`].

use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use tc::StableHasher;
use tc::regions::{Arity, Region};

use super::adt_outlives::{Mode, OutlivesBuf, OutlivesRefusal, Required, Requirements, declared, requirements_of};
use super::adt_variance::{AdtLang, LaidError, LaidNode, Solving, VarianceOf, VarianceRefusal, constrain_field, lang_variances, lay_out};
use super::borrows_unmade::AdtUnmade;

/// The version of the facts' form and rules: in every domain below, so a change of either
/// moves every digest.
pub const ADT_FACTS_VERSION: u32 = 2;

/// The `detail` of a `BorrowAdt` read row that says an ADT's head was read by the group
/// computation (detail 0 is borrow_reads.rs `adt_read`'s).
pub const ADT_FACTS_DETAIL: u64 = 7;

/// The most ADTs one table is computed over (rule 18): more is a typed refusal of the rest.
pub const MOST_ADTS: usize = 1 << 14;

const INPUT_DOMAIN: &str = "blaze-ladder front2 adt facts input v2";
const GROUP_DOMAIN: &str = "blaze-ladder front2 adt facts group v2";
const RESULT_DOMAIN: &str = "blaze-ladder front2 adt facts result v2";

/// Why an ADT has no facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdtFactsRefusal {
    /// Its head was not made.
    Unmade(AdtUnmade),
    /// A field's region column, or its clauses' regions, were not read.
    Regions(tc::region_facts::RegionsUnread),
    /// A field's type could not be laid over its column.
    Laid(LaidError),
    /// A field's variance constraints could not be taken.
    Variance(VarianceRefusal),
    /// Its outlives requirements could not be taken.
    Outlives(OutlivesRefusal),
    /// Past [`MOST_ADTS`].
    TooMany,
}

/// One ADT as given to the computation.
#[derive(Clone, Copy, Debug)]
struct InRow {
    key: u128,
    lang: AdtLang,
    lifetimes: u32,
    generics: u32,
    /// Its fields in `fields`.
    fields: (u32, u32),
    input: StableHasher,
    /// Why it has no facts at all.
    refused: Option<AdtFactsRefusal>,
    /// Why it has no outlives requirements (a declared bound not read): its variances stand.
    orefused: Option<AdtFactsRefusal>,
}

/// One field: its laid nodes in `laid`, its column.
#[derive(Clone, Copy, Debug)]
struct FieldIn<'s> {
    laid: (u32, u32),
    col: &'s [Region],
}

/// One declared outlives bound of an ADT (`owner`), in the order given (`seq`).
#[derive(Clone, Copy, Debug)]
struct Decl<'s> {
    owner: u128,
    seq: u32,
    kind: DeclKind<'s>,
}

#[derive(Clone, Copy, Debug)]
enum DeclKind<'s> {
    /// `subject: region`, the subject laid in `laid` over `col`.
    Type { laid: (u32, u32), col: &'s [Region], region: Region },
    /// `'long: 'short`.
    Region { long: Region, short: Region },
}

/// What the computation is given, filled one ADT at a time: [`AdtFactsInput::adt`], then its
/// fields with [`AdtFactsInput::field`] (a lang ADT gets none) and its declared bounds with
/// [`AdtFactsInput::declared_type`] and [`AdtFactsInput::declared_region`]; or
/// [`AdtFactsInput::refused`].
pub struct AdtFactsInput<'s, 'a> {
    rows: AVec<'a, InRow>,
    fields: AVec<'a, FieldIn<'s>>,
    decls: AVec<'a, Decl<'s>>,
    laid: AVec<'a, LaidNode>,
    alloc: Alloc<'a>,
}

impl<'s, 'a> AdtFactsInput<'s, 'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        AdtFactsInput { rows: AVec::new_in(alloc), fields: AVec::new_in(alloc), decls: AVec::new_in(alloc), laid: AVec::new_in(alloc), alloc }
    }

    /// Begins ADT `key`: `lifetimes` lifetime parameters, `generics` others, `lang`.
    pub fn adt(&mut self, key: u128, lang: AdtLang, lifetimes: u32, generics: u32) {
        let mut h = StableHasher::new(INPUT_DOMAIN);
        h.u32(ADT_FACTS_VERSION);
        h.u128(key);
        h.tag(lang as u8);
        h.u32(lifetimes);
        h.u32(generics);
        let at = self.fields.len() as u32;
        let refused = (self.rows.len() >= MOST_ADTS).then_some(AdtFactsRefusal::TooMany);
        self.rows.push(InRow { key, lang, lifetimes, generics, fields: (at, at), input: h, refused, orefused: None });
    }

    /// ADT `key` has no facts, for `why`.
    pub fn refused(&mut self, key: u128, why: AdtFactsRefusal) {
        let mut h = StableHasher::new(INPUT_DOMAIN);
        h.u32(ADT_FACTS_VERSION);
        h.u128(key);
        h.tag(0xff);
        let at = self.fields.len() as u32;
        self.rows.push(InRow { key, lang: AdtLang::Plain, lifetimes: 0, generics: 0, fields: (at, at), input: h, refused: Some(why), orefused: None });
    }

    /// Lays `ty` over `col` for the ADT begun last, its nodes and column hashed into its input
    /// digest under `tag`: the laid range, or why it could not be laid. `None`: no ADT begun.
    fn lay<F: Arity + ?Sized>(&mut self, tys: tc::PTys<'_>, arity: &F, ty: tc::PTy, col: Result<&'s [Region], tc::region_facts::RegionsUnread>, tag: u8) -> Option<Result<((u32, u32), &'s [Region]), AdtFactsRefusal>> {
        let row = self.rows.last_mut()?;
        let col = match col {
            Ok(c) => c,
            Err(u) => return Some(Err(AdtFactsRefusal::Regions(u))),
        };
        let lo = self.laid.len();
        let len = u32::try_from(col.len()).unwrap_or(u32::MAX);
        if let Err(e) = lay_out(tys, ty, arity, len, &mut self.laid, self.alloc) {
            self.laid.truncate(lo);
            return Some(Err(AdtFactsRefusal::Laid(e)));
        }
        let h = &mut row.input;
        h.tag(tag);
        for n in self.laid[lo..].iter() {
            hash_node(h, n);
        }
        for &r in col.iter() {
            hash_region(h, r);
        }
        Some(Ok(((lo as u32, self.laid.len() as u32), col)))
    }

    /// Whether the ADT begun last is refused (`outlives`: or its requirements are).
    fn last_refused(&self, outlives: bool) -> bool {
        match self.rows.last() {
            None => true,
            Some(row) => row.refused.is_some() || (outlives && row.orefused.is_some()),
        }
    }

    /// The next field of the ADT begun last: its type `ty` and column `col` (or why the column
    /// was not read), laid by the one region counter's `arity`.
    pub fn field<F: Arity + ?Sized>(&mut self, tys: tc::PTys<'_>, arity: &F, ty: tc::PTy, col: Result<&'s [Region], tc::region_facts::RegionsUnread>) {
        if self.last_refused(false) {
            return;
        }
        let Some(laid) = self.lay(tys, arity, ty, col, 0xfe) else { return };
        let fields = &mut self.fields;
        let Some(row) = self.rows.last_mut() else { return };
        match laid {
            Ok((laid, col)) => {
                fields.push(FieldIn { laid, col });
                row.fields.1 = fields.len() as u32;
            }
            Err(why) => row.refused = Some(why),
        }
    }

    /// A declared `subject: region` of the ADT begun last (its head's clause, `Outlives::Type`),
    /// the subject's type `ty` and column `col` as its clause column lays them. One that cannot
    /// be laid refuses the ADT's requirements, not its variances.
    pub fn declared_type<F: Arity + ?Sized>(&mut self, tys: tc::PTys<'_>, arity: &F, ty: tc::PTy, col: Result<&'s [Region], tc::region_facts::RegionsUnread>, region: Region) {
        if self.last_refused(true) {
            return;
        }
        let Some(laid) = self.lay(tys, arity, ty, col, 0xfd) else { return };
        let decls = &mut self.decls;
        let Some(row) = self.rows.last_mut() else { return };
        match laid {
            Ok((laid, col)) => {
                hash_region(&mut row.input, region);
                let seq = decls.len() as u32;
                decls.push(Decl { owner: row.key, seq, kind: DeclKind::Type { laid, col, region } });
            }
            Err(why) => row.orefused = Some(why),
        }
    }

    /// The ADT begun last has no requirements, for `why` (a declared bound not read).
    pub fn outlives_refused(&mut self, why: AdtFactsRefusal) {
        if let Some(row) = self.rows.last_mut()
            && row.orefused.is_none()
        {
            row.input.tag(0xfb);
            row.orefused = Some(why);
        }
    }

    /// A declared `'long: 'short` of the ADT begun last.
    pub fn declared_region(&mut self, long: Region, short: Region) {
        if self.last_refused(true) {
            return;
        }
        let Some(row) = self.rows.last_mut() else { return };
        row.input.tag(0xfc);
        hash_region(&mut row.input, long);
        hash_region(&mut row.input, short);
        let owner = row.key;
        let seq = self.decls.len() as u32;
        self.decls.push(Decl { owner, seq, kind: DeclKind::Region { long, short } });
    }

    /// The facts of every ADT given.
    pub fn solve(mut self) -> AdtFactsTable<'a> {
        let alloc = self.alloc;
        self.rows.sort_unstable_by_key(|r| r.key);
        self.rows.dedup_by_key(|r| r.key);
        self.decls.sort_unstable_by_key(|d| (d.owner, d.seq));
        let n = self.rows.len();
        // Each ADT's edges: the given ADTs its fields name, sorted, no repeats.
        let mut edge_at: AVec<'a, (u32, u32)> = AVec::with_capacity_in(n, alloc);
        let mut edges: AVec<'a, u32> = AVec::new_in(alloc);
        for row in self.rows.iter() {
            let lo = edges.len();
            for f in self.fields.get(row.fields.0 as usize..row.fields.1 as usize).unwrap_or(&[]) {
                for l in self.laid.get(f.laid.0 as usize..f.laid.1 as usize).unwrap_or(&[]) {
                    if let tc::PNode::Adt(k, _) = l.node
                        && let Ok(at) = self.rows.binary_search_by_key(&k.0, |r| r.key)
                    {
                        edges.push(at as u32);
                    }
                }
            }
            edges[lo..].sort_unstable();
            let mut w = lo;
            for i in lo..edges.len() {
                if i == lo || edges[i] != edges[w - 1] {
                    edges[w] = edges[i];
                    w += 1;
                }
            }
            edges.truncate(w);
            edge_at.push((lo as u32, edges.len() as u32));
        }
        let mut table = AdtFactsTable {
            rows: AVec::with_capacity_in(n, alloc),
            variances: AVec::new_in(alloc),
            params: AVec::new_in(alloc),
            regions: AVec::new_in(alloc),
        };
        for row in self.rows.iter() {
            let input = row.input.finish();
            table.rows.push(FactsRow { key: row.key, input, state: RowState::Pending, at: (0, 0), ostate: RowState::Pending, oat: ((0, 0), (0, 0)), result: 0 });
        }
        let groups = tarjan(&edge_at, &edges, alloc);
        let mut ctx: AVec<'a, Solving> = AVec::new_in(alloc);
        let mut next: AVec<'a, Solving> = AVec::new_in(alloc);
        for g in groups.groups.iter() {
            let members = &groups.members[g.0 as usize..g.1 as usize];
            let group_key = self.group_key(members, &edge_at, &edges, &table);
            self.solve_variances(members, &mut table, &mut ctx, &mut next);
            self.solve_outlives(members, &mut table);
            for &m in members.iter() {
                table.rows[m as usize].result = table.result_digest(m as usize, group_key);
            }
        }
        table
    }

    /// The group's key: its members' inputs, and every outside ADT a member names.
    fn group_key(&self, members: &[u32], edge_at: &[(u32, u32)], edges: &[u32], table: &AdtFactsTable<'a>) -> u64 {
        let mut key = StableHasher::new(GROUP_DOMAIN);
        key.u32(ADT_FACTS_VERSION);
        for &m in members.iter() {
            key.u128(table.rows[m as usize].key);
            key.u64(table.rows[m as usize].input);
        }
        for &m in members.iter() {
            let (lo, hi) = edge_at[m as usize];
            for &e in edges[lo as usize..hi as usize].iter() {
                if members.binary_search(&e).is_err() {
                    let o = &table.rows[e as usize];
                    key.u128(o.key);
                    key.u64(o.result);
                }
            }
        }
        key.finish()
    }

    /// The variances of a group: the fixed point from all-bivariant down (Jacobi rounds: each
    /// round reads the last round's answers). A member whose constraints cannot be taken is
    /// refused and the rounds start again without it: at most one restart per member.
    fn solve_variances(&self, members: &[u32], table: &mut AdtFactsTable<'a>, ctx: &mut AVec<'a, Solving>, next: &mut AVec<'a, Solving>) {
        for &m in members.iter() {
            let row = &self.rows[m as usize];
            let lo = table.variances.len() as u32;
            let slots = row.lifetimes.saturating_add(row.generics);
            table.variances.resize(table.variances.len() + slots as usize, Solving::Bi);
            let t = &mut table.rows[m as usize];
            t.at = (lo, lo + slots);
            t.state = match row.refused {
                Some(why) => RowState::Refused(why),
                None => RowState::Solving,
            };
        }
        let params: u32 = members.iter().map(|&m| table.rows[m as usize].at.1 - table.rows[m as usize].at.0).sum();
        let most_rounds = params.saturating_mul(2).saturating_add(2);
        'restart: for _ in 0..=members.len() {
            for &m in members.iter() {
                let t = table.rows[m as usize];
                if t.state == RowState::Solving {
                    for v in table.variances[t.at.0 as usize..t.at.1 as usize].iter_mut() {
                        *v = Solving::Bi;
                    }
                }
            }
            let mut rounds = 0;
            loop {
                if rounds >= most_rounds {
                    for &m in members.iter() {
                        if table.rows[m as usize].state == RowState::Solving {
                            table.rows[m as usize].state = RowState::Refused(AdtFactsRefusal::Variance(VarianceRefusal::Rounds { rounds }));
                        }
                    }
                    break 'restart;
                }
                rounds += 1;
                next.clear();
                let mut failed = None;
                for &m in members.iter() {
                    let t = table.rows[m as usize];
                    if t.state != RowState::Solving {
                        continue;
                    }
                    let row = &self.rows[m as usize];
                    let from = next.len();
                    next.resize(from + (t.at.1 - t.at.0) as usize, Solving::Bi);
                    match lang_variances(row.lang, row.lifetimes, row.generics) {
                        Err(why) => {
                            failed = Some((m, why));
                            break;
                        }
                        Ok(Some(fixed)) => {
                            for v in next[from..].iter_mut() {
                                *v = fixed;
                            }
                            continue;
                        }
                        Ok(None) => {}
                    }
                    let look = Look { rows: &self.rows, table: &*table };
                    for f in self.fields.get(row.fields.0 as usize..row.fields.1 as usize).unwrap_or(&[]) {
                        let laid = self.laid.get(f.laid.0 as usize..f.laid.1 as usize).unwrap_or(&[]);
                        if let Err(why) = constrain_field(laid, f.col, row.lifetimes, row.generics, &look, ctx, &mut next[from..]) {
                            failed = Some((m, why));
                            break;
                        }
                    }
                    if failed.is_some() {
                        break;
                    }
                }
                if let Some((m, why)) = failed {
                    table.rows[m as usize].state = RowState::Refused(AdtFactsRefusal::Variance(why));
                    continue 'restart;
                }
                // Write the round back; settled when nothing moved.
                let mut moved = false;
                let mut from = 0usize;
                for &m in members.iter() {
                    let t = table.rows[m as usize];
                    if t.state != RowState::Solving {
                        continue;
                    }
                    let len = (t.at.1 - t.at.0) as usize;
                    let now = &mut table.variances[t.at.0 as usize..t.at.1 as usize];
                    if *now != next[from..from + len] {
                        moved = true;
                        now.copy_from_slice(&next[from..from + len]);
                    }
                    from += len;
                }
                if !moved {
                    break 'restart;
                }
            }
        }
        for &m in members.iter() {
            let t = &mut table.rows[m as usize];
            t.state = match t.state {
                RowState::Solving | RowState::Made => RowState::Made,
                RowState::Refused(why) => RowState::Refused(why),
                // Every member was Solving or Refused here: a bug of the driver, refused by
                // name rather than answered.
                RowState::Pending => RowState::Refused(AdtFactsRefusal::Variance(VarianceRefusal::Rounds { rounds: 0 })),
            };
        }
    }

    /// The outlives requirements of a group (declared and inferred): the fixed point from the
    /// empty set up (Jacobi rounds; the sets only grow), a member that cannot be read refused
    /// and the rounds started again without it.
    fn solve_outlives(&self, members: &[u32], table: &mut AdtFactsTable<'a>) {
        let alloc = self.alloc;
        let mut old: AVec<'a, OutlivesBuf<'a>> = AVec::with_capacity_in(members.len(), alloc);
        let mut new: AVec<'a, OutlivesBuf<'a>> = AVec::with_capacity_in(members.len(), alloc);
        let mut most_rounds = 2u32;
        for &m in members.iter() {
            old.push(OutlivesBuf::new_in(alloc));
            new.push(OutlivesBuf::new_in(alloc));
            let row = &self.rows[m as usize];
            // Each round adds at least one requirement, of at most this many.
            let regions = row.lifetimes.saturating_add(1);
            most_rounds = most_rounds.saturating_add(row.lifetimes.saturating_add(row.generics).saturating_mul(regions));
            table.rows[m as usize].ostate = match row.refused.or(row.orefused) {
                Some(why) => RowState::Refused(why),
                None => RowState::Solving,
            };
        }
        'restart: for _ in 0..=members.len() {
            for b in old.iter_mut() {
                b.clear();
            }
            let mut rounds = 0;
            loop {
                if rounds >= most_rounds {
                    for &m in members.iter() {
                        if table.rows[m as usize].ostate == RowState::Solving {
                            table.rows[m as usize].ostate = RowState::Refused(AdtFactsRefusal::Outlives(OutlivesRefusal::Rounds { rounds }));
                        }
                    }
                    break 'restart;
                }
                rounds += 1;
                let mut failed = None;
                {
                    let look = OLook { rows: &self.rows, table: &*table, members, old: &old };
                    for (j, &m) in members.iter().enumerate() {
                        let out = &mut new[j];
                        out.clear();
                        if table.rows[m as usize].ostate != RowState::Solving {
                            continue;
                        }
                        let row = &self.rows[m as usize];
                        for d in self.decls_of(row.key).iter() {
                            match d.kind {
                                DeclKind::Type { laid, col, region } => {
                                    declared(self.laid.get(laid.0 as usize..laid.1 as usize).unwrap_or(&[]), col, region, out);
                                }
                                DeclKind::Region { long, short } => out.region(long, short),
                            }
                        }
                        for f in self.fields.get(row.fields.0 as usize..row.fields.1 as usize).unwrap_or(&[]) {
                            let laid = self.laid.get(f.laid.0 as usize..f.laid.1 as usize).unwrap_or(&[]);
                            if let Err(why) = requirements_of(laid, f.col, Mode::Adt, &look, out) {
                                failed = Some((m, why));
                                break;
                            }
                        }
                        if failed.is_some() {
                            break;
                        }
                    }
                }
                if let Some((m, why)) = failed {
                    table.rows[m as usize].ostate = RowState::Refused(AdtFactsRefusal::Outlives(why));
                    continue 'restart;
                }
                let moved = old.iter().zip(new.iter()).any(|(o, n)| o.params.as_slice() != n.params.as_slice() || o.regions.as_slice() != n.regions.as_slice());
                core::mem::swap(&mut old, &mut new);
                if !moved {
                    break 'restart;
                }
            }
        }
        for (j, &m) in members.iter().enumerate() {
            let t = &mut table.rows[m as usize];
            match t.ostate {
                RowState::Solving | RowState::Made => {
                    let p = table.params.len() as u32;
                    table.params.extend_from_slice(&old[j].params);
                    let r = table.regions.len() as u32;
                    table.regions.extend_from_slice(&old[j].regions);
                    t.oat = ((p, table.params.len() as u32), (r, table.regions.len() as u32));
                    t.ostate = RowState::Made;
                }
                RowState::Refused(_) => {}
                RowState::Pending => t.ostate = RowState::Refused(AdtFactsRefusal::Outlives(OutlivesRefusal::Rounds { rounds: 0 })),
            }
        }
    }

    /// The declared bounds of ADT `key`, in the order given.
    fn decls_of(&self, key: u128) -> &[Decl<'s>] {
        let lo = self.decls.partition_point(|d| d.owner < key);
        let hi = self.decls.partition_point(|d| d.owner <= key);
        self.decls.get(lo..hi).unwrap_or(&[])
    }
}

/// The fixed point's view of the variances: a member's as the last round left them, an
/// outside ADT's as its group settled them.
struct Look<'t, 'a> {
    rows: &'t [InRow],
    table: &'t AdtFactsTable<'a>,
}

impl VarianceOf for Look<'_, '_> {
    fn variance(&self, adt: u128, index: u32) -> Result<Solving, VarianceRefusal> {
        let unsolved = VarianceRefusal::Unsolved { adt, index };
        let at = self.rows.binary_search_by_key(&adt, |r| r.key).map_err(|_| unsolved)?;
        let t = self.table.rows.get(at).ok_or(unsolved)?;
        match t.state {
            RowState::Made | RowState::Solving => {
                if index >= t.at.1 - t.at.0 {
                    return Err(unsolved);
                }
                self.table.variances.get((t.at.0 + index) as usize).copied().ok_or(unsolved)
            }
            RowState::Pending | RowState::Refused(_) => Err(unsolved),
        }
    }
}

/// The fixed point's view of the requirements: a member's as the last round left them
/// (`old`, by its place in `members`), an outside ADT's as its group settled them.
struct OLook<'t, 'a> {
    rows: &'t [InRow],
    table: &'t AdtFactsTable<'a>,
    members: &'t [u32],
    old: &'t [OutlivesBuf<'a>],
}

impl Required for OLook<'_, '_> {
    fn required(&self, adt: u128) -> Result<Requirements<'_>, OutlivesRefusal> {
        let unsolved = OutlivesRefusal::Unsolved { adt };
        let at = self.rows.binary_search_by_key(&adt, |r| r.key).map_err(|_| unsolved)?;
        match self.members.binary_search(&(at as u32)) {
            Ok(j) => {
                let t = self.table.rows.get(at).ok_or(unsolved)?;
                let b = self.old.get(j).ok_or(unsolved)?;
                match t.ostate {
                    RowState::Solving => Ok(Requirements { params: &b.params, regions: &b.regions }),
                    RowState::Made | RowState::Pending | RowState::Refused(_) => Err(unsolved),
                }
            }
            Err(_) => self.table.required(adt),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowState {
    Pending,
    Solving,
    Made,
    Refused(AdtFactsRefusal),
}

#[derive(Clone, Copy, Debug)]
struct FactsRow {
    key: u128,
    /// The input digest: what was read of the ADT's head.
    input: u64,
    /// Its variances, in the table's list at `at`.
    state: RowState,
    at: (u32, u32),
    /// Its requirements, in the table's `params` and `regions` at `oat`.
    ostate: RowState,
    oat: ((u32, u32), (u32, u32)),
    /// The result digest: its group's key and its answers.
    result: u64,
}

/// The facts of a set of ADTs, by checker key: a record of the computation, read in place.
pub struct AdtFactsTable<'a> {
    rows: AVec<'a, FactsRow>,
    variances: AVec<'a, Solving>,
    params: AVec<'a, (u32, Region)>,
    regions: AVec<'a, (Region, Region)>,
}

impl<'a> AdtFactsTable<'a> {
    fn row(&self, adt: u128) -> Option<&FactsRow> {
        let at = self.rows.binary_search_by_key(&adt, |r| r.key).ok()?;
        self.rows.get(at)
    }

    /// The variance of each of `adt`'s parameters, lifetimes first then its other generics in
    /// order; `None` where its facts were refused or not given.
    pub fn variances(&self, adt: u128) -> Option<&[Solving]> {
        let r = self.row(adt)?;
        match r.state {
            RowState::Made => self.variances.get(r.at.0 as usize..r.at.1 as usize),
            RowState::Pending | RowState::Solving | RowState::Refused(_) => None,
        }
    }

    /// `adt`'s outlives requirements, declared and inferred, in its own numbering; `None`
    /// where they were refused or not given.
    pub fn outlives(&self, adt: u128) -> Option<Requirements<'_>> {
        let r = self.row(adt)?;
        match r.ostate {
            RowState::Made => {
                let ((p0, p1), (r0, r1)) = r.oat;
                Some(Requirements { params: self.params.get(p0 as usize..p1 as usize)?, regions: self.regions.get(r0 as usize..r1 as usize)? })
            }
            RowState::Pending | RowState::Solving | RowState::Refused(_) => None,
        }
    }

    /// Why `adt` has no variances, then why it has no requirements (`None`: it has them, or
    /// was not given).
    pub fn refused(&self, adt: u128) -> (Option<AdtFactsRefusal>, Option<AdtFactsRefusal>) {
        let Some(r) = self.row(adt) else { return (None, None) };
        let why = |s: RowState| match s {
            RowState::Refused(why) => Some(why),
            RowState::Pending | RowState::Solving | RowState::Made => None,
        };
        (why(r.state), why(r.ostate))
    }

    /// The result digest of `adt`'s facts.
    pub fn digest(&self, adt: u128) -> Option<u64> {
        self.row(adt).map(|r| r.result)
    }

    /// Every ADT the computation read, by key, and its input digest: one `BorrowAdt` row of
    /// detail [`ADT_FACTS_DETAIL`] each (rule 21).
    pub fn reads(&self) -> impl Iterator<Item = (u128, u64)> + '_ {
        self.rows.iter().map(|r| (r.key, r.input))
    }

    /// The bounds a signature's or an impl header's type implies (RFC 1214), laid as `laid`
    /// over `col` in the fn's numbering: every reference's and every ADT's requirements
    /// anywhere in it, `'static` included, into `out`. An ADT it names whose requirements are
    /// not known refuses the whole answer, by name.
    pub fn implied(&self, laid: &[LaidNode], col: &[Region], out: &mut OutlivesBuf<'_>) -> Result<(), OutlivesRefusal> {
        requirements_of(laid, col, Mode::Fn, self, out)
    }

    fn result_digest(&self, at: usize, group_key: u64) -> u64 {
        let Some(t) = self.rows.get(at) else { return 0 };
        let mut h = StableHasher::new(RESULT_DOMAIN);
        h.u32(ADT_FACTS_VERSION);
        h.u64(group_key);
        h.u128(t.key);
        match self.variances(t.key) {
            Some(vs) => {
                h.tag(1);
                for v in vs.iter() {
                    h.tag(v.tag());
                }
            }
            None => h.tag(0),
        }
        match self.outlives(t.key) {
            Some(req) => {
                h.tag(1);
                h.u32(req.params.len() as u32);
                for &(p, r) in req.params.iter() {
                    h.u32(p);
                    hash_region(&mut h, r);
                }
                h.u32(req.regions.len() as u32);
                for &(l, s) in req.regions.iter() {
                    hash_region(&mut h, l);
                    hash_region(&mut h, s);
                }
            }
            None => h.tag(0),
        }
        h.finish()
    }
}

impl Required for AdtFactsTable<'_> {
    fn required(&self, adt: u128) -> Result<Requirements<'_>, OutlivesRefusal> {
        self.outlives(adt).ok_or(OutlivesRefusal::Unsolved { adt })
    }
}

/// Why a fn's implied bounds are not known: none are then assumed beyond its declared ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImpliedRefusal {
    /// A signature's or an impl header's column was not read.
    Regions(tc::region_facts::RegionsUnread),
    /// Its type could not be laid over its column.
    Laid(LaidError),
    /// An ADT it names has no requirements known, or one could not be instantiated.
    Outlives(OutlivesRefusal),
}

/// The types a fn assumes well-formed (RFC 1214): its signature's (inputs and output, laid
/// over `FnPtr(inputs.., output)`) and, for a member of an impl, the impl header's (the self
/// type, the trait reference), each laid over its column in the fn's numbering. Filled before
/// the ADT facts are computed ([`ImpliedInput::adts`] names the ADTs they need), then answered
/// by [`ImpliedInput::solve`].
pub struct ImpliedInput<'a> {
    laid: AVec<'a, LaidNode>,
    cols: AVec<'a, Region>,
    /// Each piece: its range in `laid`, its range in `cols`.
    pieces: AVec<'a, ((u32, u32), (u32, u32))>,
    refused: Option<ImpliedRefusal>,
    /// Per item a piece was read of (the fn, its impl), in the order first given: what was
    /// read of it ([`ImpliedInput::reads`]).
    whos: AVec<'a, (u128, StableHasher)>,
    alloc: Alloc<'a>,
}

/// The `detail` of the `BorrowHead` row of what the implied bounds read of an item (its
/// signature's or impl header's types over their columns): [`ImpliedInput::reads`].
pub const IMPLIED_DETAIL: u64 = 8;

const IMPLIED_DOMAIN: &str = "blaze-ladder front2 implied v1";

impl<'a> ImpliedInput<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        ImpliedInput { laid: AVec::new_in(alloc), cols: AVec::new_in(alloc), pieces: AVec::new_in(alloc), refused: None, whos: AVec::new_in(alloc), alloc }
    }

    /// A type the fn assumes well-formed whose column was not read, of item `who`: the
    /// answer's refusal.
    pub fn unread(&mut self, who: u128, u: tc::region_facts::RegionsUnread) {
        let at = self.who(who);
        let h = &mut self.whos[at].1;
        h.tag(0xfe);
        h.u128(u.item.0);
        if self.refused.is_none() {
            self.refused = Some(ImpliedRefusal::Regions(u));
        }
    }

    /// The place of `who` in `whos`, added on first sight.
    fn who(&mut self, who: u128) -> usize {
        match self.whos.iter().position(|w| w.0 == who) {
            Some(at) => at,
            None => {
                let mut h = StableHasher::new(IMPLIED_DOMAIN);
                h.u32(ADT_FACTS_VERSION);
                h.u128(who);
                self.whos.push((who, h));
                self.whos.len() - 1
            }
        }
    }

    /// One type the fn assumes well-formed, read of item `who`: `ty` over `col`.
    pub fn piece<F: Arity + ?Sized>(&mut self, who: u128, tys: tc::PTys<'_>, arity: &F, ty: tc::PTy, col: &[Region]) {
        let at = self.who(who);
        let h = &mut self.whos[at].1;
        if self.refused.is_some() {
            h.tag(0xff);
            return;
        }
        let lo = self.laid.len();
        let len = u32::try_from(col.len()).unwrap_or(u32::MAX);
        if let Err(e) = lay_out(tys, ty, arity, len, &mut self.laid, self.alloc) {
            self.laid.truncate(lo);
            h.tag(0xfd);
            self.refused = Some(ImpliedRefusal::Laid(e));
            return;
        }
        h.tag(1);
        h.u32((self.laid.len() - lo) as u32);
        for n in self.laid[lo..].iter() {
            hash_node(h, n);
        }
        h.u32(col.len() as u32);
        for &r in col.iter() {
            hash_region(h, r);
        }
        let c0 = self.cols.len();
        self.cols.extend_from_slice(col);
        self.pieces.push(((lo as u32, self.laid.len() as u32), (c0 as u32, self.cols.len() as u32)));
    }

    /// Each item a piece was read of, and the digest of what was read: one `BorrowHead` row
    /// of detail [`IMPLIED_DETAIL`] each (rule 21). The ADTs' requirements are the table's
    /// rows ([`AdtFactsTable::reads`]).
    pub fn reads(&self) -> impl Iterator<Item = (u128, u64)> + '_ {
        self.whos.iter().map(|&(k, h)| (k, h.finish()))
    }

    /// The ADTs the pieces name: the roots the ADT facts must be computed for.
    pub fn adts(&self) -> impl Iterator<Item = u128> + '_ {
        self.laid.iter().filter_map(|n| match n.node {
            tc::PNode::Adt(k, _) => Some(k.0),
            _ => None,
        })
    }

    /// The bounds the pieces imply, by `table`'s requirements, into `out` (cleared first).
    pub fn solve(&self, table: &AdtFactsTable<'_>, out: &mut OutlivesBuf<'_>) -> Result<(), ImpliedRefusal> {
        out.clear();
        if let Some(why) = self.refused {
            return Err(why);
        }
        for &((l0, l1), (c0, c1)) in self.pieces.iter() {
            let laid = self.laid.get(l0 as usize..l1 as usize).unwrap_or(&[]);
            let col = self.cols.get(c0 as usize..c1 as usize).unwrap_or(&[]);
            table.implied(laid, col, out).map_err(ImpliedRefusal::Outlives)?;
        }
        Ok(())
    }
}

/// Tarjan's strongly connected components over `n` nodes: each group's members (sorted) in
/// `members`, the groups in the order Tarjan emits them, every group after the groups it
/// reaches.
struct Groups<'a> {
    members: AVec<'a, u32>,
    groups: AVec<'a, (u32, u32)>,
}

const UNSEEN: u32 = u32::MAX;

fn tarjan<'a>(edge_at: &[(u32, u32)], edges: &[u32], alloc: Alloc<'a>) -> Groups<'a> {
    let n = edge_at.len();
    let mut index: AVec<'a, u32> = AVec::from_elem_in(UNSEEN, n, alloc);
    let mut low: AVec<'a, u32> = AVec::from_elem_in(0, n, alloc);
    let mut on: AVec<'a, bool> = AVec::from_elem_in(false, n, alloc);
    let mut stack: AVec<'a, u32> = AVec::new_in(alloc);
    let mut calls: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
    let mut out = Groups { members: AVec::new_in(alloc), groups: AVec::new_in(alloc) };
    let mut next = 0u32;
    for root in 0..n as u32 {
        if index[root as usize] != UNSEEN {
            continue;
        }
        calls.push((root, edge_at[root as usize].0));
        index[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on[root as usize] = true;
        while let Some(&(v, pos)) = calls.last() {
            let hi = edge_at[v as usize].1;
            if pos < hi {
                let w = edges[pos as usize];
                if let Some(top) = calls.last_mut() {
                    top.1 = pos + 1;
                }
                if index[w as usize] == UNSEEN {
                    index[w as usize] = next;
                    low[w as usize] = next;
                    next += 1;
                    stack.push(w);
                    on[w as usize] = true;
                    calls.push((w, edge_at[w as usize].0));
                } else if on[w as usize] {
                    low[v as usize] = low[v as usize].min(index[w as usize]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(p, _)) = calls.last() {
                low[p as usize] = low[p as usize].min(low[v as usize]);
            }
            if low[v as usize] == index[v as usize] {
                let lo = out.members.len();
                while let Some(w) = stack.pop() {
                    on[w as usize] = false;
                    out.members.push(w);
                    if w == v {
                        break;
                    }
                }
                out.members[lo..].sort_unstable();
                out.groups.push((lo as u32, out.members.len() as u32));
            }
        }
    }
    out
}

/// What the computation reads of one laid node.
fn hash_node(h: &mut StableHasher, n: &LaidNode) {
    use tc::PNode as N;
    h.u32(n.own);
    h.u32(n.kids);
    h.u32(n.nth);
    match n.node {
        N::Ref(m, _) => {
            h.tag(1);
            h.tag(m as u8);
        }
        N::Ptr(m, _) => {
            h.tag(2);
            h.tag(m as u8);
        }
        N::Adt(k, _) => {
            h.tag(3);
            h.u128(k.0);
        }
        N::Param(p) => {
            h.tag(4);
            h.u32(p);
        }
        N::ConstParam(p) => {
            h.tag(5);
            h.u32(p);
        }
        N::FnPtr(..) => h.tag(6),
        N::Dynamic(_) => h.tag(7),
        N::Slice(_) | N::Array(..) | N::Pat(..) | N::Tuple(_) => h.tag(8),
        N::Alias { trait_, name, .. } | N::ExProj { trait_, name, .. } => {
            h.tag(9);
            h.u128(trait_.0);
            h.u64(name.0);
        }
        N::Opaque(k, _) | N::FnDef(k, _) | N::ExTrait(k, _) | N::ExAuto(k) => {
            h.tag(10);
            h.u128(k.0);
        }
        // An extern type, by its item: a tag no earlier node had, so no digest made before it
        // moves.
        N::Foreign(k) => {
            h.tag(12);
            h.u128(k.0);
        }
        N::Record(s, _) => {
            h.tag(13);
            h.u64(s.0);
        }
        N::Union(_) => h.tag(14),
        N::Bool
        | N::Char
        | N::Int(_)
        | N::Uint(_)
        | N::Float(_)
        | N::Str
        | N::Never
        | N::Error
        | N::ConstValue { .. }
        | N::Infer
        | N::Closure { .. }
        | N::Future(..)
        | N::Coroutine { .. } => h.tag(11),
    }
}

fn hash_region(h: &mut StableHasher, r: Region) {
    match r {
        Region::Static => h.tag(0),
        Region::Param(i) => {
            h.tag(1);
            h.u32(i);
        }
        Region::Late { binder, var } => {
            h.tag(2);
            h.u32(binder);
            h.u32(var);
        }
    }
}
