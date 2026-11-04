//! Module scope: everything a module creates between `begin` and `end` (systems, observers, named queries) becomes a child of
//! the entity `systems.<name>`; deleting it removes all of it (port of `beginModule`/`endModule` and the manager's
//! `deleteModule`). The entity is `systems.<name>`, NOT `<name>`: a module's components live under `<name>.*` and must survive
//! the module entity's deletion on reload (a child of a deleted entity is deleted with it).
pub use hyperion_ecs::module::delete_module_entity;
use hyperion_ecs::sys;
use hyperion_ecs::world::{Id, World};

pub struct ModuleCtx {
    world: World,
    name: String,
    entity: Id,
    prev_scope: Id,
}

impl ModuleCtx {
    pub fn begin(world: World, name: &str) -> ModuleCtx {
        let path = format!("systems.{name}");
        let e = world.ensure_entity(&path);
        world.add_id(e, unsafe { sys::EcsModule });
        let prev = world.set_scope(e);
        ModuleCtx { world, name: name.to_string(), entity: e, prev_scope: prev }
    }
    pub fn entity(&self) -> Id {
        self.entity
    }
    /// Restore the scope that was current at `begin`.
    pub fn end(&self) {
        self.world.set_scope(self.prev_scope);
    }
    /// Delete `systems.<name>` (idempotent). The manager does the same after `deinit`.
    pub fn teardown(&self) {
        delete_module_entity(self.world, &self.name);
    }
}

impl Drop for ModuleCtx {
    fn drop(&mut self) {
        self.end();
    }
}
