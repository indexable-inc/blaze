//! Each fn's trait bounds with lifetime arguments, as blaze-flow-lower's `Facts::ties` asks
//! them (piece RB8, blaze-flow-lower trait_ties.rs): a callee's tie its trait lifetimes to the
//! caller's regions at a call, the body's own say which of its regions answer them.
//!
//! Read from what the borrow check already holds, never lowered again:
//! - which bounds there are and on which trait: the head's resolved `bounds` (the container's,
//!   then the fn's own, each `(param, trait)`), and for a trait's method its implicit
//!   `Self: Trait<'params>` (param 0, the head's `SelfTy::Trait`);
//! - each trait's lifetime parameters, in order, from its header text (`trait_header`);
//! - each bound's lifetime arguments from the fn's own text (`SigBuf::trait_bounds`, read by
//!   the same reader as its lifetime part, so a lifetime has the same `Named` number): the row
//!   whose bare name resolves to the parameter (through the head's `params`, as
//!   call_generics.rs resolves `T: 'a`) and whose last path segment is the trait's name. None,
//!   two, a count that differs from the trait's, or a `'_`: the row is kept unread, never
//!   guessed. A bound its own `for<..>` quantifies fixes no region: no row. A trait method's
//!   implicit row takes each trait lifetime by name from the method's text; one the text never
//!   writes is a `Named` past every other (it ties nothing the signature names).
//!
//! RULE 21: the borrow verdict reads these, so each fn's answer is a read row of the body
//! (`FactKind::BorrowHead`, detail [`DETAIL`], keyed by the fn: its text), and each trait's
//! header answer one more (detail [`TRAIT_DETAIL`], keyed by the trait).

use blaze_flow_lower::sig::trait_header;
use blaze_flow_lower::{AssocRead, Region, SigBuf, Tie, Ties};
use blaze_id::ItemKey as LibKey;
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use super::super::heads::FnHead;
use super::super::keys::{Keys, checker_of};
use super::super::reads::{Fact, FactKind, Read};
use super::super::source::NamesSource;
use super::super::types::SelfTy;

/// What trait `trait_`'s text says of its associated type `name`, as blaze-flow-lower
/// `read_shape` asks it (generic_call.rs, "PROJECTIONS" and "OPEN PROJECTIONS"): whether the
/// trait declares a lifetime parameter, and whether `name` has generics of its own. `None`: the
/// trait's text is not given or not read. Its row is sig_impl.rs `trait_read`, keyed by the
/// trait, which the caller files for every trait it asks (rule 21).
pub fn assoc_read<'n, S: NamesSource<'n>>(names: S, keys: &Keys, trait_: tc::ItemKey, name: tc::Name, alloc: Alloc<'_>) -> Option<AssocRead> {
    let (trait_lifetimes, generic) = super::super::sig_impl::trait_assoc(names, keys, trait_, alloc)?;
    Some(AssocRead { trait_lifetimes, generic: generic.iter().any(|g| tc::Name::of(g) == name) })
}

/// The `detail` of a fn's ties row (call_generics.rs files 1, sig_impl.rs 2 and 3).
pub const DETAIL: u64 = 4;
/// The `detail` of a trait's header row read for a tie.
pub const TRAIT_DETAIL: u64 = 5;

const DOMAIN: &str = "blaze-ladder front2 borrow trait ties read v1";
const TRAIT_DOMAIN: &str = "blaze-ladder front2 borrow tie trait header read v1";

/// Every fn's rows, sorted by checker key, and the rows read for them.
pub struct TiesTable<'a> {
    /// `(fn, first row, end)` into `ties`.
    index: AVec<'a, (u128, u32, u32)>,
    ties: AVec<'a, Tie>,
    /// Every row's regions; a [`Tie`]'s `lo..hi` index here.
    regions: AVec<'a, Region>,
    reads: AVec<'a, Read>,
    /// The traits whose header row is filed.
    traits: AVec<'a, u128>,
}

impl<'a> TiesTable<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> TiesTable<'a> {
        TiesTable { index: AVec::new_in(alloc), ties: AVec::new_in(alloc), regions: AVec::new_in(alloc), reads: AVec::new_in(alloc), traits: AVec::new_in(alloc) }
    }

    /// Fn `item`'s rows, if filed.
    pub fn get(&self, item: tc::ItemKey) -> Option<Ties<'_>> {
        let at = self.index.binary_search_by_key(&item.0, |&(k, _, _)| k).ok()?;
        let (_, lo, hi) = self.index[at];
        Some(Ties { rows: self.ties.get(lo as usize..hi as usize)?, regions: &self.regions })
    }

    /// The read rows, for the body's read list.
    pub fn reads(&self) -> &[Read] {
        &self.reads
    }

    /// Files fn `k`'s rows and their read rows. `trait_text`: a trait's item text by its library
    /// key (a trait method's container is the head's `SelfTy::Trait`). Call in ascending `k`.
    pub fn gather<'t>(&mut self, k: u128, head: Option<&FnHead<'_>>, sig: Option<&SigBuf<Alloc<'a>>>, trait_text: &dyn Fn(LibKey) -> Option<&'t str>, alloc: Alloc<'a>) {
        let (Some(h), Some(sig)) = (head, sig) else {
            let mut hasher = StableHasher::new(DOMAIN);
            hasher.tag(0);
            self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: k, detail: DETAIL }, answer: Some(hasher.finish()) });
            return;
        };
        let start = self.ties.len();
        let mut names: AVec<'a, tc::Name> = AVec::new_in(alloc);
        if let SelfTy::Trait(lib) = h.self_ {
            let tk = checker_of(lib);
            names.clear();
            let read = match trait_text(lib) {
                Some(text) => trait_header(text, &mut names, alloc).ok(),
                None => None,
            };
            self.trait_row(tk, read.map(|n| (n, &names[..])));
            match read {
                Some(_) if names.is_empty() => {}
                Some(_) => {
                    let lo = self.regions.len() as u32;
                    for (i, n) in names.iter().enumerate() {
                        let r = sig.names.iter().find(|(m, _)| m == n).map_or(Region::Named((sig.names.len() + i) as u32), |&(_, r)| r);
                        self.regions.push(r);
                    }
                    self.ties.push(Tie { param: 0, trait_: tk, lo, hi: self.regions.len() as u32, read: true });
                }
                None => self.unread(0, tk),
            }
        }
        for b in h.bounds.iter() {
            let tk = checker_of(b.trait_);
            if self.ties[start..].iter().any(|t| t.param == b.param && t.trait_ == tk) {
                continue;
            }
            names.clear();
            let read = trait_text(b.trait_).map(|text| trait_header(text, &mut names, alloc));
            let tname = match read {
                Some(Ok(n)) => n,
                _ => {
                    self.trait_row(tk, None);
                    self.unread(b.param, tk);
                    continue;
                }
            };
            self.trait_row(tk, Some((tname, &names[..])));
            if names.is_empty() {
                continue;
            }
            let mut hit = None;
            let mut many = false;
            for tb in sig.trait_bounds.iter().filter(|tb| tb.trait_ == tname) {
                let p = if tb.param == tc::Name::of("Self") { Some(0) } else { h.params.iter().find(|(n, _)| *n == tb.param).map(|&(_, i)| i) };
                if p == Some(b.param) {
                    many |= hit.is_some();
                    hit = Some(*tb);
                }
            }
            match hit {
                // A bound its `for<..>` quantifies fixes no region of the fn.
                Some(tb) if !many && tb.binder && !tb.unread => {}
                Some(tb) if !many && !tb.unread && (tb.hi - tb.lo) as usize == names.len() => {
                    let lo = self.regions.len() as u32;
                    self.regions.extend_from_slice(&sig.trait_bound_regions[tb.lo as usize..tb.hi as usize]);
                    self.ties.push(Tie { param: b.param, trait_: tk, lo, hi: self.regions.len() as u32, read: true });
                }
                _ => self.unread(b.param, tk),
            }
        }
        let mut hasher = StableHasher::new(DOMAIN);
        hasher.tag(1);
        hasher.u32((self.ties.len() - start) as u32);
        for t in self.ties[start..].iter() {
            hasher.u32(t.param);
            hasher.u128(t.trait_.0);
            hasher.tag(u8::from(t.read));
            hasher.u32(t.hi - t.lo);
            for &r in self.regions[t.lo as usize..t.hi as usize].iter() {
                region(&mut hasher, r);
            }
        }
        self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: k, detail: DETAIL }, answer: Some(hasher.finish()) });
        if self.index.last().is_none_or(|&(last, _, _)| last < k) {
            self.index.push((k, start as u32, self.ties.len() as u32));
        }
    }

    fn unread(&mut self, param: u32, trait_: tc::ItemKey) {
        let at = self.regions.len() as u32;
        self.ties.push(Tie { param, trait_, lo: at, hi: at, read: false });
    }

    /// The row of what was taken of trait `t`'s header, once per trait: its name and lifetime
    /// parameters, or that it was not read.
    fn trait_row(&mut self, t: tc::ItemKey, read: Option<(tc::Name, &[tc::Name])>) {
        if self.traits.contains(&t.0) {
            return;
        }
        self.traits.push(t.0);
        let mut h = StableHasher::new(TRAIT_DOMAIN);
        match read {
            None => h.tag(0),
            Some((name, lifetimes)) => {
                h.tag(1);
                h.u64(name.0);
                h.u32(lifetimes.len() as u32);
                for n in lifetimes {
                    h.u64(n.0);
                }
            }
        }
        self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: t.0, detail: TRAIT_DETAIL }, answer: Some(h.finish()) });
    }
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
