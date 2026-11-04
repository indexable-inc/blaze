//! flecs script entities: a named entity holding a script (`EcsScript`), run once when made and updated in place
//! (`ecs_script_update`: what the old source made is deleted, the new source runs). A failure leaves its message in the script's
//! `error` field (read it with `get::<&flecs::Script>`).
use crate::{Entity, EntityView, IntoId, World, WorldRef};
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

    /// Make the script entity and run the file at `filename` in it (one `harness_native_ecs::embed_files` serves, or one on
    /// disk). Its `include`s resolve next to that name and load as managed scripts of their own.
    pub fn build_from_file(self, filename: &str) -> ScriptEntityView<'a> {
        let e = self.world.entity_named(&self.path);
        let file = CString::new(filename.replace('\0', " ")).expect("NUL removed");
        let _one = one_at_a_time();
        // SAFETY: zeroed desc; the entity is live; flecs copies the filename and reads the file before returning.
        unsafe {
            let mut d: sys::ecs_script_desc_t = core::mem::zeroed();
            d.entity = e.id;
            d.filename = file.as_ptr();
            sys::ecs_script_init(e.w.as_ptr(), &d);
        }
        ScriptEntityView(e)
    }
}

/// A value for one prop of a script template (`prop name: string`), set from the host by the prop's name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Prop<'s> {
    Str(&'s str),
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
}

impl core::hash::Hash for Prop<'_> {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        match *self {
            Prop::Str(s) => (0u8, s).hash(h),
            Prop::Int(v) => (1u8, v).hash(h),
            Prop::Uint(v) => (2u8, v).hash(h),
            Prop::Float(v) => (3u8, v.to_bits()).hash(h),
            Prop::Bool(v) => (4u8, v).hash(h),
        }
    }
}

/// Write `v` at the cursor's member; false when the member's type refuses it.
///
/// # Safety
/// `c` is a cursor positioned on a member.
unsafe fn put(c: &mut sys::ecs_meta_cursor_t, v: Prop<'_>) -> bool {
    // SAFETY: the caller's contract; flecs copies a string it is handed.
    unsafe {
        match v {
            Prop::Str(s) => {
                let s = CString::new(s.replace('\0', "")).unwrap_or_default();
                sys::ecs_meta_set_string(c, s.as_ptr()) == 0
            }
            Prop::Int(i) => sys::ecs_meta_set_int(c, i) == 0,
            Prop::Uint(u) => sys::ecs_meta_set_uint(c, u) == 0,
            Prop::Float(f) => sys::ecs_meta_set_float(c, f) == 0,
            Prop::Bool(b) => sys::ecs_meta_set_bool(c, b) == 0,
        }
    }
}

impl<'a> EntityView<'a> {
    /// Set props of `template` (a script's `template Name { prop ... }`) on this entity, by name, keeping the ones not named:
    /// the first set instantiates the template on it, a later one re-runs only what reads a prop that changed (flecs compares
    /// the old and new values member by member). A name the template does not have is skipped and named in the error.
    pub fn set_props(self, template: Entity, props: &[(&str, Prop<'_>)]) -> Result<(), String> {
        let (w, t) = (self.w.as_ptr(), template.0);
        let _one = one_at_a_time();
        // SAFETY: a live world and entity; `t` is a component with a reflected struct type (a template is), so a value of it can
        // be made, copied, walked with a cursor and handed to set (which copies it) before it is dropped.
        unsafe {
            let ti = sys::ecs_get_type_info(w, t);
            if ti.is_null() {
                return Err("not a template".into());
            }
            let cur = sys::ecs_get_id(w, self.id, t);
            let mut v = if cur.is_null() { sys::ecs_value_new(w, t) } else { sys::ecs_value_init(w, t, cur) };
            let mut c = sys::ecs_meta_cursor(w, t, v.ptr);
            let mut bad = Vec::new();
            sys::ecs_meta_push(&mut c);
            for (name, val) in props {
                let n = CString::new(*name).unwrap_or_default();
                // An unknown member leaves the cursor where it was: writing then would land on the previous member.
                if sys::ecs_meta_member(&mut c, n.as_ptr()) != 0 || !put(&mut c, *val) {
                    bad.push(*name);
                }
            }
            sys::ecs_meta_pop(&mut c);
            sys::ecs_set_id(w, self.id, t, (*ti).size as usize, v.ptr);
            sys::ecs_value_fini(w, &mut v);
            if bad.is_empty() { Ok(()) } else { Err(format!("no prop {bad:?}")) }
        }
    }
}

#[cfg(feature = "json")]
impl<'a> WorldRef<'a> {
    /// Set the script variable at `path` (`export mut tone_term_ground` is `tone_term_ground`) from `json` (its value as flecs
    /// reads JSON, `{"dark": {"r": 1, ...}}`), and tell every script that reads it. False when there is no such variable or the
    /// value does not fit its type.
    pub fn set_mut_var(self, path: &str, json: &str) -> bool {
        let w = self.raw().as_ptr();
        let (Ok(p), Ok(j)) = (CString::new(path.replace('.', "::")), CString::new(json)) else { return false };
        let _one = one_at_a_time();
        // SAFETY: a live world; the variable's value is a live value of its type, which ptr_from_json writes in place.
        unsafe {
            let e = sys::ecs_lookup_path_w_sep(w, 0, p.as_ptr(), c"::".as_ptr(), c"::".as_ptr(), true);
            if e == 0 {
                return false;
            }
            let v = sys::ecs_mut_var_get(w, e);
            if v.ptr.is_null() || sys::ecs_ptr_from_json(w, v.type_, v.ptr, j.as_ptr(), core::ptr::null()).is_null() {
                return false;
            }
            sys::ecs_mut_var_modified(w, e);
        }
        true
    }

    /// The script variable at `path` as JSON (an `export const` or `export mut`), None when there is none.
    #[must_use]
    pub fn var_json(self, path: &str) -> Option<String> {
        let w = self.raw().as_ptr();
        let p = CString::new(path.replace('.', "::")).ok()?;
        // SAFETY: a live world; the JSON string is flecs's, copied and freed.
        unsafe {
            let e = sys::ecs_lookup_path_w_sep(w, 0, p.as_ptr(), c"::".as_ptr(), c"::".as_ptr(), true);
            if e == 0 {
                return None;
            }
            let v = sys::ecs_const_var_get(w, e);
            let v = if v.ptr.is_null() { sys::ecs_mut_var_get(w, e) } else { v };
            if v.ptr.is_null() {
                return None;
            }
            let s = sys::ecs_ptr_to_json(w, v.type_, v.ptr);
            if s.is_null() {
                return None;
            }
            let out = std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
            if let Some(free) = sys::ecs_os_api.free_ {
                free(s.cast());
            }
            Some(out)
        }
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
