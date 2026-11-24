//! THE ROOTS OF ONE TEST'S RUN (docs/v2/plan/design/test.md, "End state", "One process per
//! test", piece 8): `blaze test` enters row n of a unit's tests record in a process of its own,
//! through the same lazy parent `blaze run` starts a program with (blaze-cli run_parent.rs
//! `Lazy`, own_process.rs): one builder, work at item grain, a run compiles what is called.
//! What differs from a program's roots ([`super::run_roots`]) is only the entry:
//! - for a test returning `()`: the test fn itself, called directly ([`EntryForm::Unit`]: the
//!   launcher calls an `extern "C" fn()` and the process exits 0 when it returns);
//! - for a test returning another `T: Termination`: its test report shim instance
//!   ([`blaze_build::TEST_REPORT_SHIM_ITEM`], tabled at the test fn), called via
//!   [`EntryForm::CMain`], which calls the test fn, passes its value to
//!   `<T as Termination>::report`, and returns the resulting `ExitCode` byte as the process status;
//! and, for a test built `panic = "unwind"`, the personality routine as the last root row,
//! exactly as a program's (personality.rs `personality_of`).
//!
//! The entry rule is front2 test_entry.rs [`head_shape`], its one home (rule 30): a free,
//! monomorphic fn taking no arguments, not `async`. What the launcher's call needs beyond the rule
//! (the Rust ABI, a safe fn) is checked here, as start.rs `rust_start` checks `main`'s.
//!
//! Nothing here reads a body. `alloc` holds the roots; the head store lives in `frame` with
//! `tables` and is dropped on return.

use core::fmt;

use blaze_id::ItemKey as LibKey;
use blaze_own::{Alloc, Vec};
use blaze_typeck::{self as tc, List};

use super::{EntryForm, MakerStart, RootTable, Roots, RootsError};
use crate::body_ids::DeclaredList;
use crate::code_records::PanicStrategy;
use crate::front2::heads::{Store, Want};
use crate::front2::item_shown::ItemShown;
use crate::front2::keys::{Keys, checker_of};
use crate::front2::program::StartOf;
use crate::front2::source::NamesSource;
use crate::front2::test_entry::{HeadShape, TERMINATION_LANG, TestEntryRefusal, head_shape};

/// Why a test fn has no roots of its own run: each by the test's printed path (rule 19) and the
/// rule that refuses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestRefusal {
    /// Its head did not lower (front2 heads.rs `Store::ensure`), in words.
    Head { test: Box<str>, why: Box<str> },
    /// The entry rule refused it (test_entry.rs `head_shape`).
    Entry { test: Box<str>, refusal: TestEntryRefusal },
    /// The launcher's call cannot enter it: `what` says why.
    Call { test: Box<str>, what: &'static str },
}

impl fmt::Display for TestRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestRefusal::Head { test, why } => write!(f, "the test fn `{test}`'s head: {why}"),
            TestRefusal::Entry { test, refusal } => write!(f, "the test fn `{test}`: {refusal}"),
            TestRefusal::Call { test, what } => write!(f, "the test fn `{test}` {what}: the launcher enters a test as a safe Rust fn"),
        }
    }
}

impl core::error::Error for TestRefusal {}

/// Whether any crate in `lang_crates` declares `#[lang = "termination"]`.
fn has_termination<'n, S: NamesSource<'n>>(names: S, lang_crates: &[usize]) -> bool {
    let mut found = false;
    for &krate in lang_crates {
        names.each_lang(krate, &mut |name, _| {
            if name == TERMINATION_LANG {
                found = true;
            }
        });
        if found {
            return true;
        }
    }
    false
}

/// The roots of the run of the test fn `test` (its library key, a tests record row's `key`):
/// row 0 the test fn at no arguments ([`EntryForm::Unit`]) for `()`, or its test report shim
/// instance ([`EntryForm::CMain`], tabled at the test fn) for any other return type; then, for
/// `panic = "unwind"`, the personality routine over `lang_crates`, its symbol in
/// [`Roots::head`]. `start` is [`StartOf::Direct`]: the entry is called directly, through no
/// `#[lang = "start"]` item. `exports`: the Rust fns the test binary's C calls, each a root row
/// before the personality's, as a program's ([`super::run_roots`]).
#[expect(clippy::too_many_arguments, reason = "each is one input of the test's roots, named at the call")]
pub fn test_roots<'n, 'a, S: NamesSource<'n>>(
    names: S,
    test: LibKey,
    lang_crates: &[usize],
    panic: PanicStrategy,
    exports: &[LibKey],
    frame: &'n blaze_mem::Frame,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
) -> Result<Roots<'a>, RootsError> {
    // On the refusal path only: the test's path as the one printer shows it.
    let shown = || -> Box<str> { ItemShown::of_lib(names, test).to_string().into_boxed_str() };
    let refused = |refusal: TestRefusal| RootsError::Test(refusal);
    let mut store = Store::new(None, frame, tables);
    let mut keys = Keys::new();
    if let Err(error) = store.ensure(names, &mut keys, Want::Fn(test)) {
        return Err(refused(TestRefusal::Head { test: shown(), why: error.to_string().into_boxed_str() }));
    }
    let key = checker_of(test);
    let returns_value = match head_shape(&store, test, key) {
        Ok(HeadShape::Unit) => false,
        Ok(HeadShape::Returns(_)) => true,
        Err(refusal) => return Err(refused(TestRefusal::Entry { test: shown(), refusal })),
    };
    // `head_shape` found the head; what the launcher's call needs of it.
    let Some(head) = store.fns.get(&key) else { return Err(refused(TestRefusal::Entry { test: shown(), refusal: TestEntryRefusal::NoHead { test } })) };
    if !matches!(head.header.abi, tc::Abi::Rust) {
        return Err(refused(TestRefusal::Call { test: shown(), what: "is not of the Rust ABI" }));
    }
    if !matches!(head.header.safety, tc::Safety::Safe) {
        return Err(refused(TestRefusal::Call { test: shown(), what: "is `unsafe`" }));
    }
    let mut table = RootTable { rows: DeclaredList::new_in(alloc), symbols: Vec::new_in(alloc), name: Vec::new_in(alloc), alloc };
    let (form, entry) = if returns_value {
        if !has_termination(names, lang_crates) {
            return Err(refused(TestRefusal::Entry { test: shown(), refusal: TestEntryRefusal::NoTermination }));
        }
        let fn_ty = store.tys.intern(tc::PNode::FnDef(key, List::EMPTY)).map_err(RootsError::Types)?;
        let shim_args = store.tys.intern_list(&[fn_ty]).map_err(RootsError::Types)?;
        let sym = table.push_as("the test report shim", key, blaze_build::TEST_REPORT_SHIM_ITEM, shim_args, store.tys.view())?;
        (EntryForm::CMain, sym)
    } else {
        let sym = table.push("the test fn", key, List::EMPTY, store.tys.view())?;
        (EntryForm::Unit, sym)
    };
    super::push_exports(&mut table, exports, store.tys.view())?;
    let personality = match crate::personality::personality_of(names, lang_crates, panic).map_err(RootsError::Personality)? {
        None => None,
        Some(routine) => Some(table.push("the personality routine", checker_of(routine), List::EMPTY, store.tys.view())?),
    };
    // The maker builds for this entry (`MakerStart::Direct`), not by the run's start rule,
    // which would look for the crate's `fn main` (run_entry.rs `root_main`).
    Ok(Roots { start: StartOf::Direct, maker_start: MakerStart::Direct(test), form, entry, rows: table.rows, symbols: table.symbols, shim: false, personality })
}
