//! The names of the lint store that are not lints: rustc's groups, alias, renamed, removed and
//! ignored names and its `rustc::internal` group (`rustc.rs`, generated), and blaze's own
//! groups and renames, written here. `const`s, which the registry's perfect hash of names is
//! built from.

use super::rustc;
pub(super) use super::rustc::{ALIASES, GROUPS, IGNORED, INTERNAL_GROUP};

/// The groups of blaze's own lints, after rustc's: `blaze::hot`, the lints of hot code
/// (blaze-lints' `blaze::HOT`, which its tests hold to this); `blaze::general`, the lints on
/// at `warn` in every workspace; `blaze::internal`, the ones of blaze's own types and
/// conventions, allowed until a workspace turns them on (blaze-lints' `blaze::Kind`);
/// `blaze::reach`, the cross-configuration reachability lints, which a workspace makes strict;
/// `blaze::edition`, what the blaze edition removes (blaze-lints' `blaze::Kind::Edition`), and
/// `blaze::proc_macro`, the procedural-macro lints among them.
pub(super) const BLAZE_GROUPS: &[(&str, &[&str])] = &[
    (
        "blaze::hot",
        &[
            "blaze::hot_clone",
            "blaze::hot_alloc",
            "blaze::hot_alloc_call",
            "blaze::hot_dyn",
            "blaze::default_hasher",
            "blaze::implicit_allocator",
            "blaze::large_move",
            "blaze::cold_error",
        ],
    ),
    (
        "blaze::general",
        &[
            "blaze::false_sharing",
            "blaze::seqcst_unexplained",
            "blaze::collect_then_consume",
            "blaze::hash_twice",
            "blaze::outline_generic_body",
            "blaze::cold_error",
            "blaze::frozen_collection",
            "blaze::intern_candidate",
            "blaze::small_map",
            "blaze::arc_clone_only_borrowed",
            "blaze::blocking_in_async",
            "blaze::relaxed_publish",
            "blaze::unwind_across_ffi",
            "blaze::unbounded_from_input",
            "blaze::dos_hasher_on_input",
            "blaze::panic_in_drop",
            "blaze::bounds_check_in_loop",
            "blaze::unbuffered_io",
            "blaze::stdout_lock_in_loop",
            "blaze::front_vec_ops",
            "blaze::missing_size_hint",
            "blaze::fmt_allocates",
            "blaze::jagged_vec",
            "blaze::stable_sort_needless",
            "blaze::dyn_in_hot_field",
            "blaze::profile_hot",
            "blaze::stale_hot_marker",
            "blaze::hot_marked_cold",
            "blaze::push_without_reserve",
            "blaze::scratch_in_loop",
            "blaze::boxed_recursive_node",
            "blaze::wide_index",
            "blaze::bounded_heap_field",
            "blaze::enum_vec_padded",
            "blaze::dyn_fn_param",
            "blaze::box_large_new",
            "blaze::checkfree",
            "blaze::nopanic",
            "blaze::noalloc",
            "blaze::nonblocking",
            "blaze::pure",
            "blaze::capabilities",
            "blaze::unreachable",
            "blaze::todo",
            "blaze::undeclared_effect",
            "blaze::unfulfilled_effect",
            "blaze::negative_effect",
            "blaze::borrow_workaround",
            "blaze::native_contracts",
            "blaze::dead",
            "blaze::test_only",
            "blaze::cfg_only",
        ],
    ),
    (
        "blaze::pedantic_perf",
        &[
            "blaze::deep_drop_at_exit",
            "blaze::pod_serialized_by_field",
            "blaze::packable",
            "blaze::soa_candidate",
            "blaze::vecreport",
        ],
    ),
    // What the blaze edition removes or respells (docs/v2/plan/design/edition.md): the code blaze
    // cannot analyse, what blaze's standard library replaces (`std_use`), `lossy_as`,
    // `implicit_overflow`,
    // `global_state`, `async_fn`, `mut_ref`, and the marks it writes at a crate's border
    // (`unmarked_*`). Allowed until a workspace turns the group on, but for the members whose
    // advice this blaze already has (`ready`), which warn.
    (
        "blaze::edition",
        &[
            "blaze::proc_macro_derive",
            "blaze::proc_macro_attribute",
            "blaze::proc_macro_fn_like",
            "blaze::build_script_ambient",
            "blaze::not_latest_edition",
            "blaze::std_use",
            "blaze::lossy_as",
            "blaze::implicit_overflow",
            "blaze::global_state",
            "blaze::async_fn",
            "blaze::unmarked_pure",
            "blaze::unmarked_nopanic",
            "blaze::unmarked_noalloc",
            "blaze::unmarked_nonblocking",
            "blaze::mut_ref",
            // Code written the way the edition wants it (docs/v2/plan/design/facts.md).
            "blaze::program_lifetime",
            "blaze::could_be_program_lifetime",
            "blaze::could_be_ref32",
        ],
    ),
    (
        "blaze::internal",
        &[
            "blaze::hot_clone",
            "blaze::hot_alloc",
            "blaze::hot_dyn",
            "blaze::default_hasher",
            "blaze::parallelizable",
            "blaze::serial_dependence",
            "blaze::fiber_boundary",
            "blaze::hot_alloc_call",
            "blaze::error_debug",
            "blaze::wall_clock",
            "blaze::untyped_error",
            "blaze::dropped_error",
            "blaze::hash_order_output",
            "blaze::held_across_wait",
            "blaze::thread_local_in_waiting_fn",
            "blaze::lock_across_query",
            "blaze::lock_held_across_slow",
            "blaze::blocking_on_pool",
            "blaze::unnamed_wait",
            "blaze::shared_collection_lock",
            "blaze::open_wait_loop",
            "blaze::unrecorded_span",
            "blaze::implicit_allocator",
            "blaze::fixed_key_lookup",
            "blaze::struct_update",
            "blaze::bool_param",
            "blaze::many_bools",
            "blaze::default_policy",
            "blaze::sans_io_core",
            "blaze::key_ignores_field",
            "blaze::wildcard_arm",
            "blaze::large_move",
            "blaze::syscall_in_loop",
            "blaze::query_in_loop",
            "blaze::string_key",
            "blaze::nll_workaround",
            "blaze::lenient_deserialize",
            "blaze::durable_write",
            "blaze::unbounded_wait",
            "blaze::combined_identity",
            "blaze::span_field_unrecorded",
            "blaze::env_read",
            "blaze::untied_child",
            "blaze::unbounded_test_wait",
            "blaze::reads_positions",
            "blaze::cbor_field_order",
            "blaze::gapped_literal",
            "blaze::retry",
            "blaze::unwired",
            "blaze::tmp_path",
            "blaze::narrowed_len",
            "blaze::process_static",
        ],
    ),
    // The cross-configuration reachability lints (docs/v2/plan/design/multiconfig-check.md), also in
    // `blaze::general`: one name to make them strict, `reach = "deny"`.
    ("blaze::reach", &["blaze::dead", "blaze::test_only", "blaze::cfg_only"]),
    // The three kinds of procedural macro a workspace crate defines, also in `blaze::edition`:
    // one name for all of them, `proc_macro = "warn"` or `#![allow(blaze::proc_macro)]`.
    (
        "blaze::proc_macro",
        &["blaze::proc_macro_derive", "blaze::proc_macro_attribute", "blaze::proc_macro_fn_like"],
    ),
];

/// `register_renamed`: the old name and the new. blaze-lints' `blaze::RENAMED`, then rustc's.
pub(super) const RENAMED: &[(&str, &str)] = &{
    let mut out = [("", ""); BLAZE_RENAMED.len() + rustc::RENAMED.len()];
    let mut at = 0;
    while at < out.len() {
        out[at] = match at < BLAZE_RENAMED.len() {
            true => BLAZE_RENAMED[at],
            false => rustc::RENAMED[at - BLAZE_RENAMED.len()],
        };
        at += 1;
    }
    out
};

const BLAZE_RENAMED: &[(&str, &str)] = &[("blaze::std_hashmap", "blaze::default_hasher")];

/// `register_removed`: the name and why. blaze's own, deleted with no alias (AGENTS.md rule 30),
/// then rustc's.
pub(super) const REMOVED: &[(&str, &str)] = &{
    let mut out = [("", ""); BLAZE_REMOVED.len() + rustc::REMOVED.len()];
    let mut at = 0;
    while at < out.len() {
        out[at] = match at < BLAZE_REMOVED.len() {
            true => BLAZE_REMOVED[at],
            false => rustc::REMOVED[at - BLAZE_REMOVED.len()],
        };
        at += 1;
    }
    out
};

const BLAZE_REMOVED: &[(&str, &str)] = &[(
    "blaze::unneeded_effect",
    "a word of a function's own list it is proved not to have is `blaze::unfulfilled_effect`, deny: `#[blaze::expect(..)]` is exact",
)];
