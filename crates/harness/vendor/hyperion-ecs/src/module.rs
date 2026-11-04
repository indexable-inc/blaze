//! Host half of the module scope: delete `systems.<name>`. `ModuleCtx` (the begin/end scope guard) lives in `hyperion-ecs-sdk`.
//! The entity is `systems.<name>`, NOT `<name>`: a module's components live under `<name>.*` and must survive
//! the module entity's deletion on reload (a child of a deleted entity is deleted with it).
use crate::sys;
use crate::world::World;

/// Delete `systems.<name>` and with it every system, observer and named query the module made.
pub fn delete_module_entity(world: World, name: &str) {
    let e = world.lookup(&format!("systems.{name}"));
    // SAFETY: EcsModule is an immutable flecs global.
    if e != 0 && world.has_id(e, unsafe { sys::EcsModule }) {
        world.delete(e);
    }
}
