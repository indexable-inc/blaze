//! The type check of a project's OWN bodies through front2, for `blaze check`'s TYPES stage
//! (the "Driver and CLI" lane's caller): one [`ProjectWorld`] per command over the resolved
//! names, then one [`ProjectWorld::check`] per body, each giving a [`BodyVerdict`]: the
//! program's diagnostics with item-relative ranges, the world's gaps by their typed reasons,
//! the one refusal that stopped the body (never counted as a pass), and the digest of the read
//! list the check left. Nothing is printed and nothing is cached here: the caller keys and
//! keeps verdicts.
//!
//! "Own" is the program's crate, index 0 of `names` ([`super::Front2::new`]'s convention).

use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_lower::scoped::Prepares;
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::resolve::{At, Names};
use blaze_surface::ItemDetail;

use super::Front2;
use super::FrontError;
use super::body::{Body, BodyError, lower_fn, lower_member};
use super::buckets::Buckets;
use super::check::{CheckFail, Records, BuildOrder, check_body, eval_len};
use super::consts::{ConstFail, Consts};
use super::heads::{HeadError, Store, Want};
use super::keys::{MemberKind, checker_of};
use super::spell::Spelling;
use super::types::Why;
use super::world_items::Gap;
use crate::refusal::SourceFile;

/// How many constants an array length in one body's heads may name before the body is
/// refused ([`Refused::Lengths`]): reach.rs's and the census's `MOST_LENS`.
const MOST_LENS: u32 = 64;

/// The domain of a body's read-list digest ([`BodyVerdict::reads`]).
const READS_DOMAIN: &str = "blaze-ladder front2 body reads v1";

/// One body of the program's own crate: a free fn, or a fn member of an impl or a trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BodyRef<'n> {
    Free(LibKey),
    Member { container: LibKey, is_trait: bool, name: &'n str },
}

/// Where a body is written: its item's file (an index into the crate's files) and its start
/// in that file, or `None` when a macro wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyAt {
    pub file: u32,
    pub start: u32,
}

/// One diagnostic of a body: its code, what it is about, its range relative to the start of
/// the text the body was lowered from (a free fn's item text, a member's own text).
#[derive(Debug)]
pub struct BodyDiagnostic<'a> {
    pub code: &'static str,
    pub text: AString<'a>,
    pub start: u32,
    pub end: u32,
}

/// Why a body was not checked to its end: each a refusal, never a pass.
#[derive(Debug)]
pub enum Refused {
    /// A declaration with no body (`fn f();`, an intrinsic).
    NoBody,
    /// Lowering refused it.
    Lower(BodyError),
    /// Its names could not be spelled.
    Spell(Why),
    /// The head of its container (an impl, a trait) was refused.
    Container(HeadError),
    /// A member's container head files no fn of that name.
    MemberNotFiled,
    /// The member's own head was refused: its checker key; the store's `fn_gaps` row says why.
    MemberHead(blaze_typeck::ItemKey),
    /// An array length in its heads names a constant the evaluator refused.
    Length(ConstFail),
    /// More than [`MOST_LENS`] constants named by lengths.
    Lengths,
    /// The check itself stopped (a malformed body, a bound of the checker, a resolution, a
    /// record the world could not make): `CheckFail` minus its gaps and diagnostics.
    Check(CheckFail),
}

impl Refused {
    /// The refusal's class, for a table or a status line.
    pub fn text(&self) -> &'static str {
        match self {
            Refused::NoBody => "no body",
            Refused::Lower(_) => "lowering refused",
            Refused::Spell(_) => "spelling refused",
            Refused::Container(_) => "container head refused",
            Refused::MemberNotFiled => "member not filed",
            Refused::MemberHead(_) => "member head refused",
            Refused::Length(_) => "array length constant refused",
            Refused::Lengths => "too many array length constants",
            Refused::Check(_) => "check stopped",
        }
    }
}

/// What one body's check gave.
#[derive(Debug)]
pub struct BodyVerdict<'a> {
    /// The program's errors, sorted by range.
    pub diagnostics: AVec<'a, BodyDiagnostic<'a>>,
    /// The questions the world could not answer yet, each typed (the body is then not checked).
    pub gaps: AVec<'a, Gap>,
    /// The one refusal that stopped the body, if any.
    pub refused: Option<Refused>,
    /// The digest of the check's read list (each question and its answer's hash, sorted by
    /// question): `Some` only for a check that finished clean (a check with diagnostics does not
    /// hand its read list back from front2/check.rs yet).
    pub reads: Option<ContentId>,
}

impl BodyVerdict<'_> {
    /// Clean: no diagnostic, no gap, no refusal.
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty() && self.gaps.is_empty() && self.refused.is_none()
    }
}

/// The front end and its records over one command's names: made once, then one body at a time.
pub struct ProjectWorld<'n> {
    pub front: Front2<'n>,
    store: Store<'n>,
    buckets: Option<Buckets>,
    consts: Consts<'n>,
    prepared: &'n Prepares<'n>,
}

impl<'n> ProjectWorld<'n> {
    /// The world over `names` (the program's crate at index 0, its files `user`), its frame
    /// the caller's, alive for the command.
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], alloc: Alloc<'n>) -> Result<ProjectWorld<'n>, FrontError> {
        let front = Front2::new(names, frame, user)?;
        let store = Store::new(front.index.lang("sized"), frame);
        Ok(ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared: Prepares::new_in(alloc) })
    }

    /// Every body of the program's own crate, in key order (free fns, then each container's
    /// fn members), each with where it is written.
    pub fn bodies<'a>(&self, alloc: Alloc<'a>) -> AVec<'a, (BodyRef<'n>, Option<BodyAt>)> {
        let names = self.front.names;
        let mut out: AVec<'a, (BodyRef<'n>, Option<BodyAt>)> = AVec::new_in(alloc);
        let at = |key: LibKey, text: &str| -> Option<BodyAt> {
            let place = names.place(key)?;
            let At::File(file) = place.at else { return None };
            let (base, _) = names.krate(0).outline_at(place.at)?;
            let (b, t) = (base.as_ptr() as usize, text.as_ptr() as usize);
            (t >= b && t <= b + base.len()).then(|| BodyAt { file, start: (t - b) as u32 })
        };
        for (key, _, _) in names.items(0) {
            let Some(it) = names.item_text(key) else { continue };
            match self.front.resolver.detail(key) {
                Some(ItemDetail::Fn { .. }) if !it.text.trim_end().ends_with(';') => out.push((BodyRef::Free(key), at(key, it.text))),
                Some(d @ (ItemDetail::Impl | ItemDetail::Trait)) => {
                    for m in names.members(key).unwrap_or(&[]) {
                        let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                        out.push((BodyRef::Member { container: key, is_trait: matches!(d, ItemDetail::Trait), name }, at(key, m.text)));
                    }
                }
                _ => {}
            }
        }
        // Members are filed under their container: a free fn listed again as a member is one body.
        let members: AVec<'a, LibKey> = {
            let mut v = AVec::new_in(alloc);
            for (key, _, _) in names.items(0) {
                for m in names.members(key).unwrap_or(&[]) {
                    v.push(m.key);
                }
            }
            v.sort_unstable();
            v
        };
        out.retain(|(b, _)| !matches!(b, BodyRef::Free(k) if members.binary_search(k).is_ok()));
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Checks one body: lowered, its heads made (an array length's constant evaluated as met),
    /// then checked over the command's records.
    pub fn check<'a>(&mut self, body: BodyRef<'n>, alloc: Alloc<'a>) -> BodyVerdict<'a> {
        let mut verdict = BodyVerdict { diagnostics: AVec::new_in(alloc), gaps: AVec::new_in(alloc), refused: None, reads: None };
        let prepared = self.prepared;
        let mut r = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: Some((&mut self.consts, prepared)),
        };
        let lowered = match lower(&mut r, prepared, body) {
            Ok(b) => b,
            Err(why) => {
                verdict.refused = Some(why);
                return verdict;
            }
        };
        let view = lowered.lowered.view();
        let spell = match Spelling::of(&view) {
            Ok(s) => s,
            Err(why) => {
                verdict.refused = Some(Refused::Spell(why));
                return verdict;
            }
        };
        let scope = lowered.scope(&spell);
        match check_body(&mut r, &lowered, &scope) {
            Ok(done) => {
                for d in done.diags.iter() {
                    verdict.diagnostics.push(BodyDiagnostic { code: d.code.code(), text: AString::new_in(alloc), start: d.at.start, end: d.at.end });
                }
                if done.diags.is_empty() {
                    verdict.reads = Some(reads_digest(&done.reads, alloc));
                }
            }
            Err(CheckFail::Diags { diags, .. }) => {
                for (code, at, about) in diags.iter() {
                    verdict.diagnostics.push(BodyDiagnostic { code: code.code(), text: AString::from_str_in(about, alloc), start: at.start, end: at.end });
                }
            }
            Err(CheckFail::Gap { gaps, .. }) => {
                for g in gaps {
                    verdict.gaps.push(g);
                }
            }
            Err(other) => verdict.refused = Some(Refused::Check(other)),
        }
        verdict.diagnostics.sort_by_key(|d| (d.start, d.end, d.code));
        verdict
    }
}

/// Lowers `body`, evaluating each constant an array length in its heads names, at most
/// [`MOST_LENS`] of them; a member's container head is made first.
fn lower<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, body: BodyRef<'n>) -> Result<Body<'n>, Refused> {
    enum Target {
        Free(LibKey),
        Member(blaze_typeck::ItemKey),
    }
    let target = match body {
        BodyRef::Free(lib) => Target::Free(lib),
        BodyRef::Member { container, is_trait, name } => {
            let wants: &[Want] = if is_trait { &[Want::Head(container), Want::Members(container)] } else { &[Want::Impl(container)] };
            for &w in wants {
                let mut lens = 0;
                loop {
                    match r.store.ensure(r.names, r.keys, w) {
                        Ok(()) => break,
                        Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                            lens += 1;
                            eval_len(r, c).map_err(Refused::Length)?;
                        }
                        Err(HeadError::Item(_, Why::NeedConst(..))) => return Err(Refused::Lengths),
                        Err(e) => return Err(Refused::Container(e)),
                    }
                }
            }
            let k = r.keys.filed_member(checker_of(container), MemberKind::Fn, name).ok_or(Refused::MemberNotFiled)?;
            if r.store.fn_gaps.get(&k).is_some() {
                return Err(Refused::MemberHead(k));
            }
            Target::Member(k)
        }
    };
    let mut lens = 0;
    loop {
        let lowered = match target {
            Target::Free(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            Target::Member(k) => lower_member(r.names, prepared, r.keys, r.store, k),
        };
        let c = match &lowered {
            Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) => *c,
            Err(BodyError::NoBody(_)) => return Err(Refused::NoBody),
            _ => return lowered.map_err(Refused::Lower),
        };
        if lens == MOST_LENS {
            return Err(Refused::Lengths);
        }
        lens += 1;
        eval_len(r, c).map_err(Refused::Length)?;
    }
}

/// The digest of a read list: each question (its derived `Hash` bytes) and its answer's hash,
/// in the list's order (sorted by question, deduplicated, by blaze-typeck's contract).
fn reads_digest(reads: &blaze_typeck::ReadList<'_>, alloc: Alloc<'_>) -> ContentId {
    use core::hash::{Hash, Hasher};
    struct Bytes<'b>(AVec<'b, u8>);
    impl Hasher for Bytes<'_> {
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    let mut out = Bytes(AVec::new_in(alloc));
    for (q, h) in reads.rows.column::<0>().iter().zip(reads.rows.column::<1>().iter()) {
        q.hash(&mut out);
        out.write(&h.to_le_bytes());
    }
    ContentId::of(READS_DOMAIN, &out.0)
}
