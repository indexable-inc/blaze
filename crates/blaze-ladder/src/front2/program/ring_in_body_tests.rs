//! One symbol on both sides for a foreign fn declared in a BODY: the code's (`symbols_of`, what
//! the lazy program's fill.rs and tabled.rs give a callee with no body) and the link table's
//! root (links.rs `scope_links`, the native image's roots: natives.rs `native_key`).
//!
//! ring 0.17.14 calls `prefixed_extern!` inside fns (src/limb.rs `limbs_less_than_limbs`, the
//! sha2, aes and gcm ffi), reached by any program that does TLS or checks a signature:
//! ```text
//! fn limbs_less_than_limbs(a: &[Limb], b: &[Limb]) -> Result<LimbMask, LenMismatchError> {
//!     prefixed_extern! {
//!         fn LIMBS_less_than(a: *const Limb, b: *const Limb, num_limbs: c::NonZero_size_t)
//!             -> LimbMask;
//!     }
//!     ..
//! }
//! ```
//! A macro call in statement position makes its block a module of the names library
//! (blaze-scope blocks.rs), expanded there once by the one driver: `LIMBS_less_than` is an item
//! of that module under the library's key, its `link_name` (`concat!(prefix!(),
//! stringify!(LIMBS_less_than))`) expanded at its place by the crate's solve (the textual cut
//! of the block: a `macro_rules!` written before it in the block is in scope, one after is
//! not), its row in the crate's `values`, read by both sides.
//!
//! Tests:
//! - `ring_s_in_body_call_is_one_symbol`: the code's symbol and the link root are
//!   `ring_core_0_17_14__LIMBS_less_than`, and the item's key is the library's (no `INNER` bit).
//! - `ring_s_call_in_an_expression_macro_is_one_symbol`: ring's src/aead/aes/ffi.rs shape
//!   (`set_encrypt_key!`, `bn_mul_mont_ffi!`, `sha2_ffi!`: an expression macro whose expansion
//!   is a block expression holding the `prefixed_extern!` call): the same, through the block
//!   the expansion writes (blaze-scope expanded_blocks.rs `statements_as_items`).
//! - `a_macro_written_after_the_item_in_its_block_is_not_in_scope`: TWIN of the textual cut:
//!   the value's macro written after the item in the block is refused on both sides by name;
//!   CONTROL, written before it, the value is the symbol on both sides.
//! - `an_extern_block_in_an_unwritten_block_is_a_root`: a hand-written extern block in a block
//!   the library does not write (links.rs `Gather::body_blocks`) gives its roots; one under a
//!   false `cfg` (on its fn, on its block's statement) gives none (CONTROL: without the `cfg`
//!   it does); a value written as a call there is `SymbolWhy::InBody`.
//! - `gcloud_s_member_shape_gathers_an_empty_table`: prost-derive's `merge_field` member (a
//!   body that writes a block and reads `external_id`) gives an empty table, no `Unkeyed`.
//! - `an_extern_block_in_a_method_is_a_root`: a member's in-body extern blocks are roots,
//!   in a written block and in an unwritten one.
//! - `an_unreached_no_mangle_fn_is_an_export`: a module-level `#[no_mangle]`/`#[export_name]`
//!   fn no Rust code calls is in the crate's exports with its item; one with neither is not.
//!
//! The lines these tests print when they pass:
//! ```text
//! ring in body: code ring_core_0_17_14__LIMBS_less_than, root ring_core_0_17_14__LIMBS_less_than
//! ring in an expression macro: code ring_core_0_17_14__aes_hw_set_encrypt_key, root ring_core_0_17_14__aes_hw_set_encrypt_key
//! ring in body: after the item: refused on both sides; before it: late_fn on both sides
//! ring in body: unwritten blocks: ["deep", "hand_written", "renamed_c"]; under cfg(any()): none; a call: InBody
//! ring in body: gcloud's member: an empty table; the old rule's three conditions hold
//! ring in body: methods: ["in_method", "in_written_method"]
//! ring in body: unreached exports: ["c_calls_this", "renamed_export"]
//! ```

use blaze_expand::seen::SeenRoot;
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use blaze_scope::attr_values::{RowRefusal, ValueRefused, ValueWhy, row};
use blaze_scope::crate_env::{CrateEnv, EnvFacts};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, CrateNames, Names, SourceTexts, resolve_crates};
use blaze_scope::task::{ItemText, Serial};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use crate::front2::links::{LinksError, SymbolWhy, scope_links};

const ROOT: &str = "/ring/src/lib.rs";

/// One crate, `ring` 0.17.14, its root `text`.
struct OneCrate {
    text: &'static str,
    env: CrateEnv<'static>,
}

impl SourceTexts<'static> for OneCrate {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        (krate == 0 && path == ROOT).then_some(self.text)
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
        (krate == 0).then_some(&self.env)
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn bump_static() -> blaze_own::Bump<'static> {
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let frame: &'static blaze_mem::Frame = blaze_own::Box::leak_static(blaze_own::Box::new_in(frame, a()));
    frame.alloc(blaze_own::Erased::new(frame)).bump()
}

/// `text` after ring's macros, solved as crate `ring` of package version 0.17.14.
fn solved(text: &str) -> &'static Names<'static> {
    let text: &'static str = std::boxed::Box::leak(std::format!("{RING}{text}").into_boxed_str());
    let facts = EnvFacts {
        crate_name: "ring",
        package: Some(("ring", "0.17.14")),
        bin: None,
        seen: SeenRoot::Package,
        read_folder: "/ring/",
        build_script: false,
        script: None,
        test_or_bench: false,
        bin_exes: blaze_scope::crate_env::BinNames::NONE,
        meta: None,
    };
    let sources = OneCrate { text, env: CrateEnv::of(facts, a()).unwrap_or_else(|e| panic!("the crate's environment: {e:?}")) };
    let input = CrateInput {
        name: "ring",
        key: CrateItemKey(ContentId::of("blaze-ladder ring in-body test crate", b"ring")),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root: ROOT,
        shown_from: "/ring/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    };
    let bump = bump_static();
    let names = resolve_crates(&[input], Sysroot::default(), &sources, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    blaze_own::Box::leak_static(blaze_own::Box::new_in(names, a()))
}

/// The text of the item named `name` among the crate's item texts.
fn text_named<'a>(c: &CrateNames<'a>, name: &str) -> ItemText<'a> {
    c.tables.texts.values().find(|(t, _)| t.name == Some(name)).map(|(t, _)| *t).unwrap_or_else(|| panic!("no item `{name}`"))
}

/// The code's symbol of the foreign fn named `name`: `symbols_of` as fill.rs asks it of a
/// library item (links.rs `values_key` of a key with no `INNER` bit is the key).
fn code_symbol(names: &'static Names<'static>, name: &str) -> Result<Option<std::string::String>, std::string::String> {
    let item = text_named(names.crates()[0], name);
    super::symbols_of(names, item.key, item.key, item.text, name).map(|s| s.link_name.map(|l| l.into_string())).map_err(|e| std::format!("{e:?}"))
}

/// ring 0.17.14 src/prefixed.rs, its four macros as written (as blaze-scope attr_values.rs's
/// tests have them), the builtins they call declared for a `no_core` crate.
const RING: &str = "#![no_core]\n\
    #[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
    #[rustc_builtin_macro]\nmacro_rules! stringify { () => {} }\n\
    #[rustc_builtin_macro]\nmacro_rules! env { () => {} }\n\
    macro_rules! core_name_and_version {\n\
        () => {\n\
            concat!(\n\
                env!(\"CARGO_PKG_NAME\"),\n\
                \"_core_\",\n\
                env!(\"CARGO_PKG_VERSION_MAJOR\"),\n\
                \"_\",\n\
                env!(\"CARGO_PKG_VERSION_MINOR\"),\n\
                \"_\",\n\
                env!(\"CARGO_PKG_VERSION_PATCH\"),\n\
                \"_\",\n\
                env!(\"CARGO_PKG_VERSION_PRE\"), // Often empty\n\
            )\n\
        };\n\
    }\n\
    macro_rules! prefix {\n\
        ( ) => {\n\
            concat!(core_name_and_version!(), \"_\")\n\
        };\n\
    }\n\
    macro_rules! prefixed_extern {\n\
        {\n\
            $(\n\
                $( #[$meta:meta] )*\n\
                $vis:vis fn $name:ident ( $( $arg_pat:ident : $arg_ty:ty ),* $(,)? )\n\
                $( -> $ret_ty:ty )?;\n\
            )+\n\
        } => {\n\
            extern \"C\" {\n\
                $(\n\
                    prefixed_item! {\n\
                        link_name\n\
                        $name\n\
                        {\n\
                            $( #[$meta] )*\n\
                            $vis fn $name ( $( $arg_pat : $arg_ty ),* ) $( -> $ret_ty )?;\n\
                        }\n\
                    }\n\
                )+\n\
            }\n\
        };\n\
    }\n\
    macro_rules! prefixed_item {\n\
        {\n\
            $attr:ident\n\
            $name:ident\n\
            { $item:item }\n\
        } => {\n\
            #[$attr = concat!(prefix!(), stringify!($name))]\n\
            $item\n\
        };\n\
    }\n\
    macro_rules! noop { {} => {} }\n";

/// ring's src/limb.rs shape: the call in the fn's body, the fn called after it.
const LIMB: &str = "pub fn limbs_less_than_limbs(a: *const usize, b: *const usize, n: usize) -> usize {\n\
        prefixed_extern! {\n\
            fn LIMBS_less_than(a: *const usize, b: *const usize, num_limbs: usize)\n\
                -> usize;\n\
        }\n\
        unsafe { LIMBS_less_than(a, b, n) }\n\
    }\n";

#[test]
fn ring_s_in_body_call_is_one_symbol() {
    let names = solved(LIMB);
    let c = names.crates()[0];
    let item = text_named(c, "LIMBS_less_than");
    assert!(!item.key.is_inner(), "the block's module holds it under the library's key: {:x}", item.key);
    let code = code_symbol(names, "LIMBS_less_than").unwrap_or_else(|e| panic!("the code's symbol: {e}"));
    assert_eq!(code.as_deref(), Some("ring_core_0_17_14__LIMBS_less_than"), "the code's symbol");
    let links = scope_links(c, bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
    assert_eq!(links.foreign.as_slice(), ["ring_core_0_17_14__LIMBS_less_than"], "the link root");
    std::println!("ring in body: code {}, root {}", code.as_deref().unwrap_or(""), links.foreign[0]);
}

/// ring 0.17.14 src/aead/aes/ffi.rs: `set_encrypt_key!` is called in an expression and writes
/// a block expression holding `prefixed_extern!`. The expression call is written into its
/// block's module and expanded there; its block expression is a module of its own
/// (`__blaze_block_x<tag>_0`, the scope table's `ScopeAt::Own` for the lowering's block of the
/// same expansion), where the `prefixed_extern!` call is expanded once.
#[test]
fn ring_s_call_in_an_expression_macro_is_one_symbol() {
    let names = solved(
        "macro_rules! set_encrypt_key {\n\
            ( $name:ident, $bits:expr $(,)? ) => {{\n\
                prefixed_extern! {\n\
                    fn $name(user_key: *const u8, bits: usize, key: *mut u8) -> usize;\n\
                }\n\
                $name(0 as *const u8, $bits, 0 as *mut u8)\n\
            }};\n\
        }\n\
        pub fn new_hw(bits: usize) -> usize {\n\
            unsafe { set_encrypt_key!(aes_hw_set_encrypt_key, bits) }\n\
        }\n",
    );
    let c = names.crates()[0];
    let item = text_named(c, "aes_hw_set_encrypt_key");
    assert!(!item.key.is_inner(), "the expansion's block module holds it under the library's key: {:x}", item.key);
    let code = code_symbol(names, "aes_hw_set_encrypt_key").unwrap_or_else(|e| panic!("the code's symbol: {e}"));
    assert_eq!(code.as_deref(), Some("ring_core_0_17_14__aes_hw_set_encrypt_key"), "the code's symbol");
    let links = scope_links(c, bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
    assert_eq!(links.foreign.as_slice(), ["ring_core_0_17_14__aes_hw_set_encrypt_key"], "the link root");
    std::println!("ring in an expression macro: code {}, root {}", code.as_deref().unwrap_or(""), links.foreign[0]);
}

/// The Reference, "Macros By Example", "Textual scope": a `macro_rules!` is in scope after its
/// definition, in a block as in a module. The block is written (its statement call `noop!`),
/// so its hand-written extern block is an item of the block's module whose value is expanded
/// at its place.
#[test]
fn a_macro_written_after_the_item_in_its_block_is_not_in_scope() {
    fn body(above: bool) -> std::string::String {
        let def = "macro_rules! late { () => { \"late_\" } }\n";
        let item = "extern \"C\" { #[link_name = concat!(late!(), \"fn\")] fn late_fn(); }\n";
        let (first, second) = if above { (def, item) } else { (item, def) };
        std::format!("pub fn g() {{\nnoop! {{}}\n{first}{second}}}\n")
    }
    let below = solved(&body(false));
    let c = below.crates()[0];
    let key = text_named(c, "late_fn").key;
    let refused = row(&c.values, key, "link_name").map(|r| r.value).unwrap_or_else(|| panic!("no row of `late_fn`: {:?}", c.values.as_slice()));
    assert!(
        matches!(refused, Err(ValueRefused { why: ValueWhy::NotFound { mac: "late", .. }, .. })),
        "a macro written after the item is not in its textual scope: {:?}",
        refused.map_err(|e| std::format!("{e}"))
    );
    assert!(code_symbol(below, "late_fn").is_err(), "the code refuses it");
    assert!(
        matches!(scope_links(c, bump_static()), Err(LinksError::Symbol { item: "late_fn", why: SymbolWhy::Value(RowRefusal::Typed(ValueRefused { why: ValueWhy::NotFound { mac: "late", .. }, .. })), .. })),
        "the link table refuses it, naming the item and the macro"
    );

    // CONTROL: written above the item, it is in scope; both sides give its value.
    let above = solved(&body(true));
    assert_eq!(code_symbol(above, "late_fn").as_ref().map(|s| s.as_deref()), Ok(Some("late_fn")));
    let links = scope_links(above.crates()[0], bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
    assert_eq!(links.foreign.as_slice(), ["late_fn"]);
    std::println!("ring in body: after the item: refused on both sides; before it: late_fn on both sides");
}

/// The Reference, "External blocks" ([items.extern.namespace]): an extern block may stand in a
/// block, and its items link by name. No block here holds an item that makes a module or a
/// statement call, so the library writes none: the link table reads them itself.
#[test]
fn an_extern_block_in_an_unwritten_block_is_a_root() {
    let roots = |text: &str| -> std::vec::Vec<&'static str> {
        let names = solved(text);
        let links = scope_links(names.crates()[0], bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
        links.foreign.iter().copied().collect()
    };
    let plain = "pub fn plain() -> usize {\n\
            extern \"C\" { fn hand_written(x: usize) -> usize; #[link_name = \"renamed_c\"] fn other(); }\n\
            unsafe { hand_written(1) }\n\
        }\n\
        #[cfg(any())] fn off() { extern \"C\" { fn gone(); } }\n\
        pub fn under_cfg() { let _ = 1; #[cfg(any())] { extern \"C\" { fn gone_too(); } } }\n\
        pub fn nested() { fn inner() { extern \"C\" { fn deep(); } } }\n";
    let found = roots(plain);
    assert_eq!(found, ["deep", "hand_written", "renamed_c"], "the roots of extern blocks in bodies, at any depth, none under a false `cfg`");

    // CONTROL: without the `cfg`s, the two blocks are roots.
    let on = "#[cfg(all())] fn off() { extern \"C\" { fn gone(); } }\n\
        pub fn under_cfg() { let _ = 1; #[cfg(all())] { extern \"C\" { fn gone_too(); } } }\n";
    assert_eq!(roots(on), ["gone", "gone_too"], "control: a true `cfg` keeps them");

    // A value written as a macro call has no row in a block the library does not write.
    let names = solved("pub fn called() { extern \"C\" { #[link_name = concat!(\"a\", \"b\")] fn ab(); } }\n");
    assert!(
        matches!(scope_links(names.crates()[0], bump_static()), Err(LinksError::Symbol { item: "ab", attr: "link_name", why: SymbolWhy::InBody, .. })),
        "a call value in an unwritten block is refused by name"
    );
    std::println!("ring in body: unwritten blocks: {found:?}; under cfg(any()): none; a call: InBody");
}

/// gcloud-sdk 0.29.0's shape, end to end (the run of 08:40 on tempo refused its whole table:
/// `Unkeyed { krate: "gcloud_sdk", at: ItemIndex(2) }`): prost-derive writes `impl Message for
/// T { fn encode_raw(..) {..} fn merge_field(..) {..} }`, `merge_field`'s body calling a macro
/// and reading `self.external_id`. Written here by hand (no proc macro in a unit crate): the
/// member is the same entry of an impl's text. The table is empty, with no refusal.
///
/// CONTROL of the cause: the three conditions the old rule refused on hold of this crate: the
/// names hold the member by owner only (no key by place, what the old walk asked), its text
/// holds the bytes `extern`, and its body writes a block.
///
/// Prints `ring in body: gcloud's member: an empty table; the old rule's three conditions hold`.
#[test]
fn gcloud_s_member_shape_gathers_an_empty_table() {
    let names = solved(
        "pub struct Msg { external_id: usize }\n\
        pub trait Message { fn merge_field(&mut self, tag: u32) -> usize; }\n\
        impl Message for Msg {\n\
            fn encode_raw(&self) {}\n\
            fn merge_field(&mut self, tag: u32) -> usize {\n\
                let name = stringify!(Msg);\n\
                match tag { 1 => self.external_id, _ => 0 }\n\
            }\n\
        }\n",
    );
    let c = names.crates()[0];
    let links = scope_links(c, bump_static()).unwrap_or_else(|e| panic!("gcloud's shape was refused: {e:?}"));
    assert!(links.foreign.is_empty() && links.exports.is_empty() && links.decls.is_empty(), "nothing to link");

    let member = c.tables.members.values().flat_map(|list| list.iter()).find(|m| m.name == Some("merge_field") && m.text.contains("external_id"));
    let member = member.unwrap_or_else(|| panic!("no member `merge_field` with a body"));
    assert!(!c.tables.by_place.values().any(|k| *k == member.key), "a member has no key by place");
    assert!(member.text.contains("extern"), "the bytes `extern` (`external_id`)");
    assert!(blaze_file_items::nested::scan(member.text, a()).writes(), "the body writes a block (its `stringify!` call)");
    std::println!("ring in body: gcloud's member: an empty table; the old rule's three conditions hold");
}

/// THE RULE: a module-level `#[no_mangle]` (or `#[export_name]`) fn or static of a crate is an
/// export of the program's image whether or not any Rust code reaches it: the Reference,
/// [abi.no_mangle]: "the item is publicly exported from the produced library or object file,
/// similar to the used attribute", and [abi.used]: kept "even if the variable is not used, or
/// referenced". C linked into the program (a build script's archive) may call it by name, and
/// no Rust walk sees that call. So the crate's link table lists it with its item, from the
/// crate's own walk (never from what the program reached), as natives.rs hands every crate's
/// exports to the image (`Wanted.rust`). TWIN: the same fn with no attribute is no export.
///
/// Prints `ring in body: unreached exports: ["c_calls_this", "renamed_export"]`.
#[test]
fn an_unreached_no_mangle_fn_is_an_export() {
    let names = solved(
        "#[no_mangle] pub extern \"C\" fn c_calls_this(x: usize) -> usize { x }\n\
        mod m { #[export_name = \"renamed_export\"] pub extern \"C\" fn inner() {} }\n\
        pub extern \"C\" fn plain_c_abi() {}\n",
    );
    let c = names.crates()[0];
    let links = scope_links(c, bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
    let found: std::vec::Vec<(&str, blaze_id::ItemKey)> = links.exports.iter().map(|e| (e.name, e.item)).collect();
    assert_eq!(found, [("c_calls_this", text_named(c, "c_calls_this").key), ("renamed_export", text_named(c, "inner").key)], "each export with its item");
    assert!(!links.exports.iter().any(|e| e.ident == "plain_c_abi"), "twin: an `extern \"C\"` fn with no attribute is no export");
    std::println!("ring in body: unreached exports: {:?}", links.exports.iter().map(|e| e.name).collect::<std::vec::Vec<_>>());
}

/// The Reference, "External blocks": an extern block may stand in any block, a method's body
/// too. A member's body is keyed by its owner (blaze-scope lower.rs `assoc`), and a member
/// whose body writes a block (`noop! {}`) has that block written as a module under the
/// member's key: its extern block is read there, once; one in a body that writes none is read
/// by the link table itself. Both are roots, and nothing is refused as unkeyed.
///
/// Prints `ring in body: methods: ["in_method", "in_written_method"]`.
#[test]
fn an_extern_block_in_a_method_is_a_root() {
    let names = solved(
        "pub struct S;\n\
        impl S {\n\
            pub fn plain(&self) { extern \"C\" { fn in_method(); } }\n\
            pub fn written(&self) { noop! {} extern \"C\" { fn in_written_method(); } }\n\
        }\n",
    );
    let links = scope_links(names.crates()[0], bump_static()).unwrap_or_else(|e| panic!("the link table: {e:?}"));
    assert_eq!(links.foreign.as_slice(), ["in_method", "in_written_method"]);
    std::println!("ring in body: methods: {:?}", links.foreign.as_slice());
}
