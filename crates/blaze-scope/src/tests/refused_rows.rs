//! MR (2026-11-17): EVERY refused macro call has a row in its crate's names record, with its
//! macro path as written, its shape, its file and a typed reason naming what refused it
//! (blaze-scope call_refusal.rs; rules 3, 19, 26).
//!
//! tempo's `blaze check` of 08:13 (g081341): 62 causes, 261 items not checked, each
//! "not checked because macro call <key> failed, and the records hold no note of its reason":
//! the record kept a row only for a call whose expansion RAN and failed
//! (`ExpandNote::Failed`); a call refused for any other reason (a proc macro whose module
//! cannot run, a host read refused, ..) left none, and the reader had nothing to print.
//!
//! One crate, four calls refused four ways: a `macro_rules!` call no rule takes (a failure), an
//! `include!` whose nested `env!` the host refuses, a derive of a proc-macro crate whose runner
//! says its module is not in the store, and a derive of a proc-macro crate no runner knows.
//! CONTROL: with names_record.rs `failed_section` back to rows of `ExpandNote::Failed` notes
//! only, the three other calls have no row and the test fails at "has a row".

use std::string::{String, ToString};
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, Produced};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::CallShape;
use crate::call_refusal::{CallRefusal, RefusalKind};
use crate::graph::Sysroot;
use crate::names_record::{FailedRow, NamesRecord, NamesRecordError, Verified, encode, refused_row};
use crate::resolve::{CrateInput, SourceTexts, aligned, resolve_crates};
use crate::task::Serial;

const USER: &str = "#![no_core]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
#[rustc_builtin_macro]
macro_rules! env { () => {} }
macro_rules! make {
    (fn $n:ident) => { pub fn $n() {} };
}
make!(struct foo);
pub mod ffi {
    include!(concat!(env!(\"NOPE\"), \"/b.rs\"));
}
#[derive(procs::Show)]
pub struct A;
#[derive(bare::Plain)]
pub struct B;
pub fn here() {}
";

const PROCS: &str = "#![no_core]
#[proc_macro_derive(Show)]
pub fn show(item: TokenStream) -> TokenStream { item }
";

const BARE: &str = "#![no_core]
#[proc_macro_derive(Plain)]
pub fn plain(item: TokenStream) -> TokenStream { item }
";

/// The runner key of `procs`; `bare` has none.
const RUNNER: CrateItemKey = CrateItemKey(ContentId([7; 32]));

/// What the runner says of `procs`'s module: the words blaze-proc-macro's `Finding` gives a
/// module the store does not hold under the key asked.
const MISSING: &str = "procs 0.1.0: no compiled module in the store or the cache (module key k-procs)";

struct Files(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for Files {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }

    fn proc_macro_unusable(&self, _: usize, _: &MacroDefinition<'_>) -> Option<&'static str> {
        Some(MISSING)
    }
}

static FILES: Files = Files(&[&[("/user/lib.rs", USER)], &[("/procs/lib.rs", PROCS)], &[("/bare/lib.rs", BARE)]]);

const PROC: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };

/// Rule: each call the solve stored refused has a row, read back by its key equal to the
/// solve's own join ([`refused_row`]), with the macro's path as written, its shape, its file and
/// its reason's kind and words; a key that is no refused call answers `None`; two encodes are
/// equal bytes; a version-4 record (rows of failed calls only) is refused by name (rule 26).
/// Passing prints "test tests::refused_rows::every_refused_call_leaves_a_row_that_reads_back ... ok".
#[test]
fn every_refused_call_leaves_a_row_that_reads_back() {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let crates: [CrateInput<'static>; 3] = [
        input("user", "/user/lib.rs", &[("procs", 1), ("bare", 2)]),
        CrateInput { implicit: PROC, ..input("procs", "/procs/lib.rs", &[]) },
        CrateInput { implicit: PROC, ..input("bare", "/bare/lib.rs", &[]) },
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    let written = encode(user, alloc).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let record = NamesRecord::open(Verified::check(aligned(&written, alloc)).unwrap_or_else(|e| panic!("check: {e:?}")));

    let mut refused: Vec<ItemKey> = user.expansions.iter().filter(|(_, e)| e.refused.is_some()).map(|(k, _)| *k).collect();
    refused.sort_unstable();
    let mut rows: Vec<FailedRow<'_>> = Vec::new();
    for call in refused {
        let row = record.failed(call, alloc).unwrap_or_else(|e| panic!("failed: {e:?}"));
        let row = row.unwrap_or_else(|| panic!("the refused call {call:?} has a row"));
        assert_eq!(Some(row), refused_row(user, call, alloc), "the row reads back as the solve's join");
        rows.push(row);
    }
    let shown: Vec<String> = rows.iter().map(|r| std::format!("{} ({}) in {}: {}", r.path, r.shape.name(), r.file, r.why)).collect();
    let by = |path: &str| *rows.iter().find(|r| r.path == path).unwrap_or_else(|| panic!("`{path}` has a row; rows: {shown:#?}"));

    let make = by("make");
    assert_eq!((make.shape, make.implicit, make.file), (CallShape::FnLike, false, "/user/lib.rs"));
    assert!(matches!(make.why, CallRefusal::Failed(_)), "{:?}", make.why);
    assert!(make.why.to_string().contains("no rule of the macro's 1 matched"), "{}", make.why);

    let include = by("include");
    // No environment is given here, so the host refuses the nested `env!` read (a crate with an
    // environment that lacks the variable fails it instead: blaze-cli's
    // `a_refused_include_is_named_once_with_its_reason`).
    assert_eq!(include.why.kind(), RefusalKind::HostRead, "{}", include.why);
    assert!(include.why.to_string().contains("`NOPE`"), "the variable is named: {}", include.why);

    let show = by("procs::Show");
    assert_eq!((show.shape, show.why.kind()), (CallShape::Derive, RefusalKind::ProcModule), "{}", show.why);
    assert!(show.why.to_string().contains(MISSING), "the runner's words: {}", show.why);

    let plain = by("bare::Plain");
    assert_eq!((plain.shape, plain.why.kind()), (CallShape::Derive, RefusalKind::ProcModule), "{}", plain.why);
    assert!(plain.why.to_string().contains("known to no proc-macro runner"), "{}", plain.why);

    assert!(rows.iter().all(|r| r.why.kind() != RefusalKind::Unnoted), "every refusal noted its reason: {shown:#?}");
    // The control of the lookup: a key that is no refused call.
    assert!(record.failed(user.root, alloc).unwrap_or_else(|e| panic!("failed: {e:?}")).is_none());
    // Deterministic: a second encode writes equal bytes.
    let again = encode(user, alloc).unwrap_or_else(|e| panic!("encode: {e:?}"));
    assert_eq!(written[..], again[..]);
    // An old home's record (version 4: failed calls only) is refused, and made again.
    let mut old = Vec::from(&written[..]);
    old[4..8].copy_from_slice(&4u32.to_le_bytes());
    assert!(matches!(Verified::check(aligned(&old, alloc)), Err(NamesRecordError::NotThisRecord)));
}
