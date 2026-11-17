//! What each local of a lowered body stands for in the program: the ONE table of identities
//! (ASYNC.md P8, main's ruling of 2026-11-16, async-P8-4). The flow layer names every local it
//! makes and emits the table in `LoweredAsync::identity`; the builder never invents a slot that
//! lives across a suspension: at each such site it looks the local up here by the same key and
//! writes the value to that local's place in the state. A slot the builder keeps across a
//! suspension that this table does not name is refused, naming the expression: a gap of the
//! flow body, fixed there (a temporary the flow does not know is also missing from what the
//! check believes is held across the `.await`).
//!
//! A key is the program's own: a binding's pattern, or an expression and the role of the
//! local made for it. Two locals of one body may share a key only when at most one is saved
//! across a suspension (state_recipe.rs refuses a saved key twice, naming the expression: the
//! fix is a role of its own for one of them, here). Not `blaze_flow::Origin`, which is a
//! lifetime origin of the borrow check.

use blaze_typeck::{ExprId, PatId};

/// Which local an expression made, when it makes more than one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Role {
    /// The expression's value (a call's result, an operand held while the others are
    /// evaluated, a value adjusted by unsizing or a pointer cast).
    Value,
    /// A borrow taken for the expression: its adjustment's autoref (`&x` for `x.method()`), an
    /// overloaded deref's or index's receiver (`Index::index(&base, i)`), a guard's fake borrow.
    Borrow,
    /// An `.await`'s future `tmp`, `into_future`'s result (the Reference, "Await expressions").
    Awaitee,
    /// An `.await`'s `Pin<&mut tmp>`, made at the loop's head each poll.
    Pin,
    /// The result of a call the lowering makes for the expression: an `.await`'s value (what
    /// `Poll::Ready` held), an overloaded deref's or index's reference.
    Result,
}

/// What a local stands for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Identity {
    /// Local 0: the return place.
    Return,
    /// Argument local `k` (`1..=args`): a capture of an async block, a parameter of a fn.
    Arg(u32),
    /// The binding of pattern `p`.
    Binding(PatId),
    /// A temporary made for expression `e` in role `r`.
    Temp(ExprId, Role),
}

impl Identity {
    /// The expression a refusal about this local names: the temporary's own, else `None` (a
    /// binding is named by its pattern, an argument or the return place by the body).
    pub fn expr(self) -> Option<ExprId> {
        match self {
            Identity::Temp(e, _) => Some(e),
            Identity::Return | Identity::Arg(_) | Identity::Binding(_) => None,
        }
    }
}
