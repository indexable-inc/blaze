//! The one home of the macro and attribute names keying treats by name: the quiet list (macro
//! calls that make no item), the late-read macros, and the attributes that are never macros.

#![blaze::sans_io]

use blaze_keyed::kind::Late;

/// Macro calls whose expansion makes no item: std's and core's built-in and formatting macros.
/// A body that calls only these sets no flag in its node's piece; any other call may make items
/// no text shows, so it sets the flag ([`crate::rows::CALLS`]) and what it makes is a fact its
/// body's task makes. Sorted, for the binary search.
///
/// `include` is not here: what it includes is code, whose names no text shows. `env`,
/// `option_env`, `include_str` and `include_bytes` are, with their values late reads
/// ([`late_of`]).
pub const QUIET: [&str; 47] = [
    "addr_of",
    "addr_of_mut",
    "asm",
    "assert",
    "assert_eq",
    "assert_matches",
    "assert_ne",
    "cfg",
    "cfg_select",
    "column",
    "compile_error",
    "concat",
    "concat_bytes",
    "const_format_args",
    "dbg",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_matches",
    "debug_assert_ne",
    "env",
    "eprint",
    "eprintln",
    "file",
    "format",
    "format_args",
    "format_args_nl",
    "include_bytes",
    "include_str",
    "line",
    "matches",
    "module_path",
    "naked_asm",
    "offset_of",
    "option_env",
    "panic",
    "pin",
    "print",
    "println",
    "ready",
    "stringify",
    "todo",
    "try",
    "unimplemented",
    "unreachable",
    "vec",
    "write",
    "writeln",
];

const _: () = {
    let mut at = 1;
    while at < QUIET.len() {
        assert!(less(QUIET[at - 1].as_bytes(), QUIET[at].as_bytes()), "QUIET is sorted, each once");
        at += 1;
    }
};

const fn less(a: &[u8], b: &[u8]) -> bool {
    let mut at = 0;
    while at < a.len() && at < b.len() {
        if a[at] != b[at] {
            return a[at] < b[at];
        }
        at += 1;
    }
    a.len() < b.len()
}

/// Whether a call of the macro named `name` (its last path segment) is quiet.
pub fn is_quiet(name: &[u8]) -> bool {
    QUIET.binary_search_by(|it| it.as_bytes().cmp(name)).is_ok()
}

/// The late read a call of `name` makes, if it is one of the five.
pub fn late_of(name: &[u8]) -> Option<Late> {
    match name {
        b"env" => Some(Late::Env),
        b"option_env" => Some(Late::OptionEnv),
        b"include" => Some(Late::Include),
        b"include_str" => Some(Late::IncludeStr),
        b"include_bytes" => Some(Late::IncludeBytes),
        _ => None,
    }
}

/// `Late` from its byte in a [`crate::rows::LateRow`].
pub fn late_from(byte: u8) -> Option<Late> {
    match byte {
        1 => Some(Late::Env),
        2 => Some(Late::OptionEnv),
        3 => Some(Late::Include),
        4 => Some(Late::IncludeStr),
        5 => Some(Late::IncludeBytes),
        _ => None,
    }
}

/// Tool attribute namespaces: an attribute whose path starts with one is inert.
pub const TOOLS: [&str; 7] = ["blaze", "clippy", "diagnostic", "miri", "rust_analyzer", "rustdoc", "rustfmt"];

/// Built-in attribute macros whose expansion reads no body: `derive` (its input is the item's
/// header; in a body it sets the flag, since the impls it makes are in no text), the test
/// harness's, and `define_opaque` (not in syntax's table yet).
pub const BUILT_IN_MACROS: [&str; 7] =
    ["bench", "cfg_eval", "define_opaque", "derive", "global_allocator", "test", "test_case"];

/// Whether the attribute whose path's first segment is `first` (`whole`: one segment) is inert:
/// built in, a tool's, or a built-in attribute macro. Any other is an attribute macro, whose
/// expansion reads the whole item: its item's header cuts no body.
pub fn is_inert(first: &str, whole: bool) -> bool {
    match whole {
        true => syntax::builtin_attrs::is_built_in(first) || BUILT_IN_MACROS.contains(&first),
        false => TOOLS.contains(&first),
    }
}
