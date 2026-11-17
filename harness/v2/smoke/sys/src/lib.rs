unsafe extern "C" {
    fn smoke_two() -> i32;
}

pub fn two() -> i32 {
    // SAFETY: `smoke_two` takes nothing and returns an int; build.rs compiles it.
    unsafe { smoke_two() }
}
