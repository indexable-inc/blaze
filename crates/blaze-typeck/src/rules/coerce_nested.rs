//! When a coercion between two instances of one ADT is an unsizing to ask about.
//!
//! The Reference, "Type coercions", "Unsized Coercions": "Additionally, a type `Foo<T>` can
//! implement `CoerceUnsized<Foo<U>>` when `T` implements `Unsize<U>` or `CoerceUnsized<Foo<U>>`.
//! This allows it to provide an unsized coercion to `Foo<U>`." The pinned library (nightly
//! 2026-10-27) has both forms: alloc/src/boxed.rs
//! `impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Box<U, A>> for Box<T, A> {}`
//! (the pointee unsizes), and core/src/pin.rs
//! `impl<Ptr, U> CoerceUnsized<Pin<U>> for Pin<Ptr> where Ptr: CoerceUnsized<U> + PinSafePointer,
//! U: PinSafePointer,` (the argument is itself a pointer that coerces). So the `dyn` or slice
//! that makes a coercion an unsizing may sit any number of pointers down: `Pin<Box<dyn Tr>>`'s
//! argument is `Box<dyn Tr>`, not the object.
//!
//! The coercion site asks ONE question, `Source: CoerceUnsized<Target>`, and the solver answers
//! it from the impls (the nested `Ptr: CoerceUnsized<U>` is the solver's own where-clause goal,
//! then `T: Unsize<dyn ..>` its built-in). What this file decides is only WHEN that question is
//! worth asking: when the target reaches a `dyn` or a slice through ADT arguments, references
//! and raw pointers. That filter is why the question is not asked of `Box<?0>` to `Box<u8>`,
//! where `?0: Unsize<u8>` is ambiguous and would be registered and then fail: the same filter the
//! site had, before only one level deep (an argument of the target that IS a `dyn` or slice),
//! which is the step that refused `Pin<Box<{async block}>>` to
//! `Pin<Box<dyn Future<Output = u8> + Send>>` (tempo's `BoxFuture` bodies).

use blaze_own::{Alloc, Vec};

use crate::machine::Machine;
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// How many types [`Machine::target_reaches_unsized`] looks at (rule 18). Past it the walk
/// stops filtering and the site asks the solver, whose answer is the rule itself: the filter
/// only spares a question, it never decides one.
const MAX_WALK: u32 = 256;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Whether the target's arguments `lb` reach a `dyn` or a slice through ADT arguments,
    /// `&`/`&mut` and raw pointers: `Box<dyn Tr>` (one level), `Pin<Box<dyn Tr>>`,
    /// `Pin<&mut dyn Tr>`, a user `Wrap<Rc<[u8]>>`. A work list, no recursion.
    pub(crate) fn target_reaches_unsized(&mut self, lb: TyList) -> bool {
        let mut work: Vec<Ty, Alloc<'a>> = self.list(lb);
        let mut seen = 0u32;
        while let Some(t) = work.pop() {
            seen += 1;
            if seen > MAX_WALK {
                return true;
            }
            match self.view(t) {
                TyView::Dynamic(_) | TyView::Slice(_) => return true,
                TyView::Adt(_, args) => {
                    let args = self.list(args);
                    for &a in args.iter() {
                        work.push(a);
                    }
                }
                TyView::Ref(_, pointee) | TyView::Ptr(_, pointee) => work.push(pointee),
                _ => {}
            }
        }
        false
    }
}
