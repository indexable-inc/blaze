//! Wave 3A end-to-end tests: real `#![no_core]` Rust programs compiled through `blaze-ladder`'s
//! front-end, `Walk`, and `Maker` directly into `wasm64` (`memory64`) `.wasm` binaries via
//! `blaze_ladder::run::make_one::wasm64::compile_walk_to_wasm64`, validated with
//! `blaze_export::wasm64::check_wasm64`.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_export::wasm64::{Wasm64Opts, check_wasm64};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Grant, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::wasm64::{
    Wasm64BuildOpts, compile_walk_to_wasm64, default_wasm64_import,
};
use blaze_ladder::run::make_one::{Maker, NamesRecords, SecondCheck, walk_records};
use blaze_ladder::run::special::MakerStart;
use blaze_ladder::symbols::static_symbol;
use blaze_own::{AVec, Alloc, Vec as OwnVec};
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/wasm64_e2e/";
const ROOT: &str = "/wasm64_e2e/src/main.rs";
const LANG_CRATES: [usize; 1] = [0];
const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// Test-only grant proving every walked item in these synthetic fixtures may be compiled to LIR.
#[derive(Clone, Copy)]
struct TestGrant;

impl Grant for TestGrant {}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"wasm64 rust e2e").get::<CodeBuilder>()
}

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

    fn file(
        &self,
        krate: usize,
        path: blaze_scope::package_path::PackagePath<'_>,
    ) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = self.origins.get(krate).and_then(|m| m.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).expect("expansion origins")
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn made_origins(
    call: LibKey,
    origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>,
) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("origins of expansion {call:x}: {e:?}"),
    }
}

struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

fn fixture(text: &str) -> Fixture {
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let files = vec![(String::from(ROOT), String::from(text))];
    let user = UserCrate {
        name: "wasm64_e2e",
        dir: DIR,
        root: ROOT,
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] =
        Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs
        .resolve(&crates, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(
            &names_record::encode(c, a).unwrap_or_else(|e| panic!("names record: {e:?}")),
            blaze_own::testing::bump(),
        ));
        kept.items.push(aligned(
            &items_record::encode_with(c, &names, a)
                .unwrap_or_else(|e| panic!("items record: {e:?}")),
            blaze_own::testing::bump(),
        ));
        kept.files
            .push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(
            c.expansions
                .iter()
                .filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text))))
                .collect(),
        );
        kept.origins.push(
            c.expansions
                .iter()
                .filter_map(|(call, e)| {
                    e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))
                })
                .collect(),
        );
    }
    Fixture { kept, keys, crate_names }
}

fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(
        blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("reader frame"),
    ));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> =
        Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn item_named(f: &Fixture, kind: ItemKind, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(
        records.each_item(0, &mut |key, _| {
            if found.is_none()
                && records.kind_of_item(key) == Some(kind)
                && records.name_of(key) == Some(name)
            {
                found = Some(checker_of(key));
            }
        }),
        "items record should list items"
    );
    found.unwrap_or_else(|| panic!("no item `{name}` of kind {kind:?}"))
}

struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId {
            key: Hasher::new("wasm64 e2e item").part(&item.0.to_le_bytes()[..]).finish(),
            reads: &READS,
        })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

const RUST_WASM64_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for i32 {}
impl Copy for usize {}
impl<T> Copy for *const T {}
impl<T> Copy for *mut T {}

unsafe extern "C" {
    fn fd_write(fd: i32, iovs: *const u8, iovs_len: usize, nwritten: *mut usize) -> i32;
    fn proc_exit(code: i32);
}

static MSG: [u8; 6] = *b"hello\n";

struct Pair {
    a: i32,
    b: i32,
}

fn pick<T>(cond: bool, x: T, y: T) -> T {
    if cond { x } else { y }
}

fn sum_loop(n: i32) -> i32 {
    let mut i = 0;
    let mut acc = 0;
    while i < n {
        acc = acc + i;
        i = i + 1;
    }
    acc
}

fn add_one(x: i32) -> i32 {
    x + 1
}

fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
    f(x)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let p = Pair { a: sum_loop(5), b: apply(add_one, 10) };
    let chosen: i32 = pick(true, p.a, p.b);
    let ptr: *const u8 = &MSG as *const [u8; 6] as *const u8;
    let first_byte: u8 = unsafe { *ptr };
    let byte: u8 = pick(false, first_byte, 42u8);
    let mut nwritten: usize = 0;
    let _rc = unsafe { fd_write(1, ptr, 6, &mut nwritten as *mut usize) };
    let res = chosen + byte as i32;
    unsafe { proc_exit(res) };
    res
}
"#;

#[test]
fn real_rust_program_compiles_to_valid_wasm64() {
    let f = fixture(RUST_WASM64_PROGRAM);
    let entry = item_named(&f, ItemKind::Function, "main");
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let mut walk_maker = Maker::new(reader(&f), Some(entry), &b, alloc());
    let (walk, _counts) = walk_records(entry, &EveryItem, &mut store, &b, &producer(), &mut walk_maker, alloc())
        .unwrap_or_else(|e| panic!("walk_records failed: {e}"));

    let main_sym = walk.symbol(0);
    let mut wasm_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&f),
        Some(entry),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        alloc(),
    );

    let opts = Wasm64BuildOpts {
        module_name: "rust_wasm64_e2e",
        start_symbol: Some(main_sym),
        exports: &[(main_sym, "main")],
        export_globals: &[],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };

    let mut wasm: AVec<'static, u8> = OwnVec::new_in(alloc());
    let built = compile_walk_to_wasm64(
        &walk,
        |_| Some(TestGrant),
        &mut wasm_maker,
        &opts,
        default_wasm64_import,
        &mut wasm,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("compile_walk_to_wasm64 failed: {e}"));

    assert!(built.func_count >= 5, "expected >= 5 funcs, got {}", built.func_count);
    assert!(built.static_count >= 1, "expected >= 1 static, got {}", built.static_count);
    assert_eq!(built.import_count, 2, "expected 2 WASI imports (fd_write, proc_exit)");

    let checked = check_wasm64(wasm.as_slice())
        .unwrap_or_else(|e| panic!("check_wasm64 rejected emitted wasm: {e}"));
    assert!(checked.memory64, "module must be memory64");
    assert!(!checked.has_blaze_host_v1, "blaze_host_v1 was disabled");
    assert_eq!(checked.import_count, 2, "expected 2 imports in binary");
    assert!(checked.func_count >= 5, "expected >= 5 funcs in binary, got {}", checked.func_count);
    assert!(checked.has_memory_export, "must export memory");
    assert!(checked.has_start_export, "must synthesize and export _start");
    assert!(checked.table_size >= 2, "fn pointer add_one must populate indirect table");
    assert!(checked.elem_count >= 1, "indirect table must have at least 1 element");
    assert!(checked.data_segments >= 1, "MSG static must emit at least 1 data segment");
}

const STATIC_FN_PTR_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}
impl Copy for fn(i32) -> i32 {}

fn double_it(x: i32) -> i32 {
    x + x
}

static DISPATCH: fn(i32) -> i32 = double_it;

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let f = DISPATCH;
    f(21)
}
"#;

#[test]
fn static_fn_pointer_relocation_and_determinism() {
    let f = fixture(STATIC_FN_PTR_PROGRAM);
    let entry = item_named(&f, ItemKind::Function, "main");
    let dispatch_item = item_named(&f, ItemKind::Static, "DISPATCH");
    let dispatch_sym = static_symbol(dispatch_item);
    let b = builder();
    let mut store = MemoryStore::new_in(alloc());
    let mut walk_maker = Maker::new(reader(&f), Some(entry), &b, alloc());
    let (walk, _counts) = walk_records(entry, &EveryItem, &mut store, &b, &producer(), &mut walk_maker, alloc())
        .unwrap_or_else(|e| panic!("walk_records failed: {e}"));

    let main_sym = walk.symbol(0);
    let opts = Wasm64BuildOpts {
        module_name: "static_fn_ptr_e2e",
        start_symbol: Some(main_sym),
        exports: &[(main_sym, "main")],
        export_globals: &[("dispatch_addr", dispatch_sym)],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };

    let compile_once = || {
        let mut wasm_maker = Maker::<TestGrant, SecondCheck>::of_start(
            reader(&f),
            Some(entry),
            MakerStart::ByRule,
            SecondCheck,
            &b,
            None,
            alloc(),
        );
        let mut wasm: AVec<'static, u8> = OwnVec::new_in(alloc());
        let built = compile_walk_to_wasm64(
            &walk,
            |_| Some(TestGrant),
            &mut wasm_maker,
            &opts,
            default_wasm64_import,
            &mut wasm,
            alloc(),
        )
        .unwrap_or_else(|e| panic!("compile_walk_to_wasm64 failed: {e}"));
        (built, wasm)
    };

    let (built_a, wasm_a) = compile_once();
    let (built_b, wasm_b) = compile_once();

    assert_eq!(wasm_a.as_slice(), wasm_b.as_slice(), "wasm64 emission must be byte-for-byte deterministic");
    assert_eq!(built_a.exported, built_b.exported);
    assert_eq!(built_a.import_count, 0);
    assert!(built_a.static_count >= 1, "DISPATCH static must be emitted");
    assert!(built_a.func_count >= 3, "expected main, double_it, and _start");

    let checked = check_wasm64(wasm_a.as_slice())
        .unwrap_or_else(|e| panic!("check_wasm64 rejected static fn ptr module: {e}"));
    assert!(checked.memory64);
    assert!(!checked.has_blaze_host_v1);
    assert_eq!(checked.import_count, 0);
    assert!(checked.has_start_export);
    assert!(checked.has_memory_export);
    assert!(checked.table_size >= 2, "static fn ptr relocation must intern double_it into table 0");
    assert!(checked.elem_count >= 1);
    assert!(checked.data_segments >= 1);
    // 3 built-in globals ($__stack_pointer, $__heap_base, blaze_ring_abi_version) + dispatch_addr
    assert!(checked.global_count >= 4, "expected exported dispatch_addr global, got {}", checked.global_count);
}

const EXTERN_WEAK_MISSING_PROGRAM: &str = r#"
#![no_core]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}
impl Copy for u8 {}
impl Copy for usize {}
impl<T> Copy for *const T {}

unsafe extern "C" {
    #[linkage = "extern_weak"]
    static MISSING_WEAK_SYM: u8;
}

static WEAK_HOLDER: *const u8 = unsafe { &MISSING_WEAK_SYM as *const u8 };

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let direct = unsafe { &MISSING_WEAK_SYM as *const u8 as usize };
    let via_static = WEAK_HOLDER as usize;
    if direct == 0 && via_static == 0 { 0 } else { 1 }
}
"#;

const EXTERN_STRONG_MISSING_PROGRAM: &str = r#"
#![no_core]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}
impl Copy for u8 {}
impl Copy for usize {}
impl<T> Copy for *const T {}

unsafe extern "C" {
    static MISSING_STRONG_SYM: u8;
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    unsafe { &MISSING_STRONG_SYM as *const u8 as usize as i32 }
}
"#;

#[test]
fn extern_weak_missing_static_resolves_to_zero_while_strong_fails() {
    let b = builder();

    // Missing `#[linkage = "extern_weak"]` static (both direct and via static datum) compiles
    // cleanly to a valid Wasm64 module with null address 0.
    let weak_f = fixture(EXTERN_WEAK_MISSING_PROGRAM);
    let weak_entry = item_named(&weak_f, ItemKind::Function, "main");
    let mut weak_store = MemoryStore::new_in(alloc());
    let mut weak_walk_maker = Maker::new(reader(&weak_f), Some(weak_entry), &b, alloc());
    let (weak_walk, _) = walk_records(
        weak_entry,
        &EveryItem,
        &mut weak_store,
        &b,
        &producer(),
        &mut weak_walk_maker,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("walk_records failed for extern_weak: {e}"));
    let weak_main_sym = weak_walk.symbol(0);
    let weak_opts = Wasm64BuildOpts {
        module_name: "extern_weak_missing",
        start_symbol: Some(weak_main_sym),
        exports: &[(weak_main_sym, "main")],
        export_globals: &[],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };
    let mut weak_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&weak_f),
        Some(weak_entry),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        alloc(),
    );
    let mut wasm: AVec<'static, u8> = OwnVec::new_in(alloc());
    let built = compile_walk_to_wasm64(
        &weak_walk,
        |_| Some(TestGrant),
        &mut weak_maker,
        &weak_opts,
        default_wasm64_import,
        &mut wasm,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("compile_walk_to_wasm64 failed for missing extern_weak: {e}"));
    assert_eq!(built.import_count, 0);
    let checked = check_wasm64(wasm.as_slice())
        .unwrap_or_else(|e| panic!("check_wasm64 rejected extern_weak module: {e}"));
    assert!(checked.memory64);
    assert_eq!(checked.import_count, 0);
    assert_eq!(checked.elem_count, 0, "absent weak static must resolve to null, not intern a table entry");

    // Control: missing strong `extern "C"` static still fails with UnresolvedImport.
    let strong_f = fixture(EXTERN_STRONG_MISSING_PROGRAM);
    let strong_entry = item_named(&strong_f, ItemKind::Function, "main");
    let mut strong_store = MemoryStore::new_in(alloc());
    let mut strong_walk_maker = Maker::new(reader(&strong_f), Some(strong_entry), &b, alloc());
    let (strong_walk, _) = walk_records(
        strong_entry,
        &EveryItem,
        &mut strong_store,
        &b,
        &producer(),
        &mut strong_walk_maker,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("walk_records failed for extern strong: {e}"));
    let strong_main_sym = strong_walk.symbol(0);
    let strong_opts = Wasm64BuildOpts {
        module_name: "extern_strong_missing",
        start_symbol: Some(strong_main_sym),
        exports: &[(strong_main_sym, "main")],
        export_globals: &[],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };
    let mut strong_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&strong_f),
        Some(strong_entry),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        alloc(),
    );
    let mut strong_wasm: AVec<'static, u8> = OwnVec::new_in(alloc());
    let err = compile_walk_to_wasm64(
        &strong_walk,
        |_| Some(TestGrant),
        &mut strong_maker,
        &strong_opts,
        default_wasm64_import,
        &mut strong_wasm,
        alloc(),
    )
    .expect_err("missing strong extern static must fail in wasm64");
    assert!(
        err.to_string().contains("MISSING_STRONG_SYM"),
        "expected unresolved error naming MISSING_STRONG_SYM, got: {err}",
    );
}

const EXTERN_WEAK_PRESENT_FN_PROGRAM: &str = r#"
#![no_core]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}
impl Copy for u8 {}
impl Copy for usize {}
impl<T> Copy for *const T {}

mod defs {
    #[unsafe(no_mangle)]
    pub extern "C" fn present_hook() -> i32 {
        7
    }

    #[unsafe(export_name = "aliased_hook")]
    pub extern "C" fn internal_hook() -> i32 {
        9
    }
}

unsafe extern "C" {
    #[linkage = "extern_weak"]
    static present_hook: u8;

    #[linkage = "extern_weak"]
    static aliased_hook: u8;

    #[linkage = "extern_weak"]
    static absent_hook: u8;
}

static PRESENT_HOLDER: *const u8 = unsafe { &present_hook as *const u8 };
static ALIASED_HOLDER: *const u8 = unsafe { &aliased_hook as *const u8 };
static ABSENT_HOLDER: *const u8 = unsafe { &absent_hook as *const u8 };

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let direct_present = unsafe { &present_hook as *const u8 as usize };
    let direct_aliased = unsafe { &aliased_hook as *const u8 as usize };
    let direct_absent = unsafe { &absent_hook as *const u8 as usize };
    let static_present = PRESENT_HOLDER as usize;
    let static_aliased = ALIASED_HOLDER as usize;
    let static_absent = ABSENT_HOLDER as usize;
    if direct_present != 0
        && direct_present == static_present
        && direct_aliased != 0
        && direct_aliased == static_aliased
        && direct_present != direct_aliased
        && direct_absent == 0
        && static_absent == 0
    {
        0
    } else {
        1
    }
}
"#;

const EXTERN_WEAK_FOREIGN_FN_PROGRAM: &str = r#"
#![no_core]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}

unsafe extern "C" {
    #[linkage = "extern_weak"]
    fn forbidden_weak_fn() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    unsafe { forbidden_weak_fn() }
}
"#;

#[test]
fn extern_weak_static_resolves_present_rust_fn_export_and_refuses_weak_foreign_fn() {
    let b = builder();

    // 1. Present Rust `#[no_mangle]` and `#[export_name]` functions referenced via `extern_weak`
    //    statics (both directly in code and via static datums) resolve to non-null indirect table
    //    slots, while an absent weak static resolves to null (0).
    let f = fixture(EXTERN_WEAK_PRESENT_FN_PROGRAM);
    let entry = item_named(&f, ItemKind::Function, "main");
    let mut store = MemoryStore::new_in(alloc());
    let mut walk_maker = Maker::new(reader(&f), Some(entry), &b, alloc());
    let (walk, _) = walk_records(
        entry,
        &EveryItem,
        &mut store,
        &b,
        &producer(),
        &mut walk_maker,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("walk_records failed for present extern_weak fn: {e}"));
    let main_sym = walk.symbol(0);
    let opts = Wasm64BuildOpts {
        module_name: "extern_weak_present_fn",
        start_symbol: Some(main_sym),
        exports: &[(main_sym, "main")],
        export_globals: &[],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };
    let mut wasm_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&f),
        Some(entry),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        alloc(),
    );
    let mut wasm: AVec<'static, u8> = OwnVec::new_in(alloc());
    let built = compile_walk_to_wasm64(
        &walk,
        |_| Some(TestGrant),
        &mut wasm_maker,
        &opts,
        default_wasm64_import,
        &mut wasm,
        alloc(),
    )
    .unwrap_or_else(|e| panic!("compile_walk_to_wasm64 failed for present extern_weak fn: {e}"));
    assert_eq!(built.import_count, 0);
    assert!(
        built.func_count >= 4,
        "expected main, _start, present_hook, and internal_hook, got {}",
        built.func_count,
    );
    let checked = check_wasm64(wasm.as_slice())
        .unwrap_or_else(|e| panic!("check_wasm64 rejected extern_weak present fn module: {e}"));
    assert!(checked.memory64);
    assert_eq!(checked.import_count, 0);
    assert!(
        checked.table_size >= 3 && checked.elem_count >= 1,
        "both present_hook and aliased_hook must intern non-null indirect table slots (got table_size={}, elem_count={})",
        checked.table_size,
        checked.elem_count,
    );

    // 2. `#[linkage = "extern_weak"]` on a foreign function (rather than a foreign static) must
    //    be refused instead of silently degrading to a strong `Callee::C` import.
    let bad_f = fixture(EXTERN_WEAK_FOREIGN_FN_PROGRAM);
    let bad_entry = item_named(&bad_f, ItemKind::Function, "main");
    let mut bad_store = MemoryStore::new_in(alloc());
    let mut bad_maker = Maker::new(reader(&bad_f), Some(bad_entry), &b, alloc());
    let err = walk_records(
        bad_entry,
        &EveryItem,
        &mut bad_store,
        &b,
        &producer(),
        &mut bad_maker,
        alloc(),
    )
    .err()
    .expect("`#[linkage = \"extern_weak\"]` on a foreign fn must be refused");
    assert!(
        err.to_string().contains("extern_weak"),
        "expected refusal mentioning extern_weak, got: {err}",
    );
}
