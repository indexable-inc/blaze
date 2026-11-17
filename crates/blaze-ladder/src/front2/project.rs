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
use blaze_scope::resolve::Names;

use super::Front2;
use super::FrontError;
use super::body::{Body, BodyError};
use super::lower_made::{LowerTarget, lower_made};
use super::buckets::Buckets;
use super::check::{CheckFail, Records, BuildOrder, eval_len};
use super::consts::{ConstFail, Consts};
use super::heads::{HeadError, Store, Want};
use super::keys::{MemberKind, checker_of};
use super::project_records::{BodySource, ByChain, ChainError, ChainOf, Listed, index_by_chain, list_bodies};
use super::reads::{Asked, BodyReads, Reach, ReadsError, facts_with_buckets};
use super::source::{self, NamesSource};
use super::spell::Spelling;
use super::types::Why;
use super::world_items::Gap;
use crate::refusal::SourceFile;

/// A head as the drive's `Signature` record (D114): the unit, its digest, its place.
pub mod head_digest;

/// Every body of the program, each checked once: the listed bodies and, through each body's
/// lowering, the fns declared in it ([`ProjectWorld::each_body`]).
pub mod every_body;

/// How many constants an array length in one body's heads may name before the body is
/// refused ([`Refused::Lengths`]): reach.rs's and the census's `MOST_LENS`.
pub(super) const MOST_LENS: u32 = 64;

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
    /// The digest of [`BodyVerdict::facts`] (`BodyReads::digest`): `Some` for a check that
    /// finished (clean or with diagnostics: the list is then whole, every fact with its
    /// answer), `None` for one that stopped or never ran, and for a list that was refused.
    pub reads: Option<ContentId>,
    /// The body's read list as facts (front2 reads.rs): what its check asked of the world, each
    /// with the digest of the answer it got, sorted by fact. Every body has one, whatever its
    /// verdict; `reach` says how far the check got. `Err`: a read that names no fact, by name.
    pub facts: Result<BodyReads<'a>, ReadsError>,
}

impl BodyVerdict<'_> {
    /// Clean: no diagnostic, no gap, no refusal.
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty() && self.gaps.is_empty() && self.refused.is_none()
    }
}

/// The front end and its records over one command's names: made once, then one body at a time.
///
/// Its names come from the source `S` (`source::NamesSource`), in the shape of
/// `check::Records`: the names this command solved (`ProjectWorld<'n>`, made by
/// [`ProjectWorld::new`]) or each crate's records (`ProjectWorld<'n, &source::Records>`, made
/// by [`ProjectWorld::from_records`]). Every method is one function over both.
pub struct ProjectWorld<'n, S = &'n Names<'n>> {
    pub front: Front2<'n, S>,
    store: Store<'n>,
    buckets: Option<Buckets>,
    consts: Consts<'n>,
    prepared: &'n Prepares<'n>,
    /// The command's allocator: the body index ([`ProjectWorld::chains`]) is made in it once.
    alloc: Alloc<'n>,
    /// The bodies by (file, chain), made by the first [`ProjectWorld::chains`].
    by_chain: Option<ByChain<'n>>,
}

impl<'n> ProjectWorld<'n> {
    /// The world over `names` (the program's crate at index 0, its files `user`), its frame
    /// the caller's, alive for the command.
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], alloc: Alloc<'n>) -> Result<ProjectWorld<'n>, FrontError> {
        let front = Front2::new(names, frame, user)?;
        let mut store = Store::new(front.index.lang("sized"), frame);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        Ok(ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared: Prepares::new_in(alloc), alloc, by_chain: None })
    }
}

impl<'n> ProjectWorld<'n, &'n source::Records<'n>> {
    /// The world over each crate's records, nothing solved in this command (the program's
    /// crate at index 0): `Front2::from_records`, which reads only the lang rows of
    /// `lang_crates`. What a record could not answer is in `records.errors()`: read it after
    /// each body, a verdict over a log that is not empty is void.
    pub fn from_records(records: &'n source::Records<'n>, frame: &'n blaze_mem::Frame, lang_crates: &[usize], alloc: Alloc<'n>) -> Result<ProjectWorld<'n, &'n source::Records<'n>>, FrontError> {
        let front = Front2::from_records(records, frame, lang_crates)?;
        let mut store = Store::new(front.index.lang("sized"), frame);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        Ok(ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared: Prepares::new_in(alloc), alloc, by_chain: None })
    }

    /// The run's sources a body's macro calls expand with (the crate's environment, texts and
    /// proc-macro runner: [`super::body_host::BodySources`]), set once by the command before
    /// any body; until then every body `env!`, include and proc macro is refused by name.
    pub fn set_body_sources(&mut self, sources: &'n dyn super::body_host::BodySources) {
        self.store.sources = Some(sources);
    }
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// Every body of the program's own crate, in key order (free fns, then each container's
    /// fn members), each with where it is written.
    pub fn bodies<'a>(&self, alloc: Alloc<'a>) -> AVec<'a, (BodyRef<'n>, Option<BodyAt>)> {
        let listed = list_bodies(self.front.names, alloc);
        let mut out: AVec<'a, (BodyRef<'n>, Option<BodyAt>)> = AVec::new_in(alloc);
        for l in listed.iter() {
            out.push((l.body, l.at));
        }
        out
    }

    /// [`ProjectWorld::bodies`] with each body's entry in its file's outline.
    pub fn listed<'a>(&self, alloc: Alloc<'a>) -> AVec<'a, Listed<'n>> {
        list_bodies(self.front.names, alloc)
    }

    /// The program's own bodies by (file, item chain), made on the first call in the world's
    /// allocator and kept: each file that holds a body is outlined at most once per world.
    pub fn chains(&mut self) -> Result<&ByChain<'n>, ChainError<'n>> {
        let index = match self.by_chain.take() {
            Some(index) => index,
            None => self.index_with(&blaze_file_items::item_chain)?,
        };
        Ok(self.by_chain.insert(index))
    }

    /// The body the command names by its file (the file's index in the program's own crate:
    /// an `At::File`'s, the items record's file row) and the chain of its outline entry
    /// (`blaze_file_items::item_chain(&outline, item, text)`): one binary search in
    /// [`ProjectWorld::chains`]. `Ok(None)`: no body of that file has that chain.
    pub fn body_by_chain(&mut self, file: u32, chain: ContentId) -> Result<Option<BodyRef<'n>>, ChainError<'n>> {
        Ok(self.chains()?.get(file, chain))
    }

    /// The body index made with `chain` as the chain function, not kept: the seam
    /// [`ProjectWorld::chains`] fills with `blaze_file_items::item_chain`, and a test with a
    /// planted bug.
    pub fn index_with(&self, chain: ChainOf<'_>) -> Result<ByChain<'n>, ChainError<'n>> {
        let listed = list_bodies(self.front.names, self.alloc);
        index_by_chain(self.front.names, &listed, chain, self.alloc)
    }

    /// Checks one body: lowered, its heads made (an array length's constant evaluated as met),
    /// then checked over the command's records.
    pub fn check<'a>(&mut self, body: BodyRef<'n>, alloc: Alloc<'a>) -> BodyVerdict<'a> {
        self.check_then(body, super::stalled_goals::Stalls::Gap, alloc, |_, _, _| {})
    }

    /// [`ProjectWorld::check`], then `then` over the records, the lowered body and its
    /// `Checked` record when the check finished (front2/borrows.rs `check_borrows`: the move
    /// and borrow check takes both, so nothing is lowered or type checked twice).
    ///
    /// `stalls`: what the check does with an auto-trait goal of an async block's future that
    /// fails only on its superset interior (front2 stalled_goals.rs): `Decide` only for a `then`
    /// that decides every stalled row (`check_borrows`).
    pub(super) fn check_then<'a, F>(&mut self, body: BodyRef<'n>, stalls: super::stalled_goals::Stalls, alloc: Alloc<'a>, then: F) -> BodyVerdict<'a>
    where
        F: FnOnce(&mut Records<'_, 'n, S>, &Body<'n>, &blaze_typeck::Checked<'static>),
    {
        let mut verdict =
            BodyVerdict { diagnostics: AVec::new_in(alloc), gaps: AVec::new_in(alloc), refused: None, reads: None, facts: Ok(BodyReads::not_checked(alloc)) };
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
        // The read list as facts (front2 reads.rs), for every outcome: the list of the run
        // that finished, or the questions of the last round of a check that stopped.
        let module = checker_of(lowered.module);
        // Its paths and segments only (the check below makes records into `r.store.tys`, so
        // no view of it is held here); a type read through this view is refused, never wrong.
        let written = lowered.typeck.view(blaze_typeck::PTys::EMPTY);
        let mut asked = Asked::new_in(alloc);
        let checked = super::body_reads::check_body_filed_as(&mut r, &lowered, &scope, None, Some(&mut asked), stalls);
        // The buckets as the check left them: what each impl question read, per crate.
        let buckets = r.buckets.as_ref();
        let whole = |list: &blaze_typeck::ReadList<'_>| -> Result<BodyReads<'a>, ReadsError> {
            let rows = facts_with_buckets(list.questions(), Some(list.rows.column::<1>()), module, written.paths, written.segments, buckets, alloc)?;
            Ok(BodyReads { rows, reach: Reach::Whole })
        };
        verdict.facts = match (&checked, &asked.finished) {
            (Ok((_, filed)), _) => whole(&filed.reads),
            (Err(_), Some(list)) => whole(list),
            (Err(_), None) => {
                facts_with_buckets(&asked.questions, None, module, written.paths, written.segments, buckets, alloc).map(|rows| BodyReads { rows, reach: Reach::Asked })
            }
        };
        if let Ok(list) = &verdict.facts
            && list.reach == Reach::Whole
            && matches!(checked, Ok(_) | Err(CheckFail::Diags { .. }))
        {
            verdict.reads = Some(list.digest(alloc));
        }
        match checked {
            Ok((done, _)) => {
                for d in done.diags.iter() {
                    verdict.diagnostics.push(BodyDiagnostic { code: d.code.code(), text: AString::new_in(alloc), start: d.at.start, end: d.at.end });
                }
                then(&mut r, &lowered, &done);
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

/// Lowers `body` by [`lower_made`] (each head and constant its lowering needs made first); a
/// member's container head is made first, each constant an array length in it names
/// evaluated, at most [`MOST_LENS`] of them.
fn lower<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, body: BodyRef<'n>) -> Result<Body<'n>, Refused> {
    let target = match body {
        BodyRef::Free(lib) => LowerTarget::Free(lib),
        BodyRef::Member { container, is_trait, name } => {
            // One want, with each constant its heads' array lengths name evaluated, at most
            // `MOST_LENS` of them.
            let made = |r: &mut Records<'_, 'n, S>, w: Want| -> Result<(), Refused> {
                let mut lens = 0;
                loop {
                    match r.store.ensure(r.names, r.keys, w) {
                        Ok(()) => return Ok(()),
                        Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                            lens += 1;
                            eval_len(r, c).map_err(Refused::Length)?;
                        }
                        Err(HeadError::Item(_, Why::NeedConst(..))) => return Err(Refused::Lengths),
                        Err(e) => return Err(Refused::Container(e)),
                    }
                }
            };
            let wants: &[Want] = if is_trait { &[Want::Head(container), Want::Members(container)] } else { &[Want::Impl(container)] };
            for &w in wants {
                made(&mut *r, w)?;
            }
            let k = r.keys.filed_member(checker_of(container), MemberKind::Fn, name).ok_or(Refused::MemberNotFiled)?;
            // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`), never made
            // with its impl's head; its own refusal is its `fn_gaps` row, read just below.
            if !is_trait {
                made(&mut *r, Want::MemberFn(container, k))?;
            }
            if r.store.fn_gaps.get(&k).is_some() {
                return Err(Refused::MemberHead(k));
            }
            LowerTarget::Member(k)
        }
    };
    match lower_made(r, prepared, target) {
        Ok(body) => Ok(body),
        Err(BodyError::NoBody(_)) => Err(Refused::NoBody),
        // The evaluator's refusal of a constant a length names: this module's class for it.
        Err(BodyError::Const { error, .. }) => Err(Refused::Length(*error)),
        Err(e) => Err(Refused::Lower(e)),
    }
}

