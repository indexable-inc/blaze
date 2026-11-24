//! A kept body check asked again: its read list (each question with its answer's digest)
//! answered by today's world through the same recorder, so the kept result is reused with no
//! check when every answer is as it was (read-addressed.md, "End state: what an edit costs",
//! the body row; rules work-once and verify-once: the replay is the one check a kept record
//! gets, and `Holds` is its proof). A body whose own text moved has another key and is never
//! replayed: the caller checks it.
//!
//! The questions are taken in the read list's order (sorted by question), so the verdict and
//! the question it names are the same on any machine and in any run. The digest compared is
//! the one the check recorded, made by the same code (`Reads::digest`), so a replay can only
//! disagree with a check where an answer moved.
//!
//! What a replay cannot ask is said by name, never guessed: a `ConstHolds` question holds the
//! digest of ground arguments the check inferred, which the read list does not keep, and a
//! `Path` question whose path the body no longer writes at that scope names no segments to ask
//! with. Both are [`Replay::Unaskable`]: the body is checked again.

use blaze_own::{Alloc, Vec};

use crate::body::BodyView;
use crate::error::{CheckError, Stop};
use crate::ids::Name;
use crate::world::{Question, ReadList, Reads, World};

/// What a replay of a kept read list found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    /// Every question read gets an answer with the digest kept: the kept result stands.
    Holds,
    /// The first question, in read-list order, whose answer's digest moved: check again.
    Moved(Question),
    /// The first question the world has no answer to now: fetch it, then replay again.
    Missing(Question),
    /// The first question a replay cannot ask (its inputs were the check's own): check again.
    Unaskable(Question),
}

/// Whether the read list `kept`, recorded by a check of `body`, holds over `world` today.
/// `Err` only for a malformed body or world, as a check would refuse it.
pub fn replay<'a, W: World>(world: &W, body: &BodyView<'_>, kept: &ReadList<'_>, alloc: Alloc<'a>) -> Result<Replay, CheckError> {
    let mut reads = Reads::new_in(world, alloc);
    let mut names: Vec<Name, Alloc<'a>> = Vec::new_in(alloc);
    let questions = kept.rows.column::<0>();
    let digests = kept.rows.column::<1>();
    for (&q, &was) in questions.iter().zip(digests.iter()) {
        let holes = reads.hole_count();
        match ask(&mut reads, body, q, &mut names) {
            Ok(Asked::Answered) => {}
            Ok(Asked::Unaskable) => return Ok(Replay::Unaskable(q)),
            Err(Stop::Need(need)) => return Ok(Replay::Missing(need)),
            Err(Stop::Error(error)) => return Err(error),
        }
        if reads.hole_count() != holes {
            return Ok(Replay::Missing(q));
        }
        // No answer recorded today where the kept list holds one (an opaque revealed to the
        // check and seen as itself now: world.rs `Reads::opaque_record`): the answer moved.
        if reads.recorded(q) != Some(was) {
            return Ok(Replay::Moved(q));
        }
    }
    Ok(Replay::Holds)
}

/// Whether [`ask`] put the question to the world.
enum Asked {
    Answered,
    Unaskable,
}

/// Puts `q` to the world through the recorder, as the check did: the recorder files its digest.
fn ask<'w, 'a, W: World>(reads: &mut Reads<'w, 'a, W>, body: &BodyView<'_>, q: Question, names: &mut Vec<Name, Alloc<'a>>) -> Result<Asked, Stop> {
    match q {
        Question::FnSig(item) => {
            reads.fn_sig(item)?;
        }
        Question::Predicates(item) => {
            reads.predicates(item)?;
        }
        Question::Adt(item) => {
            reads.adt(item)?;
        }
        Question::ImplsOf(trait_, shape) => {
            reads.impls_of(trait_, shape)?;
        }
        Question::ImplHeader(item) => {
            reads.impl_header(item)?;
        }
        Question::Inherent(shape, name) => {
            reads.inherent(shape, name)?;
        }
        Question::AssocItem(container, name) => {
            reads.assoc_item(container, name)?;
        }
        Question::TraitsInScope(scope) => {
            reads.traits_in_scope(scope)?;
        }
        Question::Path(scope, path) => {
            // The check asked with every segment's name of the body's path at `path`
            // (rules/lit_path.rs `value_path`, rules/adt.rs `struct_path`): the same here.
            let Some(written) = body.paths.get(path.ix()).copied() else {
                return Ok(Asked::Unaskable);
            };
            if written.scope != scope || written.lang.is_some() {
                return Ok(Asked::Unaskable);
            }
            names.clear();
            for segment in body.segment_list(written.segments)? {
                names.push(segment.name);
            }
            // Asked again for its read (and its digest): the answer itself is not used here.
            let _answer = reads.resolve_path(scope, path, names)?;
        }
        Question::Lang(item) => {
            reads.lang_item(item)?;
        }
        Question::LangPath(item) => {
            reads.lang_path(item)?;
        }
        Question::Const(item) => {
            reads.const_value(item)?;
        }
        Question::Opaque(item) => {
            reads.opaque_hidden(item)?;
        }
        Question::Attrs(item) => {
            reads.attrs(item)?;
        }
        Question::TraitDef(item) => {
            reads.trait_def(item)?;
        }
        Question::AssocParent(item) => {
            reads.assoc_parent(item)?;
        }
        Question::ConstHolds(..) => return Ok(Asked::Unaskable),
        Question::ConstParams(item) => {
            reads.const_params(item)?;
        }
        Question::Closure(owner, ordinal) => {
            reads.closure(owner, ordinal)?;
        }
        Question::Coroutine(owner, ordinal) => {
            reads.coroutine(owner, ordinal)?;
        }
        Question::AssocGenerics(trait_, name) => {
            reads.assoc_generics(trait_, name)?;
        }
        Question::TypeAlias(item) => {
            reads.type_alias(item)?;
        }
        Question::FieldDefaults(adt) => {
            reads.field_defaults(adt)?;
        }
        Question::MemberVisible(container, name) => {
            reads.member_visible(container, name)?;
        }
        Question::MemberUnstable(container, name) => {
            reads.member_unstable(container, name)?;
        }
    }
    Ok(Asked::Answered)
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_own::{Alloc, Vec};

    use super::{Replay, replay};
    use crate::body::{BindAnn, BodyView, Expr, Pat};
    use crate::columns::Checked;
    use crate::ids::{Edition, ExprId, List, Name, PatId, Range, ScopeId};
    use crate::infer::InferCtx;
    use crate::portable::PTys;
    use crate::toy::{FieldTy, ToyStruct, ToyWorld, struct_key};
    use crate::world::{Needs, Question};
    use crate::{Outcome, check};

    /// The field every struct of the test has.
    const F: Name = Name(100);

    /// `S0 { f: i32 }` and `S1 { f: u8 }`; the owner is `fn(p: S0) -> i32`.
    fn world() -> ToyWorld {
        let mut structs = Vec::new_in(Global);
        for ty in [FieldTy::I32, FieldTy::U8] {
            let mut fields = Vec::new_in(Global);
            fields.push((F, ty));
            structs.push(ToyStruct { fields });
        }
        match ToyWorld::new(&structs, &[FieldTy::Struct(0)], FieldTy::I32) {
            Ok(w) => w,
            Err(e) => panic!("the toy world could not be built: {e}"),
        }
    }

    static EXPRS: [Expr; 3] = [
        Expr::Local(PatId(0)),
        Expr::Field { base: ExprId(0), name: F },
        Expr::Block { stmts: List::EMPTY, tail: Some(ExprId(1)), scope: ScopeId(0) },
    ];
    static EXPR_RANGES: [Range; 3] = [Range { start: 0, end: 1 }, Range { start: 1, end: 2 }, Range { start: 2, end: 3 }];
    static PATS: [Pat; 1] = [Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name(0), sub: None, ty: None }];
    static PAT_RANGES: [Range; 1] = [Range { start: 0, end: 1 }];
    static PARAMS: [PatId; 1] = [PatId(0)];

    /// `{ p.f }`, the owner's body: it reads the owner's signature and `S0`, never `S1`.
    fn body(w: &ToyWorld) -> BodyView<'static> {
        BodyView {
            owner: w.owner,
            edition: Edition::E2021,
            params: &PARAMS,
            root: ExprId(2),
            exprs: &EXPRS,
            expr_ranges: &EXPR_RANGES,
            pats: &PATS,
            pat_ranges: &PAT_RANGES,
            stmts: &[],
            expr_lists: &[],
            pat_lists: &[],
            stmt_lists: &[],
            arms: &[],
            field_inits: &[],
            field_pats: &[],
            paths: &[],
            segments: &[],
            segment_ranges: &[],
            types: PTys::EMPTY,
            type_lists: &[],
            type_list_ranges: &[],
            method_name_ranges: &[],
            opt_types: &[],
            fmt_uses: &[],
            coroutine: None,
            super_lets: &[],
            asm_pieces: &[],
            asm_operands: &[],
            asm_clobbers: &[],
            asm_text: "",
            call_arg_ints: &[],
            legacy_args: &[],
            unsafe_blocks: &[],
            expr_editions: &[],
            pat_editions: &[],
            record_expr_schemas: &[],
            closure_static_params: &[],
        }
    }

    fn checked(w: &ToyWorld) -> Checked<'static> {
        let mut ty = InferCtx::new_in(Alloc::global());
        match check(&body(w), w, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            Ok(Outcome::Need(n)) => panic!("a full world was missing {:?}", n.questions),
            Err(e) => panic!("the check failed: {e}"),
        }
    }

    fn replayed(w: &ToyWorld, kept: &Checked<'_>) -> Replay {
        match replay(w, &body(w), &kept.reads, Alloc::global()) {
            Ok(r) => r,
            Err(e) => panic!("the replay failed: {e}"),
        }
    }

    /// A kept check stands exactly while every answer it read stands: over the world it was
    /// made in it holds; an answer it never read (`S1`) moves nothing, and a fresh check there
    /// gives the same result; the answer it read (`S0`) moved is named; an answer the world
    /// no longer has is named as missing. A replay that always said `Holds` fails the third
    /// assertion, one that compared no digest the third too, one that ignored holes the fourth.
    /// Expected: `test replay::tests::a_replay_holds_until_an_answer_it_read_moves ... ok`.
    #[test]
    fn a_replay_holds_until_an_answer_it_read_moves() {
        let w = world();
        let kept = checked(&w);
        assert!(kept.reads.holds(&Question::Adt(struct_key(0))), "the body reads S0: {:?}", kept.reads);
        assert!(!kept.reads.holds(&Question::Adt(struct_key(1))), "the body never reads S1: {:?}", kept.reads);
        assert_eq!(replayed(&w, &kept), Replay::Holds, "over the world it was checked in");

        let mut unread = world();
        unread.perturb(1);
        assert_eq!(replayed(&unread, &kept), Replay::Holds, "S1 changed, which the body never read");
        assert!(kept.same_result(&checked(&unread)), "a replay that holds must equal a fresh check");

        let mut read = world();
        read.perturb(0);
        assert_eq!(replayed(&read, &kept), Replay::Moved(Question::Adt(struct_key(0))), "S0 changed, which the body read");

        let mut gone = world();
        gone.missing.push(Question::Adt(struct_key(0)));
        assert_eq!(replayed(&gone, &kept), Replay::Missing(Question::Adt(struct_key(0))), "S0's record is not at hand");
    }

    /// The opaque's digest in a run over `w` that asks its hidden type twice (as a check asks
    /// it in its last round and again in the stalled-goal decision after it: front2
    /// stalled_goals.rs), with the read list the run leaves.
    fn opaque_read(w: &ToyWorld, opaque: crate::ids::ItemKey) -> (Option<u64>, crate::world::ReadList<'static>) {
        let alloc = blaze_own::testing::heap().alloc();
        let mut reads = crate::world::Reads::new_in(w, alloc);
        for _ in 0..2 {
            if let Err(e) = reads.opaque_record(opaque) {
                panic!("the opaque's hidden type was not answered: {e:?}");
            }
        }
        let (list, _) = reads.finish(alloc);
        (list.hash_of(&Question::Opaque(opaque)), list)
    }

    /// Rule 21 on an opaque seen as itself (tempo, 2026-11-21: 22 bodies refused "the fact
    /// Opaque .. has two answers (09804c76d055e559, ..)", the first the digest of "not
    /// revealed" in every row). A world that sees the opaque as itself reads nothing of the
    /// program: no row; so the list a check made before a reveal and the decision's after it
    /// never hold two answers to it. TWIN: a revealed opaque IS a read, one row, and its
    /// digest moves with the hidden type (an edit of the defining body that changes it
    /// rechecks the reader); a replay of that list over a world that no longer reveals it
    /// names the opaque as moved (no answer recorded where one was kept), never `Holds`.
    /// Planted: recording `None` as an answer (world.rs `Reads::opaque_record`) fails the
    /// first assertion.
    /// Expected: `test replay::tests::an_opaque_seen_as_itself_is_no_read_and_a_revealed_one_is ... ok`.
    #[test]
    fn an_opaque_seen_as_itself_is_no_read_and_a_revealed_one_is() {
        let opaque = crate::ids::ItemKey(0x7_0000);
        let unrevealed = world();
        let (seen_as_itself, list) = opaque_read(&unrevealed, opaque);
        assert_eq!(seen_as_itself, None, "an opaque seen as itself is no read: {list:?}");

        let mut small = world();
        let mut wide = world();
        if let Err(e) = small.reveal(opaque, FieldTy::U8).and_then(|()| wide.reveal(opaque, FieldTy::I32)) {
            panic!("the hidden types could not be made: {e}");
        }
        let (s, small_list) = opaque_read(&small, opaque);
        let (w, _) = opaque_read(&wide, opaque);
        assert!(s.is_some() && w.is_some(), "TWIN: a revealed opaque is a read: {small_list:?}");
        assert_eq!(small_list.rows.len(), 1, "TWIN: asked twice, one row: {small_list:?}");
        assert_ne!(s, w, "TWIN: the read's digest moves with the hidden type");

        let replay_over = |w: &ToyWorld| match replay(w, &body(w), &small_list, blaze_own::testing::heap().alloc()) {
            Ok(r) => r,
            Err(e) => panic!("the replay failed: {e}"),
        };
        assert_eq!(replay_over(&small), Replay::Holds, "over the world it was read in");
        assert_eq!(replay_over(&unrevealed), Replay::Moved(Question::Opaque(opaque)), "a revealed read replayed where the opaque is seen as itself");
    }
}
