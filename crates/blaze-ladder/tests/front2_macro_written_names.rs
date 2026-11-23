//! Check the bodies twice. The second front end reads the serialized scope, item and
//! expansion records, including the token origins returned by the attribute runner.

#![feature(allocator_api)]

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, TokenStream, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{Body, lower_fn};
use blaze_ladder::front2::body_host::{BodySources, Sourced};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records as Check, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::resolve::Ns;
use blaze_lower::scoped::Prepares;
use blaze_mem::Frame;
use blaze_own::{AVec, Alloc};
use blaze_scope::expansion_records::{self, Directory, ExpansionRecord, Split};
use blaze_scope::graph::Sysroot;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{At, CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const HEADER: &str = "#![feature(no_core, lang_items)]
#![no_core]
#[lang = \"sized\"] pub trait Sized {}
";
const MACROS: &str = "#![no_core]
#[proc_macro_attribute] pub fn layout(_: TokenStream, i: TokenStream) -> TokenStream { i }
#[proc_macro_attribute] pub fn again(_: TokenStream, i: TokenStream) -> TokenStream { i }
#[proc_macro_attribute] pub fn test_case(_: TokenStream, i: TokenStream) -> TokenStream { i }
#[proc_macro] pub fn layout_fields(i: TokenStream) -> TokenStream { i }
";
const ROOTS: [&str; 3] = ["/app/lib.rs", "/dep/lib.rs", "/macros/lib.rs"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Guest {
    Dev,
    WithoutSlots,
    WithoutDebugAssertions,
}

struct Files<'s> {
    app: &'s str,
    dep: &'s str,
    guest: Guest,
}

impl<'s, 't: 's> SourceTexts<'s> for Files<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'s str> {
        match (krate, path) {
            (0, "/app/lib.rs") => Some(self.app),
            (1, "/dep/lib.rs") => Some(self.dep),
            (2, "/macros/lib.rs") => Some(MACROS),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        let identity = match self.guest {
            Guest::Dev => "dev contract macro",
            Guest::WithoutSlots => "contract macro without the sibling module",
            Guest::WithoutDebugAssertions => "contract macro without debug assertions",
        };
        (krate == 2).then(|| CrateItemKey(ContentId::of("fixture proc macro", identity.as_bytes())))
    }

    fn run_proc_macro<'a>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        if Some(def.site().krate) != self.proc_macro_key(2) {
            return Err(refused);
        }
        let count = call.input.tokens().len();
        let mut tokens = TokenStream::new_in(alloc);
        let mut origins = AVec::new_in(alloc);
        if (call.kind, def.proc_name()) == (ProcKind::Attr, Some("layout")) {
            // One named field per fixture. The slot name derives from the field spelling.
            let field = call.input.tokens().windows(2).find_map(|pair| {
                matches!(pair[1].kind, blaze_expand::TokenKind::Punct { char: ':', .. }).then(|| call.input.text_of(&pair[0]))
            }).ok_or(refused)?;
            let slot = field.to_uppercase();
            // contract emits an unwrapped pub mod first. quote! gives these tokens
            // call-site spans, including the name constructed by format_ident!.
            if self.guest != Guest::WithoutSlots {
                // The cfg surrounds the generator in the guest. Its output contains
                // either an unconditional declaration or no declaration at all.
                let bytes = if self.guest == Guest::Dev { format!("pub const {slot}_BYTES: usize = 1;") } else { String::new() };
                let module = format!("pub mod slots {{ use super::*; pub const {slot}: u8 = 1; pub const {slot}_OFFSET: usize = 0; {bytes} }}");
                let slots = lex(&module, Edition::E2021, alloc).map_err(|_| refused)?;
                for i in 0..slots.len() {
                    tokens.push_from(&slots, i).map_err(|_| refused)?;
                    origins.push(TokenOrigin::Made(Transparency::Transparent));
                }
            }
            let transformed = format!("pub struct Layout {{ pub {field}: u8 }}
                impl Layout {{ fn __new() -> Self {{ Self {{ {field}: slots::{slot} }} }} }}
                impl crate::ContractStorage for Layout {{}}");
            let layout = lex(&transformed, Edition::E2021, alloc).map_err(|_| refused)?;
            for i in 0..layout.len() {
                let word = layout.text_at(i);
                if i == 0 || word == field || matches!(word, "Layout" | "u8") {
                    let from = (0..count).find(|&j| call.input.text_at(j) == word).ok_or(refused)?;
                    tokens.push_from(call.input, from).map_err(|_| refused)?;
                    origins.push(TokenOrigin::Input(from as u32));
                } else {
                    tokens.push_from(&layout, i).map_err(|_| refused)?;
                    origins.push(TokenOrigin::Made(Transparency::Transparent));
                }
            }
        } else if (call.kind, def.proc_name()) == (ProcKind::FnLike, Some("layout_fields")) && count == 1 {
            // gen_test_fields_layout constructs these three idents using ident.span().
            let slot = call.input.text_at(0).to_uppercase();
            // The real helper returns vec![RustStorageField::new(...)]. Forward the
            // three slot paths through one more macro in the consumer, as vec! does.
            let fields = format!("one_field![(slots::{slot}, slots::{slot}_OFFSET, slots::{slot}_BYTES)]");
            tokens = lex(&fields, Edition::E2021, alloc).map_err(|_| refused)?;
            for i in 0..tokens.len() {
                origins.push(if tokens.text_at(i).starts_with(slot.as_str()) { TokenOrigin::Input(0) } else { TokenOrigin::Made(Transparency::Transparent) });
            }
        } else if call.kind == ProcKind::Attr && matches!(def.proc_name(), Some("again" | "test_case")) {
            for i in 0..count {
                tokens.push_from(call.input, i).map_err(|_| refused)?;
                origins.push(TokenOrigin::Input(i as u32));
            }
            if def.proc_name() == Some("test_case") {
                // test-case re-emits the original async helper and puts its generated
                // cases in a sibling module. Tokio wraps the cases' bodies in async.
                let case = lex("#[cfg(test)] mod record_gas {
                    use super::*;
                    fn t7() { let _body = async { super::record_gas(1u8, 2u8).await }; }
                }", Edition::E2021, alloc).map_err(|_| refused)?;
                for i in 0..case.len() {
                    tokens.push_from(&case, i).map_err(|_| refused)?;
                    origins.push(TokenOrigin::Made(Transparency::Transparent));
                }
            }
        } else {
            return Err(refused);
        }
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

struct Held<'a> {
    names: &'a [u8],
    items: &'a [u8],
    file: &'a str,
    expansions: &'a Split<'a>,
}

impl<'a> Held<'a> {
    fn expansion(&self, key: ItemKey) -> Option<(expansion_records::Entry<'a>, ExpansionRecord<'a>)> {
        let directory = Directory::open(&self.expansions.directory).expect("open the expansion directory");
        let entry = directory.entry(key).expect("read the expansion entry")?;
        let row = self.expansions.records.iter().find(|r| r.key == entry.record).expect("stored expansion bytes");
        let record = ExpansionRecord::open(&row.bytes).expect("open the expansion record");
        Some((entry, record))
    }
}

struct Kept<'a>(Vec<Held<'a>>);

impl<'a> Load<'a> for Kept<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> { Some(self.0.get(krate)?.names) }
    fn items(&self, krate: usize) -> Option<&'a [u8]> { Some(self.0.get(krate)?.items) }
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        (path.as_str() == "lib.rs").then_some(self.0.get(krate)?.file)
    }
    /// One open of the stored record serves the text and both origin parts, as the store's
    /// loader reads it (blaze-cli front_load.rs `StoreLoad::expansion_read`).
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'a>> {
        let (entry, record) = self.0.get(krate)?.expansion(call)?;
        Some(LoadedExpansion { text: entry.text(&record).expect("text extent"), origins: Some((entry.call_part, record.column)) })
    }
}

fn keep<'a>(names: &Names<'_>, frame: &'a Frame, alloc: Alloc<'a>) -> Kept<'a> {
    Kept(names.crates().iter().map(|c| {
        let [file] = c.files.as_slice() else { panic!("expected one file in {}", c.name) };
        Held {
            names: aligned(&names_record::encode(c, alloc).expect("encode names"), blaze_own::testing::bump()),
            items: aligned(&items_record::encode_with(c, names, alloc).expect("encode items"), blaze_own::testing::bump()),
            file: frame.alloc_str(file.text),
            expansions: frame.alloc(expansion_records::write(c, alloc).expect("encode expansions")),
        }
    }).collect())
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    emitted: bool,
    // The expected diagnostic code and a distinguishing part of its message.
    error: Option<(&'static str, &'static str)>,
}

fn check_recording_captures(body: &Body<'_>) {
    use blaze_typeck::{Expr, Name, Pat};

    let lowered = &body.typeck;
    let mut formats = lowered.exprs.iter().filter_map(|expr| match expr {
        Expr::FormatArgs { args, .. } => Some(&lowered.expr_lists[args.range()]),
        _ => None,
    });
    // format_args! places explicit arguments first. Every implicit argument must
    // refer to the let binding named by its placeholder in this recording step.
    for (positional, captures) in [
        (2, &["before", "after", "owner_before", "owner_after"][..]),
        (0, &["pooled_before", "pooled_after"][..]),
    ] {
        let args = formats.next().expect("the recording step formats both credit balances");
        assert_eq!(args.len(), positional + captures.len());
        for (expr, &word) in args[positional..].iter().zip(captures) {
            let Expr::Local(pat) = lowered.exprs[expr.0 as usize] else { panic!("{{{word}}} lost its local binding") };
            let Pat::Bind { name, .. } = lowered.pats[pat.0 as usize] else { panic!("{{{word}}} must refer to a let binding") };
            assert_eq!(name, Name::of(word), "{{{word}}} selects its own binding");
        }
    }
    assert!(formats.next().is_none());
}

fn check<'n, S: NamesSource<'n>>(front: &mut Front2<'n, S>, cases: &[Case], source: &str, host: &'n dyn BodySources) {
    let alloc = blaze_own::testing::heap().alloc();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    store.sources = Some(host);
    for case in cases {
        let name = case.name;
        let origin = if case.emitted { " (re-emitted)" } else { "" };
        let root = front.names.root_module(0).expect("the consumer root");
        let key = front.resolver.path(root, false, &[name], Ns::Value).expect("the fixture function").key;
        let place = front.names.place_of(key).expect("the fixture function's place");
        assert_eq!(matches!(place.at, At::Expansion(_)), case.emitted, "{source} {name}: body origin");
        store.ensure(front.names, &mut front.keys, Want::Fn(key)).expect("the fixture function's head");
        let body = lower_fn(front.names, prepared, &mut front.keys, &mut store, key).unwrap_or_else(|e| panic!("{source} {name}: {e}"));
        if name == "record_gas" {
            check_recording_captures(&body);
        }
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("body spelling");
        let scope = body.scope(&spell);
        let mut buckets = None;
        let mut check = Check { names: front.names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut check, &body, &scope), case.error) {
            (Ok(done), None) => {
                assert!(done.diags.is_empty(), "{source} {name}: {:?}", done.diags);
                println!("{source} {name}{origin}: clean");
            }
            (Err(CheckFail::Diags { diags, .. }), Some((code, words))) => {
                let [(got, _, message)] = diags.as_slice() else { panic!("{source} {name}: {diags:?}") };
                assert_eq!(got.code(), code, "{source} {name}: {message}");
                assert!(message.contains(words), "{source} {name}: {message}");
                println!("{source} {name}{origin}: {code}");
            }
            (Ok(_), Some((code, _))) => panic!("{source} {name}: expected {code}"),
            (Err(error), _) => panic!("{source} {name}: {error:?}"),
        }
    }
}

fn both(files: Files<'_>, feature: bool, cases: &[Case]) {
    let host = Sourced(&files);
    let input = |i: usize| CrateInput {
        name: ["app", "dep", "macros"][i],
        key: CrateItemKey(ContentId::of(if feature { "macro written names fixture with fields" } else { "macro written names fixture" }, ROOTS[i].as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: i == 2, entry: false },
        root: ROOTS[i],
        shown_from: ["/app/", "/dep/", "/macros/"][i],
        features: if feature && i == 1 { &["fields"] } else { &[] },
        deps: match i { 0 => &[("dep", 1), ("macros", 2)], 1 => &[("macros", 2)], _ => &[] },
        cfg: match i {
            0 => &[("debug_assertions", None), ("test", None)],
            1 if feature => &[("feature", Some("fields"))],
            2 if files.guest != Guest::WithoutDebugAssertions => &[("debug_assertions", None), ("proc_macro", None)],
            2 => &[("proc_macro", None)],
            _ => &[],
        },
        script: None,
    };
    let alloc = blaze_own::testing::heap().alloc();
    let inputs = [input(0), input(1), input(2)];
    let records_frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("records frame");
    let records_memory = blaze_own::Erased::new(&records_frame);
    let records_alloc = records_memory.handle();
    let kept = {
        let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("solved body frame");
        let names = resolve_crates(&inputs, Sysroot::default(), &files, &Serial, blaze_own::testing::bump()).expect("solve the fixture");
        let mut front = Front2::new(&names, &frame, &[], alloc).expect("front end over solved names");
        check(&mut front, cases, "names", &host);
        keep(&names, &records_frame, records_alloc)
    };
    let keys = records_frame.alloc(inputs.map(|c| c.key));
    let log = records_frame.alloc(Log::new(&["app", "dep", "macros"], records_alloc));
    let records = &*records_frame.alloc(Records::new(keys, &kept, log, &records_frame, records_alloc));
    let mut front = Front2::from_records(records, &records_frame, &[0, 1]).expect("front end over records");
    check(&mut front, cases, "records", &host);
    assert!(records.errors().is_empty(), "{:?}", records.errors());
}

#[test]
fn a_local_attribute_adds_a_sibling_module() {
    let app = [HEADER, "
pub trait ContractStorage {}
macro_rules! one_field { ($field:expr) => { $field }; }
mod slots { pub const FIELD_A: bool = false; }
fn layout() -> (u8, bool) {
    let byte = {
        let before: u8 = slots::FIELD_A;
        #[macros::layout] pub struct Layout { field_a: u8 }
        before
    };
    (byte, slots::FIELD_A)
}
fn layout_in_closure() -> (u8, usize, usize) {
    #[macros::layout] pub struct Layout { field_a: u8 }
    (|| macros::layout_fields!(field_a))()
}"].concat();
    for guest in [Guest::Dev, Guest::WithoutSlots] {
        let error = (guest == Guest::WithoutSlots).then_some(("E0308", "bool"));
        let mut cases = vec![Case { name: "layout", emitted: false, error }];
        if guest == Guest::Dev {
            cases.push(Case { name: "layout_in_closure", emitted: false, error: None });
        }
        both(Files { app: &app, dep: "#![no_core]", guest }, false, &cases);
    }
}

#[test]
fn a_dependency_exports_the_enabled_constants_of_a_generated_module() {
    let dep = r#"#![feature(macro_metavar_expr_concat)]
#![no_core]
#[macro_export] macro_rules! named { ($name:ident) => { pub const $name: u8 = 1; }; }
#[macro_export] macro_rules! bytes { ($base:ident) => { pub const ${concat($base, _BYTES)}: u8 = 1; }; }
#[macro_export] macro_rules! layout {
    attr() ($item:item) => {
        $item
        pub mod m {
            pub const DIRECT: u8 = 1;
            $crate::named!(NESTED_BYTES);
            $crate::bytes!(PASTED);
            #[cfg(any(test, feature = "fields"))] pub const FIELD_BYTES: u8 = 2;
        }
    };
}
#[crate::layout] pub struct Layout;
"#;
    let app = [HEADER, "
use dep::m;
fn direct() -> u8 { m::DIRECT }
fn nested() -> u8 { m::NESTED_BYTES }
fn pasted() -> u8 { m::PASTED_BYTES }
fn gated() -> u8 { m::FIELD_BYTES }
"].concat();
    for feature in [true, false] {
        let cases = [
            Case { name: "direct", emitted: false, error: None },
            Case { name: "nested", emitted: false, error: None },
            Case { name: "pasted", emitted: false, error: None },
            Case { name: "gated", emitted: false, error: (!feature).then_some(("E0425", "FIELD_BYTES")) },
        ];
        both(Files { app: &app, dep, guest: Guest::Dev }, feature, &cases);
    }
}

#[test]
fn a_contract_guest_controls_the_export_of_bytes_constants() {
    let dep = "#![no_core]
pub trait ContractStorage {}
pub mod tip_fee_manager {
    #[macros::layout] pub struct Layout { user_tokens: u8 }
}";
    let app = [HEADER, "
macro_rules! one_field { ($field:expr) => { $field }; }
fn user_tokens() -> (u8, usize, usize) {
    use dep::tip_fee_manager::slots;
    macros::layout_fields!(user_tokens)
}"].concat();
    for guest in [Guest::Dev, Guest::WithoutDebugAssertions] {
        let error = (guest == Guest::WithoutDebugAssertions).then_some(("E0425", "USER_TOKENS_BYTES"));
        both(Files { app: &app, dep, guest }, false, &[Case { name: "user_tokens", emitted: false, error }]);
    }
}

#[test]
fn macro_locals_in_written_and_reemitted_bodies() {
    const LIBRARY: &str = r#"
#[rustc_builtin_macro] macro_rules! format_args { ($($arg:tt)*) => {}; }
macro_rules! format { ($($arg:tt)*) => { format_args!($($arg)*) }; }
pub mod fmt {
    #[lang = "format_arguments"] pub struct Arguments;
    pub trait Display {}
    impl Display for u8 {}
}
pub mod future {
    #[lang = "future_trait"] pub trait Future { #[lang = "future_output"] type Output; }
    pub trait IntoFuture {
        type Output;
        type IntoFuture: Future<Output = Self::Output>;
        #[lang = "into_future"] fn into_future(self) -> Self::IntoFuture;
    }
    impl<F: Future> IntoFuture for F {
        type Output = F::Output;
        type IntoFuture = F;
        fn into_future(self) -> F { self }
    }
}
"#;
    const GOOD: &str = "fn f() {
        macro_rules! two { ($e:expr) => {{ let before = $e; let after = before + 1; after }} }
        let x = two!(1);
        let _: u8 = x;
    }";
    const FORWARDED: &str = "fn forwarded() -> u8 {
        macro_rules! credits { ($n:expr) => { $n }; }
        macro_rules! record { ($user:expr) => {{
            let before = credits!($user);
            let after = credits!(before + 1);
            credits!(after)
        }}; }
        record!(1u8)
    }";
    // The six names in Tempo's gas snapshots occur inside the literals. The
    // positional arguments and credits! inputs cross the same expansion boundaries.
    const GAS: &str = r#"async fn record_gas(user: u8, owner: u8) {
        macro_rules! credits { ($index:expr) => {{ $index }}; }
        macro_rules! pooled_credits { () => {{ 0u8 }}; }
        macro_rules! record_tx_with_cross_user_credits { ($credit_user:expr, $owner:expr) => {
            async {
                let before = credits!($credit_user);
                let owner_before = credits!($owner);
                let pooled_before = pooled_credits!();
                let after = credits!($credit_user);
                let owner_after = credits!($owner);
                let pooled_after = pooled_credits!();
                let _ = format!("user{}: {before} -> {after}; user{}: {owner_before} -> {owner_after}", $credit_user, $owner);
                let _ = format!("DEX TIP-1060: {pooled_before} -> {pooled_after}");
            }
        }; }
        record_tx_with_cross_user_credits!(user, owner).await;
    }"#;
    // Moving the literal to the call site changes which 'before' it can capture.
    const CALLER_LITERAL: &str = r#"fn caller_literal() {
        macro_rules! record { ($message:literal) => {{
            let before = 1u8;
            let _ = format!($message);
        }}; }
        record!("{before}");
    }"#;
    // The caller's argument and the template binding occupy the same lexical block.
    // Resolving 'before' by spelling alone would accept this program.
    const BAD: &str = "fn escaped() -> u8 {
        macro_rules! take { ($e:expr) => {{ let before = 1u8; $e }} }
        take!(before)
    }";
    for emitted in [false, true] {
        let attr = if emitted { "#[macros::again]" } else { "" };
        let gas_attr = if emitted { "#[macros::test_case]" } else { "" };
        let app = format!("{HEADER}{LIBRARY}\n{attr}\n{GOOD}\n{attr}\n{FORWARDED}\n{gas_attr}\n{GAS}\n{attr}\n{CALLER_LITERAL}\n{attr}\n{BAD}");
        let cases = [
            Case { name: "f", emitted, error: None },
            Case { name: "forwarded", emitted, error: None },
            Case { name: "record_gas", emitted, error: None },
            Case { name: "caller_literal", emitted, error: Some(("E0425", "before")) },
            Case { name: "escaped", emitted, error: Some(("E0425", "before")) },
        ];
        both(Files { app: &app, dep: "#![no_core]", guest: Guest::Dev }, false, &cases);
    }
}
