//! `unstable_name_collisions`, from rustc's UI tests (`tests/ui/inference/`) with their
//! `.stderr` and auxiliary crates.

use crate::{
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, normalized, warning},
};

/// The std the test's `assert_eq!` comes from: a macro that evaluates both sides, all the test
/// asks of it.
const STD: &str = "//- minicore: sized
//- /std.rs crate:std edition:2015 library
pub mod prelude {
    pub mod rust_2015 {
        pub use core::prelude::rust_2015::*;
        pub use crate::assert_eq;
    }
}
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (_left, _right) => {}
        }
    };
}
";

#[test]
fn inference_unstable() {
    let source = include_str!("unstable_names/inference_unstable.rs");
    let fixture = format!(
        "{STD}//- /auxiliary/inference_unstable_iterator.rs crate:inference_unstable_iterator edition:2015 library\n{}\
         //- /auxiliary/inference_unstable_itertools.rs crate:inference_unstable_itertools edition:2015 library\n{}\
         //- /inference_unstable.rs crate:other:main edition:2015 \
         deps:std,inference_unstable_iterator,inference_unstable_itertools\n{source}",
        include_str!("unstable_names/auxiliary/inference_unstable_iterator.rs"),
        include_str!("unstable_names/auxiliary/inference_unstable_itertools.rs"),
    );
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::RunPass, &crate::Options::default()));
    let written = normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }));
    assert_reports(
        &written,
        &[
            warning(16, &[]),
            warning(19, &[]),
            warning(22, &[]),
            warning(25, &[]),
            warning(28, &[]),
        ],
    );
}
