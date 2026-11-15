//! `unreachable_pub` (`lints/names/items/publicity.rs`) where a module is reached through a
//! `use`: blaze-check's `sync::lot`, a `pub mod` of the private `lock` that `sync` re-exports,
//! holds `pub use super::{MutexGuard, ..}`, which another crate names as
//! `blaze_check::sync::lot::MutexGuard`. The lint called each of them unreachable, since it read
//! a `use`'s reach from its modules' own visibilities alone (2026-11-12, 4 of the 33 warnings of
//! a bstd hello world).

use crate::tests::{assert_reports, error};

const REEXPORTED: &str = r#"#![crate_type = "lib"]
#![deny(unreachable_pub)]

mod lock {
    pub struct Guard;
    pub mod lot {
        pub use super::Guard;
        pub struct Mutex;
    }
    pub struct Hidden;
}
pub use lock::lot;

mod private {
    pub use super::lock::Hidden as Other;
}
"#;

/// A `pub use` in a module another crate can name is reachable, whatever its module's own
/// path is; one in a module nothing exports is not, and neither is the item it names.
#[test]
fn a_use_in_a_module_reached_through_a_use_is_reachable() {
    let stderr = super::ui_core("publicity.rs", "2021", "sized", REEXPORTED);
    // Lines 7 (`pub use super::Guard`), 5 and 8 are reachable through `pub use lock::lot`:
    // `assert_reports` fails on any error beside the two expected.
    assert_reports(
        &stderr,
        &[
            error(10, &["unreachable `pub` item", "pub(crate)"]),
            error(15, &["unreachable `pub` item", "pub(crate)"]),
        ],
    );
}
