//! The move and borrow check's reads, filed in the body's read list (front2 reads.rs) beside
//! the type check's (rule 21, keys cover every input).
//!
//! The borrow verdict of a body reads facts the type check's list may not hold: whether each
//! ADT the body's types name is `Copy` and implements `Drop`, its fields and whether its head
//! declares lifetimes, and each callee's (and the owner's) signature regions as its head
//! record holds them (D148), whether it writes a lifetime, whether it takes `&self`. The type check asks none of that for a body that only moves a value: an ADT that
//! gains `impl Copy` (or a callee whose head gains `<'a>`) left the body's list, and so its
//! digest, unchanged, while its borrow notes changed. Each such fact is now a row:
//!
//! - [`FactKind::BorrowAdt`] about the ADT: its answer the digest of what the borrow check took
//!   (made or not; `Copy` and `Drop` as answered, unknown included; the flags; each variant's
//!   field names in order);
//! - [`FactKind::BorrowHead`] about the fn: its answer the digest of the head as the borrow check
//!   read it (made or not; the two callee flags; the head record's signature column, or why it
//!   was not read: D148).
//!
//! The answer is a digest of the borrow check's OWN derived facts, not of the text they came
//! from, so an edit inside a callee's body (which its item text holds) moves no caller's list.
//! These rows are made where the facts are gathered (borrows.rs `gather_facts`: one home) and
//! filed by [`file_borrow_reads`] into the verdict of [`super::project::ProjectWorld::check_borrows`];
//! a body whose types were not checked clean was not borrow checked and lists none.

use blaze_flow_lower::{AdtFacts, BodyOut, CalleeFacts, FieldLead, FieldRegion, Region, RegionsUnread, SigLife, UnreadWhy, Variance};
use blaze_own::Alloc;
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use super::project::BodyVerdict;
use super::reads::{Fact, FactKind, Read, ReadsError};

/// The domain of a [`FactKind::BorrowAdt`] answer.
/// v3 (lane B11): an ADT's row also holds its fields' regions, its `Copy` impl's needs, and what
/// reading its fields' written types asked (ADT lifetime counts, path answers).
/// v4 (lane B13): and its lifetime parameters' variances (`Facts::adt_variances`).
/// v5 (lane B15): a fn-pointer field now has positions, and a lifetime its binder binds is a
/// field region of its own (`FieldRegion::Bound`, tag 3); its variances read the pointer.
/// v6 (D148, piece L6): the fields' regions are the head record's field columns; no written
/// type is read, so the asked ADT counts and path answers are no longer in the row.
/// v7: structural `needs_drop` and `drop_args` (borrow_fields.rs `drop_facts_of`).
/// v8: `field_row` lays `PNode::Alias` fields (`CharSliceSearcher`, `CharArrayRefSearcher`).
/// v9: `drop_strict` (RFC 1327 `#[may_dangle]` / transitive strict drop args, `drop_facts_of`).
const ADT_DOMAIN: &str = "blaze-ladder front2 borrow adt read v9";
/// The domain of a [`FactKind::BorrowHead`] answer.
/// v2 (piece RB8): a head's verdict now also reads its trait bounds' lifetime arguments
/// (borrows_ties.rs), so a list filed under v1 never answers for one checked since.
/// v3 (lane B11): the head's text was read through the front end's world (deleted by L6):
/// each path answer it took was in the row.
/// v4 (lane B15): a fn-pointer type in the head now has positions, and a lifetime its binder
/// binds is read as `Region::Bound` (tag 5), so the regions of such a head changed.
/// v5 (D148, piece LJ2): a head's regions are its head record's column (blaze-typeck
/// `Region::{Static, Param, Late}`, each child's slots, the params' late flags and bounds), or
/// the typed `RegionsUnread`; no text is read, so no ADT or path answer is in the row.
const HEAD_DOMAIN: &str = "blaze-ladder front2 borrow head read v5";

/// What the borrow check took about one ADT.
pub enum AdtAnswer<'f> {
    /// The store could not make its row: the lowering refuses a body that needs it.
    NotMade,
    Made {
        /// `Copy` as the selection answered it (`None`: not known).
        copy: Option<bool>,
        /// `Drop` as the selection answered it (`None`: not known).
        drop_impl: Option<bool>,
        facts: AdtFacts,
        /// Its variants in order, each as the number of its fields; their names, in order, are
        /// `fields`, variant after variant.
        variant_lens: &'f [u32],
        fields: &'f [tc::Name],
        /// Beside `fields`: the type parameter each is declared as, when a bare one.
        field_params: &'f [Option<u32>],
        /// Beside `fields`: how each one's declared type begins (blaze-flow-lower `FieldLead`).
        field_leads: &'f [FieldLead],
        /// Beside `fields`: each one's regions as `(start, len)` into `region_list`, or
        /// `(u32::MAX, 0)` for none (borrow_fields.rs `NO_ROW`).
        field_regions: &'f [(u32, u32)],
        region_list: &'f [FieldRegion],
        /// Which arguments its one `Copy` impl needs `Copy` (`None`: not read).
        copy_args: Option<&'f [bool]>,
        /// Which arguments its drop glue drops (`None`: not read; borrow_fields.rs `drop_facts_of`).
        drop_args: Option<&'f [bool]>,
        /// Which arguments reach a non-`#[may_dangle]` `Drop` impl (`None`: not read; RFC 1327).
        drop_strict: Option<&'f [bool]>,
        /// The ADT's unique `Deref` impl (`Facts::deref_impl`), when any.
        deref_impl: Option<u128>,
        /// Its lifetime parameters' variances, in head order (`None`: not read; borrows.rs
        /// `adt_variances_of`).
        variances: Option<&'f [Variance]>,
    },
}

/// What the borrow check took about one fn's head.
pub enum HeadAnswer<'s> {
    /// The store could not make its head: a call of it is related coarsely.
    NotMade,
    Made {
        callee: CalleeFacts,
        /// The head record's signature regions as the lowering reads them (D148), or why they
        /// were not read (the item and the typed cause).
        sig: Result<SigLife<'s>, RegionsUnread>,
    },
}

fn option_bool(h: &mut StableHasher, v: Option<bool>) {
    h.tag(match v {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    });
}

fn flag(h: &mut StableHasher, v: bool) {
    h.tag(u8::from(v));
}

/// One slot of a head record's column (blaze-typeck regions.rs `Region`).
fn region(h: &mut StableHasher, r: Region) {
    let (tag, a, b) = match r {
        Region::Static => (0, 0, 0),
        Region::Param(i) => (1, i, 0),
        Region::Late { binder, var } => (2, binder, var),
    };
    h.tag(tag);
    h.u32(a);
    h.u32(b);
}

fn slots(h: &mut StableHasher, rs: &[Region]) {
    h.u32(rs.len() as u32);
    for &r in rs {
        region(h, r);
    }
}

/// Why a head's regions were not read (blaze-typeck region_facts.rs `UnreadWhy`).
fn unread(h: &mut StableHasher, why: UnreadWhy) {
    let (tag, n) = match why {
        UnreadWhy::NoHead => (0, 0),
        UnreadWhy::HeadRefused => (1, 0),
        UnreadWhy::RecordForm { found } => (2, found),
        UnreadWhy::Refused { what } => {
            h.tag(3);
            hash_text(h, what);
            return;
        }
    };
    h.tag(tag);
    h.u32(n);
}

/// A refusal's name into a read's answer: its length, then its bytes (deterministic: the
/// text is a `&'static str` of the producer).
pub(super) fn hash_text(h: &mut StableHasher, text: &str) {
    h.u32(u32::try_from(text.len()).unwrap_or(u32::MAX));
    for b in text.bytes() {
        h.tag(b);
    }
}

/// The row of what the borrow check took about the ADT `adt`.
pub fn adt_read(adt: u128, answer: AdtAnswer<'_>) -> Read {
    let mut h = StableHasher::new(ADT_DOMAIN);
    match answer {
        AdtAnswer::NotMade => h.tag(0),
        AdtAnswer::Made { copy, drop_impl, facts, variant_lens, fields, field_params, field_leads, field_regions, region_list, copy_args, drop_args, drop_strict, deref_impl, variances } => {
            h.tag(1);
            option_bool(&mut h, copy);
            option_bool(&mut h, drop_impl);
            let AdtFacts { copy: c, drop_impl: d, needs_drop, lifetimes, union, is_enum, is_box } = facts;
            for v in [c, d, needs_drop, union, is_enum, is_box] {
                flag(&mut h, v);
            }
            h.u32(lifetimes);
            h.u32(variant_lens.len() as u32);
            for &len in variant_lens {
                h.u32(len);
            }
            h.u32(fields.len() as u32);
            for name in fields {
                h.u64(name.0);
            }
            h.u32(field_params.len() as u32);
            for &p in field_params {
                match p {
                    None => h.tag(0),
                    Some(p) => {
                        h.tag(1);
                        h.u32(p);
                    }
                }
            }
            h.u32(field_leads.len() as u32);
            for &lead in field_leads {
                let (tag, a, b) = match lead {
                    FieldLead::Param(p) => (0, p, 0),
                    FieldLead::Ref { mutbl, param } => (1 + u8::from(mutbl == tc::Mutbl::Mut), u32::from(param.is_some()), param.unwrap_or(0)),
                    FieldLead::Plain => (3, 0, 0),
                    FieldLead::Unread => (4, 0, 0),
                };
                h.tag(tag);
                h.u32(a);
                h.u32(b);
            }
            // Each field's regions, resolved (the row says what was taken, not where it lives).
            h.u32(field_regions.len() as u32);
            for &(start, len) in field_regions {
                match region_list.get(start as usize..start as usize + len as usize) {
                    Some(rs) if start != u32::MAX => {
                        h.tag(1);
                        h.u32(len);
                        for &fr in rs {
                            let (tag, n) = match fr {
                                FieldRegion::Lifetime(i) => (0, i),
                                FieldRegion::Static => (1, 0),
                                FieldRegion::Arg(i) => (2, i),
                                FieldRegion::Bound(i) => (3, i),
                            };
                            h.tag(tag);
                            h.u32(n);
                        }
                    }
                    _ => h.tag(0),
                }
            }
            match copy_args {
                None => h.tag(0),
                Some(need) => {
                    h.tag(1);
                    h.u32(u32::try_from(need.len()).unwrap_or(u32::MAX));
                    for &n in need {
                        flag(&mut h, n);
                    }
                }
            }
            match drop_args {
                None => h.tag(0),
                Some(need) => {
                    h.tag(1);
                    h.u32(u32::try_from(need.len()).unwrap_or(u32::MAX));
                    for &n in need {
                        flag(&mut h, n);
                    }
                }
            }
            match drop_strict {
                None => h.tag(0),
                Some(strict) => {
                    h.tag(1);
                    h.u32(u32::try_from(strict.len()).unwrap_or(u32::MAX));
                    for &n in strict {
                        flag(&mut h, n);
                    }
                }
            }
            match deref_impl {
                None => h.tag(0),
                Some(imp) => {
                    h.tag(1);
                    h.u128(imp);
                }
            }
            match variances {
                None => h.tag(0),
                Some(vs) => {
                    h.tag(1);
                    h.u32(vs.len() as u32);
                    for &v in vs {
                        h.tag(match v {
                            Variance::Co => 0,
                            Variance::Contra => 1,
                            Variance::In => 2,
                        });
                    }
                }
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowAdt, key: adt, detail: 0 }, answer: Some(h.finish()) }
}

/// The row of what the borrow check took about the head of the fn `item`.
pub fn head_read(item: u128, answer: HeadAnswer<'_>) -> Read {
    let mut h = StableHasher::new(HEAD_DOMAIN);
    match answer {
        HeadAnswer::NotMade => h.tag(0),
        HeadAnswer::Made { callee, sig } => {
            h.tag(1);
            let CalleeFacts { writes_lifetimes, self_ref } = callee;
            flag(&mut h, writes_lifetimes);
            flag(&mut h, self_ref);
            match sig {
                Ok(life) => {
                    h.tag(1);
                    // The column as the lowering reads it: the root's own slots, then each
                    // child's (every input in order, the output last), so a slot moved from one
                    // input to another moves the row even where the flat slots are equal.
                    let col = life.regions().col;
                    slots(&mut h, col.own());
                    h.u32(col.children());
                    for k in 0..col.children() {
                        match col.child(k) {
                            Some(rs) => {
                                h.tag(1);
                                slots(&mut h, rs);
                            }
                            None => h.tag(0),
                        }
                    }
                    // The lifetime parameters: their count, which are late-bound, their bounds.
                    let params = life.params();
                    h.u32(params.count());
                    for i in 0..params.count() {
                        option_bool(&mut h, params.is_late(i).ok());
                    }
                    let bounds = life.bounds();
                    h.u32(bounds.len() as u32);
                    for &(a, b) in bounds {
                        region(&mut h, a);
                        region(&mut h, b);
                    }
                    // The body's value's regions (an `async fn`'s written return type).
                    match life.body() {
                        BodyOut::Output => h.tag(0),
                        BodyOut::Async(body) => {
                            h.tag(1);
                            slots(&mut h, body);
                        }
                    }
                }
                Err(RegionsUnread { item, why }) => {
                    h.tag(2);
                    h.u128(item.0);
                    unread(&mut h, why);
                }
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: item, detail: 0 }, answer: Some(h.finish()) }
}

/// The `detail` of an impl's row ([`impl_read`]): `BorrowHead` rows are 0 (a head),
/// call_generics.rs 1, borrows_ties.rs 4 and 5.
pub const IMPL_DETAIL: u64 = 6;

/// The row of what the borrow check took about impl `imp`'s REGIONS, which the type check's
/// reads do not hold (they cover the impl's types: `type A = Inner<'i>` and `type A =
/// Inner<'static>` erase to one type): its header columns (self type, trait reference), its
/// lifetime count and bounds, and the value column of each associated type `names` the body's
/// projections selected it for (`names` sorted, deduplicated). Each is the head record's, or
/// why it was not read. A lifetime-only edit of the impl then moves the reading body's list
/// (rule 21); an edit of another impl moves nothing.
pub fn impl_read(imp: u128, names: &[tc::Name], regions: &dyn blaze_flow_lower::RegionFacts) -> Read {
    let mut h = StableHasher::new(HEAD_DOMAIN);
    h.tag(b'i');
    let key = tc::ItemKey(imp);
    match regions.impl_header(key) {
        Ok((self_col, trait_col)) => {
            h.tag(1);
            slots(&mut h, self_col.as_slice());
            match trait_col {
                Some(t) => {
                    h.tag(1);
                    slots(&mut h, t.as_slice());
                }
                None => h.tag(0),
            }
        }
        Err(RegionsUnread { item, why }) => {
            h.tag(2);
            h.u128(item.0);
            unread(&mut h, why);
        }
    }
    match regions.lifetime_params(key) {
        Ok(p) => {
            h.tag(1);
            h.u32(p.count());
            let bounds = p.outlives();
            h.u32(bounds.len() as u32);
            for &(a, b) in bounds {
                region(&mut h, a);
                region(&mut h, b);
            }
        }
        Err(RegionsUnread { item, why }) => {
            h.tag(2);
            h.u128(item.0);
            unread(&mut h, why);
        }
    }
    h.u32(names.len() as u32);
    for &n in names {
        h.u64(n.0);
        match regions.impl_assoc(key, n) {
            Ok(v) => {
                h.tag(1);
                slots(&mut h, v.as_slice());
            }
            Err(RegionsUnread { item, why }) => {
                h.tag(2);
                h.u128(item.0);
                unread(&mut h, why);
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: imp, detail: IMPL_DETAIL }, answer: Some(h.finish()) }
}

/// The `detail` of a trait's `Self` supertraits read for transitive `Copy` and `'static` bounds (borrows.rs).
pub const COPY_SUPERTRAIT_DETAIL: u64 = 9;
const COPY_SUPERTRAIT_DOMAIN: &str = "blaze-ladder front2 borrow copy supertrait v2";

/// The `detail` of a nested ADT's drop summary read (borrow_fields.rs `drop_facts_of`).
pub const DROP_ADT_DETAIL: u64 = 10;
const DROP_ADT_DOMAIN: &str = "blaze-ladder front2 borrow drop adt v1";

/// The row of what the borrow check took about trait `trait_key`'s direct `Self` supertraits
/// (and whether it directly declares `Self: 'static`).
/// `supers` is `None` when the trait head was not made, else `(sorted direct supertrait keys, self_static)`.
pub fn copy_supertrait_read(trait_key: u128, supers: Option<(&[u128], bool)>) -> Read {
    let mut h = StableHasher::new(COPY_SUPERTRAIT_DOMAIN);
    match supers {
        None => h.tag(0),
        Some((list, self_static)) => {
            h.tag(1);
            h.tag(u8::from(self_static));
            h.u32(u32::try_from(list.len()).unwrap_or(u32::MAX));
            for &sup in list {
                h.u128(sup);
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: trait_key, detail: COPY_SUPERTRAIT_DETAIL }, answer: Some(h.finish()) }
}

/// The row of what the structural `needs_drop` walk took about ADT `adt`: whether it was made,
/// its kind, whether it is `ManuallyDrop`/`PhantomData`, whether it is unconditionally `Copy`,
/// its `Drop` impl answer, its generic count, and the structural digest of its fields' types
/// (`fields_digest`).
#[expect(clippy::too_many_arguments, reason = "each structural drop input hashed into the read answer")]
pub fn drop_adt_read(
    adt: u128,
    made: bool,
    union: bool,
    inert: bool,
    uncond_copy: bool,
    drop_impl: Option<bool>,
    generics: u32,
    fields_digest: u64,
) -> Read {
    let mut h = StableHasher::new(DROP_ADT_DOMAIN);
    flag(&mut h, made);
    flag(&mut h, union);
    flag(&mut h, inert);
    flag(&mut h, uncond_copy);
    option_bool(&mut h, drop_impl);
    h.u32(generics);
    h.u64(fields_digest);
    Read { fact: Fact { kind: FactKind::BorrowAdt, key: adt, detail: DROP_ADT_DETAIL }, answer: Some(h.finish()) }
}

/// The detail of a [`FactKind::BorrowHead`] row holding a trait's or opaque's region clauses
/// (supertrait elaboration and `impl Trait` bounds in `blaze-borrow`).
pub const CLAUSES_DETAIL: u64 = 11;
const CLAUSES_DOMAIN: &str = "blaze-ladder front2 borrow clauses v1";

/// The row of what the borrow check read of item `item`'s region clauses and lifetime count.
pub fn clauses_read(item: u128, regions: &dyn blaze_flow_lower::RegionFacts) -> Read {
    let mut h = StableHasher::new(CLAUSES_DOMAIN);
    let key = tc::ItemKey(item);
    match regions.lifetime_params(key) {
        Ok(p) => {
            h.tag(1);
            h.u32(p.count());
        }
        Err(u) => {
            h.tag(0);
            unread(&mut h, u.why);
        }
    }
    match regions.clauses(key) {
        Ok(cls) => {
            h.tag(1);
            h.u32(u32::try_from(cls.len()).unwrap_or(u32::MAX));
            for cv in cls {
                h.u32(cv.nth);
                h.u32(cv.binders);
                h.u32(u32::try_from(cv.col.as_slice().len()).unwrap_or(u32::MAX));
                for &r in cv.col.as_slice() {
                    region(&mut h, r);
                }
            }
        }
        Err(u) => {
            h.tag(0);
            unread(&mut h, u.why);
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: item, detail: CLAUSES_DETAIL }, answer: Some(h.finish()) }
}

/// Files the borrow check's `rows` into the body's read list in `verdict` (sorted by fact, one
/// row per fact) and makes its digest again. A list already refused stays refused; one fact
/// with two answers refuses the list by name (rule 21), never merged.
pub fn file_borrow_reads<'a>(verdict: &mut BodyVerdict<'a>, rows: &[Read], alloc: Alloc<'a>) {
    if rows.is_empty() {
        return;
    }
    let Ok(list) = &mut verdict.facts else { return };
    list.rows.extend_from_slice(rows);
    list.rows.sort_unstable();
    let mut kept = 0;
    let mut two: Option<ReadsError> = None;
    for i in 0..list.rows.len() {
        let row = list.rows[i];
        if kept > 0 && list.rows[kept - 1].fact == row.fact {
            if let (Some(first), Some(second)) = (list.rows[kept - 1].answer, row.answer)
                && first != second
                && two.is_none()
            {
                two = Some(ReadsError::TwoAnswers { fact: row.fact, first, second });
            }
            continue;
        }
        list.rows[kept] = row;
        kept += 1;
    }
    list.rows.truncate(kept);
    if let Some(why) = two {
        verdict.facts = Err(why);
        verdict.reads = None;
        return;
    }
    if verdict.reads.is_some() {
        verdict.reads = Some(list.digest(alloc));
    }
}
