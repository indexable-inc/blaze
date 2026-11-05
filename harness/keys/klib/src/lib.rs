//! Each function's code depends on something that is not the text of this file.

pub fn outside() -> &'static str {
    include_str!("../../shared.txt")
}

pub fn inside() -> &'static str {
    include_str!("inside.txt")
}

pub fn generated() -> u32 {
    include!(concat!(env!("OUT_DIR"), "/gen.rs"))
}

pub fn script_env() -> &'static str {
    env!("KLIB_FROM_SCRIPT")
}

pub fn script_cfg() -> bool {
    cfg!(klib_flag)
}

pub fn feature() -> bool {
    cfg!(feature = "extra")
}

pub fn process_env() -> &'static str {
    option_env!("KEYS_PROCESS").unwrap_or("unset")
}

pub fn config_env() -> &'static str {
    option_env!("KEYS_CONFIG").unwrap_or("unset")
}

pub fn flag_cfg() -> bool {
    cfg!(keys_flag)
}

pub fn assertions() -> bool {
    cfg!(debug_assertions)
}

pub fn overflows(x: u8) -> bool {
    std::panic::catch_unwind(|| x + std::hint::black_box(1)).is_err()
}

pub fn scaled(x: u32) -> u32 {
    x * kdep::SCALE + kdep::inlined(x)
}

pub fn shape() -> usize {
    std::mem::size_of::<kdep::Shape>()
}

/// rustc's `StableCrateId` of this crate is in the hash, and `-C metadata` is in that.
pub fn identity() -> String {
    pub struct Marker;
    format!("{:?}", std::any::TypeId::of::<Marker>())
}

pub fn described() -> String {
    kdep::describe::<(u8, u32)>()
}

/// Which of the sums of the dependency panic when they overflow, called from here.
pub fn checks(x: u8) -> [bool; 5] {
    let caught = |f: &dyn Fn() -> u8| std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err();
    [
        caught(&|| kdep::stepped(x)),
        caught(&|| kdep::sum(x, std::hint::black_box(1))),
        caught(&|| kdep::widened(x)),
        caught(&|| kdep::bumped(x)),
        caught(&|| kdep::small(std::hint::black_box(x))),
    ]
}
