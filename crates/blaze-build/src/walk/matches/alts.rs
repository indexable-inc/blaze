//! A guarded arm whose pattern holds or-patterns: its guard runs once for EACH alternative
//! that matches, in order, until one run is true.
//!
//! The Reference, "Match expressions", "Match guards":
//! - [expr.match.guard.behavior] "When the pattern matches successfully, the pattern guard is
//!   executed. If all of the guard condition operands evaluate to true and all of the `let`
//!   patterns successfully match their scrutinees, the match arm is successfully matched
//!   against and the arm body is executed."
//! - [expr.match.guard.next] "Otherwise, the next pattern, including other matches with the
//!   `|` operator in the same arm, is tested."
//! - its note: "Multiple matches using the `|` operator can cause the pattern guard and the
//!   side effects it has to execute multiple times", with `match 1 { 1 | _ if { i.set(i.get()
//!   + 1); false } => {} _ => {} }` after which `i.get()` is 2.
//!
//! and "Patterns", "Or-patterns", [patterns.behavior.nested-or-patterns]: "The dynamic
//! semantics of pattern matching a scrutinee expression `e_s` against a pattern `c(p | q,
//! ..rest)` at depth `d` where `c` is some constructor, `p` and `q` are arbitrary patterns, and
//! `rest` is optionally any remaining potential factors in `c`, is defined as being the same
//! as that of `c(p, ..rest) | c(q, ..rest)`."
//!
//! So a guarded arm `P if g => body` IS the arms `P_1 if g => body`, .., `P_n if g => body`,
//! `P_1..P_n` the or-free patterns `P` expands to by that rule, applied to the first
//! or-pattern in written order first (the leftmost or-pattern is the slowest digit:
//! `(a | b, c | d)` is `(a, c) | (a, d) | (b, c) | (b, d)`; the Reference's rule names the
//! or-pattern as the constructor's first factor and gives no order for two or-patterns in
//! one constructor: this is its rule read left to right). A failed guard goes on to `P_(i+1)`
//! with the bindings `P_(i+1)` makes; the last one's goes to the next arm.
//!
//! Built as that: one chain of tests per expansion (walk/matches.rs `test_pat` told which
//! alternative of each or-pattern to take), each storing its bindings into the names' one
//! slots (matches/forms.rs) and its own number into a word, all meeting at ONE guard and ONE
//! body (an expression is built once: its slots, scopes and temporaries are one each); a false
//! guard switches on the word to the chain after the one that matched. An arm with no guard
//! needs none of this: its tests have no effects, so the first alternative that matches at
//! each or-pattern is the first expansion that matches.
//!
//! The expansions of one arm are capped at [`MOST_ALTS`]; past it the arm is refused by name.

use core::alloc::Allocator;

use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Flags, Ty};
use blaze_own::Vec;
use blaze_typeck::{Pat, PatId};

use super::super::{Repr, Walk, malformed};
use super::At;
use crate::BuildError;

/// The most or-free patterns one guarded arm's pattern may expand to.
pub(super) const MOST_ALTS: u32 = 64;

/// One or-pattern's choice in an expansion: the or-pattern, the alternative taken, how many
/// alternatives it has.
pub(in super::super) type Choice = (PatId, u32, u32);

/// The word an expansion's number is kept in.
const WORD: Repr = Repr::Int { ty: Ty::I32, signed: false };

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether pattern `root` holds an or-pattern, by a work list.
    pub(in super::super) fn has_or(&self, root: PatId) -> Result<bool, BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        while let Some(q) = todo.pop() {
            match self.pat(q)? {
                Pat::Or { .. } => return Ok(true),
                Pat::Bind { sub: Some(s), .. } => todo.push(s),
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    for &s in subs {
                        todo.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps {
                        todo.push(fp.pat);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Completes `choice`, the or-patterns of `root` in written order (pre-order, left to
    /// right) with the alternative each takes: the entries it holds are kept (the walk must
    /// meet the same or-patterns again), and every or-pattern met after them takes its first
    /// alternative. Only the alternative taken is entered, so an or-pattern inside another
    /// alternative is not met.
    fn choose(&self, root: PatId, choice: &mut Vec<Choice, A>) -> Result<(), BuildError> {
        let kept = choice.len();
        let mut met = 0usize;
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        // Children are pushed last first, so they pop in written order.
        while let Some(q) = todo.pop() {
            match self.pat(q)? {
                Pat::Or { alts } => {
                    let list = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern past `pat_lists`"))?;
                    let j = if met < kept {
                        let (was, j, _) = choice[met];
                        if was != q {
                            return Err(malformed("an arm's expansion met another or-pattern than the one it chose for"));
                        }
                        j
                    } else {
                        choice.push((q, 0, list.len() as u32));
                        0
                    };
                    met += 1;
                    todo.push(*list.get(j as usize).ok_or(malformed("an or-pattern of no alternatives"))?);
                }
                Pat::Bind { sub: Some(s), .. } => todo.push(s),
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    for &s in subs.iter().rev() {
                        todo.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps.iter().rev() {
                        todo.push(fp.pat);
                    }
                }
                _ => {}
            }
        }
        if met < kept {
            return Err(malformed("an arm's expansion met fewer or-patterns than it chose for"));
        }
        Ok(())
    }

    /// Moves `choice` to the next expansion: the last or-pattern with an alternative left
    /// takes it, and the or-patterns after it are forgotten (`choose` meets them anew, each at
    /// its first alternative). False after the last expansion.
    fn advance(choice: &mut Vec<Choice, A>) -> bool {
        while let Some((p, j, n)) = choice.pop() {
            if j + 1 < n {
                choice.push((p, j + 1, n));
                return true;
            }
        }
        false
    }

    /// The tests of guarded arm pattern `root` (it holds an or-pattern) at `at`, from the
    /// current block: one chain per expansion in order (the module doc), each going to `pass`
    /// with its bindings stored and its number in the word, a chain that fails to the next
    /// one, the last to `next`. Returns the word, where the arm's blocks start in
    /// `arm_blocks` (block `j` there is where a guard that failed after expansion `j` goes
    /// on) and how many expansions there are, for `guard_alts`.
    pub(super) fn test_alts(&mut self, root: PatId, at: At, pass: blaze_lir::Block, next: blaze_lir::Block) -> Result<(SlotId, u32, u32), BuildError> {
        let live = self.live;
        let (size, align) = self.size_align(WORD);
        let word = self.b.slot(size, align, SlotKind::Temp);
        let chains = self.arm_blocks.len() as u32;
        let mut choice: Vec<Choice, A> = Vec::new_in(self.alloc);
        let mut n = 0u32;
        loop {
            self.choose(root, &mut choice)?;
            if n == MOST_ALTS {
                return Err(BuildError::UnsupportedPat {
                    pat: root,
                    what: "a guarded arm whose or-patterns expand to more than 64 patterns (its guard runs once per expansion that matches)",
                });
            }
            let matched = self.create();
            let following = self.create();
            self.arm_blocks.push(following);
            self.test_pat_chosen(root, at, matched, following, Some(choice.as_slice()))?;
            self.begin(matched);
            self.live = live;
            let a = self.b.slot_addr(word);
            let k = self.b.iconst(Ty::I32, u128::from(n));
            let attrs = self.mem(WORD);
            self.b.store(Flags::NONE, k, a, 0, attrs);
            self.jump(pass, &[]);
            n += 1;
            self.begin(following);
            self.live = live;
            if !Self::advance(&mut choice) {
                self.jump(next, &[]);
                return Ok((word, chains, n));
            }
        }
    }

    /// The guard of an arm `test_alts` tested, its value on top: true goes on to the body;
    /// false to the chain after the expansion that matched (its number is in `word`), which
    /// for the last one is the next arm.
    pub(in super::super) fn guard_alts(&mut self, word: SlotId, chains: u32, n: u32) -> Result<(), BuildError> {
        let g = self.pop()?;
        let c = self.cond(g)?;
        let body = self.create();
        let failed = self.create();
        self.br(c, (body, &[]), (failed, &[]));
        self.begin(failed);
        let a = self.b.slot_addr(word);
        let attrs = self.mem(WORD);
        let k = self.b.load(Ty::I32, Flags::NONE, a, 0, attrs);
        let after = |w: &Self, j: u32| w.arm_blocks.get((chains + j) as usize).copied().ok_or(malformed("a guarded arm's chains past `arm_blocks`"));
        let last = n.checked_sub(1).ok_or(malformed("a guarded arm of no expansions"))?;
        let mut cases: Vec<(u128, blaze_lir::Block), A> = Vec::new_in(self.alloc);
        for j in 0..last {
            cases.push((u128::from(j), after(self, j)?));
        }
        let default = after(self, last)?;
        self.switch(k, default, &cases);
        self.begin(body);
        Ok(())
    }
}
