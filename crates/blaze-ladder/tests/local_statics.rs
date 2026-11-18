//! A `static` declared in a BODY is an ordinary static: its head is its declared type and its
//! mutability, from its body's lowering (front2 locals.rs), its initializer a constant context,
//! its value a static of the run. Each position std writes one in, as a `no_core` program
//! walked over NAMES RECORDS (no refusal, the records' log empty) and run whole to a status
//! computed through the static.
//!
//! The stop that started it (g204119, std hello world): std's `thread_local_inner!` declares
//! `#[thread_local] static __RUST_STD_INTERNAL_VAL: $crate::..::EagerStorage<$t> =
//! $crate::..::EagerStorage::new(__RUST_STD_INTERNAL_INIT);` in a closure of a `const { }` block
//! of `OUTPUT_CAPTURE`'s initializer. The body's lowering copies that item out of the
//! expansion's words, which SPELL `$crate`; lexed again, `$` stopped the type, the `=` fell in an
//! error node, and the static had "no body" (heads.rs `LOCAL_NO_BODY`). blaze-lower now lowers a
//! text that spells `$crate` from its words (lib.rs `spelled_words`):
//! [`a_static_a_body_macro_writes_with_dollar_crate`] and
//! [`the_thread_local_shape_with_dollar_crate`] are that case.
//!
//! The Reference: "Static items" ([items.static.init]: a static outside an extern block has an
//! initializer); "Statements", "Item declarations": an item in a block is in scope in the block,
//! "it is otherwise identical in meaning to declaring the item outside the statement block";
//! "Mutable statics"; "Closure types"; "Constant evaluation", "Const blocks". `#[thread_local]`
//! is the unstable attribute std's `thread_local!` writes (feature `thread_local`).
//!
//! The fixture, reader and walk are tests/foreign_abi_everywhere.rs's, which are
//! tests/d106_records.rs's (one test file cannot use another's helpers).

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"local statics").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (as tests/d106_records.rs `Kept`).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<&'static str> {
        self.expansions.get(krate)?.get(&call).copied()
    }

    fn expansion_origins(&self, krate: usize, call: LibKey) -> Option<(&'static [u8], &'static [u8])> {
        let bytes = self.origins.get(krate)?.get(&call).copied()?;
        // The fixture keeps the joined form: its two parts, cut where the column starts (empty:
        // the text keeps none).
        if bytes.is_empty() {
            return Some((&[], &[]));
        }
        Some(blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}")))
    }
}

/// The origin column an expansion's text keeps, as a loader serves it: empty for a text that
/// keeps none by design.
fn made_origins(call: LibKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `text` solved once, each crate's two records written from the solve.
fn fixture(text: &str) -> Fixture {
    let a = alloc();
    let files = [(String::from(ROOT), String::from(text))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, a).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), a));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), a));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it.
fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

/// What the records' log holds, as text.
fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// A stand-in for the command's BodyCheck keys that answers for every item (as
/// tests/d106_records.rs `EveryItem`).
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("local statics every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// The checker key of crate 0's free fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = Vec::new();
    assert!(records.each_item(0, &mut |key, _| if records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
        found.push(key);
    }), "the program's items record lists its items");
    let [key] = found.as_slice() else { panic!("{} fns named `{name}` (one wanted)", found.len()) };
    checker_of(*key)
}

/// One walk of `f`'s program from `main` over a fresh reader and a fresh `Maker`: its counts, or
/// the walk's refusal with the records' log beside it.
fn try_walk(f: &Fixture) -> Result<String, String> {
    let main = fn_named(f, "main");
    let names = reader(f);
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let mut maker = Maker::new(names, Some(main), &b, alloc());
    match walk_records(main, &EveryItem, &mut store, &b, &producer(), &mut maker, alloc()) {
        Ok((_, counts)) if logged(names.records).is_empty() => Ok(counts.to_string()),
        Ok((_, counts)) => Err(format!("the walk ({counts}) left the records' log {:?}", logged(names.records))),
        Err(e) => Err(format!("the record walk: {e}; the records filed {:?}", logged(names.records))),
    }
}

/// The program `text` run whole: its status.
fn run_whole(text: &str) -> u8 {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"))
}

/// A `#![no_core]` program: the lang items it needs (the closure traits as the pinned
/// library's core declares them, as tests/front2_closure_fn_ptr.rs does), `never()`, `items`,
/// and a `main` returning `ret` (an `i32` expression).
fn program(items: &str, ret: &str) -> String {
    format!(
        "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs, thread_local)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {{}}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {{}}

#[lang = \"sized\"]
trait Sized: MetaSized {{}}

#[lang = \"copy\"]
trait Copy {{}}

#[lang = \"tuple_trait\"]
trait Tuple {{}}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {{
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {{
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {{
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}}

fn never() -> bool {{
    false
}}

{items}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    {ret}
}}
"
    )
}

/// `items` and `ret` as [`program`] writes them: walked over the records with no refusal and
/// nothing filed, then run whole to status 3.
fn runs_to_three(label: &str, items: &str, ret: &str) {
    let text = program(items, ret);
    let f = fixture(&text);
    let counts = try_walk(&f).unwrap_or_else(|e| {
        // Printed too: a failed run shows its test's own output first.
        println!("local static {label}: {e}");
        panic!("local static {label}: {e}")
    });
    let status = run_whole(&text);
    assert_eq!(status, 3, "{label}: the run's status, read through the static");
    println!("local static {label}: {counts}; run status {status}");
}

/// A static with an initializer in a fn body, read and returned.
/// Prints "local static fn body: {counts}; run status 3".
#[test]
fn a_static_in_a_fn_body_is_read() {
    runs_to_three(
        "fn body",
        "fn read_local() -> i32 {
    static S: i32 = 3;
    S
}",
        "read_local()",
    );
}

/// A `static mut` in a fn body, written then read through a raw pointer to it (the Reference,
/// "Mutable statics"; `&raw mut` makes no reference to it).
#[test]
fn a_static_mut_in_a_fn_body_is_written_then_read() {
    runs_to_three(
        "static mut",
        "fn write_then_read() -> i32 {
    static mut N: i32 = 1;
    unsafe {
        let p = &raw mut N;
        *p = 3;
        *p
    }
}",
        "write_then_read()",
    );
}

/// A static declared in a closure's body, the closure called.
#[test]
fn a_static_in_a_closure_body_is_read() {
    runs_to_three(
        "closure body",
        "fn via_closure() -> i32 {
    let f = || {
        static C: i32 = 3;
        C
    };
    f()
}",
        "via_closure()",
    );
}

/// std's `thread_local!` shape without `#[thread_local]`: a static in a closure inside a
/// `const { }` block whose value is the closure as a fn pointer, called by `main`.
#[test]
fn a_static_in_a_closure_of_a_const_block_is_read() {
    runs_to_three(
        "closure of a const block",
        "fn key() -> fn() -> i32 {
    const {
        let f: fn() -> i32 = || {
            static V: i32 = 3;
            V
        };
        f
    }
}",
        "(key())()",
    );
}

/// The same with `#[thread_local]` on the static (each thread its own `V`; this program has
/// one thread): read through the thread-local getter, as a module-level `#[thread_local]`
/// static is (run/special.rs).
#[test]
fn a_thread_local_static_in_a_closure_of_a_const_block_is_read() {
    runs_to_three(
        "thread-local in a closure of a const block",
        "fn key() -> fn() -> i32 {
    const {
        let f: fn() -> i32 = || {
            #[thread_local]
            static V: i32 = 3;
            V
        };
        f
    }
}",
        "(key())()",
    );
}

/// THE CAUSE: a static a macro call in a body writes, whose type and initializer spell
/// `$crate` (the expansion's words do; the body's lowering copies the item's text out of them).
/// Before blaze-lower's `spelled_words`, its `=` was lost and it was "declared in a body with
/// no body". Prints "local static dollar crate in a body: {counts}; run status 3".
#[test]
fn a_static_a_body_macro_writes_with_dollar_crate() {
    runs_to_three(
        "dollar crate in a body",
        "pub type Num = i32;

pub const THREE: Num = 3;

macro_rules! local_static {
    () => {{
        static V: $crate::Num = $crate::THREE;
        V
    }};
}

fn via_macro() -> i32 {
    local_static!()
}",
        "via_macro()",
    );
}

/// std's `thread_local_inner!` const arm, whole: a macro call in a constant's initializer
/// writes a const block whose closure declares a `#[thread_local]` static spelling `$crate` in
/// its type and initializer; the constant is the closure as a fn pointer.
#[test]
fn the_thread_local_shape_with_dollar_crate() {
    runs_to_three(
        "thread_local shape with dollar crate",
        "pub struct Storage(pub i32);

impl Storage {
    pub const fn new(v: i32) -> Storage {
        Storage(v)
    }

    pub fn get(&self) -> i32 {
        self.0
    }
}

macro_rules! tl_inner {
    ($t:ty, $init:expr) => {{
        const INIT: $t = $init;
        const {
            let f: fn() -> i32 = || {
                #[thread_local]
                static VAL: $crate::Storage = $crate::Storage::new(INIT);
                VAL.get()
            };
            f
        }
    }};
}

const KEY: fn() -> i32 = tl_inner!(i32, 3);",
        "KEY()",
    );
}

/// A fn declared in a body with no body (not Rust: "Functions", [items.fn.body]) is refused BY
/// NAME (rule 19): its kind, its name as written and the body it is in, never a bare key.
/// Prints "local static refusal: {the refusal}".
#[test]
fn a_bodiless_fn_in_a_body_is_refused_by_name() {
    let items = "fn holder() -> i32 {
    fn nobody() -> i32;
    if never() {
        return nobody();
    }
    3
}";
    let text = program(items, "holder()");
    let f = fixture(&text);
    let e = try_walk(&f).expect_err("a fn with no body outside an `extern` block is refused");
    assert!(e.contains("a fn in a block `nobody`"), "the refusal names the item: {e}");
    // Its body by path (`holder`, under whatever the crate root's path is shown as).
    assert!(e.contains("declared in the body of") && e.contains("holder"), "the refusal names its body: {e}");
    println!("local static refusal: {e}");
}
