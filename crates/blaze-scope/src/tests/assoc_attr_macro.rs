//! Attribute procedural macros on associated items inside `impl` and `trait` blocks
//! (`Lower::assoc` in `task/lower.rs`).
//!
//! The Reference, "Procedural Macros", "Attribute macros": an attribute macro replaces the item
//! it is applied to with the zero or more items in its expansion. Inside an `impl` or `trait`
//! block, the attribute macro's `CallSrc.owner` is the container's key, so its expansion is
//! lowered through `Lower::assoc(None, owner)` and joins the container's members.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallShape, Edition, ItemDetail, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, Names, NamesNote, SourceTexts, resolve_crates};
use crate::task::{LowerNoteKind, Serial};

fn resolved<S: SourceTexts<'static>>(crates: &[CrateInput<'static>], sources: &'static S) -> Names<'static> {
    resolve_crates(crates, Sysroot::default(), sources, &Serial, super::SyncFrame::leaked())
        .unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

const MACROS: &str = "#![no_core]
#[proc_macro_attribute]
pub fn ref_cast_custom(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn outer_attr(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn inner_attr(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn expand_multi(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn expand_empty(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn assoc_const(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn assoc_type(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn never_called(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([13; 32]));

static NEVER_CALLED_COUNT: AtomicUsize = AtomicUsize::new(0);
static COND_CALLED_COUNT: AtomicUsize = AtomicUsize::new(0);

fn run_assoc_proc<'b>(def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
    let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
    let has_ident = |needle: &str| (0..call.input.tokens().len()).any(|i| call.input.text_at(i) == needle);
    let text = match (call.kind, def.proc_name()) {
        (ProcKind::Attr, Some("ref_cast_custom")) if has_ident("new") => {
            "pub(crate) fn new(x: i32) -> i32 { x + 1 }"
        }
        (ProcKind::Attr, Some("ref_cast_custom")) if has_ident("second") => {
            "pub(crate) fn second(x: i32) -> i32 { x + 2 }"
        }
        (ProcKind::Attr, Some("ref_cast_custom")) if has_ident("cond") => {
            COND_CALLED_COUNT.fetch_add(1, Ordering::Relaxed);
            "pub fn cond() -> i32 { 7 }"
        }
        (ProcKind::Attr, Some("ref_cast_custom")) if has_ident("trait_m") => {
            "fn trait_m(&self) -> i32 { 11 }"
        }
        (ProcKind::Attr, Some("ref_cast_custom")) if has_ident("from_mac") => {
            "pub fn from_mac() -> i32 { 19 }"
        }
        (ProcKind::Attr, Some("outer_attr")) => {
            // Confirm `outer_attr` was stripped from `call.input` before invocation, while `inner_attr` remains.
            assert!(!has_ident("outer_attr"), "invoked attribute must be stripped from proc-macro input");
            assert!(has_ident("inner_attr"), "inner stacked attribute must remain in proc-macro input");
            "#[macros::inner_attr] pub fn stacked() -> i32;"
        }
        (ProcKind::Attr, Some("inner_attr")) => {
            assert!(!has_ident("inner_attr"), "inner attribute must be stripped from proc-macro input");
            "pub fn stacked() -> i32 { 42 }"
        }
        (ProcKind::Attr, Some("expand_multi")) => {
            "pub const N: usize = 3; pub type Out = i32; pub fn multi_a() -> i32 { 1 } pub fn multi_b() -> i32 { 2 }"
        }
        (ProcKind::Attr, Some("expand_empty")) => "",
        (ProcKind::Attr, Some("assoc_const")) => "pub const K: u32 = 99;",
        (ProcKind::Attr, Some("assoc_type")) => "type Assoc = u64;",
        (ProcKind::Attr, Some("never_called")) => {
            NEVER_CALLED_COUNT.fetch_add(1, Ordering::Relaxed);
            "pub fn should_not_exist() {}"
        }
        _ => return Err(refused),
    };
    let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
    let mut origins = AVec::new_in(alloc);
    origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
    Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
}

struct Runner(&'static str);

impl SourceTexts<'static> for Runner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (0, "/user/lib.rs") => Some(self.0),
            (1, "/macros/lib.rs") => Some(MACROS),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        run_assoc_proc(def, call, alloc)
    }
}

fn crates_with_cfg(cfg: &'static [(&'static str, Option<&'static str>)]) -> [CrateInput<'static>; 2] {
    let user = CrateInput { cfg, ..input("user", "/user/lib.rs", &[("macros", 1)]) };
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    [user, CrateInput { implicit, ..macros }]
}

const POSITIVE_USER: &str = r#"#![no_core]
macro_rules! gen_impl {
    ($t:ident) => {
        impl $t {
            #[macros::ref_cast_custom]
            pub fn from_mac() -> i32;
        }
    };
}

pub struct S;
pub struct T;

pub trait Tr {
    #[macros::assoc_type]
    type Assoc;
    #[macros::ref_cast_custom]
    fn trait_m(&self) -> i32;
}

impl S {
    #[macros::ref_cast_custom]
    pub(crate) fn new(x: i32) -> i32;

    #[macros::ref_cast_custom]
    pub(crate) fn second(x: i32) -> i32;

    #[macros::outer_attr]
    #[macros::inner_attr]
    pub fn stacked() -> i32;

    #[macros::expand_multi]
    fn placeholder();

    #[macros::expand_empty]
    fn erased();

    #[macros::assoc_const]
    pub const K: u32 = 0;

    #[cfg(false)]
    #[macros::never_called]
    pub fn dead();

    #[inline]
    #[allow(dead_code)]
    #[doc = "kept"]
    #[expect(unused)]
    #[cold]
    #[must_use]
    #[track_caller]
    #[clippy::all]
    #[rustfmt::skip]
    pub fn builtin_and_tool_attrs(&self) -> i32 { 0 }
}

impl Tr for S {
    #[macros::assoc_type]
    type Assoc = u8;
}

gen_impl!(T);
"#;

static POSITIVE_FILES: Runner = Runner(POSITIVE_USER);

#[test]
fn attr_proc_macros_expand_on_assoc_items_in_impls_and_traits() {
    NEVER_CALLED_COUNT.store(0, Ordering::Relaxed);
    let crates = crates_with_cfg(&[]);
    let names = resolved(&crates, &POSITIVE_FILES);
    let user = names.krate(0);
    let root = names.root(0);
    let errors = &user.done.get(&root).expect("root done").scope.errors;
    assert!(errors.is_empty(), "unexpected scope errors: {errors:?}; notes: {:?}", names.notes(0));
    assert_eq!(NEVER_CALLED_COUNT.load(Ordering::Relaxed), 0, "disabled #[cfg(false)] attr macro must not be invoked");

    // Inherent `impl S`:
    let impl_s = user.tables.impls.iter().find(|i| i.header == "impl S").expect("impl S").key;
    let members_s = names.members(impl_s).expect("impl S members");

    // Every expected expanded member is present exactly once with its expanded body (no unexpanded `;` duplicate).
    let mut names_s: Vec<&str> = members_s.iter().filter_map(|m| m.name).collect();
    names_s.sort_unstable();
    assert_eq!(
        names_s,
        ["K", "N", "Out", "builtin_and_tool_attrs", "multi_a", "multi_b", "new", "second", "stacked"],
        "impl S must hold expanded members, multi-item expansions, and plain methods; no erased/dead/placeholder members"
    );

    let find_s = |n: &str| members_s.iter().find(|m| m.name == Some(n)).expect(n);
    let new_m = find_s("new");
    let second_m = find_s("second");
    let stacked_m = find_s("stacked");
    let k_m = find_s("K");
    let plain_m = find_s("builtin_and_tool_attrs");

    assert!(new_m.written_by.is_some(), "`new` must be written by its attr macro call");
    assert!(second_m.written_by.is_some(), "`second` must be written by its attr macro call");
    assert_ne!(new_m.written_by, second_m.written_by, "two methods with the same attr macro must get distinct call keys");
    assert!(new_m.text.contains("x + 1"), "`new` must hold expanded body, got {:?}", new_m.text);
    assert!(second_m.text.contains("x + 2"), "`second` must hold expanded body, got {:?}", second_m.text);
    assert!(stacked_m.written_by.is_some() && stacked_m.text.contains("42"), "stacked attr macros must expand in order: {:?}", stacked_m.text);
    assert!(k_m.written_by.is_some() && k_m.text.contains("99"), "assoc const attr macro must expand: {:?}", k_m.text);
    assert!(plain_m.written_by.is_none(), "built-in and tool attributes must not invoke attr macros");
    assert!(matches!(find_s("Out").detail, ItemDetail::TypeAlias));
    assert!(matches!(find_s("N").detail, ItemDetail::Const { .. }));

    // Trait `impl Tr for S`: assoc `type` expanded to `u64`.
    let impl_tr = user.tables.impls.iter().find(|i| i.header == "impl Tr for S").expect("impl Tr for S").key;
    let tr_impl_members = names.members(impl_tr).expect("impl Tr for S members");
    assert_eq!(tr_impl_members.len(), 1);
    assert!(tr_impl_members[0].written_by.is_some() && tr_impl_members[0].text.contains("u64"), "trait impl assoc type: {:?}", tr_impl_members[0].text);

    // Trait `Tr`: both `type Assoc` and `fn trait_m` expanded.
    let tr_key = names
        .item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["Tr"] }, Alloc::global())
        .expect("trait Tr")
        .def
        .key;
    let tr_members = names.members(tr_key).expect("trait Tr members");
    let mut tr_names: Vec<&str> = tr_members.iter().filter_map(|m| m.name).collect();
    tr_names.sort_unstable();
    assert_eq!(tr_names, ["Assoc", "trait_m"]);
    let trait_m = tr_members.iter().find(|m| m.name == Some("trait_m")).expect("trait_m");
    assert!(trait_m.written_by.is_some() && trait_m.text.contains("11"), "trait method attr macro: {:?}", trait_m.text);

    // Macro-generated `impl T`: `#[macros::ref_cast_custom] pub fn from_mac()` expanded cleanly.
    let impl_t = user.tables.impls.iter().find(|i| i.header == "impl T").expect("impl T").key;
    let t_members = names.members(impl_t).expect("impl T members");
    assert_eq!(t_members.len(), 1);
    assert_eq!(t_members[0].name, Some("from_mac"));
    assert!(t_members[0].written_by.is_some() && t_members[0].text.contains("19"), "macro-generated impl attr macro: {:?}", t_members[0].text);

    // Verify all assoc attr calls were recorded as `CallShape::Attr` with `owner: Some(...)`.
    let assoc_attr_calls: usize = user
        .tables
        .calls
        .values()
        .filter(|c| c.kind == CallShape::Attr && c.owner.is_some())
        .count();
    assert!(assoc_attr_calls >= 9, "expected at least 9 assoc attr macro calls, got {assoc_attr_calls}");
}

const CFG_ATTR_USER: &str = r#"#![no_core]
pub struct C;
impl C {
    #[cfg_attr(flag, macros::ref_cast_custom)]
    pub fn cond() -> i32 { 0 }
}
"#;

static CFG_ATTR_FILES: Runner = Runner(CFG_ATTR_USER);

#[test]
fn cfg_attr_on_assoc_item_invokes_attr_macro_only_when_active() {
    // Active: `flag` set -> invokes `macros::ref_cast_custom`, replacing body `{ 0 }` with `{ 7 }`.
    COND_CALLED_COUNT.store(0, Ordering::Relaxed);
    static FLAG_ON: [(&str, Option<&str>); 1] = [("flag", None)];
    let crates_on = crates_with_cfg(&FLAG_ON);
    let names_on = resolved(&crates_on, &CFG_ATTR_FILES);
    let impl_on = names_on.krate(0).tables.impls[0].key;
    let members_on = names_on.members(impl_on).expect("members when flag on");
    assert_eq!(members_on.len(), 1);
    assert!(members_on[0].written_by.is_some(), "active cfg_attr must expand via attr macro");
    assert!(members_on[0].text.contains("7"), "active cfg_attr must produce expanded body: {:?}", members_on[0].text);
    assert_eq!(COND_CALLED_COUNT.load(Ordering::Relaxed), 1, "active cfg_attr must invoke proc-macro once");

    // Inactive: `flag` unset -> keeps original method without invoking proc-macro.
    COND_CALLED_COUNT.store(0, Ordering::Relaxed);
    let crates_off = crates_with_cfg(&[]);
    let names_off = resolved(&crates_off, &CFG_ATTR_FILES);
    let impl_off = names_off.krate(0).tables.impls[0].key;
    let members_off = names_off.members(impl_off).expect("members when flag off");
    assert_eq!(members_off.len(), 1);
    assert!(members_off[0].written_by.is_none(), "inactive cfg_attr must keep written member directly");
    assert!(members_off[0].text.contains("0"), "inactive cfg_attr must preserve original body: {:?}", members_off[0].text);
    assert_eq!(COND_CALLED_COUNT.load(Ordering::Relaxed), 0, "inactive cfg_attr must not invoke proc-macro");
}

const MALFORMED_ATTR_USER: &str = r#"#![no_core]
pub struct Bad;
impl Bad {
    #[bad::]
    pub fn broken();

    pub fn ok() {}
}
"#;

static MALFORMED_ATTR_FILES: Runner = Runner(MALFORMED_ATTR_USER);

#[test]
fn malformed_attr_path_on_assoc_item_emits_note_without_keeping_broken_member() {
    let crates = crates_with_cfg(&[]);
    let names = resolved(&crates, &MALFORMED_ATTR_FILES);
    let impl_bad = names.krate(0).tables.impls[0].key;
    let members = names.members(impl_bad).expect("impl Bad members");
    let member_names: Vec<Option<&str>> = members.iter().map(|m| m.name).collect();
    assert_eq!(member_names, [Some("ok")], "broken member with malformed attr path must not be kept alongside `ok`");
    let has_note = names.notes(0).iter().any(|n| {
        matches!(n, NamesNote::Lower(lower) if matches!(lower.kind, LowerNoteKind::AttrMacroNotLowered { attr: "bad::" }) && lower.what == "broken")
    });
    assert!(has_note, "malformed attr path must emit LowerNoteKind::AttrMacroNotLowered, got: {:?}", names.notes(0));
}
