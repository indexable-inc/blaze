//! [`Scope`]: where a borrowed handle to the world comes from. A scoped accessor (`hyperion_module::svc::<T>(&scope)`, `hyperion_world::store(&scope)`)
//! takes `&scope` and ties what it returns to that borrow, so nothing read through it outlives the callback, `init`/`deinit` or world-thread entry
//! that owns the scope (docs/plain-abi-plan.md).
use hyperion_ecs::system::Iter;
use hyperion_ecs::world::World;

/// Anything that lends the world: a system callback's [`Iter`], a module `Ctx` (init, deinit; implemented in `hyperion-module`), a borrowed
/// [`World`] handle (service entries and helpers on the world thread). References to a scope are scopes.
pub trait Scope {
    fn scope_world(&self) -> World;
}

impl Scope for World {
    #[inline]
    fn scope_world(&self) -> World {
        *self
    }
}

impl Scope for Iter<'_> {
    #[inline]
    fn scope_world(&self) -> World {
        self.world()
    }
}

impl<S: Scope + ?Sized> Scope for &S {
    #[inline]
    fn scope_world(&self) -> World {
        (**self).scope_world()
    }
}

impl<S: Scope + ?Sized> Scope for &mut S {
    #[inline]
    fn scope_world(&self) -> World {
        (**self).scope_world()
    }
}
