//! The scene's behaviour in flecs script (flecs.script.input): a widget's `async { const e = await on.click(this) ... }`
//! hears the clicks the input pass makes (an `Activated` Pressable, src/app.rs `forward_clicks`), and hands the host what
//! to do through `hn.act(name, alt)`, an async function queued here and run by `App::actions` beside the `Action` names.
//! Also flecs's explorer (flecs.dev/explorer, the Flecs Shell) behind `HARNESS_NATIVE_EXPLORER=1`: the REST server on
//! 127.0.0.1:27750 and the stats it shows; nothing is imported, and nothing runs, without it.
use harness_native_flecs::prelude::*;
use hyperion_ecs::sys;
use std::ffi::{CStr, c_char};
use std::sync::Mutex;

/// What scripts asked for since the last frame's actions: an action name and whether Option was down.
static ACTS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

unsafe extern "C" {
    fn FlecsScriptInputImport(world: *mut sys::ecs_world_t);
    fn FlecsRestImport(world: *mut sys::ecs_world_t);
    fn FlecsStatsImport(world: *mut sys::ecs_world_t);
}

/// The localhost port the explorer's REST server listens on (flecs's default).
pub const EXPLORER_PORT: u16 = 27750;

/// `hn.act(name: string, alt: bool) -> bool`: queue `name` for the host, resolved at once (true).
unsafe extern "C" fn act(
    _ctx: *const sys::ecs_function_ctx_t,
    argc: i32,
    argv: *const sys::ecs_value_t,
    future: *mut sys::ecs_script_future_t,
) {
    // SAFETY: flecs calls with the two declared parameters, a string and a bool, alive for the call; the future is ours to
    // resolve and release once.
    unsafe {
        if argc == 2 {
            let args = std::slice::from_raw_parts(argv, 2);
            let name = *args[0].ptr.cast::<*const c_char>();
            let name = if name.is_null() {
                String::new()
            } else {
                CStr::from_ptr(name).to_string_lossy().into_owned()
            };
            let alt = *args[1].ptr.cast::<bool>();
            ACTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((name, alt));
        }
        let ok = true;
        let v = sys::ecs_value_t {
            type_: sys::FLECS_IDecs_bool_tID_,
            ptr: core::ptr::from_ref(&ok).cast_mut().cast(),
        };
        sys::ecs_script_future_resolve(future, &v);
        sys::ecs_script_future_release(future);
    }
}

/// Before the scene loads: flecs.script.input (`on.click`) and `hn.act`; the explorer when asked for.
pub fn install(world: &World) {
    let w = world.world_ref().raw().as_ptr();
    // SAFETY: a live world, before any script runs; the descriptor's strings are static.
    unsafe {
        sys::ecs_import_c(w, Some(FlecsScriptInputImport), c"FlecsScriptInput".as_ptr());
        let hn = sys::ecs_entity_init(
            w,
            &sys::ecs_entity_desc_t { name: c"hn".as_ptr(), ..core::mem::zeroed() },
        );
        let mut d: sys::ecs_async_function_desc_t = core::mem::zeroed();
        d.name = c"act".as_ptr();
        d.parent = hn;
        d.return_type = sys::FLECS_IDecs_bool_tID_;
        d.params[0] = sys::ecs_script_parameter_t {
            name: c"name".as_ptr(),
            type_: sys::FLECS_IDecs_string_tID_,
            optional: false,
            brief: core::ptr::null(),
        };
        d.params[1] = sys::ecs_script_parameter_t {
            name: c"alt".as_ptr(),
            type_: sys::FLECS_IDecs_bool_tID_,
            optional: false,
            brief: core::ptr::null(),
        };
        d.callback = Some(act);
        sys::ecs_async_function_init(w, &d);
    }
    if std::env::var("HARNESS_NATIVE_EXPLORER").as_deref() == Ok("1") {
        explorer(world);
    }
}

/// flecs's REST server and stats on 127.0.0.1 (open https://flecs.dev/explorer, or the Flecs Shell in it).
fn explorer(world: &World) {
    let w = world.world_ref().raw().as_ptr();
    // SAFETY: a live world; EcsRest's ipaddr is copied by its set hook (a static string either way).
    unsafe {
        sys::ecs_import_c(w, Some(FlecsStatsImport), c"FlecsStats".as_ptr());
        sys::ecs_import_c(w, Some(FlecsRestImport), c"FlecsRest".as_ptr());
        let rest = sys::EcsRest {
            port: EXPLORER_PORT,
            ipaddr: c"127.0.0.1".as_ptr().cast_mut(),
            impl_: core::ptr::null_mut(),
            readonly: false,
        };
        let id = sys::FLECS_IDEcsRestID_;
        sys::ecs_set_id(w, id, id, size_of::<sys::EcsRest>(), core::ptr::from_ref(&rest).cast());
    }
    eprintln!(
        "harness-native: flecs explorer on http://127.0.0.1:{EXPLORER_PORT} (open https://flecs.dev/explorer)"
    );
}

/// A click the input pass made on `target` (with Option or not), told to the scripts waiting on it (and its parents).
pub fn click(world: &World, target: u64, alt: bool) {
    let w = world.world_ref().raw().as_ptr();
    let mut ev = sys::EcsScriptMouseEvent {
        target,
        screen_x: 0.0,
        screen_y: 0.0,
        local_x: 0.0,
        local_y: 0.0,
        delta_x: 0.0,
        delta_y: 0.0,
        button: 0,
        buttons: 1,
        ctrl: false,
        shift: false,
        alt,
        meta: false,
        id: 0,
    };
    // SAFETY: a live world, outside progress; the event is read during the call.
    unsafe {
        sys::ecs_script_mouse_event(w, &ev);
        ev.buttons = 0;
        sys::ecs_script_mouse_event(w, &ev);
    }
}

/// What scripts asked for since the last call.
pub fn drain() -> Vec<(String, bool)> {
    std::mem::take(&mut *ACTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}
