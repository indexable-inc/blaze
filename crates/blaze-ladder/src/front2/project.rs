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
use super::tables::cost::{Cost, Part};
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

/// One body of the program's own crate: a free fn, `const` or `static` (its initializer), or a
/// fn or `const` member of an impl or a trait (`kind`: [`MemberKind::Fn`] or
/// [`MemberKind::Const`]; a member's key is made from its kind and its name, keys.rs
/// `filed_member`). The Reference, "Constant items" and "Static items": the initializer is an
/// expression of the declared type, checked as a body is ([`ProjectWorld::check`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BodyRef<'n> {
    Free(LibKey),
    Member { container: LibKey, is_trait: bool, kind: MemberKind, name: &'n str },
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
    /// The other places the text names, as data (item-relative ranges, like `start..end`):
    /// for a future that is not `Send`, where the value it holds is declared and the await it
    /// is held across. The printer turns each into `file:line:col`.
    pub notes: AVec<'a, DiagNote>,
}

/// A place a finding's text names besides its own range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagNote {
    pub what: NoteKind,
    pub start: u32,
    pub end: u32,
}

/// What a [`DiagNote`]'s place is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    /// Where a value an async block holds across an await is declared (a binding, a capture,
    /// a temporary's expression).
    HeldValue,
    /// The await a value is held across.
    HeldAcross,
}

/// A finding of a typed lint in a body (blaze-typeck `TypedFinding`, `args-inline` the first):
/// no error of the program's types; its level is the configuration's, at print time (rule 81).
#[derive(Debug)]
pub struct BodyLintFinding<'a> {
    /// The lint's name (blaze-lint `Rule::name`'s spelling: `args-inline`).
    pub lint: &'static str,
    /// The whole message, naming the item it is about (rule 19).
    pub text: AString<'a>,
    /// Item-relative (rule 71).
    pub start: u32,
    pub end: u32,
}

/// A construct of a checked body that the check could not decide (blaze-typeck `Undecided`):
/// no error and no pass, so the command files it as "not checked" (rule 3), never as clean.
#[derive(Debug)]
pub struct BodyUndecided<'a> {
    /// Why it is not checked (blaze-typeck `Undecided::says`).
    pub cause: &'static str,
    /// What it is about, naming the item (rule 19).
    pub text: AString<'a>,
    /// Item-relative (rule 71).
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
            Refused::Length(ConstFail::RequiredBody { .. }) => "a constant required by this body was refused",
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
    /// The typed lints' findings of a check that finished, sorted by range; none of them makes
    /// the body unclean ([`BodyVerdict::is_clean`]).
    pub findings: AVec<'a, BodyLintFinding<'a>>,
    /// What a check that finished could not decide, sorted by range: each is "not checked" to
    /// whoever prints the verdict ([`BodyUndecided`]).
    pub undecided: AVec<'a, BodyUndecided<'a>>,
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
    /// The body's checked record (blaze-typeck checked_record.rs, D163 R1): the columns a run
    /// reads in place of checking it again, encoded once where the check left them (borrows.rs
    /// `check_borrows_in`). `None` for a check that did not finish or whose columns are not
    /// clean (a diagnostic, a stalled goal, an undecided place: the record keeps none); `Err`
    /// for a clean check the record refused, by name. Whether it is kept is the caller's (after
    /// a clean verdict, borrows included).
    pub typed: Option<Result<AVec<'a, u8>, blaze_typeck::checked_record::CheckedRecordError>>,
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
    pub(super) store: Store<'n>,
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
    /// the caller's, alive for the command; `alloc` is `'static` because the head store's
    /// tables are (`Store::new`).
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], alloc: Alloc<'static>) -> Result<ProjectWorld<'n>, FrontError> {
        let front = Front2::new(names, frame, user, alloc)?;
        let mut store = Store::new(front.index.lang("sized"), frame, alloc);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
        Ok(ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared: Prepares::new_in(bump), alloc, by_chain: None })
    }
}

impl<'n> ProjectWorld<'n, &'n source::Records<'n>> {
    /// The world over each crate's records, nothing solved in this command (the program's
    /// crate at index 0): `Front2::from_records`, which reads only the lang rows of
    /// `lang_crates`. What a record could not answer is in `records.errors()`: read it after
    /// each body, a verdict over a log that is not empty is void.
    pub fn from_records(records: &'n source::Records<'n>, frame: &'n blaze_mem::Frame, lang_crates: &[usize], alloc: Alloc<'static>) -> Result<ProjectWorld<'n, &'n source::Records<'n>>, FrontError> {
        let front = Front2::from_records(records, frame, lang_crates)?;
        let mut store = Store::new(front.index.lang("sized"), frame, alloc);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
        Ok(ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared: Prepares::new_in(bump), alloc, by_chain: None })
    }
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// The run's sources a body's macro calls expand with (the crate's environment, texts and
    /// proc-macro runner: [`super::body_host::BodySources`]), set once by the command before
    /// any body (over records, or per body task over the view: tables/task.rs `BodyFrame`);
    /// until then every body `env!`, include and proc macro is refused by name.
    pub fn set_body_sources(&mut self, sources: &'n dyn super::body_host::BodySources) {
        self.store.sources = Some(sources);
    }

    /// The world over a front end its caller made: front2 tables/task.rs makes one per BODY
    /// over the read-only view (`Tables`), so its key map, head store, impl index, constants
    /// and prepared macros are the task's own and nothing is kept from one body to the next.
    pub fn over(front: Front2<'n, S>, alloc: Alloc<'static>) -> ProjectWorld<'n, S> {
        Self::over_with(front, None, alloc)
    }

    /// As [`ProjectWorld::over`], sharing a caller-owned `prepared` macro table across a stretch
    /// of bodies so each body leaves 0 bytes in `front.frame` for `Prepares` and macro definitions
    /// are compiled once per stretch instead of leaked per body. When `prepared` is `None`,
    /// allocates a fresh `Prepares` in `front.frame`.
    pub fn over_with(front: Front2<'n, S>, prepared: Option<&'n Prepares<'n>>, alloc: Alloc<'static>) -> ProjectWorld<'n, S> {
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        store.pointers = super::receiver::Pointers::find(&front.index, front.resolver);
        let prepared = match prepared {
            Some(prepared) => prepared,
            None => {
                let bump = front.frame.alloc(blaze_own::Erased::new(front.frame)).bump();
                Prepares::new_in(bump)
            }
        };
        ProjectWorld { front, store, buckets: None, consts: Consts::default(), prepared, alloc, by_chain: None }
    }

    /// The meter the checks file their parts in (front2 tables/cost.rs): a measurement only.
    pub fn set_cost(&mut self, cost: Option<&'n super::tables::cost::Cost<'n>>) {
        self.store.cost = cost;
    }

    /// The request's table of shared trait goals (front2 tables/goals.rs): every body checked
    /// over this world looks its root goals up there and puts what it solved.
    pub fn set_goal_share(&mut self, goals: Option<&'n dyn blaze_solve::share::GoalShare>) {
        self.store.goals = goals;
    }

    /// The shared goals the last body's check took, in order (blaze-solve `GoalHit`).
    pub fn goal_hits(&self) -> &[blaze_solve::share::GoalHit] {
        &self.store.goal_hits
    }

    /// How the head store meets a trait's members' needs (heads.rs `NeedsMet`).
    pub fn set_needs(&mut self, needs: super::heads::NeedsMet) {
        self.store.needs = needs;
    }

    /// How the store's world meets a head it lacks (front2 asks.rs, D161 step 3's seam): under
    /// `Once` the store's board is made now, from the store's own allocator.
    pub fn set_asks(&mut self, met: super::asks::AsksMet) -> Result<(), blaze_core::claim_table::ClaimError> {
        self.store.asks = super::asks::Asks::of(met, self.store.tys.alloc())?;
        Ok(())
    }

    /// The frozen heads the store files every head from (front2 tables/seeds.rs): it lowers none.
    pub fn set_seeds(&mut self, seeds: Option<&'n dyn super::tables::seeds::HeadSeed<'n>>) {
        self.store.seeds = seeds;
    }

    /// Capture the definitions the task evaluates, including its local bodies and callees.
    pub fn record_const_texts(&mut self) {
        use super::tables::recorder::{Recorder, RecorderOpts};
        self.store.maps.const_text_reads = Some(Recorder::new(RecorderOpts { alloc: self.store.tys.alloc() }));
    }

    /// A pooled file job shares this world. Each subsequent body must receive
    /// the defining-text reads even for values the job has already computed.
    pub fn record_shared_const_texts(&mut self) {
        let alloc = self.store.tys.alloc();
        self.store.maps.const_proofs.get_or_insert_with(|| super::heads::ConstProofs::new(alloc));
        self.record_const_texts();
    }

    /// The text reads for this body task. The engine places these on the defining files.
    pub fn take_const_texts(&mut self) -> Result<AVec<'static, super::tables::recorder::TableRead>, super::tables::recorder::ReadsFull> {
        let reads = self.store.maps.const_text_reads.take().expect("the body task enabled constant definition reads");
        reads.distinct().map(|(keys, _)| keys)
    }

    /// Each head the store holds (a fn's, an ADT's, an impl's, a trait's, a constructor's), by
    /// checker key, in the store's order (callers sort): what a body's own store made for it.
    pub fn each_held_head(&self, f: &mut dyn FnMut(u128)) {
        let s = &self.store;
        s.fns.keys().chain(s.adts.keys()).chain(s.impls.keys()).chain(s.trait_defs.keys()).chain(s.ctors.keys()).for_each(|k| f(k.0));
    }

    /// Why the store's last seed failed, taken.
    pub fn take_seed_error(&mut self) -> Option<super::tables::seeds::SeedError> {
        self.store.seed_error.take()
    }

    /// The wants the store made by lowering (heads.rs `Store::lowered`).
    pub fn lowered(&self) -> u32 {
        self.store.lowered
    }

    /// The fn whose head filed opaque type `key` (heads.rs `Store::opaques`), if it is one: an
    /// opaque's key is in no row of the key map, and its head is its fn's.
    pub fn opaque_fn(&self, key: blaze_typeck::ItemKey) -> Option<blaze_typeck::ItemKey> {
        // The default of a provided trait fn's `impl Trait` is that fn's (rpitit.rs `filed_as`).
        self.store.opaques.get(&super::rpitit::filed_as(&self.store, key)).map(|&(fn_, _)| fn_)
    }

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

    /// [`ProjectWorld::listed`] holding what `listing` names (project_records.rs `Listing`):
    /// with `FnsAndInitializers`, each `const` and `static` initializer too.
    pub fn listed_with<'a>(&self, listing: super::project_records::Listing, alloc: Alloc<'a>) -> AVec<'a, Listed<'n>> {
        super::project_records::list_with(self.front.names, listing, alloc)
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

    /// The body that declared a local item, when `key` is a body-local checker key. The
    /// enclosing key is kept with the local head while its body is lowered, so callers can
    /// attach facts of a body-local impl to the `BodyCheck` that publishes them.
    pub fn local_body(&self, key: blaze_typeck::ItemKey) -> Option<blaze_typeck::ItemKey> {
        self.store.locals.head(key).map(|head| head.enclosing)
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
    ///
    /// It takes the production path for an auto-trait goal of an async block's future that
    /// fails only on the check's superset interior (`Stalls::Decide`, as `check_borrows`, which
    /// `blaze check` runs: tables/task.rs): the goal is stalled, each block's witness lowered and
    /// the goal decided on the exact interior (borrows.rs `witness_and_decide`). A harness on
    /// another path than the command's is how "works in a harness" and "the command does it"
    /// came apart before.
    pub fn check<'a>(&mut self, body: BodyRef<'n>, alloc: Alloc<'a>) -> BodyVerdict<'a> {
        let mut reads: AVec<'a, super::reads::Read> = AVec::new_in(alloc);
        let mut decision: Option<super::stalled_goals::Decision<'a>> = None;
        let mut verdict = self.check_then(body, super::stalled_goals::Stalls::Decide, alloc, |r, lowered, done| {
            decision = Some(super::borrows::witness_and_decide(r, lowered, done, &mut reads, alloc));
        });
        // What the witness lowering and the decision read, in the body's read list (rule 21).
        super::borrow_reads::file_borrow_reads(&mut verdict, &reads, alloc);
        if let Some(decision) = decision {
            decision.file(&mut verdict);
        }
        verdict
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
        // One body's shared-goal hits at a time: a task that checks several bodies (a body and
        // its local fns, tables/task.rs) sums them itself; an early return below must not
        // leave the body before's to be counted again.
        self.store.goal_hits.clear();
        let mut verdict =
            BodyVerdict { diagnostics: AVec::new_in(alloc), findings: AVec::new_in(alloc), undecided: AVec::new_in(alloc), gaps: AVec::new_in(alloc), refused: None, reads: None, facts: Ok(BodyReads::not_checked(alloc)), typed: None };
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
        let cost = r.store.cost;
        let lowering = Cost::enter(cost, Part::Lower);
        // Boxed, lowered in a frame of its own: this frame stays on the stack under the whole
        // check (blaze-int notes t29 overflow-bt4.txt: 27,728 bytes).
        let mut lowered = match lower_boxed(&mut r, prepared, body, alloc) {
            Ok(b) => b,
            // A required anonymous body's check can stop on a world gap. That is an
            // unanswered input to this body, not a refusal of the constant itself. Keep the
            // typed gap at the outer body's boundary so the caller can make the same progress
            // decision it would have made if the gap had been reached directly.
            Err(Refused::Length(super::consts::ConstFail::RequiredBody {
                cause: super::consts::RequiredBodyCause::Check(CheckFail::Gap { gaps, .. }),
                ..
            })) => {
                verdict.gaps.extend(gaps);
                return verdict;
            }
            // A macro's own error at a call (`include_str!` of a missing file, `compile_error!`,
            // an unset `env!`: body.rs `call_error`): the program's error there, as rustc
            // reports it, never a refusal. rustc gives these errors no code. `reads` stays
            // `None`: the check did not run, so no certificate stands on it.
            Err(Refused::Lower(BodyError::Calls { errors, .. })) => {
                for e in errors.iter() {
                    let mut text = AString::new_in(alloc);
                    text.push_str("the call of `");
                    text.push_str(&e.path);
                    text.push_str("!`: ");
                    text.push_str(&e.why);
                    verdict.diagnostics.push(BodyDiagnostic { code: "", text, start: e.at.start, end: e.at.end, notes: AVec::new_in(alloc) });
                }
                verdict.diagnostics.sort_by_key(|d| (d.start, d.end, d.code));
                return verdict;
            }
            // A body whose lowering found only errors of the program's TEXT (blaze-lower
            // `DiagKind`: a syntax error, `break` outside a loop, ..): the program's errors
            // there, never a refusal ([`lowering_errors`]). A lowering with any other finding
            // (a construct not lowered yet, a macro call not taken, the fn cfg'd out) stays a
            // refusal. As above, `reads` stays `None`.
            Err(Refused::Lower(BodyError::Lowered { findings, .. })) if findings.iter().all(|f| matches!(f, super::body::BodyFinding::Diag { .. })) => {
                lowering_errors(&findings, &mut verdict, alloc);
                return verdict;
            }
            Err(why) => {
                verdict.refused = Some(why);
                return verdict;
            }
        };
        drop(lowering);
        if !lowered.typeck.inline_fors.is_empty() {
            if let Err(err) = super::inline_for::expand_concrete_body(&mut r, prepared, &mut lowered) {
                match err {
                    CheckFail::Diags { diags, .. } => {
                        for (code, at, about) in diags.iter() {
                            verdict.diagnostics.push(BodyDiagnostic { code: code.code(), text: AString::from_str_in(about, alloc), start: at.start, end: at.end, notes: AVec::new_in(alloc) });
                        }
                    }
                    CheckFail::Gap { gaps, .. } => {
                        for g in gaps {
                            verdict.gaps.push(g);
                        }
                    }
                    CheckFail::InlineFor { at, why, .. } => {
                        verdict.diagnostics.push(BodyDiagnostic { code: "E0277", text: AString::from_str_in(why, alloc), start: at.start, end: at.end, notes: AVec::new_in(alloc) });
                    }
                    other => verdict.refused = Some(Refused::Check(other)),
                }
                verdict.diagnostics.sort_by_key(|d| (d.start, d.end, d.code));
                return verdict;
            }
        }
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
        let rounds = Cost::enter(cost, Part::Rounds);
        let checked = super::body_reads::check_body_filed_as(&mut r, &*lowered, &scope, None, Some(&mut asked), stalls);
        drop(rounds);
        let _reads = Cost::enter(cost, Part::Reads);
        // The buckets as the check left them: what each impl question read, per crate, kept to
        // the crates the body sees (its check world's, `Reveal::InBody`: buckets.rs `Seen`).
        let seen = super::buckets::Seen::of_checked_body(r.names, lowered.module).map_err(ReadsError::Unseen);
        let buckets = r.buckets.as_ref();
        let whole = |list: &blaze_typeck::ReadList<'_>| -> Result<BodyReads<'a>, ReadsError> {
            let buckets = match buckets {
                Some(b) => Some((b, seen?)),
                None => None,
            };
            let rows = facts_with_buckets(list.questions(), Some(list.rows.column::<1>()), module, written.paths, written.segments, buckets, alloc)?;
            Ok(BodyReads { rows, reach: Reach::Whole })
        };
        verdict.facts = match (&checked, &asked.finished) {
            (Ok((_, filed)), _) => whole(&filed.reads),
            (Err(_), Some(list)) => whole(list),
            (Err(_), None) => match buckets {
                Some(b) => seen.and_then(|seen| facts_with_buckets(&asked.questions, None, module, written.paths, written.segments, Some((b, seen)), alloc)),
                None => facts_with_buckets(&asked.questions, None, module, written.paths, written.segments, None, alloc),
            }
            .map(|rows| BodyReads { rows, reach: Reach::Asked }),
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
                    verdict.diagnostics.push(BodyDiagnostic { code: d.code.code(), text: AString::new_in(alloc), start: d.at.start, end: d.at.end, notes: AVec::new_in(alloc) });
                }
                for f in done.findings.iter() {
                    verdict.findings.push(lint_finding(&r, f, alloc));
                }
                for u in done.undecided.iter() {
                    let at = u.at();
                    let text = match *u {
                        blaze_typeck::Undecided::TargetFeatureCall { callee, .. } => {
                            format!("the call of `{}` is not checked for `unsafe`", super::check::item_shown(&r, callee))
                        }
                    };
                    verdict.undecided.push(BodyUndecided { cause: u.says(), text: AString::from_str_in(&text, alloc), start: at.start, end: at.end });
                }
                then(&mut r, &*lowered, &done);
            }
            Err(CheckFail::Diags { diags, .. }) => {
                for (code, at, about) in diags.iter() {
                    verdict.diagnostics.push(BodyDiagnostic { code: code.code(), text: AString::from_str_in(about, alloc), start: at.start, end: at.end, notes: AVec::new_in(alloc) });
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

/// A typed lint's finding with its words, the struct named by its path (rule 19).
fn lint_finding<'a, 'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, f: &blaze_typeck::TypedFinding, alloc: Alloc<'a>) -> BodyLintFinding<'a> {
    let at = f.at();
    let mut text = AString::new_in(alloc);
    match *f {
        blaze_typeck::TypedFinding::ArgsInline { misuse, adt, .. } => {
            let shown = super::item_shown::ItemShown::of(r.names, &*r.keys, adt);
            text.push_str("`");
            match shown.path() {
                Some(path) => text.push_str(path),
                None => text.push_str(&shown.boxed()),
            }
            text.push_str("` is an options struct (`#[blaze::args]`), a call's named arguments and nothing else: it ");
            text.push_str(misuse.says());
        }
        // rustc's words for `unsafe_op_in_unsafe_fn` (edition 2024).
        blaze_typeck::TypedFinding::UnsafeOpInUnsafeFn { op, .. } => {
            text.push_str(&super::check::unsafe_op_shown(r, op));
            text.push_str(" is unsafe and requires unsafe block (the body of an `unsafe fn` is no unsafe block in edition 2024)");
        }
    }
    BodyLintFinding { lint: f.lint(), text, start: at.start, end: at.end }
}

/// Lowers `body` by [`lower_made`] (each head and constant its lowering needs made first); a
/// member's container head is made first, each constant an array length in it names
/// evaluated, at most [`MOST_LENS`] of them.
/// [`lower`], boxed in `alloc` in a frame of its own ([`ProjectWorld::check_then`]'s frame
/// holds the box's two words, not the body).
#[inline(never)]
fn lower_boxed<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, body: BodyRef<'n>, alloc: Alloc<'a>) -> Result<blaze_own::Box<Body<'n>, Alloc<'a>>, Refused> {
    Ok(blaze_own::Box::new_in(lower(r, prepared, body)?, alloc))
}

const _: () = assert!(core::mem::size_of::<blaze_own::Box<Body<'static>, Alloc<'static>>>() == 2 * core::mem::size_of::<usize>());

fn lower<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, body: BodyRef<'n>) -> Result<Body<'n>, Refused> {
    let target = match body {
        BodyRef::Free(lib) => LowerTarget::Free(lib),
        BodyRef::Member { container, is_trait, kind, name } => {
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
            let k = r.keys.filed_member(checker_of(container), kind, name).ok_or(Refused::MemberNotFiled)?;
            // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`), never made
            // with its impl's head; its own refusal is its `fn_gaps` row, read just below. An
            // impl's `const` is made with its impl's head (heads/member_fn.rs `member_head_want`).
            if !is_trait && kind == MemberKind::Fn {
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

/// The code a body's text-does-not-parse error is filed under: blaze's code of a syntax error.
/// blaze-cli engine_bodies.rs `found_of` files a diagnostic of this code as its `SYNTAX` error
/// with the PARSER's own words ("expected expression"), found where the lowering's diagnostic
/// is, in the entry's text parsed once more for that body alone: the lowering's `Diag` keeps
/// the kind and place, not the parser's message.
pub const SYNTAX_CODE: &str = "syntax";

/// The program's errors of a body whose lowering found only errors of its text (each finding a
/// `BodyFinding::Diag`), filed in `verdict` sorted by place. A syntax error first: ONE error,
/// the first in the text, as the parser's recovery makes whatever follows it (a second
/// diagnostic after a parse error is the recovery's, not the program's). Without one, each
/// diagnostic is its own error, with rustc's code where rustc has one and its words. Every
/// kind is named here (no `_`), so a new kind of the lowering is decided, never swept in.
fn lowering_errors<'a>(findings: &[super::body::BodyFinding], verdict: &mut BodyVerdict<'a>, alloc: Alloc<'a>) {
    use blaze_lower::form::DiagKind as K;
    use super::body::BodyFinding;
    let diags = || findings.iter().filter_map(|f| match f {
        BodyFinding::Diag { kind, range } => Some((*kind, *range)),
        BodyFinding::Macro { .. } | BodyFinding::Unhandled { .. } | BodyFinding::CfgOut => None,
    });
    let push = |verdict: &mut BodyVerdict<'a>, code: &'static str, text: AString<'a>, range: blaze_surface::Range| {
        verdict.diagnostics.push(BodyDiagnostic { code, text, start: range.start, end: range.end, notes: AVec::new_in(alloc) });
    };
    if let Some((_, range)) = diags().filter(|(kind, _)| *kind == K::Syntax).min_by_key(|(_, range)| (range.start, range.end)) {
        push(verdict, SYNTAX_CODE, AString::from_str_in("this body's text does not parse here", alloc), range);
        return;
    }
    for (kind, range) in diags() {
        let mut text = AString::new_in(alloc);
        // rustc's code and words for each (the Reference's rule where rustc has no code).
        let code = match kind {
            K::Syntax => SYNTAX_CODE,
            K::BreakOutsideLoop => {
                text.push_str("`break` or `continue` outside of a loop");
                "E0268"
            }
            K::UndeclaredLabel => {
                text.push_str("use of undeclared label");
                "E0426"
            }
            K::ContinueToBlock => {
                text.push_str("`continue` pointing to a labeled block");
                "E0696"
            }
            K::AwaitOutsideAsync => {
                text.push_str("`await` is only allowed inside `async` functions and blocks");
                "E0728"
            }
            K::YieldOutsideCoroutine => {
                text.push_str("yield expression outside of coroutine literal");
                "E0627"
            }
            K::InclusiveRangeNoEnd => {
                text.push_str("inclusive range with no end");
                "E0586"
            }
            K::FormatArgMissing => {
                text.push_str("invalid reference to positional argument in format string");
                ""
            }
            K::FruInDestructuringAssignment => {
                text.push_str("functional record updates are not allowed in destructuring assignments");
                ""
            }
            K::PatternShadows => {
                text.push_str("this identifier pattern may not bind its name: it names a static, a tuple struct or a tuple variant (or is a `ref`, `mut` or `@` binding of a constant, a unit struct or a unit variant)");
                "E0530"
            }
            K::Asm(rule) => {
                text = blaze_own::format_in!(alloc, "this `asm!` breaks the Reference's inline-assembly rule {rule:?}");
                ""
            }
        };
        push(verdict, code, text, range);
    }
    verdict.diagnostics.sort_by_key(|d| (d.start, d.end, d.code));
}

