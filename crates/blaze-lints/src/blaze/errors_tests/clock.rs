//! `wall_clock` with no `clippy.toml`: clippy refuses a `lint` key in its entries, so the lint
//! knows the wall clock's paths itself, over a `std` whose `time` has them.

use super::check_in;

const CLOCK_CRATES: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod time {
    pub struct Duration(u64);
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant {
            loop {}
        }
        pub fn elapsed(&self) -> Duration {
            loop {}
        }
        pub fn duration_since(&self, _earlier: Instant) -> Duration {
            loop {}
        }
    }
    pub struct SystemTime(u64);
    impl SystemTime {
        pub fn now() -> SystemTime {
            loop {}
        }
        pub fn elapsed(&self) -> Duration {
            loop {}
        }
    }
}
pub mod thread {
    pub fn sleep(_length: crate::time::Duration) {}
    pub fn yield_now() {}
}
//- /src/lib.rs crate:main deps:hir_ty,std
"#;

#[test]
fn wall_clock_reads_are_found_with_no_configuration() {
    check_in(
        CLOCK_CRATES,
        r#"
use std::time::{Duration, Instant, SystemTime};
pub fn started() -> Instant {
    Instant::now() //~ wall_clock
}
pub fn stamped() -> SystemTime {
    SystemTime::now() //~ wall_clock
}
pub fn took(start: &Instant) -> Duration {
    start.elapsed() //~ wall_clock
}
pub fn aged(stamp: &SystemTime) -> Duration {
    stamp.elapsed() //~ wall_clock
}
pub fn between(start: &Instant, end: Instant) -> Duration {
    start.duration_since(end)
}
pub fn waited(length: Duration) {
    std::thread::sleep(length); //~ wall_clock
    std::thread::yield_now();
}
"#,
        "-A clippy::all --warn=blaze::wall_clock",
    );
}
