//! The move and borrow check's reads, filed in the body's read list (front2 reads.rs) beside
//! the type check's (rule 21, keys cover every input).
//!
//! The borrow verdict of a body reads facts the type check's list may not hold: whether each
//! ADT the body's types name is `Copy` and implements `Drop`, its fields and whether its head
//! declares lifetimes, and each callee's (and the owner's) signature as its TEXT says it (the
//! lifetime part `read_signature` reads, whether it writes a lifetime, whether it takes
//! `&self`). The type check asks none of that for a body that only moves a value: an ADT that
//! gains `impl Copy` (or a callee whose head gains `<'a>`) left the body's list, and so its
//! digest, unchanged, while its borrow notes changed. Each such fact is now a row:
//!
//! - [`FactKind::BorrowAdt`] about the ADT: its answer the digest of what the borrow check took
//!   (made or not; `Copy` and `Drop` as answered, unknown included; the flags; each variant's
//!   field names in order);
//! - [`FactKind::BorrowHead`] about the fn: its answer the digest of the head as the borrow check
//!   read it (made or not; the two callee flags; the lifetime part read, or why it was refused;
//!   each ADT the reading asked about and whether its head declares lifetimes).
//!
//! The answer is a digest of the borrow check's OWN derived facts, not of the text they came
//! from, so an edit inside a callee's body (which its item text holds) moves no caller's list.
//! These rows are made where the facts are gathered (borrows.rs `gather_facts`: one home) and
//! filed by [`file_borrow_reads`] into the verdict of [`super::project::ProjectWorld::check_borrows`];
//! a body whose types were not checked clean was not borrow checked and lists none.

use core::cell::RefCell;

use blaze_flow_lower::{AdtFacts, BodyOut, CalleeFacts, FieldLead, FieldRegion, Region, SigLife, SigRefusal, Variance};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use super::project::BodyVerdict;
use super::reads::{Fact, FactKind, Read, ReadsError};

/// The domain of a [`FactKind::BorrowAdt`] answer.
/// v3 (lane B11): an ADT's row also holds its fields' regions, its `Copy` impl's needs, and what
/// reading its fields' written types asked (ADT lifetime counts, path answers).
/// v4 (lane B13): and its lifetime parameters' variances (`Facts::adt_variances`).
const ADT_DOMAIN: &str = "blaze-ladder front2 borrow adt read v4";
/// The domain of a [`FactKind::BorrowHead`] answer.
/// v2 (piece RB8): a head's verdict now also reads its trait bounds' lifetime arguments
/// (borrows_ties.rs), so a list filed under v1 never answers for one checked since.
/// v3 (lane B11): the head's text is read through the front end's world (borrow_sig_world.rs):
/// each path answer it took is in the row.
const HEAD_DOMAIN: &str = "blaze-ladder front2 borrow head read v3";

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
        /// Its lifetime parameters' variances, in head order (`None`: not read; borrows.rs
        /// `adt_variances_of`).
        variances: Option<&'f [Variance]>,
        /// What reading its fields' written types asked: each ADT's lifetime count, sorted,
        /// and each path answer's digest, sorted (borrow_sig_world.rs).
        asked_adts: &'f [(u128, u32)],
        asked_paths: &'f [u64],
    },
}

/// What the borrow check took about one fn's head.
pub enum HeadAnswer<'s> {
    /// The store could not make its head: a call of it is related coarsely.
    NotMade,
    Made {
        callee: CalleeFacts,
        /// The lifetime part `read_signature` read, or why it refused the text.
        sig: Result<SigLife<'s>, SigRefusal>,
        /// Each ADT the reading asked about, sorted, with how many lifetimes its head declares.
        adts: &'s [(u128, u32)],
        /// Each path answer the reading took, as its digest, sorted (borrow_sig_world.rs).
        paths: &'s [u64],
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

fn region(h: &mut StableHasher, r: Region) {
    let (tag, n) = match r {
        Region::Static => (0, 0),
        Region::Named(n) => (1, n),
        Region::Anon(n) => (2, n),
        Region::SelfTy(n) => (3, n),
        Region::Hidden(n) => (4, n),
    };
    h.tag(tag);
    h.u32(n);
}

fn refusal(h: &mut StableHasher, why: SigRefusal) {
    let (tag, a, b) = match why {
        SigRefusal::Token { at } => (0, at, 0),
        SigRefusal::NoFn => (1, 0, 0),
        SigRefusal::ParamCount { text, head } => (2, text, head),
        SigRefusal::Unaligned { at } => (3, at, 0),
        SigRefusal::ElisionUndecided { at } => (5, at, 0),
        SigRefusal::ObjectDefault { at } => (6, at, 0),
        SigRefusal::Unknown => (7, 0, 0),
        SigRefusal::TypeNode => (8, 0, 0),
        SigRefusal::Bound => (9, 0, 0),
        SigRefusal::Assoc { at, why } => (10, at, why as u32),
        SigRefusal::AsyncOutputNotGiven => (11, 0, 0),
        SigRefusal::Path { at, why } => (12, at, why as u32),
    };
    h.tag(tag);
    h.u32(a);
    h.u32(b);
}

/// The row of what the borrow check took about the ADT `adt`.
pub fn adt_read(adt: u128, answer: AdtAnswer<'_>) -> Read {
    let mut h = StableHasher::new(ADT_DOMAIN);
    match answer {
        AdtAnswer::NotMade => h.tag(0),
        AdtAnswer::Made { copy, drop_impl, facts, variant_lens, fields, field_params, field_leads, field_regions, region_list, copy_args, variances, asked_adts, asked_paths } => {
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
                    h.u32(need.len() as u32);
                    for &n in need {
                        flag(&mut h, n);
                    }
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
            h.u32(asked_adts.len() as u32);
            for &(a, lifetimes) in asked_adts {
                h.u128(a);
                h.u32(lifetimes);
            }
            h.u32(asked_paths.len() as u32);
            for &p in asked_paths {
                h.u64(p);
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
        HeadAnswer::Made { callee, sig, adts, paths } => {
            h.tag(1);
            let CalleeFacts { writes_lifetimes, self_ref } = callee;
            flag(&mut h, writes_lifetimes);
            flag(&mut h, self_ref);
            match sig {
                Ok(life) => {
                    h.tag(1);
                    h.u32(life.regions.len() as u32);
                    for &r in life.regions {
                        region(&mut h, r);
                    }
                    h.u32(life.ends.len() as u32);
                    for &e in life.ends {
                        h.u32(e);
                    }
                    h.u32(life.bounds.len() as u32);
                    for &(a, b) in life.bounds {
                        region(&mut h, a);
                        region(&mut h, b);
                    }
                    // The body's value's regions (an `async fn`'s written return type).
                    match life.body {
                        BodyOut::Output => h.tag(0),
                        BodyOut::Async(body) => {
                            h.tag(1);
                            h.u32(body.len() as u32);
                            for &r in body {
                                region(&mut h, r);
                            }
                        }
                        BodyOut::AsyncUnread(why) => {
                            h.tag(2);
                            refusal(&mut h, why);
                        }
                    }
                }
                Err(why) => {
                    h.tag(2);
                    refusal(&mut h, why);
                }
            }
            h.u32(adts.len() as u32);
            for &(adt, lifetimes) in adts {
                h.u128(adt);
                h.u32(lifetimes);
            }
            h.u32(paths.len() as u32);
            for &p in paths {
                h.u64(p);
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: item, detail: 0 }, answer: Some(h.finish()) }
}

/// A recorder of the ADTs `read_signature` asks about (its `adt_lifetimes` is a `&dyn Fn`):
/// each asked ADT with the answer given.
pub struct AskedAdts<'a> {
    asked: RefCell<AVec<'a, (u128, u32)>>,
    alloc: Alloc<'a>,
}

impl<'a> AskedAdts<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> AskedAdts<'a> {
        AskedAdts { asked: RefCell::new(AVec::new_in(alloc)), alloc }
    }

    /// Notes that the reading asked about `adt` and was told `lifetimes`.
    pub fn note(&self, adt: tc::ItemKey, lifetimes: u32) {
        self.asked.borrow_mut().push((adt.0, lifetimes));
    }

    /// Every ADT asked, sorted, each once; the recorder is left empty for the next head.
    pub fn take_sorted(&self) -> AVec<'a, (u128, u32)> {
        let mut out = core::mem::replace(&mut *self.asked.borrow_mut(), AVec::new_in(self.alloc));
        out.sort_unstable();
        out.dedup();
        out
    }
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
