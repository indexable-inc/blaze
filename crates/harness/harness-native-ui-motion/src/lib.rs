//! Generic visibility animation. All persistent state belongs to the host world.
use harness_native_flecs::prelude::*;
use harness_native_api::{moving, settle};
use harness_native_ui_motion_api::{Motion, Shown, ShownWhen, Target};
use harness_native_ui_spring_api::{Spring, Tune};

/// Install systems in the module scope; deleting that scope removes all callbacks before unloading.
pub fn install(world: WorldRef<'_>) {
    // Scene springs: toward the host's Target when it set one, else 1 while the app is in any (ShownWhen, state).
    let app_id = *world.lookup(harness_native_api::APP).id();
    world.system_named::<(&Motion, &mut Shown)>("Show").each_iter(move |it, row, (m, sh)| {
        let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
        let app = w.entity_from_id(app_id);
        let target = e.try_cloned::<&Target>().map_or_else(
            || {
                let mut on = false;
                e.each_target(ShownWhen::id(), |t| on |= app.has((flecs::Wildcard::ID, *t.id())));
                f32::from(u8::from(on))
            },
            |t| t.0,
        );
        if !sh.started {
            sh.spring = Spring::at(target);
            sh.started = true;
        }
        if settle(&mut sh.spring, target, Tune { omega: m.omega, zeta: m.zeta }, dt) {
            moving(&w);
        }
    });
}

harness_native_api::module!("motion", install, [harness_native_api::API_LAYOUTS, harness_native_ui_motion_api::API_LAYOUTS]);
