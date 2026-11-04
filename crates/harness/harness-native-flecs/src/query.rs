//! Typed queries over hyperion-ecs's: `Q` is a component borrow (`&T`, `&mut T`), a tuple of up to four, or `()`, each a term
//! whose id is resolved by name and layout hash. A query whose component the world does not have matches nothing.
//!
//! Beside the fetched terms a builder takes filter terms ([`Filters`]): a tag or pair the entity must have (`with`), must not
//! have (`without`), or that its parent (or any `rel` target) must have (`with_up`). They are matched by flecs, not tested per
//! row in the closure, and come after the fetched terms, so a fetched term's field index is its place in `Q`.
use crate::{Access, EntityView};
use core::marker::PhantomData;
use hyperion_ecs::untyped::UntypedWorld as _;
use hyperion_ecs::{Id, sys};

pub use hyperion_ecs::Term;

/// The terms of a typed query and how one matched row is borrowed.
pub trait QueryTuple {
    type Out<'x>;
    /// One term per element (`[in]` for `&T`, `[inout]` for `&mut T`), or `None` when a component is missing from `w`.
    fn terms(w: hyperion_ecs::World) -> Option<Vec<Term>>;
    /// # Safety
    /// `it` is a live iterator of a query built from [`terms`](Self::terms), `row < it.count`, and the borrows end before the
    /// iterator moves on.
    unsafe fn fetch<'x>(it: *const sys::ecs_iter_t, row: usize) -> Self::Out<'x>;
}

fn term<A: Access>(w: hyperion_ecs::World) -> Option<Term> {
    let id: Id = A::id(w);
    (id != 0).then(|| if A::MUT { Term::new(id).rw() } else { Term::new(id).read() })
}

/// # Safety
/// As [`QueryTuple::fetch`]; field `index` holds `A::C`.
unsafe fn field<'x, A: Access>(it: *const sys::ecs_iter_t, index: i8, row: usize) -> A::Out<'x> {
    // SAFETY: the caller's contract; a shared field (a singleton, an `up` source) has one value for every row.
    unsafe {
        let size = size_of::<A::C>();
        if size == 0 {
            return A::out(core::ptr::NonNull::<A::C>::dangling().as_ptr());
        }
        let p = sys::ecs_field_w_size(it, size, index).cast::<A::C>();
        let p = if sys::ecs_field_is_self(it, index) { p.add(row) } else { p };
        A::out(p)
    }
}

impl<A: Access> QueryTuple for A {
    type Out<'x> = A::Out<'x>;
    fn terms(w: hyperion_ecs::World) -> Option<Vec<Term>> {
        Some(vec![term::<A>(w)?])
    }
    unsafe fn fetch<'x>(it: *const sys::ecs_iter_t, row: usize) -> A::Out<'x> {
        // SAFETY: the caller's contract.
        unsafe { field::<A>(it, 0, row) }
    }
}

macro_rules! tuple {
    ($($a:ident $i:literal),+) => {
        impl<$($a: Access),+> QueryTuple for ($($a,)+) {
            type Out<'x> = ($($a::Out<'x>,)+);
            fn terms(w: hyperion_ecs::World) -> Option<Vec<Term>> {
                Some(vec![$(term::<$a>(w)?),+])
            }
            unsafe fn fetch<'x>(it: *const sys::ecs_iter_t, row: usize) -> Self::Out<'x> {
                // SAFETY: the caller's contract; the terms are distinct components, so the borrows do not alias.
                unsafe { ($(field::<$a>(it, $i, row),)+) }
            }
        }
    };
}
impl QueryTuple for () {
    type Out<'x> = ();
    fn terms(_: hyperion_ecs::World) -> Option<Vec<Term>> {
        Some(Vec::new())
    }
    unsafe fn fetch<'x>(_: *const sys::ecs_iter_t, _: usize) -> Self::Out<'x> {}
}

/// Filter terms of a query, system or observer: matched, never fetched. An id the world cannot resolve (a component it does
/// not have) makes a `with` unmatchable ([`Filters::missing`]) and a `without` a no-op.
#[derive(Default)]
pub(crate) struct Filters {
    pub(crate) terms: Vec<Term>,
    pub(crate) missing: bool,
}

impl Filters {
    pub(crate) fn must(&mut self, t: Term) {
        if t.id == 0 {
            self.missing = true;
        } else {
            self.terms.push(t);
        }
    }

    pub(crate) fn must_not(&mut self, t: Term) {
        if t.id != 0 {
            self.terms.push(t.not());
        }
    }
}

/// The filter-term methods every builder shares (it has `w: hyperion_ecs::World` and `filters: Filters`).
macro_rules! filter_methods {
    () => {
        /// Match only entities that have `id` (a tag, a component, or a pair; `(Rel, flecs::Wildcard::ID)` for any target).
        /// In an observer, adding or removing it fires the observer.
        #[must_use]
        pub fn with(mut self, id: impl $crate::IntoId) -> Self {
            let id = id.into_id(self.w);
            self.filters.must($crate::query::Term::new(id).inout_none());
            self
        }

        /// Match only entities that do not have `id`.
        #[must_use]
        pub fn without(mut self, id: impl $crate::IntoId) -> Self {
            let id = id.into_id(self.w);
            self.filters.must_not($crate::query::Term::new(id).inout_none());
            self
        }

        /// Match only entities whose `rel` target (`flecs::ChildOf::ID`: the parent) has `id`, searching up the relationship.
        #[must_use]
        pub fn with_up(mut self, id: impl $crate::IntoId, rel: impl $crate::IntoId) -> Self {
            let (id, rel) = (id.into_id(self.w), rel.into_id(self.w));
            self.filters.must($crate::query::Term::up(id, rel).inout_none());
            self
        }

        /// Match only entities whose `rel` target does not have `id` (`without_up(Node::id(), flecs::ChildOf::ID)`: the roots of
        /// the node trees).
        #[must_use]
        pub fn without_up(mut self, id: impl $crate::IntoId, rel: impl $crate::IntoId) -> Self {
            let (id, rel) = (id.into_id(self.w), rel.into_id(self.w));
            self.filters.must_not($crate::query::Term::up(id, rel).inout_none());
            self
        }
    };
}
pub(crate) use filter_methods;

tuple!(A 0, B 1);
tuple!(A 0, B 1, C 2);
tuple!(A 0, B 1, C 2, D 3);

pub struct QueryBuilder<'a, Q> {
    w: hyperion_ecs::World,
    name: Option<String>,
    filters: Filters,
    _q: crate::Marker<'a, Q>,
}

impl<'a, Q: QueryTuple> QueryBuilder<'a, Q> {
    pub(crate) fn new(w: hyperion_ecs::World) -> Self {
        Self { w, name: None, filters: Filters::default(), _q: PhantomData }
    }

    filter_methods!();

    /// Name the query's entity, a child of the current scope.
    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    #[must_use]
    pub fn build(self) -> Query<'a, Q> {
        let q = if self.filters.missing {
            None
        } else {
            Q::terms(self.w).and_then(|terms| {
                let b = self.w.query(self.name.as_deref());
                terms.into_iter().chain(self.filters.terms).fold(b, |b, t| b.term(t)).build().ok()
            })
        };
        Query { w: self.w, q, _q: PhantomData }
    }
}

/// A query; freed when dropped.
pub struct Query<'a, Q> {
    w: hyperion_ecs::World,
    q: Option<hyperion_ecs::Query>,
    _q: crate::Marker<'a, Q>,
}

impl<Q> Drop for Query<'_, Q> {
    fn drop(&mut self) {
        if let Some(q) = self.q.take() {
            q.fini();
        }
    }
}

impl<'a, Q: QueryTuple> Query<'a, Q> {
    pub fn each(&self, mut f: impl FnMut(Q::Out<'_>)) {
        self.each_entity(|_, v| f(v));
    }

    pub fn each_entity(&self, mut f: impl FnMut(EntityView<'a>, Q::Out<'_>)) {
        let Some(q) = self.q else { return };
        q.each_table(|it| {
            let raw: *const sys::ecs_iter_t = &raw const *it.raw;
            for (row, &e) in it.entities().iter().enumerate() {
                // SAFETY: `row` is in the table; the borrows end with this call, before the iterator moves.
                f(EntityView::new(self.w, e), unsafe { Q::fetch(raw, row) });
            }
            true
        });
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.q.map_or(0, |q| q.count())
    }
}
