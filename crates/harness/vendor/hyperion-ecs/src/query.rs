//! Queries: a handle to an `ecs_query_t`, optionally a named child of the module scope (deleted with the module).
use crate::component::Error;
use crate::sys;
use crate::system::{Iter, Term, fill_terms};
use crate::world::{World, cs};

#[derive(Clone, Copy, Debug)]
pub struct Query {
    world: World,
    raw: *mut sys::ecs_query_t,
}

pub struct QueryBuilder {
    world: World,
    name: Option<String>,
    terms: Vec<Term>,
}

impl QueryBuilder {
    pub(crate) fn new(world: World, name: Option<&str>) -> QueryBuilder {
        QueryBuilder { world, name: name.map(str::to_string), terms: vec![] }
    }
    pub fn term(mut self, t: Term) -> Self {
        self.terms.push(t);
        self
    }
    pub fn build(self) -> Result<Query, Error> {
        let w = self.world;
        // SAFETY: zeroed desc.
        unsafe {
            let mut d: sys::ecs_query_desc_t = core::mem::zeroed();
            if let Some(n) = &self.name {
                d.entity = w.ensure_entity(n);
            } else {
                d.entity = w.scoped_anon();
            }
            fill_terms(&mut d.terms, &self.terms);
            let q = sys::ecs_query_init(w.as_ptr(), &d);
            if q.is_null() {
                return Err(Error::RegisterFailed(self.name.unwrap_or_else(|| "<query>".into())));
            }
            Ok(Query { world: w, raw: q })
        }
    }
}

impl Query {
    pub fn as_ptr(self) -> *mut sys::ecs_query_t {
        self.raw
    }
    /// Visit every matching table; return `false` from `f` to stop early.
    pub fn each_table(self, mut f: impl FnMut(&mut Iter) -> bool) {
        // SAFETY: the iterator is owned by this frame; an early exit must `ecs_iter_fini` it.
        unsafe {
            let mut it = sys::ecs_query_iter(self.world.as_ptr(), self.raw);
            it.ctx = core::ptr::null_mut();
            while sys::ecs_query_next(&mut it) {
                let mut iter = Iter { raw: &mut it };
                if !f(&mut iter) {
                    sys::ecs_iter_fini(&mut it);
                    return;
                }
            }
        }
    }
    pub fn count(self) -> usize {
        let mut n = 0;
        self.each_table(|it| {
            n += it.count();
            true
        });
        n
    }
    /// Free the query (also deletes its entity). A query made in a module scope is owned by the scope and freed with it: only call for queries made outside one.
    pub fn fini(self) {
        // SAFETY: the handle is not used afterwards.
        unsafe { sys::ecs_query_fini(self.raw) }
    }
}

pub(crate) fn _unused(_: &str) -> std::ffi::CString {
    cs("")
}
