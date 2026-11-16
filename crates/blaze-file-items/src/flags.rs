//! The flags of an entry's tokens ([`ItemFlags`]), read in the one pass that fills the token
//! columns (crate::walk::rows): a state machine fed each token of the text that is not trivia,
//! a body's with the rest, which answers the flags the token decides. A token's flags are its
//! innermost entry's (or the file's, outside every entry).
//!
//! What decides each flag, on tokens (a string, a comment or an identifier is one token, so the
//! word inside one is never a keyword):
//!
//! - UNSAFE: an `unsafe` keyword token.
//! - FOREIGN: `extern` then `{` (an extern block, `extern "abi" {` too), or `extern`, an ABI
//!   other than `"Rust"` or none, `fn` and a name (a `fn` pointer type, `extern "C" fn(..)`,
//!   has no name); or an attribute named in [`FOREIGN`] where an attribute's name stands.
//! - ASM: a bang call whose last path segment is in [`ASM`]: an identifier, `!`, a delimiter.
//! - MACRO_CALL: any other bang call, and an attribute that is no inert built-in one
//!   ([`INERT`], `rustc_*`, a tool's `rustfmt::..`): a derive, `#[test]`,
//!   `#[global_allocator]`, a path attribute (`#[tokio::main]`).
//!
//! An attribute's name stands right after its `[`, inside `unsafe(..)`, and after each comma of
//! a `cfg_attr(..)`'s arguments past the predicate, at any nesting of `cfg_attr`.

use blaze_surface::ItemFlags;
use blaze_surface::tool_attrs::TOOLS;
use parser::SyntaxKind::{self, *};

/// The attributes that make code foreign.
const FOREIGN: [&str; 6] = ["link", "no_mangle", "export_name", "link_section", "linkage", "naked"];

/// The inline-assembly macros.
const ASM: [&str; 3] = ["asm", "global_asm", "naked_asm"];

// The tools whose attributes are inert (`#[rustfmt::skip]`, `#[blaze::noalloc]`): one table,
// blaze-surface's `tool_attrs::TOOLS`.

/// The built-in attributes that are no macro, so no expansion adds a flag: rustc's inert
/// built-in attributes (`rustc_feature::BUILTIN_ATTRIBUTES` less its macros: `derive`, `test`,
/// `bench`, `test_case`, `global_allocator`, `cfg_eval`, `cfg_accessible`), the built-in
/// derives' helpers (`default`, `pointee`), and [`FOREIGN`]'s, which set their own flag.
/// `rustc_*` names are reserved to these and pass by their prefix. Any name not here is read as
/// a macro's: a name rustc adds later is a macro call until it is listed, the safe side.
const INERT: [&str; 85] = [
    "cfg",
    "ignore",
    "should_panic",
    "reexport_test_harness_main",
    "automatically_derived",
    "macro_use",
    "macro_escape",
    "macro_export",
    "proc_macro",
    "proc_macro_derive",
    "proc_macro_attribute",
    "path",
    "crate_name",
    "crate_type",
    "no_std",
    "no_core",
    "no_main",
    "no_implicit_prelude",
    "no_builtins",
    "no_link",
    "recursion_limit",
    "type_length_limit",
    "move_size_limit",
    "windows_subsystem",
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "must_use",
    "must_not_suspend",
    "deprecated",
    "deprecated_safe",
    "doc",
    "inline",
    "cold",
    "track_caller",
    "target_feature",
    "force_target_feature",
    "repr",
    "align",
    "non_exhaustive",
    "used",
    "link_name",
    "link_ordinal",
    "instruction_set",
    "optimize",
    "coverage",
    "debugger_visualizer",
    "collapse_debuginfo",
    "thread_local",
    "panic_handler",
    "lang",
    "feature",
    "stable",
    "unstable",
    "unstable_feature_bound",
    "marker",
    "fundamental",
    "may_dangle",
    "const_trait",
    "ffi_pure",
    "ffi_const",
    "compiler_builtins",
    "profiler_runtime",
    "needs_allocator",
    "panic_runtime",
    "needs_panic_runtime",
    "omit_gdb_pretty_printer_section",
    "register_tool",
    "default_lib_allocator",
    "no_sanitize",
    "sanitize",
    "test_runner",
    "start",
    "prelude_import",
    "patchable_function_entry",
    "allow_internal_unstable",
    "allow_internal_unsafe",
    "cfi_encoding",
    "type_const",
    "coroutine",
    "define_opaque",
    "default",
    "pointee",
];

/// What the last tokens leave open outside an attribute's own reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum After {
    Nothing,
    /// An identifier: a `!` next may make a bang call. Whether it names inline assembly.
    Name { asm: bool },
    /// `name !`: a delimiter next makes the call.
    Bang { asm: bool },
    /// `extern`, then its ABI if one was written: whether it is `"Rust"`.
    Extern { rust: Option<bool> },
    /// `extern ["abi"] fn`: a name next makes an item, foreign unless the ABI is Rust's.
    ExternFn { foreign: bool },
    /// `#`, or `#!`: a `[` next opens an attribute.
    Pound,
}

/// Where an attribute's reading is at its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Head {
    /// A name may come next.
    Expect,
    /// `unsafe`: its `(` holds the name.
    Unsafe,
    /// A name, decided by the token after it: `::` makes it a path, `(` after `cfg_attr`
    /// opens its arguments.
    Name { flags: ItemFlags, tool: bool, cfg_attr: bool },
    /// No name stands here: an argument, a predicate, a value.
    Done,
}

/// An open attribute.
#[derive(Clone, Copy, Debug)]
struct InAttr {
    /// The brackets open in it, its own `[` counted.
    depth: u32,
    /// Bit `d`: a comma at depth `d` starts a name (a `cfg_attr`'s arguments are at `d`).
    heads: u64,
    head: Head,
}

impl InAttr {
    fn token(&mut self, kind: SyntaxKind, text: &str) -> ItemFlags {
        let mut flags = ItemFlags::NONE;
        let mut opens_cfg_attr = false;
        if let Head::Name { flags: named, tool, cfg_attr } = self.head {
            self.head = Head::Done;
            match kind {
                // A path: a tool's attribute is inert, any other a macro's.
                COLON if !tool => flags |= ItemFlags::MACRO_CALL,
                COLON => {}
                L_PAREN if cfg_attr => opens_cfg_attr = true,
                _ => flags |= named,
            }
        }
        match kind {
            L_PAREN | L_BRACK | L_CURLY => {
                self.depth += 1;
                if opens_cfg_attr {
                    match 1u64.checked_shl(self.depth) {
                        Some(bit) => self.heads |= bit,
                        // A `cfg_attr` inside 64 brackets: its arguments are read as a macro's,
                        // the safe side, and no name in them is looked for.
                        None => flags |= ItemFlags::MACRO_CALL,
                    }
                }
                if self.head == Head::Unsafe && kind == L_PAREN {
                    self.head = Head::Expect;
                }
            }
            R_PAREN | R_BRACK | R_CURLY => {
                if let Some(bit) = 1u64.checked_shl(self.depth) {
                    self.heads &= !bit;
                }
                self.depth -= 1;
            }
            COMMA if 1u64.checked_shl(self.depth).is_some_and(|bit| self.heads & bit != 0) => {
                self.head = Head::Expect;
            }
            IDENT if self.head == Head::Expect => self.head = name(text),
            UNSAFE_KW if self.head == Head::Expect => self.head = Head::Unsafe,
            _ if matches!(self.head, Head::Expect | Head::Unsafe) => self.head = Head::Done,
            _ => {}
        }
        flags
    }
}

/// The reading of an attribute's name.
fn name(text: &str) -> Head {
    let text = text.strip_prefix("r#").unwrap_or(text);
    let tool = TOOLS.contains(&text);
    let flags = if text == "cfg_attr" || INERT.contains(&text) || text.starts_with("rustc_") {
        ItemFlags::NONE
    } else if FOREIGN.contains(&text) {
        ItemFlags::FOREIGN
    } else {
        // A tool's name alone (`#[rustfmt]`) is no tool attribute: a macro's.
        ItemFlags::MACRO_CALL
    };
    Head::Name { flags, tool, cfg_attr: text == "cfg_attr" }
}

/// The machine: fed every token that is not trivia, in order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scan {
    after: After,
    attr: Option<InAttr>,
}

impl Scan {
    pub(crate) const fn new() -> Scan {
        Scan { after: After::Nothing, attr: None }
    }

    /// The flags `kind`, whose text `text` gives, decides. The text is asked for only of a
    /// token it is read for (an identifier, a string), and most tokens, which decide nothing
    /// and leave nothing open, take one test.
    #[inline]
    pub(crate) fn token<'t>(&mut self, kind: SyntaxKind, text: impl FnOnce() -> &'t str) -> ItemFlags {
        if self.attr.is_none() && self.after == After::Nothing && !matches!(kind, IDENT | UNSAFE_KW | EXTERN_KW | POUND) {
            return ItemFlags::NONE;
        }
        let text = match kind {
            IDENT | STRING => text(),
            _ => "",
        };
        self.read(kind, text)
    }

    fn read(&mut self, kind: SyntaxKind, text: &str) -> ItemFlags {
        let mut flags = ItemFlags::NONE;
        if let Some(attr) = &mut self.attr {
            flags |= attr.token(kind, text);
            if attr.depth == 0 {
                self.attr = None;
            }
        }
        let next = match (self.after, kind) {
            (After::Bang { asm }, L_PAREN | L_BRACK | L_CURLY) => {
                flags |= if asm { ItemFlags::ASM } else { ItemFlags::MACRO_CALL };
                None
            }
            (After::Name { asm }, BANG) => Some(After::Bang { asm }),
            (After::Extern { .. }, STRING) => Some(After::Extern { rust: Some(text == "\"Rust\"") }),
            (After::Extern { .. }, L_CURLY) => {
                flags |= ItemFlags::FOREIGN;
                None
            }
            (After::Extern { rust }, FN_KW) => Some(After::ExternFn { foreign: rust != Some(true) }),
            (After::ExternFn { foreign: true }, IDENT) => {
                flags |= ItemFlags::FOREIGN;
                None
            }
            (After::Pound, BANG) => Some(After::Pound),
            // A `#[` inside an attribute's tokens is no attribute of the entry's.
            (After::Pound, L_BRACK) if self.attr.is_none() => {
                self.attr = Some(InAttr { depth: 1, heads: 0, head: Head::Expect });
                None
            }
            _ => None,
        };
        self.after = next.unwrap_or(match kind {
            IDENT => After::Name {
                // Each name of [`ASM`] starts with one of these bytes: most identifiers are
                // told apart by their first.
                asm: matches!(text.as_bytes().first(), Some(b'a' | b'g' | b'n' | b'r'))
                    && ASM.contains(&text.strip_prefix("r#").unwrap_or(text)),
            },
            UNSAFE_KW => {
                flags |= ItemFlags::UNSAFE;
                After::Nothing
            }
            EXTERN_KW => After::Extern { rust: None },
            POUND => After::Pound,
            _ => After::Nothing,
        });
        flags
    }
}

#[cfg(test)]
mod tests;
