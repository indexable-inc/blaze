//! The program's personality routine (docs: port27/tempo-run/UNWIND.md, 3.3; piece P3).
//!
//! A program built `panic = "unwind"` has exactly one `#[lang = "eh_personality"]` fn: std's
//! (the pinned library's `std/src/sys/personality/gcc.rs`, `rust_eh_personality`), or a
//! `#![no_core]` program's own. It is in no code record: every record's unwind entry names it
//! through the CIE's personality pointer, which is written at placement (blaze-code eh.rs
//! `Placed::personality`), so the run's parent tells the program which slot it is
//! (blaze-launch's run wire, `Down::Personality`) and the program's run mode writes that slot's
//! entry thunk into every entry it makes. The walk takes the fn as a root: nothing calls it; the
//! system's unwinder does, through the pointer (crate::seam's [`FnLang::ALL`] holds it, and
//! front2/reach.rs queues every fn of that list the program's world declares).
//!
//! A program built `panic = "abort"` lands nowhere (blaze-build `Panic::Abort` builds no
//! landing block), so no entry names a personality and none is asked for.
//!
//! The ONE lookup ([`personality_of`]) is over a names source and the program's lang crates
//! (blaze-cli run_names.rs: the program's own crate and the sysroot's), through the lang index
//! both front ends answer `fn_lang` by (front2 index.rs [`Index::fn_lang`]), so the run's
//! parent (run/special.rs `run_roots`) and the program's maker name one fn. A bstd program has
//! std in that set (bstd's `rt` names it, `extern crate std`), so std's routine is its own;
//! when bstd declares its own lang item, it is found here unchanged.

use std::string::String;

use blaze_id::ItemKey as LibKey;

use crate::code_records::PanicStrategy;
use crate::front::FnLang;
use crate::front2::index::{Index, LangTwice};
use crate::front2::source::NamesSource;

/// Why the program has no personality routine to name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PersonalityError {
    /// Built `panic = "unwind"`, and none of the crates searched (their paths, in the order
    /// searched) declares a fn under `#[lang = "eh_personality"]`: a true `no_std` program
    /// that unwinds and brings no routine.
    Missing { searched: Box<str> },
    /// Two different fns under the routine's lang names ([`FnLang::declared_as`]).
    Twice { first: LibKey, second: LibKey },
    /// The lang crates declare one lang item twice: no lang item of them is answered.
    Lang(LangTwice),
}

impl PersonalityError {
    /// The rule that refused, for a finding's class.
    pub const fn class(&self) -> &'static str {
        match self {
            PersonalityError::Missing { .. } => "no personality routine",
            PersonalityError::Twice { .. } => "two personality routines",
            PersonalityError::Lang(_) => "a lang item declared twice",
        }
    }
}

impl core::fmt::Display for PersonalityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PersonalityError::Missing { searched } => write!(
                f,
                "the program is built `panic = \"unwind\"` and none of its lang crates ({searched}) declares a `#[lang = \"eh_personality\"]` fn: no unwind entry of its code can name a personality routine"
            ),
            PersonalityError::Twice { first, second } => {
                write!(f, "two fns are declared the personality routine (`#[lang = \"eh_personality\"]`): {first:x} and {second:x}")
            }
            PersonalityError::Lang(twice) => write!(f, "the personality routine: {twice}"),
        }
    }
}

impl core::error::Error for PersonalityError {}

/// The fn every unwind entry of the program's code names as its personality routine, from
/// `names` and the program's lang crates `lang_crates`: `None` for a program built
/// `panic = "abort"` (nothing lands, so nothing names one, and nothing is read); for one built
/// `panic = "unwind"`, its `#[lang = "eh_personality"]` fn, or [`PersonalityError::Missing`]
/// naming the crates searched.
pub fn personality_of<'n, S: NamesSource<'n>>(names: S, lang_crates: &[usize], panic: PanicStrategy) -> Result<Option<LibKey>, PersonalityError> {
    if panic == PanicStrategy::Abort {
        return Ok(None);
    }
    let index = Index::from_source(names, lang_crates).map_err(PersonalityError::Lang)?;
    if let Some((first, second)) = index.fn_lang_clash(FnLang::EhPersonality) {
        return Err(PersonalityError::Twice { first, second });
    }
    match index.fn_lang(FnLang::EhPersonality) {
        Some(key) => Ok(Some(key)),
        None => Err(PersonalityError::Missing { searched: searched(names, lang_crates) }),
    }
}

/// The crates `lang_crates` by their root module's path, in order; one the source cannot name
/// by its crate key, else its index.
fn searched<'n, S: NamesSource<'n>>(names: S, lang_crates: &[usize]) -> Box<str> {
    let mut out = String::new();
    for (at, &krate) in lang_crates.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        match (names.root_module(krate).and_then(|m| names.module_path(m)), names.crate_key(krate)) {
            (Some(path), _) => out.push_str(path),
            (None, Some(key)) => out.push_str(&std::format!("crate {krate} ({key})")),
            (None, None) => out.push_str(&std::format!("crate {krate}")),
        }
    }
    out.into_boxed_str()
}
