//! Systems and observers: non-generic builders over plain `fn(&mut Iter)` callbacks. One extern "C" trampoline runs every callback under
//! `hyperion_panic::catch` (docs/host-diet.md 10.4 item 6): a panic is recorded in this image's panic slot, the image is quarantined (all
//! its trampolines return at once until it is reloaded) and nothing unwinds into flecs. The host replaces `callback` with its own trampoline
//! (signal frame and watchdog, `hyperion-host/src/guard.rs`) and calls this one inside it. The callback context is boxed and freed by flecs
//! (`ctx_free`) when the system entity is deleted. Per-system time is flecs' own (`ecs_measure_system_time`), not a span here.
use crate::component::Error;
use crate::hooks;
use crate::sys;
use crate::world::{Id, World, cs};
use core::ffi::c_void;

pub type Callback = fn(&mut Iter);

/// Frees a builder's `user` pointer when flecs deletes the system or observer (`user_owned`).
pub type FreeUser = unsafe fn(*mut c_void);

struct CbCtx {
    f: Callback,
    name: String,
    user: *mut c_void,
    free_user: Option<FreeUser>,
}

unsafe extern "C" fn trampoline(it: *mut sys::ecs_iter_t) {
    // SAFETY: flecs passes a live iterator whose ctx is the Box<CbCtx> we installed.
    let ctx = unsafe { &*((*it).ctx as *const CbCtx) };
    hyperion_panic::catch(&ctx.name, || (), || {
        // SAFETY: the iterator is valid for the duration of the callback.
        let mut iter = Iter { raw: unsafe { &mut *it } };
        (ctx.f)(&mut iter);
    });
}

unsafe extern "C" fn free_ctx(p: *mut c_void) {
    // SAFETY: `p` came from Box::into_raw in `make_ctx`; flecs calls this once.
    let ctx = unsafe { Box::from_raw(p as *mut CbCtx) };
    if let Some(free) = ctx.free_user {
        // SAFETY: the builder handed `user` over together with `free`; nothing else frees it.
        unsafe { free(ctx.user) };
    }
}

fn make_ctx(name: &str, f: Callback, user: *mut c_void, free_user: Option<FreeUser>) -> *mut c_void {
    Box::into_raw(Box::new(CbCtx { f, name: name.to_string(), user, free_user })).cast()
}

/// A flecs iterator inside a system, observer or query loop.
pub struct Iter<'a> {
    pub raw: &'a mut sys::ecs_iter_t,
}

impl<'a> Iter<'a> {
    pub fn count(&self) -> usize {
        self.raw.count as usize
    }
    pub fn world(&self) -> World {
        // SAFETY: the iterator's world is the live world (flecs 4.2: `it->world` is the real world, `it->stage` the stage).
        unsafe { World::from_raw(self.raw.world.cast()) }
    }
    /// The STAGE world of the thread running this callback (`it->stage`). Inside a `.threaded()` system every `ecs_*` call must go
    /// through this world: writes made through it are deferred into this thread's own command queue and merged at the end of the sync
    /// point; the real world ([`Iter::world`]) writes into stage 0's queue and races. Outside worker threads it is the stage of the main
    /// thread, so it is always safe to prefer. Reads through it see the main storage, not this tick's own deferred writes.
    pub fn stage(&self) -> World {
        // SAFETY: the iterator's stage is a live stage pointer for the duration of the callback.
        unsafe { World::from_raw(self.raw.stage.cast()) }
    }
    /// 0 on the main thread, 1.. on a flecs worker thread. A `.threaded()` system runs on every index in the same sync point.
    pub fn stage_id(&self) -> i32 {
        // SAFETY: valid iterator world.
        unsafe { sys::ecs_stage_get_id(self.raw.stage) }
    }
    pub fn delta_time(&self) -> f32 {
        self.raw.delta_time
    }
    pub fn entities(&self) -> &[Id] {
        if self.raw.entities.is_null() || self.raw.count <= 0 {
            return &[];
        }
        // SAFETY: flecs guarantees `count` entities at `entities` for the current table.
        unsafe { core::slice::from_raw_parts(self.raw.entities, self.raw.count as usize) }
    }
    /// The `user` pointer given to the builder (module-owned memory; null for queries).
    pub fn user(&self) -> *mut c_void {
        if self.raw.ctx.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: systems and observers carry a CbCtx here; queries iterated through `Query::each_table` set ctx to null.
        unsafe { (*(self.raw.ctx as *const CbCtx)).user }
    }
    pub fn field_id(&self, index: i8) -> Id {
        // SAFETY: valid iterator.
        unsafe { sys::ecs_field_id(self.raw, index) }
    }
    /// True when column `index` is owned by the matched entities (not shared from a prefab, `up` source or singleton).
    pub fn is_self(&self, index: i8) -> bool {
        // SAFETY: valid iterator.
        unsafe { sys::ecs_field_is_self(self.raw, index) }
    }
    /// True when the (optional) term `index` matched.
    pub fn is_set(&self, index: i8) -> bool {
        // SAFETY: valid iterator.
        unsafe { sys::ecs_field_is_set(self.raw, index) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Oper {
    And,
    Not,
    Optional,
}

/// Declared access of a term (flecs `inout` kind). flecs' pipeline analysis (docs/mt-systems.md, SOURCE.md in
/// docs/upstream/flecs-threading) reads exactly this: a later system with an `In`/`InOut` term on an id that an earlier system in the same
/// sync point wrote through a fixed source (singleton, resource) gets a merge point (barrier + command flush) inserted before it.
/// `Default` = flecs' own default (`InOut` for `$this` terms, `In` for any other source); it does NOT count as a declaration for the lint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Default,
    /// Read only (`[in]`).
    In,
    /// Write only (`[out]`). Note flecs does not order two `Out` writers of one id; use `InOut` for a shared resource you write.
    Out,
    /// Read and write (`[inout]`).
    InOut,
    /// Matched only, never read or written (`[none]`); flecs' pipeline ignores such a term, so it declares nothing to the scheduler.
    None,
    /// Matched only, and in an observer it does not trigger: only the other terms' events fire it (`[filter]`).
    Filter,
}

/// One query term. `Term::new(id)` matches self; `Term::singleton(id)` reads the component from its own entity.
#[derive(Clone, Copy, Debug)]
pub struct Term {
    pub id: Id,
    pub src: Id,
    pub oper: Oper,
    pub access: Access,
    /// Match on the target of relationship `trav` only (`up trav`); 0 = the entity itself.
    pub trav: Id,
}

impl Term {
    pub fn new(id: Id) -> Term {
        Term { id, src: 0, oper: Oper::And, access: Access::Default, trav: 0 }
    }
    pub fn singleton(id: Id) -> Term {
        Term { id, src: id, oper: Oper::And, access: Access::Default, trav: 0 }
    }
    pub fn from(id: Id, src: Id) -> Term {
        Term { id, src, oper: Oper::And, access: Access::Default, trav: 0 }
    }
    /// `id` matched on the target of relationship `rel` (`up rel`: not on the entity itself).
    pub fn up(id: Id, rel: Id) -> Term {
        Term { id, src: 0, oper: Oper::And, access: Access::Default, trav: rel }
    }
    pub fn not(mut self) -> Term {
        self.oper = Oper::Not;
        self
    }
    pub fn optional(mut self) -> Term {
        self.oper = Oper::Optional;
        self
    }
    /// Match only: do not read or write the data (tags, filters).
    pub fn inout_none(mut self) -> Term {
        self.access = Access::None;
        self
    }
    /// Match only, and never trigger an observer (`[filter]`).
    pub fn filter(mut self) -> Term {
        self.access = Access::Filter;
        self
    }
    /// `[in]`: the system only reads this term.
    pub fn read(mut self) -> Term {
        self.access = Access::In;
        self
    }
    /// `[out]`: the system only writes this term (sets it without reading it first).
    pub fn write(mut self) -> Term {
        self.access = Access::Out;
        self
    }
    /// `[inout]`: the system reads and writes this term. Use this (not `write`) for a resource token the system mutates.
    pub fn rw(mut self) -> Term {
        self.access = Access::InOut;
        self
    }
    /// True when the term reads or writes a component that is not on the matched entity itself (a singleton, fixed source or `up` traversal).
    pub fn is_shared(&self) -> bool {
        self.src != 0 || self.trav != 0
    }
    pub fn raw(self) -> sys::ecs_term_t {
        // SAFETY: zeroed ecs_term_t is "unset".
        unsafe {
            let mut t: sys::ecs_term_t = core::mem::zeroed();
            t.id = self.id;
            t.src.id = self.src;
            if self.trav != 0 {
                t.src.id = sys::EcsUp;
                t.trav = self.trav;
            }
            t.oper = match self.oper {
                Oper::And => sys::EcsAnd,
                Oper::Not => sys::EcsNot,
                Oper::Optional => sys::EcsOptional,
            } as _;
            match self.access {
                Access::Default => {}
                Access::In => t.inout = sys::EcsIn as _,
                Access::Out => t.inout = sys::EcsOut as _,
                Access::InOut => t.inout = sys::EcsInOut as _,
                Access::None => t.inout = sys::EcsInOutNone as _,
                Access::Filter => t.inout = sys::EcsInOutFilter as _,
            }
            t
        }
    }
}

pub(crate) fn fill_terms(dst: &mut [sys::ecs_term_t], terms: &[Term]) {
    for (i, t) in terms.iter().enumerate() {
        assert!(i < dst.len(), "too many terms (flecs supports {})", dst.len());
        dst[i] = t.raw();
    }
}

#[cfg(feature = "system")]
pub struct SystemBuilder {
    world: World,
    name: String,
    phase: Id,
    terms: Vec<Term>,
    user: *mut c_void,
    free_user: Option<FreeUser>,
    threaded: bool,
    immediate: bool,
    /// The author stated the access of this system (an explicit term access, [`SystemBuilder::no_ecs_access`] or [`SystemBuilder::serial`]).
    declared: bool,
}

pub struct ObserverBuilder {
    world: World,
    name: String,
    events: Vec<Id>,
    yield_existing: bool,
    terms: Vec<Term>,
    user: *mut c_void,
    free_user: Option<FreeUser>,
}

#[cfg(feature = "system")]
impl SystemBuilder {
    pub(crate) fn new(world: World, name: &str) -> SystemBuilder {
        // SAFETY: EcsOnUpdate is an immutable flecs global.
        SystemBuilder {
            world,
            name: name.into(),
            phase: unsafe { sys::EcsOnUpdate },
            terms: vec![],
            user: core::ptr::null_mut(),
            free_user: None,
            threaded: false,
            immediate: false,
            declared: false,
        }
    }
}

impl ObserverBuilder {
    pub(crate) fn new(world: World, name: &str) -> ObserverBuilder {
        // SAFETY: immutable global.
        ObserverBuilder { world, name: name.into(), events: vec![], yield_existing: false, terms: vec![], user: core::ptr::null_mut(), free_user: None }
    }
}

#[cfg(feature = "system")]
impl SystemBuilder {
    pub fn phase(mut self, phase: Id) -> Self {
        self.phase = phase;
        self
    }
    pub fn term(mut self, t: Term) -> Self {
        if t.access != Access::Default {
            self.declared = true;
        }
        self.terms.push(t);
        self
    }
    pub fn user(mut self, p: *mut c_void) -> Self {
        self.user = p;
        self
    }
    /// `user` that the system owns: `free` runs on it when flecs deletes the system (a boxed closure of the image that built it).
    pub fn user_owned(mut self, p: *mut c_void, free: FreeUser) -> Self {
        self.user = p;
        self.free_user = Some(free);
        self
    }
    /// Run this system across flecs worker threads (flecs `multi_threaded`): every matched entity is visited by exactly one worker and the
    /// callback must obey the rules of docs/mt-systems.md section 3 (own-row writes only, `it.stage()` for every other ecs call, no host
    /// call marked MainOnly). `build` clears the flag, with a warning, when the system cannot be threaded: wrong phase (only `OnUpdate`
    /// = Simulate), no `$this` term (nothing to partition), or a write through a shared source.
    pub fn threaded(mut self) -> Self {
        self.threaded = true;
        self
    }
    /// flecs `immediate`: the system gets the real world (no staging), runs alone in its own sync point on the main thread. For a system that
    /// must read or write other entities directly. Mutually exclusive with `threaded`.
    pub fn immediate(mut self) -> Self {
        self.immediate = true;
        self
    }
    /// Declaration for a system that touches no ECS data except through host calls or its own state (a poll tick, a flush).
    pub fn no_ecs_access(mut self) -> Self {
        self.declared = true;
        self
    }
    /// Declaration for a system that does arbitrary work on the main thread (the default and the safe choice): documents that no access
    /// analysis was done. Same effect as [`SystemBuilder::no_ecs_access`] for the lint; the system stays single-threaded.
    pub fn serial(mut self) -> Self {
        self.declared = true;
        self.threaded = false;
        self
    }
    pub fn build(mut self, f: Callback) -> Result<Id, Error> {
        let w = self.world;
        self.lint();
        let e = w.ensure_entity(&self.name);
        // SAFETY: zeroed desc; the ctx is a Box freed by flecs through ctx_free when the system is deleted.
        unsafe {
            let mut d: sys::ecs_system_desc_t = core::mem::zeroed();
            d.entity = e;
            fill_terms(&mut d.query.terms, &self.terms);
            d.phase = self.phase;
            d.multi_threaded = self.threaded;
            d.immediate = self.immediate;
            d.callback = Some(trampoline);
            d.ctx = make_ctx(&self.name, f, self.user, self.free_user);
            d.ctx_free = Some(free_ctx);
            let id = sys::ecs_system_init(w.as_ptr(), &d);
            if id == 0 {
                free_ctx(d.ctx);
                return Err(Error::RegisterFailed(self.name));
            }
            Ok(id)
        }
    }
}

#[cfg(feature = "system")]
impl SystemBuilder {
    /// The access lint (a warning, never a refusal): a system without a declaration, or a threaded request that cannot be honoured.
    fn lint(&mut self) {
        let warn = |name: &str, msg: &str| hooks::log(hooks::WARN, &format!("[access] system {name}: {msg} (docs/mt-systems.md)"));
        if !self.declared {
            warn(&self.name, "registered without an access declaration (term .read()/.write()/.rw(), .no_ecs_access() or .serial()); it stays single-threaded");
            self.threaded = false;
        }
        if self.threaded && self.immediate {
            warn(&self.name, "threaded and immediate are exclusive; running immediate");
            self.threaded = false;
        }
        if self.threaded {
            // SAFETY: immutable flecs global.
            let simulate = unsafe { sys::EcsOnUpdate };
            if self.phase != simulate {
                warn(&self.name, "threaded is only allowed in the Simulate phase (OnUpdate); running single-threaded");
                self.threaded = false;
            } else if !self.terms.iter().any(|t| !t.is_shared() && t.oper != Oper::Not) {
                warn(&self.name, "threaded needs a $this term to partition entities across workers; running single-threaded");
                self.threaded = false;
            } else if self.terms.iter().any(|t| t.is_shared() && matches!(t.access, Access::Out | Access::InOut)) {
                warn(&self.name, "threaded systems may not write a shared term (singleton, resource, up); running single-threaded");
                self.threaded = false;
            }
        }
    }
}

impl ObserverBuilder {
    /// An event that fires the observer (`EcsOnAdd`, `EcsOnSet`, `EcsOnRemove`, or a custom one); up to eight. With none, OnSet.
    pub fn event(mut self, ev: Id) -> Self {
        self.events.push(ev);
        self
    }
    /// At creation, run the callback once for what already matches (as if each had just been added or set): an observer a hot
    /// swap recreates catches up with the world instead of waiting for the next change.
    pub fn yield_existing(mut self) -> Self {
        self.yield_existing = true;
        self
    }
    pub fn term(mut self, t: Term) -> Self {
        self.terms.push(t);
        self
    }
    pub fn user(mut self, p: *mut c_void) -> Self {
        self.user = p;
        self
    }
    /// `user` that the system owns: `free` runs on it when flecs deletes the system (a boxed closure of the image that built it).
    pub fn user_owned(mut self, p: *mut c_void, free: FreeUser) -> Self {
        self.user = p;
        self.free_user = Some(free);
        self
    }
    pub fn build(self, f: Callback) -> Result<Id, Error> {
        let w = self.world;
        let e = w.ensure_entity(&self.name);
        let _ = cs;
        // SAFETY: as for systems.
        unsafe {
            let mut d: sys::ecs_observer_desc_t = core::mem::zeroed();
            d.entity = e;
            fill_terms(&mut d.query.terms, &self.terms);
            assert!(self.events.len() <= d.events.len(), "observer {}: too many events (flecs supports {})", self.name, d.events.len());
            if self.events.is_empty() {
                d.events[0] = sys::EcsOnSet;
            }
            for (slot, ev) in d.events.iter_mut().zip(&self.events) {
                *slot = *ev;
            }
            d.yield_existing = self.yield_existing;
            d.callback = Some(trampoline);
            d.ctx = make_ctx(&self.name, f, self.user, self.free_user);
            d.ctx_free = Some(free_ctx);
            let id = sys::ecs_observer_init(w.as_ptr(), &d);
            if id == 0 {
                free_ctx(d.ctx);
                return Err(Error::RegisterFailed(self.name));
            }
            Ok(id)
        }
    }
}
