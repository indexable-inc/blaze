//! Regression tests for Fix 1B: `keep_item` preserves parent `OriginColumn` `TextMark`s
//! (including `root_mark` for cross-crate `$crate`) across single- and multi-stage `KEEP_ITEM`
//! expansions, while preserving `SemiOpaque` marks on plain `crate` (negative control).

use blaze_expand::origin_column::{InputShape, Mapped, input_places, map_range};
use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallRole, Edition, Namespace, PathKind, WrittenVis};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::crate_roots::{Root, TextMarks, root_by_mark, root_mark};
use crate::expansion_origins::OriginsView;
use crate::graph::Sysroot;
use crate::resolve::{At, CrateInput, SourceTexts, resolve_crates};
use crate::task::Serial;

/// Proc-macro crate with two derives (`Show` with helper `show`, `Tag` with helper `tag`) so we
/// can exercise both single-stage and chained multi-stage `KEEP_ITEM` expansions.
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show(item: TokenStream) -> TokenStream { item }

#[proc_macro_derive(Tag, attributes(tag))]
pub fn tag(item: TokenStream) -> TokenStream { item }
";

/// Dependency crate defining `$crate`-using macros (like `oxc_index::define_index_type!`) and a
/// negative-control macro using plain `crate`.
const DEP: &str = "#![no_core]
pub mod nonmax {
    pub struct NonMaxU32(pub u32);
}

#[macro_export]
macro_rules! define_index {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(rename = \"idx\")]
        pub struct $name(#[show(skip)] pub(in $crate::nonmax) $crate::nonmax::NonMaxU32);
    };
}

#[macro_export]
macro_rules! define_nested_index {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(a)]
        #[derive(macros::Tag)]
        #[tag(b)]
        pub struct $name($crate::nonmax::NonMaxU32);
    };
}

#[macro_export]
macro_rules! define_plain_crate {
    ($name:ident) => {
        #[derive(macros::Show)]
        #[show(rename = \"plain\")]
        pub struct $name(crate::nonmax::NonMaxU32);
    };
}
";

const USER: &str = "#![no_core]
pub trait Shown {}
pub trait Tagged {}

dep::define_index!(NodeId);
dep::define_nested_index!(NestedId);
dep::define_plain_crate!(PlainId);
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([29; 32]));

struct WithRunner(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 2).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::Derive, Some("Show" | "Tag")) => "",
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[
    &[("/user/lib.rs", USER)],
    &[("/dep/lib.rs", DEP)],
    &[("/macros/lib.rs", MACROS)],
]);

#[test]
fn kept_item_preserves_parent_dollar_crate_and_nested_marks_with_plain_crate_control() {
    let bump = super::SyncFrame::leaked();
    let alloc = bump.alloc();
    let user_in = input("user", "/user/lib.rs", &[("dep", 1), ("macros", 2)]);
    let dep_in = input("dep", "/dep/lib.rs", &[("macros", 2)]);
    let macros_in = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [user_in, dep_in, CrateInput { implicit, ..macros_in }];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    let dep_krate = names.krate(1).key;

    let find_struct_expansion = |struct_name: &str| {
        let decl = user
            .done
            .get(&user.root)
            .unwrap_or_else(|| panic!("root solved"))
            .scope
            .get(Namespace::Types, struct_name)
            .unwrap_or_else(|| panic!("`{struct_name}` declared: notes {:?}", names.notes(0)));
        let (text_span, at) = user.tables.texts.get(&decl.def.key).unwrap_or_else(|| panic!("`{struct_name}` has text"));
        let At::Expansion(keep_call) = *at else {
            panic!("`{struct_name}` is at a keep expansion, got {at:?}");
        };
        let made = user.expansions.get(&keep_call).and_then(|e| e.made).unwrap_or_else(|| panic!("`{struct_name}` keep expansion made"));
        let bytes = made.origins.unwrap_or_else(|e| panic!("`{struct_name}` origins: {e}"));
        let view = OriginsView::open(bytes).unwrap_or_else(|e| panic!("`{struct_name}` origins open: {e}"));
        (decl.def.key, text_span.item, keep_call, made, view)
    };

    // 1. Positive: single-stage KEEP_ITEM from cross-crate macro_rules! with `$crate::nonmax::NonMaxU32`.
    let (_, _, _, node_made, node_view) = find_struct_expansion("NodeId");
    let At::Expansion(node_parent) = node_view.text else {
        panic!("NodeId keep call's parent is the macro expansion, got {:?}", node_view.text);
    };
    let node_marks = TextMarks { text: node_made.text, column: node_view.column };
    let crate_at = node_made.text.rfind("crate :: nonmax :: NonMaxU32").unwrap_or_else(|| panic!("NodeId text holds `crate :: nonmax :: NonMaxU32`: {:?}", node_made.text));
    let crate_seg = &node_made.text[crate_at..crate_at + 5];
    let node_crate_mark = node_marks.mark_of(crate_seg).unwrap_or_else(|e| panic!("mark_of(crate): {e}"));
    assert_eq!(
        node_crate_mark,
        Some(root_mark(dep_krate, Edition::E2021)),
        "KEEP_ITEM must preserve opaque root_mark(dep) on `$crate`-rendered `crate`"
    );
    assert_eq!(root_by_mark("crate", node_crate_mark), Root::Crate(dep_krate));

    // Diagnostic mapping still maps tokens through KEEP_ITEM -> parent expansion cleanly.
    let nonmax_at = node_made.text.find("NonMaxU32").unwrap_or_else(|| panic!("NodeId holds NonMaxU32"));
    let nonmax_start = u32::try_from(nonmax_at).unwrap_or(u32::MAX);
    let Mapped::Input { first, last } = map_range(node_view.column, nonmax_start, nonmax_start + 9) else {
        panic!("NonMaxU32 must map as an Input token in KEEP_ITEM's identity column");
    };
    let parent_made = user.expansions.get(&node_parent).and_then(|e| e.made).unwrap_or_else(|| panic!("parent expansion exists"));
    let parent_entry = parent_made.outline.item(node_view.item);
    let input_at = parent_entry.range.start + node_view.input_start;
    let written = &parent_made.text[input_at as usize..(input_at + node_view.input_len) as usize];
    let written_places = input_places(written, InputShape::Whole, Edition::E2021, alloc).unwrap_or_else(|e| panic!("written lexes: {e:?}"));
    let (ws, we) = (input_at + written_places[first as usize].0, input_at + written_places[last as usize].1);
    assert_eq!(&parent_made.text[ws as usize..we as usize], "NonMaxU32");

    // 2. Positive 2: chained multi-stage KEEP_ITEM (two derives + two helper attrs interleaved).
    let (_, _, _, nested_made, nested_view) = find_struct_expansion("NestedId");
    let At::Expansion(stage1_keep_call) = nested_view.text else {
        panic!("NestedId stage-2 keep call's parent is stage-1 keep call, got {:?}", nested_view.text);
    };
    let stage1_made = user.expansions.get(&stage1_keep_call).and_then(|e| e.made).unwrap_or_else(|| panic!("stage-1 keep call exists"));
    let stage1_view = OriginsView::open(stage1_made.origins.unwrap_or_else(|e| panic!("stage-1 origins: {e}")))
        .unwrap_or_else(|e| panic!("stage-1 origins open: {e}"));
    assert!(matches!(stage1_view.text, At::Expansion(_)), "stage-1 keep call's parent is the macro expansion");
    let nested_marks = TextMarks { text: nested_made.text, column: nested_view.column };
    let nested_crate_at = nested_made.text.find("crate :: nonmax :: NonMaxU32").unwrap_or_else(|| panic!("NestedId text: {:?}", nested_made.text));
    let nested_crate_mark = nested_marks
        .mark_of(&nested_made.text[nested_crate_at..nested_crate_at + 5])
        .unwrap_or_else(|e| panic!("nested mark_of(crate): {e}"));
    assert_eq!(
        nested_crate_mark,
        Some(root_mark(dep_krate, Edition::E2021)),
        "multi-stage KEEP_ITEM must preserve opaque root_mark(dep)"
    );
    assert_eq!(root_by_mark("crate", nested_crate_mark), Root::Crate(dep_krate));

    // 3. Negative control: plain `crate::nonmax::NonMaxU32` in macro_rules! stays SemiOpaque
    //    (NOT promoted to Opaque root_mark(dep)), so it resolves in the caller crate (`Root::Here`).
    let (_, _, _, plain_made, plain_view) = find_struct_expansion("PlainId");
    let plain_marks = TextMarks { text: plain_made.text, column: plain_view.column };
    let plain_crate_at = plain_made.text.find("crate :: nonmax :: NonMaxU32").unwrap_or_else(|| panic!("PlainId text: {:?}", plain_made.text));
    let plain_crate_mark = plain_marks
        .mark_of(&plain_made.text[plain_crate_at..plain_crate_at + 5])
        .unwrap_or_else(|e| panic!("plain mark_of(crate): {e}"));
    let Some(plain_mark) = plain_crate_mark else {
        panic!("plain `crate` from macro_rules! should carry its SemiOpaque mark");
    };
    assert_eq!(plain_mark.transparency, Transparency::SemiOpaque, "plain `crate` must stay SemiOpaque, not Opaque");
    assert_eq!(root_by_mark("crate", plain_crate_mark), Root::Here, "plain `crate` must resolve in caller crate");

    // Also verify `keep_item` passes `marks` to `Fragment`: `pub(in $crate::nonmax)` visibility on
    // a kept item resolves with `PathKind::CrateOf(dep_krate)` when lowered inside `keep_item`.
    let vis_crates = [
        input("user", "/user/lib.rs", &[("dep", 1), ("macros", 2)]),
        input("dep", "/dep/lib.rs", &[("macros", 2)]),
        CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) },
    ];
    let vis_files = WithRunner(&[
        &[("/user/lib.rs", "#![no_core]\ndep::mk_vis!();\n")],
        &[(
            "/dep/lib.rs",
            "#![no_core]\npub mod inner {}\n#[macro_export]\nmacro_rules! mk_vis { () => { #[derive(macros::Show)] #[show(a)] pub(in $crate::inner) struct VisS; }; }\n",
        )],
        &[("/macros/lib.rs", MACROS)],
    ]);
    let vis_names = resolve_crates(&vis_crates, Sysroot::default(), &vis_files, &Serial, bump).unwrap_or_else(|e| panic!("vis resolve_crates: {e}"));
    let vis_user = vis_names.krate(0);
    let vis_dep = vis_names.krate(1).key;
    let vis_keep = vis_user
        .expansions
        .values()
        .find_map(|e| e.decls().and_then(|d| d.items.iter().find(|it| it.name == "VisS")))
        .unwrap_or_else(|| panic!("VisS declared in keep expansion"));
    assert!(
        matches!(&vis_keep.vis, WrittenVis::In(p) if p.kind == PathKind::DollarCrate(vis_dep)),
        "keep_item Fragment::marks must resolve `pub(in $crate::inner)` to dep crate, got {:?}",
        vis_keep.vis
    );
    // Sanity-check that `CallRole::Keep` was actually exercised (not bypassed).
    assert!(
        user.expansions.values().any(|e| e.decls().is_some_and(|d| d.calls.iter().any(|c| c.role == CallRole::Keep))),
        "expected CallRole::Keep calls inside macro expansions"
    );
}
