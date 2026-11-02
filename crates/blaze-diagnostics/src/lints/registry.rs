//! Every lint rustc knows: `blaze-lint-defs`, which the front end reads too. What stays here is
//! diagnostic wording: rustc's suggestion of a similar name for an unknown one.
//!
//! Follows rustc's `compiler/rustc_lint/src/context.rs` (`no_lint_suggestion`) (MIT or
//! Apache-2.0).

pub use blaze_lint_defs::*;

use stdx::edit_distance;

/// What rustc suggests for an unknown lint `name` that [`Store::check`] found
/// [`Checked::NoLint`] with `suggest`, written with `tool::` before it when `tool` is given:
/// the similar name, and whether that one is rustc's own where a tool's was asked for.
pub fn suggestion(store: &Store, name: &str, tool: Option<&str>) -> Option<(String, bool)> {
    match tool {
        // alloc: a lint no tool has, on an error's path
        Some(tool) => no_lint_suggestion(store, &format!("{tool}::{name}"), tool),
        None => no_lint_suggestion(store, name, "clippy"),
    }
}

/// rustc's `no_lint_suggestion`: the name in lowercase when that is a lint's, else the
/// nearest name of a group (aliases left out) or a lint.
fn no_lint_suggestion(store: &Store, name: &str, tool: &str) -> Option<(String, bool)> {
    let lower = name.to_lowercase();
    if name.chars().any(char::is_uppercase) && store.find_lints(&lower).is_some() {
        return Some((lower, false));
    }
    let mut groups: Vec<&str> =
        // alloc: a suggestion for an unknown lint, on an error's path
        store.groups().filter(|it| it.alias_of.is_none()).map(|it| it.name).collect();
    groups.sort_unstable();
    // alloc: as above
    let names: Vec<&str> = groups.into_iter().chain(store.lints().map(|it| it.name)).collect();
    // alloc: as above
    let mut lookups = vec![lower.as_str()];
    if let Some(last) = lower.split("::").last() {
        lookups.push(last);
    }
    let found = edit_distance::find_best_match_for_names(&names, &lookups, None).copied();
    found.map(|it| {
        let is_rustc = lower.contains("::") && !it.starts_with(tool);
        // alloc: as above
        (it.to_owned(), is_rustc)
    })
}

#[cfg(test)]
mod tests;
