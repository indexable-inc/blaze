//! The `blaze_query::QueryKind` numbers of the resolver's queries, for traces
//! (`hir_def::nameres::kinds`). A query that is dropped keeps its number unused; a new one
//! takes the next.

blaze_query::query_kinds! {
    range: 0x2200..0x2240, owner: "the resolver";
    MODULE_DECLS = 0x2200, "module_decls";
    LEGACY_EXPORTS = 0x2201, "legacy_exports";
    SUBTREE_MACRO_EXPORTS = 0x2202, "subtree_macro_exports";
    MODULE_ROUND = 0x2203, "module_round";
    IMPORT_SHAPE = 0x2204, "import_shape";
    PARTITION = 0x2205, "partition";
    /// `module_rows` of the notes is `round_modules` with `row`; the number stays unused.
    LATEST = 0x2207, "latest";
    SCC_STATE = 0x2208, "scc_state";
    FACT_HANDLE = 0x2209, "fact_handle";
    MACRO_STEP = 0x220a, "macro_step";
    MODULE_SCOPE_NAME = 0x220b, "ModuleScope::name";
    MODULE_IMPLS = 0x220c, "module_impls";
    CRATE_ROOT = 0x220d, "crate_root";
    CRATE_DATA = 0x220e, "crate_data";
    ROOT_DECL = 0x220f, "root_decl";
    /// `child_key` is a look-up in `module_decls`' children, no query; the number stays unused.
    DECL_HANDLE = 0x2211, "decl_handle";
    ROUND_MODULES = 0x2212, "round_modules";
    ROUND_ENV = 0x2213, "round_env";
    UNRESOLVED_NAMED = 0x2214, "unresolved_named";
    ROUND_PROGRESS = 0x2215, "round_progress";
    ROUND_KIND = 0x2216, "round_kind";
    ROW = 0x2217, "row";
    ROUND_ACTIVE = 0x2218, "round_active";
    CHILD_MODULE = 0x2219, "child_module";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_distinct_in_range_and_named() {
        for &(kind, query) in ALL {
            assert!(RANGE.contains(&kind.0), "{query} is outside the range");
            assert_eq!(name(kind), Some(query));
            assert_eq!(of(query), Some(kind));
        }
        assert_eq!(name(QueryKind(0x2206)), None);
        assert_eq!(name(QueryKind(0x2210)), None);
        assert_eq!(name(QueryKind(RANGE.end)), None);
    }
}
