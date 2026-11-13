//! A lint's level as something outside the code sets it: `blaze check`'s `--allow`, `--warn`,
//! `--deny` and `--forbid` (read by the client's `cli/args.rs` and by the load's
//! `load/invocation.rs`), the workspace's lint tables, a ratchet baseline and a package's
//! `[lints]` (`load/plan/flags.rs`). The one home of what each of them is refused for.

use blaze_lint_defs::Named;

/// The levels a command line or a table sets, by the names both give them.
pub const LEVELS: [&str; 4] = ["allow", "warn", "deny", "forbid"];

/// The lint nothing sets below `deny`.
pub const PROVED: &str = "blaze::unreachable";

/// The rule [`below_floor`] refuses by, as every error of it says it.
pub const FLOOR_RULE: &str = "`blaze::unreachable` can't be set below `deny`: a \
                              `blaze::unreachable!()` compiles only where blaze proves no path \
                              reaches it";

/// Whether `level` of `lint`, by its full name, is refused by [`FLOOR_RULE`]: `allow` or `warn`
/// of exactly [`PROVED`]. `deny` and `forbid` of it are taken. Only the lint by its own name: a
/// level of a group that holds it (`blaze::general`) is not refused here.
pub fn below_floor(level: &str, lint: &str) -> bool {
    lint.strip_prefix("blaze::").is_some_and(|name| blazes_below_floor(level, name))
}

/// [`below_floor`] of blaze's lint `name`, named without its tool as the tables name it.
pub(crate) fn blazes_below_floor(level: &str, name: &str) -> bool {
    PROVED.strip_prefix("blaze::") == Some(name) && matches!(level, "allow" | "warn")
}

/// Why `blaze {command} --{level} LINT` is refused for a `command` other than `check`.
pub fn check_only(level: &str, command: &str) -> String {
    // alloc: an error, once
    format!(
        "`--{level}` sets a lint's level, which `blaze check` alone takes from its command line: \
         `blaze {command}` reads each lint's level from `[lints]` and the workspace's tables"
    )
}

/// Whether `blaze::{lint}` is a lint or group of the compile's registry, which has blaze-lints'
/// as well as the front end's: a level set for any other name would set nothing.
pub(crate) fn is_blazes(lint: &str) -> bool {
    // alloc: a lint's full name, once per line of a table or level of a command line
    let full = format!("blaze::{lint}");
    matches!(blaze_lint_defs::named(&full), Named::Lint(_) | Named::Group(_))
}

/// Why the command line's `--{level} {lint}` is refused, when it is: `lint` is neither
/// `tool::name` nor a plain name as rustc's lints have (letters, digits, `_` and `-`); it is
/// `blaze::name` and blaze has no such lint or group; or [`FLOOR_RULE`]. A name of another tool
/// or of rustc's is not looked up here: each unit's check refuses lint flags that name no lint
/// (blaze-cargo's `serve/unit/compile.rs`, `options`).
pub fn refused(level: &str, lint: &str) -> Option<String> {
    let word = |it: &str| {
        !it.is_empty() && it.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    let named = match lint.split_once("::") {
        Some((tool, name)) => word(tool) && word(name),
        None => word(lint),
    };
    if !named {
        // alloc: an error, once
        return Some(format!(
            "`{lint}` names no lint: a lint is `tool::name` (`blaze::todo`, `clippy::all`) or one \
             of rustc's by its plain name (`unused`)"
        ));
    }
    if let Some(name) = lint.strip_prefix("blaze::")
        && !is_blazes(name)
    {
        return Some(format!("`{lint}` is no lint or group of blaze's")); // alloc: an error, once
    }
    below_floor(level, lint).then(|| FLOOR_RULE.to_owned()) // alloc: an error, once
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_of_the_command_line_names_a_lint() {
        for lint in ["blaze::todo", "blaze::general", "clippy::all", "unused", "unused-imports"] {
            for level in LEVELS {
                assert_eq!(refused(level, lint), None, "--{level} {lint}");
            }
        }
        for lint in ["", "blaze::", "::todo", "a::b::c", "blaze::todo,unused", "unused imports"] {
            let why = refused("deny", lint).unwrap_or_else(|| panic!("`{lint}` was taken"));
            assert!(why.starts_with(&format!("`{lint}` names no lint: ")), "{why}");
        }
        assert_eq!(
            refused("deny", "blaze::todos").as_deref(),
            Some("`blaze::todos` is no lint or group of blaze's")
        );
    }

    #[test]
    fn the_proved_lint_is_set_to_deny_or_forbid_alone() {
        assert_eq!(refused("allow", PROVED).as_deref(), Some(FLOOR_RULE));
        assert_eq!(refused("warn", PROVED).as_deref(), Some(FLOOR_RULE));
        assert_eq!(refused("deny", PROVED), None);
        assert_eq!(refused("forbid", PROVED), None);
        assert!(!below_floor("warn", "blaze::general"), "a group is not the lint");
        assert!(matches!(blaze_lint_defs::named(PROVED), Named::Lint(_)), "{PROVED} is a lint");
    }
}
