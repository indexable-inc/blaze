//! rustc's UI test of `MapUnitFn`: `tests/ui/lint/lint_map_unit_fn.rs` (MIT or Apache-2.0), its
//! source and `.stderr` as they are (in `map_unit_fn/`).

use super::{
    CORE_CMP, CORE_CMP_PRELUDE, CORE_ITER, CORE_ITER_PRELUDE, CORE_OPS, CORE_OPS_PRELUDE, CORE_OPTION_RESULT,
    CORE_OPTION_RESULT_PRELUDE, Library, STD_VEC, STD_VEC_PRELUDE, ui_library,
};
use crate::tests::lints::bodies::calls::Ui;

#[test]
fn lint_map_unit_fn() {
    let core = format!("{CORE_OPS}{CORE_OPTION_RESULT}{CORE_CMP}{CORE_ITER}");
    let core_prelude = format!("{CORE_OPS_PRELUDE}{CORE_OPTION_RESULT_PRELUDE}{CORE_CMP_PRELUDE}{CORE_ITER_PRELUDE}");
    let library = Library {
        core: &core,
        core_prelude: &core_prelude,
        std: STD_VEC,
        std_prelude: STD_VEC_PRELUDE,
        ..Library::default()
    };
    let ui =
        Ui { name: "lint_map_unit_fn.rs", source: include_str!("map_unit_fn/lint_map_unit_fn.rs"), ..Ui::default() };
    assert_eq!(ui_library(ui, &library), include_str!("map_unit_fn/lint_map_unit_fn.stderr"));
}
