//! The emit seam implemented for blaze-flow's builder (the flow the polonius-shaped solver
//! runs on): `subset(a, b)` is `FlowBuilder::subset(a, b)` (`subset_base` at the Mid of the
//! statement pushed last, so the caller pushes the call first); `'a: 'static` is a subset into
//! the body's `'static` origin, which flow-lower makes a placeholder with known edges into
//! every placeholder (blaze-flow has no static origin of its own); a fresh origin is
//! `FlowBuilder::origin`.
#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_flow::{FlowBuilder, Origin};

use crate::relate::{Constraints, Kind};

/// [`Constraints`] into a [`FlowBuilder`].
pub struct FlowConstraints<'b, A: Allocator + Copy> {
    b: &'b mut FlowBuilder<A>,
    static_: Origin,
}

impl<'b, A: Allocator + Copy> FlowConstraints<'b, A> {
    /// Emit into `b`; `static_` the body's `'static` origin.
    pub fn new(b: &'b mut FlowBuilder<A>, static_: Origin) -> FlowConstraints<'b, A> {
        FlowConstraints { b, static_ }
    }
}

impl<A: Allocator + Copy> Constraints for FlowConstraints<'_, A> {
    fn subset(&mut self, a: Origin, b: Origin) {
        self.b.subset(a, b);
    }

    fn outlives_static(&mut self, a: Origin) {
        self.b.subset(a, self.static_);
    }

    fn fresh(&mut self, kind: Kind) -> Origin {
        self.b.origin(kind == Kind::Placeholder)
    }
}
