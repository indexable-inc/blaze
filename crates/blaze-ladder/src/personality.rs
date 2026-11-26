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
//! A program built `panic = "unwind"` that declares no routine is NOT refused here (rule 4:
//! rustc builds a `no_core` program that never lands, and so never calls a routine): its head
//! names none, and the run's parent refuses the first code record whose rows land, naming that
//! record's fn (blaze-cli run_parent.rs `ParentError::NoPersonality`). The child refuses such
//! a record too, before it places it (blaze-code eh.rs `EhError::NoPersonality`).
//!
//! The ONE lookup ([`personality_of`]) is over a names source and the program's lang crates
//! (blaze-cli run_names.rs: the program's own crate and the sysroot's), through the lang index
//! both front ends answer `fn_lang` by (front2 index.rs [`Index::fn_lang`]), so the run's
//! parent (run/special.rs `run_roots`) and the program's maker name one fn. A bstd program has
//! std in that set (bstd's `rt` names it, `extern crate std`), so std's routine is its own;
//! when bstd declares its own lang item, it is found here unchanged.

use blaze_id::ItemKey as LibKey;

use crate::code_records::PanicStrategy;
use crate::front::FnLang;
use crate::front2::index::LangTwice;
use crate::front2::source::NamesSource;

/// Why the program has no personality routine to name.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PersonalityError {
    /// Two different fns under the routine's lang names ([`FnLang::declared_as`]).
    Twice { first: LibKey, second: LibKey },
    /// The lang crates declare one lang item twice: no lang item of them is answered.
    Lang(LangTwice),
}

impl PersonalityError {
    /// The rule that refused, for a finding's class.
    pub const fn class(&self) -> &'static str {
        match self {
            PersonalityError::Twice { .. } => "two personality routines",
            PersonalityError::Lang(_) => "a lang item declared twice",
        }
    }
}

impl core::fmt::Display for PersonalityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
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
/// `panic = "unwind"`, its `#[lang = "eh_personality"]` fn, or `None` when its lang crates
/// declare none (the module's doc: refused at the first record that lands, not here).
///
/// Scans `names.each_lang(krate, ...)` directly for [`FnLang::EhPersonality`] with zero heap
/// allocations instead of building a full ~160-entry `Index::from_source` map.
pub fn personality_of<'n, S: NamesSource<'n>>(names: S, lang_crates: &[usize], panic: PanicStrategy) -> Result<Option<LibKey>, PersonalityError> {
    if panic == PanicStrategy::Abort {
        return Ok(None);
    }
    let wanted = FnLang::EhPersonality.declared_as();
    let mut found: Option<(LibKey, usize, &'n str)> = None;
    let mut clash: Option<PersonalityError> = None;
    let named = |krate: usize| (format!("{:?}", names.crate_key(krate)), format!("crate {krate} of the records"));
    for &krate in lang_crates {
        if names.proc_macro(krate) == Some(true) {
            continue;
        }
        names.each_lang(krate, &mut |name, key| {
            if clash.is_some() || !wanted.contains(&name) {
                return;
            }
            match found {
                None => found = Some((key, krate, name)),
                Some((first, _, _)) if first == key => {}
                Some((first, first_crate, first_name)) if first_name == name => {
                    clash = Some(PersonalityError::Lang(LangTwice {
                        name: name.into(),
                        first,
                        second: key,
                        crates: [named(first_crate), named(krate)],
                    }));
                }
                Some((first, _, _)) => {
                    clash = Some(PersonalityError::Twice { first, second: key });
                }
            }
        });
        if let Some(err) = clash {
            return Err(err);
        }
    }
    Ok(found.map(|(key, _, _)| key))
}

#[cfg(test)]
mod tests {
    use blaze_id::CrateItemKey;
    use blaze_ids::ContentId;
    use blaze_mem::Frame;
    use blaze_scope::graph::Sysroot;
    use blaze_scope::task::Serial;
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
    use blaze_surface::Edition;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};

    use super::{PersonalityError, personality_of};
    use crate::code_records::PanicStrategy;

    struct MapTexts<'a>(&'a [(&'a str, &'a str)]);

    impl<'a> SourceTexts<'a> for MapTexts<'a> {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            self.0.iter().find(|&&(p, _)| p == path).map(|&(_, text)| text)
        }
    }

    #[test]
    fn personality_of_fast_path_finds_skips_proc_macro_and_catches_duplicates() {
        let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();

        let files: &[(&str, &str)] = &[
            ("/a/lib.rs", "#![no_core]\n#[lang = \"eh_personality\"] pub fn pers_a() {}\n"),
            ("/b/lib.rs", "#![no_core]\n#[lang = \"eh_personality\"] pub fn pers_b() {}\n"),
            ("/c/lib.rs", "#![no_core]\npub fn plain() {}\n"),
        ];
        let texts = MapTexts(files);
        let mk = |name: &'static str, root: &'static str, proc_macro: bool| CrateInput {
            name,
            key: CrateItemKey(ContentId::of("personality fast path test", name.as_bytes())),
            root,
            shown_from: "/",
            edition: Edition::E2021,
            implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro, entry: false },
            features: &[],
            deps: &[],
            cfg: &[],
            script: None,
        };
        let inputs = [
            mk("a", "/a/lib.rs", false),
            mk("b_pm", "/b/lib.rs", true),
            mk("b", "/b/lib.rs", false),
            mk("c", "/c/lib.rs", false),
        ];
        let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
        let solved = resolve_crates(&inputs, sysroot, &texts, &Serial, bump).expect("resolve");

        // Abort returns None without touching lang crates.
        assert_eq!(personality_of(&solved, &[0, 2], PanicStrategy::Abort), Ok(None));

        // Single eh_personality found; proc-macro crate 1 is skipped.
        let found = personality_of(&solved, &[3, 1, 0], PanicStrategy::Unwind).expect("single eh_personality");
        assert!(found.is_some(), "eh_personality from crate 0 must be found");

        // None declared returns Ok(None).
        assert_eq!(personality_of(&solved, &[3, 1], PanicStrategy::Unwind), Ok(None));

        // Duplicate eh_personality across two non-proc-macro crates returns PersonalityError::Lang.
        let err = personality_of(&solved, &[0, 2], PanicStrategy::Unwind).expect_err("duplicate eh_personality must fail");
        assert!(matches!(err, PersonalityError::Lang(ref t) if t.name == "eh_personality"), "unexpected error: {err:?}");
    }
}
