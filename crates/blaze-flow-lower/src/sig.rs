//! The lifetime part of a fn's signature: which region stands at each origin position of each
//! input and of the output, and the outlives relations the signature declares (flow.md, "What
//! the signature must say (H1)").
//!
//! THE LOWERING'S VIEW is [`life`] (D148, flow.md "Lifetimes are produced once and carried
//! beside the types"): [`life::SigLife`] reads a head record's
//! [`blaze_typeck::region_facts::SigRegions`] in place, and the lowering takes nothing else for
//! a fn's lifetimes. No item's text is read for them: the text reader that stood here
//! (`read_signature` and sig/{alias,impl_life,outlives,value}.rs) was deleted by piece L6, and
//! its name is banned (text_reader_ban_tests.rs).

/// The lowering's view of a fn's signature regions (D148): a head record's
/// [`blaze_typeck::region_facts::SigRegions`] read in
/// place. Whether the flow lays a child of the column position for position as the column lays
/// its slots is the lowering's question (lib.rs `Lower::laid_as_column`), since only it counts the
/// flow's positions.
pub mod life {
    use blaze_typeck::{ItemKey, PTy};
    use blaze_typeck::region_facts::{ClauseView, ColView, Outlives, ParamsView, RegionFacts, RegionsUnread, SigRegions, UnreadWhy};
    use blaze_typeck::regions::Region;

    /// One fn's signature regions as the lowering takes them: the column laid over
    /// `FnPtr(inputs.., output)` (its root has no slot of its own; child `k` is input `k`, the
    /// last child the output), the lifetime parameters and their declared outlives clauses, and
    /// an async fn's written output.
    #[derive(Clone, Copy, Debug)]
    pub struct SigLife<'s> {
        regions: SigRegions<'s>,
    }

    impl<'s> SigLife<'s> {
        /// The view of `regions`.
        pub fn new(regions: SigRegions<'s>) -> SigLife<'s> {
            SigLife { regions }
        }

        /// The head record's regions this view reads.
        pub fn regions(&self) -> SigRegions<'s> {
            self.regions
        }

        /// Whether the column is laid as a signature's: a root with no slot of its own and at
        /// least the output as a child. A column that is not is malformed, never read.
        pub fn well_formed(&self) -> bool {
            self.regions.col.own().is_empty() && self.regions.col.children() >= 1
        }

        /// How many inputs the column lays.
        pub fn inputs(&self) -> u32 {
            self.regions.col.children().saturating_sub(1)
        }

        /// Input `k`'s slots, in its type's pre-order; `None` past the inputs.
        pub fn input(&self, k: u32) -> Option<&'s [Region]> {
            if k < self.inputs() { self.regions.col.child(k) } else { None }
        }

        /// The output's slots, in its type's pre-order.
        pub fn output(&self) -> Option<&'s [Region]> {
            self.regions.col.child(self.regions.col.children().checked_sub(1)?)
        }

        /// Every slot of the column: the inputs' in order, then the output's.
        pub fn slots(&self) -> &'s [Region] {
            self.regions.col.as_slice()
        }

        /// Whether lifetime parameter `i` is late-bound; an index the head does not declare is
        /// not one, and the lowering refuses a slot naming it (a malformed column).
        pub fn is_late(&self, i: u32) -> Option<bool> {
            if i < self.regions.params.count() { self.regions.params.is_late(i).ok() } else { None }
        }

        /// The declared outlives clauses among the fn's lifetimes and `'static`, as `(longer,
        /// shorter)`: `'a: 'b` is `(Param(a), Param(b))`.
        pub fn bounds(&self) -> &'s [(Region, Region)] {
            self.regions.params.outlives()
        }

        /// The fn's lifetime parameters (count, late-bound, outlives).
        pub fn params(&self) -> ParamsView<'s> {
            self.regions.params
        }

        /// The regions of the fn's BODY's value ([`BodyOut`]).
        pub fn body(&self) -> BodyOut<'s> {
            match self.regions.written_output {
                Some(c) => BodyOut::Async(c.as_slice()),
                None => BodyOut::Output,
            }
        }

        /// The slots of the call's value `v` ([`CallValue`]), in its type's pre-order.
        pub fn value(&self, v: CallValue<'s>) -> Option<&'s [Region]> {
            match v {
                CallValue::Returned => self.output(),
                CallValue::Awaited { slots, .. } => Some(slots),
            }
        }
    }

    /// Which value of a call its destination holds. The Reference, "Await expressions": the
    /// value of `f(x).await` is what the future's `poll` returns in `Ready`, its
    /// `Future::Output`; "Async functions": for `async fn f(x: &u8) -> T` that is `T` as
    /// written (RFC 2394), instantiated at the call. So the regions of an awaited `async fn`'s
    /// value are the written return type's column (`SigRegions::written_output`) instantiated
    /// as the call instantiates the inputs, never every region the future captured
    /// (coroutine.rs `await_done`).
    #[derive(Clone, Copy)]
    pub enum CallValue<'s> {
        /// The call's own value: the column's output.
        Returned,
        /// What an `.await` of the call yields, `Future::Output`: the type `ty` (in the head
        /// records' table) and its slots in the fn's numbering. An `async fn`'s written return
        /// type's column (`SigRegions::written_output`), or the `Output = T` binding of the
        /// opaque a fn returns (`impl Future<Output = T>`, its clause column's term: await_call.rs
        /// `opaque_output`).
        Awaited { ty: PTy, slots: &'s [Region] },
    }

    /// The regions of a body's value ([`SigLife::body`]). The Reference, "Async functions":
    /// `async fn f(x: &u8) -> T` is "roughly equivalent to" `fn f<'a>(x: &'a u8) -> impl
    /// Future<Output = T> + 'a`, the body an `async move` block whose value is the future's
    /// `Output`: so the body's value must be a `T` as written, its regions the written return
    /// type's column (RFC 2394), exactly as a fn's returned value is checked against its output.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum BodyOut<'s> {
        /// A fn's body: its value is the output.
        Output,
        /// An `async fn`'s body: its value is the written return type, these its slots.
        Async(&'s [Region]),
    }

    /// The [`RegionFacts`] of a lowering given none: every item's regions unread, `NoHead`. A
    /// call of a head whose regions are unread is refused by name (`Form::RegionsUnread`), never
    /// related by a guess; what `Facts::regions` answers when its implementer gives nothing.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct NoRegions;

    impl NoRegions {
        fn unread(item: ItemKey) -> RegionsUnread {
            RegionsUnread { item, why: UnreadWhy::NoHead }
        }
    }

    impl RegionFacts for NoRegions {
        fn sig(&self, item: ItemKey) -> Result<SigRegions<'_>, RegionsUnread> {
            Err(NoRegions::unread(item))
        }
        fn clauses(&self, item: ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread> {
            Err(NoRegions::unread(item))
        }
        fn outlives(&self, item: ItemKey) -> Result<&[Outlives], RegionsUnread> {
            Err(NoRegions::unread(item))
        }
        fn field(&self, adt: ItemKey, _: u32, _: u32) -> Result<ColView<'_>, RegionsUnread> {
            Err(NoRegions::unread(adt))
        }
        fn impl_header(&self, impl_: ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread> {
            Err(NoRegions::unread(impl_))
        }
        fn opaque_captures(&self, opaque: ItemKey) -> Result<u32, RegionsUnread> {
            Err(NoRegions::unread(opaque))
        }
        fn lifetime_params(&self, item: ItemKey) -> Result<ParamsView<'_>, RegionsUnread> {
            Err(NoRegions::unread(item))
        }
        fn impl_assoc(&self, impl_: ItemKey, _: blaze_typeck::Name) -> Result<ColView<'_>, RegionsUnread> {
            Err(NoRegions::unread(impl_))
        }
    }
}
