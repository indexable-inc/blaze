//! A component member that holds words a flecs script can write (`text: "Ask anything"`).
//!
//! flecs reflects a `char*` on its own allocator as its `string` type, and every script write frees the old words and copies the
//! new. [`ScriptString`] is that `char*`: its clone copies, its drop frees, both on flecs's allocator, which is the one the
//! lifecycle hooks flecs derives for a struct holding one use too, so a value is as safe in a component as out of it. Read it with
//! [`ScriptString::as_str`], and keep a copy as a `String` to hold the words past a change to the world.
use core::ffi::{CStr, c_char};
use core::fmt;
use hyperion_ecs::sys;
use hyperion_layout::layout::{Layout, Prim, Recipe};

/// Words on flecs's allocator, empty when nothing was written.
#[repr(transparent)]
pub struct ScriptString(*mut c_char);

/// A copy of the NUL-terminated `p` on flecs's allocator (null for null, or before a world has set the allocator up).
fn dup(p: *const c_char) -> *mut c_char {
    if p.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `strdup_` is flecs's allocator, set when the first world starts, and is read by value.
    let strdup = unsafe { sys::ecs_os_api.strdup_ };
    // SAFETY: `p` is NUL-terminated, and `strdup` copies it without keeping it.
    strdup.map_or(core::ptr::null_mut(), |strdup| unsafe { strdup(p) })
}

impl ScriptString {
    /// The words, empty when none were written or they are not UTF-8.
    #[must_use]
    pub fn as_str(&self) -> &str {
        if self.0.is_null() {
            return "";
        }
        // SAFETY: a non-null pointer is a NUL-terminated string that lives as long as `self` (flecs writes it, or `dup` made it).
        unsafe { CStr::from_ptr(self.0) }.to_str().unwrap_or("")
    }
}

impl Default for ScriptString {
    fn default() -> Self {
        Self(core::ptr::null_mut())
    }
}

impl Clone for ScriptString {
    fn clone(&self) -> Self {
        Self(dup(self.0))
    }
}

impl Drop for ScriptString {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: `free_` is flecs's allocator, read by value.
        let free = unsafe { sys::ecs_os_api.free_ };
        if let Some(free) = free {
            // SAFETY: a non-null pointer was allocated by flecs's allocator (a script write or `dup`) and is freed once, here.
            unsafe { free(self.0.cast()) };
        }
    }
}

impl From<&str> for ScriptString {
    fn from(words: &str) -> Self {
        let c = std::ffi::CString::new(words.replace('\0', "")).unwrap_or_default();
        Self(dup(c.as_ptr()))
    }
}

impl PartialEq for ScriptString {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl fmt::Debug for ScriptString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl Layout for ScriptString {
    const RECIPE: &'static Recipe = &Recipe::Prim { name: "string", kind: Prim::Str, size: size_of::<Self>(), align: align_of::<Self>() };
}
