//! The module ABI: what a den-native module cdylib exports and the host calls (docs/hot-reload.md, Native modules).
//!
//! [`module!`](crate::module!) exports, under names suffixed with the module's name (so any number of modules also link into
//! the host as rlibs, its built-in copies):
//!
//! - `den_module_manifest_<name>`: a data-only [`Manifest`] static (hyperion's schema 3) the host reads with `dlsym` before any
//!   code of the image runs: the rustc build id and this crate's layout table as the image's copy has it.
//! - `den_module_init_<name>(world) -> 0`: create the module's systems. The host calls it with the world's scope set to the
//!   module's own entity, so all it makes is a child of that entity; deinit is the host deleting it, which drops the systems'
//!   closures through the image that made them while it is still mapped. The module keeps no state of its own.
//! - `den_module_take_panic_<name>(buf, cap) -> len`: a panic the image caught at an entry (init, a system), for the host's log.
//!   After one the image is quarantined: every entry returns at once.
use core::ffi::{CStr, c_char, c_int, c_void};
use harness_native_flecs::WorldRef;
pub use hyperion_manifest_types::{DEP_REQUIRED, MANIFEST_MAGIC, MANIFEST_SCHEMA, Manifest, ManifestDep};

pub type InitFn = unsafe extern "C" fn(world: *mut c_void) -> c_int;
pub type TakePanicFn = hyperion_abi::TakePanicFn;

/// `den_module_init_<name>` returns this after a panic (the message waits in `den_module_take_panic_<name>`).
pub const INIT_PANICKED: c_int = 1;

#[must_use]
pub fn manifest_symbol(name: &str) -> String {
    format!("den_module_manifest_{name}")
}

#[must_use]
pub fn init_symbol(name: &str) -> String {
    format!("den_module_init_{name}")
}

#[must_use]
pub fn take_panic_symbol(name: &str) -> String {
    format!("den_module_take_panic_{name}")
}

// ALLOW-STATE: per image, read only by this image's host through the manifest (`deps`).
static DEPS: [ManifestDep; 1] = [ManifestDep { flags: DEP_REQUIRED, _pad: 0, layouts: &crate::API_LAYOUTS }];

/// The manifest of a module built against this crate: one required dependency, its layouts.
#[must_use]
pub const fn manifest(name: &'static CStr) -> Manifest {
    manifest_with_deps(name, &DEPS)
}

/// A module's complete API tuple. Every listed crate is checked before any of its code runs.
#[must_use]
pub const fn manifest_with_deps(name: &'static CStr, deps: &'static [ManifestDep]) -> Manifest {
    Manifest {
        magic: MANIFEST_MAGIC,
        schema: MANIFEST_SCHEMA,
        size: size_of::<Manifest>() as u32,
        name: name.as_ptr(),
        version: 1,
        contract: hyperion_abi::ABI_VERSION,
        host_need: 0,
        _pad: 0,
        build_id: hyperion_abi::BUILD_ID,
        n_deps: deps.len() as u32,
        n_data: 0,
        deps: deps.as_ptr(),
        data: core::ptr::null(),
        n_local: 0,
        _pad2: 0,
        local: core::ptr::null(),
    }
}

/// Body of `den_module_init_<name>`: `install` under this image's panic catch, after which a fault quarantines the image.
///
/// # Safety
/// `world` is the host's live world, used on the host's world thread outside `progress`.
pub unsafe fn init(world: *mut c_void, install: fn(WorldRef<'_>)) -> c_int {
    hyperion_panic::enable_quarantine();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(l) = info.location() {
            hyperion_panic::note_location(l);
        }
        default(info);
    }));
    hyperion_panic::catch(
        "init",
        || INIT_PANICKED,
        || {
            // SAFETY: the caller's contract.
            install(unsafe { WorldRef::from_ptr(world) });
            0
        },
    )
}

/// Body of `den_module_take_panic_<name>`.
///
/// # Safety
/// `buf` is writable for `cap` bytes.
pub unsafe fn take_panic(buf: *mut u8, cap: usize) -> usize {
    // SAFETY: the caller's contract.
    hyperion_panic::take(unsafe { core::slice::from_raw_parts_mut(buf, cap) })
}

/// Export a module: `harness_native_api::module!("hover", install);` with `fn install(w: WorldRef)` creating its systems.
#[macro_export]
macro_rules! module {
    ($name:literal, $install:path) => {
        $crate::module!($name, $install, [$crate::API_LAYOUTS]);
    };
    ($name:literal, $install:path, [$($layouts:path),+ $(,)?]) => {
        // Per image, data only; kept alive by its exported manifest.
        static DEN_MODULE_DEPS: &[$crate::module::ManifestDep] = &[
            $($crate::module::ManifestDep { flags: $crate::module::DEP_REQUIRED, _pad: 0, layouts: &$layouts }),+
        ];
        #[unsafe(export_name = concat!("den_module_manifest_", $name))]
        pub static DEN_MODULE_MANIFEST: $crate::module::Manifest =
            $crate::module::manifest_with_deps(match ::core::ffi::CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()) {
                Ok(c) => c,
                Err(_) => panic!("a module name has no NUL"),
            }, DEN_MODULE_DEPS);

        /// # Safety
        /// The host's module ABI (harness-native-api `module`).
        #[unsafe(export_name = concat!("den_module_init_", $name))]
        pub unsafe extern "C" fn den_module_init(world: *mut ::core::ffi::c_void) -> ::core::ffi::c_int {
            // SAFETY: the host passes its live world, outside `progress`.
            unsafe { $crate::module::init(world, $install) }
        }

        /// # Safety
        /// `buf` is writable for `cap` bytes.
        #[unsafe(export_name = concat!("den_module_take_panic_", $name))]
        pub unsafe extern "C" fn den_module_take_panic(buf: *mut u8, cap: usize) -> usize {
            // SAFETY: the caller's contract.
            unsafe { $crate::module::take_panic(buf, cap) }
        }
    };
}

/// The module's name as the manifest has it.
///
/// # Safety
/// `m` is a manifest whose `name` is null or a NUL-terminated string that outlives it.
#[must_use]
pub unsafe fn name_of(m: &Manifest) -> String {
    let p: *const c_char = m.name;
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
