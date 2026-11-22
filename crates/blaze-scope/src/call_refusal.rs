//! Why a macro call was REFUSED, as its crate's names record keeps it (rules 19 and 3): every
//! call the solve stores refused ([`crate::refused`]) has a row, never only one whose expansion
//! ran and failed. Before this a call refused for any other reason (its macro's definition not
//! held, a proc-macro module absent, a host read refused, an item walk that left no input, an
//! expansion that does not read as items, ..) left no row, and a reader of the records printed
//! "the records hold no note of its reason" (tempo's check of 2026-11-17 08:13: 62 such causes,
//! 261 items not checked behind them).
//!
//! The reason is typed: a [`RefusalKind`] (what refused it) and either blaze-expand's own record
//! of a failed expansion ([`blaze_expand::Failed`]) or the words naming what the refusal is
//! about (the macro, the nested call, the proc-macro crate and why its module cannot run, the
//! host read, the lexer's or the expander's error), made from the solve's [`ExpandNote`] by
//! [`words_of`], the one place a refusal is put in words. A record holds the words, the kind and
//! the call's place as text (its macro path as written, its shape, its file in its package):
//! nothing of the machine, no line (rule 71).

use core::fmt::{self, Write as _};

use blaze_expand::{ExpandError, HostAsk, HostError};
use blaze_own::{AString, Alloc};

use crate::resolve::{ExpandNote, ProcUnusable};

/// What refused a call: the kind of its reason, a byte in the names record's failed-call row.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefusalKind {
    /// The expansion ran and holds an error: no `macro_rules!` rule matched, `compile_error!`,
    /// a proc macro's error. Its reason is blaze-expand's record of the failure.
    Failed,
    /// The fill asked for a call the lowering does not know.
    UnknownCall,
    /// No definition of the macro is held here (a macro not read).
    NoDefinition,
    /// The macro is a proc macro whose crate's module cannot run it: no runner knows the crate,
    /// its module is not in the store, does not load, or exports no such macro.
    ProcModule,
    /// The expansion asked the host a read it refused or could not answer (`env!`,
    /// `include!`, a proc macro's run).
    HostRead,
    /// The item's attributes (its `#[cfg]`/`#[cfg_attr]` walk) left no input for the call.
    ItemAttrs,
    /// The attribute or derive is not among the item's attributes as the expander walks them.
    AttrNotFound,
    /// A text of the call or of the macro's definition did not lex.
    Lex,
    /// A builtin macro blaze-expand has no form of, or one called in another shape.
    Builtin,
    /// A call nested in an eager builtin's input could not be expanded first.
    Eager,
    /// The expander refused the call: a bound (the recursion or step limit), a token table, a
    /// proc macro's malformed output.
    Expander,
    /// The expansion does not read as items (a syntax error in what the macro wrote).
    NotItems,
    /// The expansion writes a macro definition holding another crate's `$crate` (token-mark-3).
    ForeignDollarCrate,
    /// The expansion writes another crate's root and keeps no marks to tell it by.
    RootsLost,
    /// A call written in an expansion whose input is no range of that expansion's tokens.
    NestedInput,
    /// The solve stored the call refused and noted no reason: a bug of resolve.rs
    /// `expand_decls`, named so (every way it returns nothing notes why first).
    Unnoted,
}

impl RefusalKind {
    /// Every kind, in byte order.
    pub const ALL: [RefusalKind; 16] = [
        RefusalKind::Failed,
        RefusalKind::UnknownCall,
        RefusalKind::NoDefinition,
        RefusalKind::ProcModule,
        RefusalKind::HostRead,
        RefusalKind::ItemAttrs,
        RefusalKind::AttrNotFound,
        RefusalKind::Lex,
        RefusalKind::Builtin,
        RefusalKind::Eager,
        RefusalKind::Expander,
        RefusalKind::NotItems,
        RefusalKind::ForeignDollarCrate,
        RefusalKind::RootsLost,
        RefusalKind::NestedInput,
        RefusalKind::Unnoted,
    ];

    /// Its byte, as a record holds it (1-based: 0 is no kind).
    pub fn byte(self) -> u8 {
        match self {
            RefusalKind::Failed => 1,
            RefusalKind::UnknownCall => 2,
            RefusalKind::NoDefinition => 3,
            RefusalKind::ProcModule => 4,
            RefusalKind::HostRead => 5,
            RefusalKind::ItemAttrs => 6,
            RefusalKind::AttrNotFound => 7,
            RefusalKind::Lex => 8,
            RefusalKind::Builtin => 9,
            RefusalKind::Eager => 10,
            RefusalKind::Expander => 11,
            RefusalKind::NotItems => 12,
            RefusalKind::ForeignDollarCrate => 13,
            RefusalKind::RootsLost => 14,
            RefusalKind::NestedInput => 15,
            RefusalKind::Unnoted => 16,
        }
    }

    /// The kind whose byte is `byte`, if any.
    pub fn from_byte(byte: u8) -> Option<RefusalKind> {
        RefusalKind::ALL.into_iter().find(|k| k.byte() == byte)
    }

    /// What refused the call, in words that lead its reason.
    pub fn what(self) -> &'static str {
        match self {
            RefusalKind::Failed => "its expansion failed",
            RefusalKind::UnknownCall => "the solve asked for a call the lowering does not know",
            RefusalKind::NoDefinition => "no definition of the macro is held",
            RefusalKind::ProcModule => "the proc macro cannot run",
            RefusalKind::HostRead => "a read the expansion asked was refused",
            RefusalKind::ItemAttrs => "the item's attributes leave no input for it",
            RefusalKind::AttrNotFound => "it is not among the item's attributes as the expander walks them",
            RefusalKind::Lex => "a text did not lex",
            RefusalKind::Builtin => "a builtin macro blaze-expand does not expand so",
            RefusalKind::Eager => "a call nested in an eager builtin's input was not expanded first",
            RefusalKind::Expander => "the expander refused it",
            RefusalKind::NotItems => "its expansion does not read as items",
            RefusalKind::ForeignDollarCrate => "its expansion writes a macro definition holding another crate's `$crate` (token-mark-3)",
            RefusalKind::RootsLost => "its expansion writes another crate's root and keeps no marks to tell it by",
            RefusalKind::NestedInput => "its input is no range of the tokens of the expansion it is written in",
            RefusalKind::Unnoted => "the solve noted no reason (a bug of blaze-scope resolve.rs `expand_decls`)",
        }
    }
}

/// Why a call was refused, read from a names record or the solve: blaze-expand's failure for
/// [`RefusalKind::Failed`], else the kind and the words naming what it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallRefusal<'a> {
    Failed(blaze_expand::Failed<'a>),
    Refused { kind: RefusalKind, words: &'a str },
}

impl CallRefusal<'_> {
    pub fn kind(&self) -> RefusalKind {
        match self {
            CallRefusal::Failed(_) => RefusalKind::Failed,
            CallRefusal::Refused { kind, .. } => *kind,
        }
    }
}

impl fmt::Display for CallRefusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallRefusal::Failed(failed) => write!(f, "{failed}"),
            CallRefusal::Refused { kind, words } if words.is_empty() => f.write_str(kind.what()),
            CallRefusal::Refused { kind, words } => write!(f, "{}: {words}", kind.what()),
        }
    }
}

/// The kind of the refusal `note` says, and its words (empty for a failure, whose reason is
/// its record): what the note names, with no key and no place (the row holds those). `env` is
/// the first variable whose `env!` read the host refused during the call, if any.
pub fn words_of<'b>(note: &ExpandNote<'_>, env: Option<&str>, alloc: Alloc<'b>) -> (RefusalKind, AString<'b>) {
    let mut out = AString::new_in(alloc);
    // Writing to a string does not fail.
    let kind = match *note {
        ExpandNote::Failed { .. } => RefusalKind::Failed,
        ExpandNote::UnknownCall => RefusalKind::UnknownCall,
        ExpandNote::NoDefinition { shape, name } => {
            let _ = write!(out, "the {} macro `{name}` has no definition this crate's solve reads", shape.name());
            RefusalKind::NoDefinition
        }
        ExpandNote::ProcModule { macro_name, why } => match why {
            ProcUnusable::NoRunner => {
                let _ = write!(
                    out,
                    "`{macro_name}`'s crate is known to no proc-macro runner of this command (it has no module identity, or no runner was made; the crate's `MacroModule` lookup names why)"
                );
                RefusalKind::ProcModule
            }
            ProcUnusable::Module(words) => {
                let _ = write!(out, "`{macro_name}`: {words}");
                RefusalKind::ProcModule
            }
            // The module ran it: the run's own words (the path its sandbox refused, as the
            // macro named it), in place of the bare `ProcMacro read (Limit(Paths))`.
            ProcUnusable::Run(words) => {
                let _ = write!(out, "`{macro_name}` {words}");
                RefusalKind::HostRead
            }
        },
        ExpandNote::ItemAttrs { macro_name, error } => {
            let _ = write!(out, "`{macro_name}`: {error}");
            RefusalKind::ItemAttrs
        }
        ExpandNote::AttrNotFound { name } => {
            let _ = write!(out, "`{name}`");
            RefusalKind::AttrNotFound
        }
        ExpandNote::Lex { macro_name, what, error } => {
            let _ = write!(out, "the {what:?} of a call of `{macro_name}`: {error}");
            RefusalKind::Lex
        }
        ExpandNote::BuiltinUnknown { name } => {
            let _ = write!(out, "blaze-expand has no form of the builtin `{name}`");
            RefusalKind::Builtin
        }
        ExpandNote::BuiltinOtherCalled { shape, macro_name } => {
            let _ = write!(out, "the builtin {} macro `{macro_name}` called function-like", shape.name());
            RefusalKind::Builtin
        }
        ExpandNote::Eager { macro_name, nested, why } => {
            let _ = write!(out, "`{macro_name}!`'s nested `{nested}!`: {why:?}");
            RefusalKind::Eager
        }
        ExpandNote::Expand { macro_name, error: ExpandError::Host(HostError { ask, why }) } => {
            let _ = match (ask, env) {
                (HostAsk::Env, Some(name)) => write!(out, "`{macro_name}`: the variable `{name}` this build could not answer ({why:?})"),
                _ => write!(out, "`{macro_name}`: its {ask:?} read ({why:?})"),
            };
            RefusalKind::HostRead
        }
        ExpandNote::Expand { macro_name, error } => {
            let _ = write!(out, "`{macro_name}`: {error}");
            RefusalKind::Expander
        }
        // A diagnostic that is no error refuses nothing; were it the last note of a refused
        // call, the call's reason would be lost: named as the driver's bug.
        ExpandNote::Diagnostic { macro_name, diagnostic } => {
            let _ = write!(out, "the last note of `{macro_name}`'s call is a diagnostic that is no error: {diagnostic:?}");
            RefusalKind::Unnoted
        }
        ExpandNote::NotItems { macro_name, message } => {
            let _ = write!(out, "`{macro_name}`: {message}");
            RefusalKind::NotItems
        }
        ExpandNote::ForeignDollarCrate { macro_name, .. } => {
            let _ = write!(out, "`{macro_name}`");
            RefusalKind::ForeignDollarCrate
        }
        ExpandNote::RootsLost { macro_name, lost } => {
            let _ = write!(out, "`{macro_name}`: {lost}");
            RefusalKind::RootsLost
        }
        ExpandNote::NestedInput { macro_name, error } => {
            let _ = write!(out, "`{macro_name}`: {error:?}");
            RefusalKind::NestedInput
        }
    };
    (kind, out)
}