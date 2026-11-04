//! flecs script entities: a named entity holding a script (`EcsScript`), run once when made and updated in place
//! (`ecs_script_update`: what the old source made is deleted, the new source runs). A failure leaves its message in the script's
//! `error` field (read it with `get::<&flecs::Script>`).
use crate::{EntityView, IntoId, World, WorldRef};
use hyperion_ecs::sys;
use std::ffi::CString;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// flecs reads a script's error through its log capture (`ecs_log_start_capture`), which swaps the process-wide log hook and
/// counts depth in a global: two scripts parsed at once on different threads lose each other's errors. One script runs at a
/// time in the process, whatever world it is in.
static CAPTURE: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    CAPTURE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct ScriptBuilder<'a> {
    world: WorldRef<'a>,
    path: String,
}

impl<'a> ScriptBuilder<'a> {
    pub(crate) fn new(world: WorldRef<'a>, path: &str) -> Self {
        Self { world, path: path.to_owned() }
    }

    /// Make the script entity and run `code` in it.
    pub fn build_from_code(self, code: &str) -> ScriptEntityView<'a> {
        let e = self.world.entity_named(&self.path);
        let code = CString::new(code.replace('\0', " ")).expect("NUL removed");
        let _one = one_at_a_time();
        // SAFETY: zeroed desc; the entity is live; flecs copies the code before returning.
        unsafe {
            let mut d: sys::ecs_script_desc_t = core::mem::zeroed();
            d.entity = e.id;
            d.code = code.as_ptr();
            sys::ecs_script_init(e.w.as_ptr(), &d);
        }
        ScriptEntityView(e)
    }
}

/// A script entity.
#[derive(Clone, Copy, Debug)]
pub struct ScriptEntityView<'a>(EntityView<'a>);

impl<'a> core::ops::Deref for ScriptEntityView<'a> {
    type Target = EntityView<'a>;
    fn deref(&self) -> &EntityView<'a> {
        &self.0
    }
}

impl<'a> ScriptEntityView<'a> {
    #[must_use]
    pub fn new_from(_world: &'a World, e: EntityView<'a>) -> Self {
        Self(e)
    }

    /// Replace the script's source with `code` and run it (in `instance` for a template, else the script itself). False on a parse
    /// or evaluation error; what the old source made is gone either way.
    #[must_use]
    pub fn update(&self, _world: &World, instance: Option<impl IntoId>, code: &str) -> bool {
        let e = self.0;
        let instance = instance.map_or(0, |i| i.into_id(e.w));
        let Ok(code) = CString::new(code) else { return false };
        let _one = one_at_a_time();
        // SAFETY: live world and script entity; flecs copies the code.
        unsafe { sys::ecs_script_update(e.w.as_ptr(), e.id, instance, code.as_ptr()) == 0 }
    }
}
