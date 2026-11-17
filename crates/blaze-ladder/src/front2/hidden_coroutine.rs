//! A hidden type that is an async block's future, grounded for the code builder: the one
//! function behind `opaque_hidden`'s coroutine arm (front2/coroutines.rs).
//!
//! The Reference, "Async functions": `async fn f(x: A) -> T { body }` "is roughly equivalent
//! to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`; "Async blocks": "An
//! async block is a variant of a block expression which evaluates to a future". So an `async
//! fn`'s hidden type is the future of its root block, whose `Output` is the fn's declared
//! return type.
//!
//! Since async P3 the checker's writeback names that future by its IDENTITY across bodies,
//! `PNode::Coroutine { owner, ordinal, args }` (blaze-typeck coroutine_id.rs: the block's
//! ordinal among `owner`'s async blocks in expression order; `args` the owner's generic
//! arguments, in the defining record's table). The builder's ground form is still
//! `Future(ordinal, output, FnDef(owner, owner_args))` (blaze-build coroutine.rs `identity`),
//! so this file makes that form: the block found by its ordinal in the OWNER's body (the
//! builder's `coroutines_in`, the same rule as the checker's `async_ordinal`), its output read
//! from the owner's checked expression type of that block (`PNode::Future(block, output, _)`,
//! the checker's in-body form), grounded at the owner's ground arguments.
//!
//! Refused by name, never guessed: an ordinal the owner's body does not hold, an expression
//! type that is not that block's own future, an index past a table.

use blaze_build::BuildError;
use blaze_build::layout::LayoutFacts;
use blaze_own::{Alloc, Vec};
use blaze_typeck::{BodyView, ExprId, ItemKey, List, PNode, PTy, PTys, PortableTys};

/// The async block at `ordinal` in its owner's `body` (blaze-build `coroutines_in`, the rule
/// the checker's `async_ordinal` names the hidden type by).
pub(crate) fn block_at(body: &BodyView<'_>, ordinal: u32) -> Result<ExprId, BuildError> {
    blaze_build::coroutines_in(body)
        .find(|c| c.0 == ordinal)
        .map(|c| c.1)
        .ok_or(BuildError::Malformed { what: "a hidden coroutine (`PNode::Coroutine`) whose ordinal its owner's body does not hold" })
}

/// The owner's ground arguments of a hidden coroutine: each type of `list` (in `from`, the
/// defining record's table) grounded at `args` (the opaque's ground arguments), in order.
pub(crate) fn ground_args<'s>(
    from: PTys<'_>,
    list: List,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'s>,
    facts: &dyn LayoutFacts,
) -> Result<Vec<PTy, Alloc<'s>>, BuildError> {
    let written = from.list(list).map_err(|_| BuildError::Malformed { what: "a hidden coroutine's arguments past its record's table" })?;
    let mut out: Vec<PTy, Alloc<'s>> = Vec::with_capacity_in(written.len(), scratch);
    for &t in written {
        out.push(blaze_build::ground(from, t, args, ground, scratch, facts)?);
    }
    Ok(out)
}

/// The ground type of async block `block` (ordinal `ordinal`) of `owner` at `owner_args`
/// (ground), in `ground`: `blaze_build::coroutine_identity` with the block's output. `from` and
/// `expr_ty` are the OWNER's check record (`Checked::types`, `Checked::expr_ty`): the block's
/// expression type there is its own future, `PNode::Future(block, output, _)`, with `output` of
/// the owner's generic parameters.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hidden_coroutine(
    from: PTys<'_>,
    expr_ty: &[PTy],
    block: ExprId,
    owner: ItemKey,
    ordinal: u32,
    owner_args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    facts: &dyn LayoutFacts,
) -> Result<PTy, BuildError> {
    let Some(&ty) = expr_ty.get(block.0 as usize) else {
        return Err(BuildError::Malformed { what: "an async block past its owner's checked expression types" });
    };
    let output = match from.node(ty) {
        Ok(PNode::Future(at, output, _)) if at == block => output,
        Ok(_) => return Err(BuildError::Malformed { what: "an async block whose checked expression type is not its own future" }),
        Err(_) => return Err(BuildError::Malformed { what: "an async block's checked type past its record's table" }),
    };
    let output = blaze_build::ground(from, output, owner_args, ground, scratch, facts)?;
    let list = ground.intern_list(owner_args).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    blaze_build::coroutine_identity(ground, owner, ordinal, list, output)
}

#[cfg(test)]
mod tests {
    //! `async fn add<T>(a: T, b: T) -> T` at `T = u8`: its root block's future grounds to the
    //! builder's identity form with `Output = u8`. The Reference, "Async functions": `async fn
    //! f(x: A) -> T { body }` "is roughly equivalent to" `fn f(x: A) -> impl Future<Output = T>
    //! { async move { body } }`: the future's output is the declared return type, here the
    //! owner's parameter `T` substituted by the caller's `u8`. Core's future/future.rs: `type
    //! Output;` "The type of value produced on completion."
    //!
    //! Prints: `hidden coroutine: owner 0xadd ordinal 0 output u8; a block typed u8 refused; a future of expression 1 at block 3 refused`
    //!
    //! A wrong fix fails here: an output taken from anything but the block's own future (the
    //! hidden type's arguments, the owner's first expression), an output not grounded at the
    //! owner's arguments (left `Param(0)`), or a block whose type is not a future accepted.

    use super::*;
    use blaze_build::layout::{Missing, ReprAttrs, ValidRange};
    use blaze_build::{CoroutineOf, coroutine_of};
    use blaze_typeck::{Adt, UintTy};

    const OWNER: ItemKey = ItemKey(0xadd);

    /// Facts that answer nothing: the types here hold no ADT, projection or opaque.
    struct Nothing;

    impl LayoutFacts for Nothing {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> {
            None
        }

        fn repr(&self, _: ItemKey) -> ReprAttrs {
            ReprAttrs::RUST
        }

        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
            Ok(None)
        }

        fn valid_range(&self, _: ItemKey) -> ValidRange {
            ValidRange::default()
        }
    }

    #[test]
    fn an_async_fns_hidden_coroutine_grounds_with_its_blocks_output() {
        // The owner's record: expressions 0..=2 are `u8`-typed operands, expression 3 is the
        // root async block, typed `Future(3, Param(0), ())` (the checker's in-body form).
        let mut from = PortableTys::new_in(Alloc::global());
        let byte = from.intern(PNode::Uint(UintTy::U8)).unwrap();
        let param = from.intern(PNode::Param(0)).unwrap();
        let none = from.intern_list(&[]).unwrap();
        let unit = from.intern(PNode::Tuple(none)).unwrap();
        let fut = from.intern(PNode::Future(ExprId(3), param, unit)).unwrap();
        let expr_ty = [byte, byte, byte, fut];

        let mut ground = PortableTys::new_in(Alloc::global());
        let g_byte = ground.intern(PNode::Uint(UintTy::U8)).unwrap();
        let t = hidden_coroutine(from.view(), &expr_ty, ExprId(3), OWNER, 0, &[g_byte], &mut ground, Alloc::global(), &Nothing)
            .unwrap_or_else(|e| panic!("the root block's future was refused: {e}"));
        let list = ground.intern_list(&[g_byte]).unwrap();
        assert_eq!(coroutine_of(ground.view(), t), Some(CoroutineOf { owner: OWNER, owner_args: list, ordinal: 0, output: g_byte }), "not the identity form with `Output = u8`");

        // The control: a block whose checked type is `u8`, not its own future.
        let refused = match hidden_coroutine(from.view(), &expr_ty, ExprId(2), OWNER, 0, &[g_byte], &mut ground, Alloc::global(), &Nothing) {
            Ok(_) => panic!("a block typed `u8` grounded as a coroutine"),
            Err(e) => format!("{e}"),
        };
        assert!(refused.contains("not its own future"), "the refusal does not name the form: {refused}");

        // The control: the future of ANOTHER expression at the block's index.
        let other = from.intern(PNode::Future(ExprId(1), param, unit)).unwrap();
        let mixed = [byte, byte, byte, other];
        let refused = match hidden_coroutine(from.view(), &mixed, ExprId(3), OWNER, 0, &[g_byte], &mut ground, Alloc::global(), &Nothing) {
            Ok(_) => panic!("the future of expression 1 was taken as block 3's"),
            Err(e) => format!("{e}"),
        };
        assert!(refused.contains("not its own future"), "the refusal does not name the form: {refused}");
        println!("hidden coroutine: owner {:#x} ordinal 0 output u8; a block typed u8 refused; a future of expression 1 at block 3 refused", OWNER.0);
    }
}
