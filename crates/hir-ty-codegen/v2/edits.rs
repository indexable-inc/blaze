//! The edit fuzzer: an incremental build after any sequence of edits gives what a clean build
//! of the edited source does (docs/v2/plan/design/verify.md), judged by the invalidation oracle
//! (`blaze-v2-proof`, docs/v2/plan.md).
//!
//! A small real crate (a struct, consts, an `#[inline]` callee, a pair of `cfg`-chosen
//! functions, a generic, a static, and a boxed struct whose `Drop` comes and goes, boxed by a
//! small `alloc` of the fixture's) is rendered from a [`Model`]; Hegel draws a sequence of
//! [`Edit`]s, each a change of the model, so every edited source is a valid crate, and whether
//! allocations may move to the stack (`Policy::heap_to_stack`, `hir-ty-mir`'s `HeapToStack`).
//! After each edit the one database is changed in place and built again with the memo and
//! store the builds before it left, every store hit checked by the store check
//! ([`blaze_store_check::Setting::ALL`]); a fresh database of the same text is built with an empty
//! memo and store. The oracle then holds the incremental build to the clean one ([`verdict`]):
//!
//! - the codegen inputs (each instance with its code hash) and the kernel's entries (each
//!   instance with the key its dev template is kept under) by [`blaze_v2_proof::apart`];
//! - the kernel's entries as a trace ([`blaze_v2_proof::kernel`]) by [`blaze_v2_proof::under`],
//!   which names the stale entry and why (taken from the store with other bytes than a clean
//!   build makes: its identity misses an input; made under a key the clean build has not: its
//!   identity depends on history), and by [`blaze_v2_proof::over`] against the build before,
//!   where an entry compiled again under a key the store holds is a missed hit;
//! - the code, statics, symbols, roots, items and digests by `apart` too, a function's code by
//!   its first differing byte ([`blaze_store_check::bytes`]).
//!
//! Every finding of a step is reported at once. Hegel shrinks a failure to the shortest edit
//! sequence that still breaks it; save that as a `#[test]` calling [`replay`], named for the
//! property it breaks.
//!
//! The same model checks code identity (`hir-ty-identity`, docs/v2/plan/design/code-identity.md):
//! an edit of comments, formatting or item order keeps every function's `instance_identity` and
//! the root's `code_id`, and an edit of behaviour changes the root's `code_id`
//! (`edits/identity.rs`).
//!
//! `HEGEL_TEST_CASES=<n>` runs either longer.

#[path = "tests/db.rs"]
mod db;
mod identity;

use std::collections::BTreeMap;

use blaze_cas::Key;
use blaze_codegen::{
    InstanceKey,
    driver::{self, Built, KernelEntry, Lookup},
    memo::Memo,
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize},
};
use blaze_query::QueryKey;
use blaze_v2_proof::{Apart, Stale, Trace, TraceBuilder, apart, kernel, over, under};
use hegel::{Hegel, Settings, TestCase, generators as gs};
use hir_expand::fixture;

use crate::v2::{
    Backends, BuildTime, Databases, HashSource, HirFrontend, ItemHashes, RootKind, Runs,
};
use db::TestDB;

/// The width of one field of the fixture's struct.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Width {
    U8,
    U32,
    U64,
}

impl Width {
    fn name(self) -> &'static str {
        match self {
            Width::U8 => "u8",
            Width::U32 => "u32",
            Width::U64 => "u64",
        }
    }
}

/// One edit of the fixture crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    /// A field more on `P` (up to four): its layout and every body that builds or reads it.
    AddField(Width),
    /// Const `K{0}` set to `{1}`.
    ChangeConst(usize, u32),
    /// The body of the `#[inline]` callee `mix`, `x ^ {0}`.
    ChangeInlined(u32),
    /// Which of the two `cfg`-chosen `mode`s is compiled.
    FlipCfg,
    /// Items `{0}` and `{1}` of the file swapped.
    Reorder(usize, usize),
    /// The comment in `entry`'s body, which shifts no item.
    EditComment(u32),
    /// The layout: `{0}` blank lines after each item and the indent of `entry`'s body, which
    /// moves every item after the first.
    Reformat(u8),
    /// `impl Drop` for the boxed struct `B` added or removed: the drop of `boxed`'s box runs
    /// the program's code or none, and with `heap_to_stack` its box stays on the heap or moves
    /// to the stack (rule 21: `boxed`'s key covers the impl).
    ToggleDrop,
}

/// What the fixture's source is rendered from.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Model {
    fields: Vec<Width>,
    consts: [u32; 2],
    inlined: u32,
    cfg_on: bool,
    /// The items in their file order, by [`ITEMS`]' index.
    order: Vec<usize>,
    comment: u32,
    /// [`Edit::Reformat`]'s layout.
    spacing: u8,
    /// `B` has an `impl Drop` ([`Edit::ToggleDrop`]).
    dropping: bool,
}

/// How many items the file has.
const ITEMS: usize = 8;

/// What the edited crate links, never edited: minicore, and an `alloc` (a lang crate, as std's
/// is) with a `Box` that allocates by `malloc`, which `boxed` boxes with.
const LIBRARIES: &str = r#"//- minicore: sized, copy, drop, transmute, size_of
//- /alloc.rs crate:alloc
#![feature(lang_items)]
use core::mem::{size_of, transmute};
extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(p: *mut u8);
}
pub struct Global;
#[lang = "owned_box"]
pub struct Box<T: ?Sized, A = Global>(*mut T, A);
impl<T: ?Sized, A> Drop for Box<T, A> {
    fn drop(&mut self) {
        unsafe { free(self.0 as *mut u8) }
    }
}
#[rustc_intrinsic]
pub unsafe fn write_via_move<T>(ptr: *mut T, value: T);
impl<T> Box<T> {
    pub fn new(x: T) -> Self {
        let ptr = unsafe { malloc(size_of::<T>()) } as *mut T;
        unsafe { write_via_move(ptr, x) };
        unsafe { transmute(ptr) }
    }
}
"#;

impl Model {
    fn new() -> Model {
        Model {
            fields: vec![Width::U32],
            consts: [3, 5],
            inlined: 9,
            cfg_on: true,
            order: (0..ITEMS).collect(),
            comment: 0,
            spacing: 0,
            dropping: false,
        }
    }

    fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::AddField(width) if self.fields.len() < 4 => self.fields.push(width),
            Edit::AddField(_) => {}
            Edit::ChangeConst(at, value) => self.consts[at % 2] = value,
            Edit::ChangeInlined(value) => self.inlined = value,
            Edit::FlipCfg => self.cfg_on = !self.cfg_on,
            Edit::Reorder(a, b) => self.order.swap(a % ITEMS, b % ITEMS),
            Edit::EditComment(value) => self.comment = value,
            Edit::Reformat(spacing) => self.spacing = spacing % 3,
            Edit::ToggleDrop => self.dropping = !self.dropping,
        }
    }

    fn item(&self, at: usize) -> String {
        match at {
            0 => {
                let fields: String = (self.fields.iter().enumerate())
                    .map(|(n, it)| format!("    pub f{n}: {},\n", it.name()))
                    .collect();
                let inits: String =
                    (self.fields.iter().enumerate()).map(|(n, it)| format!("f{n}: x as {}, ", it.name())).collect();
                let sum: String = (0..self.fields.len()).map(|n| format!(" ^ self.f{n} as u32")).collect();
                format!(
                    "pub struct P {{\n{fields}}}\n\
                     impl P {{\n    pub fn new(x: u32) -> P {{ P {{ {inits}}} }}\n    \
                     pub fn sum(&self) -> u32 {{ 0{sum} }}\n}}\n"
                )
            }
            1 => format!("pub const K0: u32 = {};\npub const K1: u32 = {};\n", self.consts[0], self.consts[1]),
            2 => format!("#[inline]\npub fn mix(x: u32) -> u32 {{ x ^ {} }}\n", self.inlined),
            3 => {
                let on = if self.cfg_on { "all()" } else { "any()" };
                format!("#[cfg({on})]\npub fn mode() -> u32 {{ 1 }}\n#[cfg(not({on}))]\npub fn mode() -> u32 {{ 2 }}\n")
            }
            4 => {
                let indent = " ".repeat(4 * (1 + usize::from(self.spacing)));
                format!(
                    "pub fn entry(x: u32) -> u32 {{\n{indent}// note {}\n{indent}P::new(x).sum() ^ mix(x) ^ K0 ^ mode() ^ pick(x, K1) ^ read_s()\n}}\n",
                    self.comment
                )
            }
            5 => "pub fn pick<T: Copy>(a: T, _b: T) -> T { a }\npub fn wide(x: u64) -> u64 { pick(x, 0) }\n".to_owned(),
            6 => "pub static S: u32 = K0 ^ K1;\npub fn read_s() -> u32 { S }\n".to_owned(),
            _ => {
                let drop = if self.dropping { "impl Drop for B {\n    fn drop(&mut self) {}\n}\n" } else { "" };
                format!(
                    "pub struct B {{\n    pub v: u32,\n}}\n{drop}\
                     pub fn boxed(x: u32) -> u32 {{\n    let b = alloc::Box::new(B {{ v: x }});\n    b.v\n}}\n"
                )
            }
        }
    }

    /// The edited crate's one file.
    fn body(&self) -> String {
        let mut out = String::new();
        for at in &self.order {
            out.push_str(&self.item(*at));
            out.push_str(&"\n".repeat(usize::from(self.spacing)));
        }
        out
    }

    /// The fixture: [`LIBRARIES`] and the edited crate, at `/lib.rs`.
    fn source(&self) -> String {
        format!("{LIBRARIES}//- /lib.rs crate:edited deps:alloc\n{}", self.body())
    }
}

fn edit(tc: &TestCase) -> Edit {
    match tc.draw(gs::integers::<u8>().max_value(7)) {
        0 => Edit::AddField(tc.draw(gs::sampled_from(vec![Width::U8, Width::U32, Width::U64]))),
        1 => Edit::ChangeConst(
            tc.draw(gs::integers().max_value(1)),
            tc.draw(gs::integers::<u32>().max_value(99)),
        ),
        2 => Edit::ChangeInlined(tc.draw(gs::integers::<u32>().max_value(99))),
        3 => Edit::FlipCfg,
        4 => Edit::Reorder(
            tc.draw(gs::integers().max_value(ITEMS - 1)),
            tc.draw(gs::integers().max_value(ITEMS - 1)),
        ),
        5 => Edit::Reformat(tc.draw(gs::integers::<u8>().max_value(2))),
        6 => Edit::EditComment(tc.draw(gs::integers::<u32>().max_value(99))),
        _ => Edit::ToggleDrop,
    }
}

/// The store the incremental builds share: what each build put, by key.
#[derive(Default)]
struct Kept(BTreeMap<Key, Vec<u8>>);

impl Lookup for Kept {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }
}

/// A dev build of the fixture's toolchain (its crates are x86-64 Linux's, whatever the host;
/// nothing built runs), set up once a process, allocations moving to the stack where
/// `heap_to_stack` says.
fn policy(heap_to_stack: bool) -> Policy {
    static TOOLCHAIN: std::sync::OnceLock<blaze_codegen::settings::Toolchain> =
        std::sync::OnceLock::new();
    let toolchain = TOOLCHAIN.get_or_init(|| {
        let target = crate::Target::for_fixtures().triple();
        // The fixture's own code generator: nothing it makes is kept past the process.
        let code =
            crate::v2::CodegenCode(blaze_ids::ContentId::of("blaze v2 edits fixture code", b""));
        let target_data = crate::v2::toolchain::fixture_target_data(target);
        let toolchain = crate::v2::toolchain_for(&code, target, &target_data, &[], Runs::Elsewhere)
            .expect("the fixture's toolchain");
        crate::v2::select(&toolchain).expect("the fixture's code generator");
        toolchain
    });
    Policy {
        toolchain: toolchain.clone(),
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks: false,
            debug_assertions: false,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
            stencils: false,
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
        proc_macro_bridge: false,
        report: false,
        fuel: false,
        heap_to_stack,
    }
}

/// The library's build over `db` with `memo` and `store`, its store hits checked as
/// `verify` samples them, allocations moving to the stack where `heap_to_stack` says.
fn build(
    db: &TestDB,
    memo: &Memo,
    store: &Kept,
    verify: blaze_store_check::Setting,
    heap_to_stack: bool,
) -> Result<Built, String> {
    let policy = policy(heap_to_stack);
    // Named by path, so two databases of one text give one key ([`HashSource::ById`] would not).
    let hashes = ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned())));
    let build_time = BuildTime::default();
    let frontend = HirFrontend::new(Databases::new(db), HashSource::Items(hashes), build_time)
        .with_policy(&policy);
    let krate = fixture::test_crate(db);
    let roots = frontend.roots(krate, RootKind::Library).map_err(|it| it.what)?;
    let origin = frontend.with(|hir| frontend.origin(hir, krate));
    let asked: Vec<_> = roots.roots.iter().map(|it| (it.clone(), origin)).collect();
    let backends = Backends::default();
    driver::Build::new(&backends, &frontend, &policy, memo)
        .verify_store(verify)
        .run_with_statics(&asked, &roots.statics, &roots.check_roots(origin), store)
        .map_err(|errors| errors.into_iter().map(|it| it.what).collect::<Vec<_>>().join("\n"))
}

/// The trace the oracle holds of `built`'s kernel entries ([`blaze_v2_proof::kernel`]): each
/// dev template a row of its step under its key, its fingerprint its bytes' (the store's for a
/// hit, the build's put for one it made), kept when it was a hit. And each row's instance, to
/// name it.
fn traced(built: &Built, store: &Kept) -> (Trace, BTreeMap<QueryKey, InstanceKey>) {
    let mut trace = TraceBuilder::default();
    let mut rows = BTreeMap::new();
    for KernelEntry { instance, step, key, hit } in &built.kernel {
        let row = kernel::entry(*step, *key).unwrap_or_else(|why| panic!("{why}"));
        let bytes = if *hit {
            store.0.get(key).map(Vec::as_slice)
        } else {
            built
                .puts
                .binary_search_by(|it| it.key.cmp(key))
                .ok()
                .map(|at| built.puts[at].bytes.as_slice())
        };
        let Some(bytes) = bytes else {
            let whence = if *hit { "the store has" } else { "the build put" };
            panic!(
                "the kernel entry {key:?} of instance {instance:?} was {} and {whence} no bytes under it",
                if *hit { "a hit" } else { "made" }
            );
        };
        kernel::record(&mut trace, row, kernel::value(bytes), [], *hit)
            .unwrap_or_else(|why| panic!("{why}"));
        rows.insert(row, *instance);
    }
    (trace.finish(), rows)
}

/// `pairs` as a map, for [`apart`].
fn map<K: Ord + Clone, V: Clone>(pairs: impl IntoIterator<Item = (K, V)>) -> BTreeMap<K, V> {
    pairs.into_iter().collect()
}

/// One line of each key where `incremental` and `clean` differ in `what`.
fn each_apart<K: Ord + Clone + std::fmt::Debug, V: PartialEq + Clone + std::fmt::Debug>(
    what: &str,
    incremental: &BTreeMap<K, V>,
    clean: &BTreeMap<K, V>,
    name: &dyn Fn(&K) -> String,
    out: &mut Vec<String>,
) {
    for it in apart(incremental, clean) {
        out.push(match it {
            Apart::OnlyFirst { key, value } => {
                format!("{what} of {}: only incremental, {value:?}", name(&key))
            }
            Apart::OnlySecond { key, value } => {
                format!("{what} of {}: only clean, {value:?}", name(&key))
            }
            Apart::Differ { key, first, second } => {
                format!("{what} of {}: {first:?} incremental, {second:?} clean", name(&key))
            }
        });
    }
}

/// Everything the oracle finds of `incremental` against `clean` (and `before`, the trace of
/// the build before it): what a build gives the linker, its codegen inputs, its kernel entries,
/// its keys and its digests (never its stats, puts or memo use, which a warm build has others
/// of). Empty when the incremental build is the clean one.
fn verdict(
    before: &Trace,
    incremental: (&Built, &Trace, &BTreeMap<QueryKey, InstanceKey>),
    clean: (&Built, &Trace, &BTreeMap<QueryKey, InstanceKey>),
) -> Vec<String> {
    let (inc, inc_trace, inc_rows) = incremental;
    let (cl, cl_trace, cl_rows) = clean;
    let names: BTreeMap<InstanceKey, &str> = cl
        .debug_names
        .iter()
        .chain(&inc.debug_names)
        .map(|(key, name)| (*key, name.as_str()))
        .collect();
    let function =
        |key: &InstanceKey| format!("`{}` ({key:?})", names.get(key).copied().unwrap_or("?"));
    let entry = |row: &QueryKey| match cl_rows.get(row).or_else(|| inc_rows.get(row)) {
        Some(instance) => {
            format!("the kernel entry {:?} of {}", Key(row.input), function(instance))
        }
        None => format!("the kernel row {row:?}"),
    };
    let mut out = Vec::new();

    each_apart(
        "the codegen input",
        &map(inc.inputs.iter().copied()),
        &map(cl.inputs.iter().copied()),
        &function,
        &mut out,
    );
    let keyed = |built: &Built| map(built.kernel.iter().map(|it| (it.instance, it.key)));
    for it in apart(&keyed(inc), &keyed(cl)) {
        if let Apart::Differ { key, first, second } = it {
            out.push(format!(
                "the kernel entry of {} is kept under {first:?} incremental and {second:?} clean: its identity depends on what was built before",
                function(&key)
            ));
        }
    }
    for stale in under(inc_trace, cl_trace, &[]) {
        out.push(match stale {
            Stale::UnrecordedRead { query, .. } => format!(
                "{} holds other bytes than a clean build makes under the same key, and records no read: its identity misses an input its bytes depend on (rule 21)",
                entry(&query)
            ),
            Stale::Unverified { query, read, saw, now } => format!(
                "{} was taken from the store though its read {read:?} saw {saw:?} and is now {now:?}",
                entry(&query)
            ),
            Stale::Absent { query } => format!("{} was made by the clean build and never reached by the incremental one", entry(&query)),
            Stale::Downstream { query, .. } => format!("{} read otherwise than the clean build and came out otherwise", entry(&query)),
        });
    }
    let (_, needless) = over(before, inc_trace);
    for row in needless {
        out.push(format!("{} was compiled again though the build before kept it under the same key: a missed store hit", entry(&row)));
    }

    fn functions(built: &Built) -> BTreeMap<InstanceKey, &blaze_codegen::CompiledFunction> {
        map(built.functions.iter().map(|it| (it.key, it)))
    }
    for it in apart(&functions(inc), &functions(cl)) {
        out.push(match it {
            Apart::OnlyFirst { key, .. } => format!("function {}: only incremental", function(&key)),
            Apart::OnlySecond { key, .. } => format!("function {}: only clean", function(&key)),
            Apart::Differ { key, first: a, second: b } => format!(
                "function {}: code differs first at byte {:?}; relocs equal: {}, data equal: {}, lines equal: {}",
                function(&key),
                blaze_store_check::bytes::first_difference(&a.code, &b.code),
                a.relocs == b.relocs,
                a.data == b.data,
                a.lines == b.lines,
            ),
        });
    }
    let debug = |key: &dyn std::fmt::Debug| format!("{key:?}");
    fn statics(
        built: &Built,
    ) -> BTreeMap<(blaze_items::ItemKey, blaze_ids::ContentId), &blaze_codegen::Static> {
        map(built.statics.iter().map(|it| ((it.item, it.instance), it)))
    }
    each_apart("the static", &statics(inc), &statics(cl), &|key| debug(key), &mut out);
    let set = |keys: &[InstanceKey]| map(keys.iter().map(|it| (*it, ())));
    each_apart("the root", &set(&inc.roots), &set(&cl.roots), &function, &mut out);
    each_apart(
        "the patchable slot",
        &set(&inc.patchable),
        &set(&cl.patchable),
        &function,
        &mut out,
    );
    // A symbol and how it is defined: an edit that makes a definition weak is a difference.
    let symbols = |built: &Built| {
        // clone: the report's own copy of each name, once per comparison
        map(built.symbols.iter().map(|(key, name, linkage)| (*key, (name.clone(), *linkage))))
    };
    each_apart("the symbol", &symbols(inc), &symbols(cl), &function, &mut out);
    let static_symbols = |built: &Built| {
        map(built.static_symbols.iter().map(|(item, id, name, linkage)| {
            // clone: the report's own copy of each name, once per comparison
            ((*item, *id), (name.clone(), *linkage))
        }))
    };
    each_apart(
        "the static symbol",
        &static_symbols(inc),
        &static_symbols(cl),
        &|key| debug(key),
        &mut out,
    );
    let items = |built: &Built| map(built.items.iter().map(|it| (*it, ())));
    each_apart("the item", &items(inc), &items(cl), &|key| debug(key), &mut out);
    each_apart(
        "the debug name",
        &map(inc.debug_names.iter().cloned()),
        &map(cl.debug_names.iter().cloned()),
        &function,
        &mut out,
    );
    let digests = |built: &Built| map(built.digests.functions.iter().copied());
    each_apart("the function digest", &digests(inc), &digests(cl), &function, &mut out);
    let digests = |built: &Built| map(built.digests.statics.iter().copied());
    each_apart("the static digest", &digests(inc), &digests(cl), &|key| debug(key), &mut out);
    let digests = |built: &Built| map(built.digests.debug_names.iter().copied());
    each_apart("the debug name digest", &digests(inc), &digests(cl), &function, &mut out);
    out
}

/// The property: after each of `edits`, an incremental build (store hits all checked) is a
/// clean build of the same text, as the oracle judges it, both with allocations moving to the
/// stack where `heap_to_stack` says.
fn replay(edits: &[Edit], heap_to_stack: bool) {
    let mut model = Model::new();
    let (mut db, file) = database(&model.source());
    let (memo, mut store) = (Memo::new(), Kept::default());
    let first = build(&db, &memo, &store, blaze_store_check::Setting::ALL, heap_to_stack)
        .unwrap_or_else(|why| panic!("{why}\n{}", model.source()));
    let (mut before, _) = traced(&first, &store);
    store.0.extend(first.puts.into_iter().map(|it| (it.key, it.bytes)));
    for (n, edit) in edits.iter().enumerate() {
        model.apply(*edit);
        let (text, body) = (model.source(), model.body());
        db.set_file_text(file, &body);
        let incremental = build(&db, &memo, &store, blaze_store_check::Setting::ALL, heap_to_stack);
        // Given the file's text as the incremental database is, before any query: a clean
        // build of the same bytes, whatever the fixture's parser does to its text.
        let (mut fresh, fresh_file) = database(&text);
        fresh.set_file_text(fresh_file, &body);
        let clean = build(
            &fresh,
            &Memo::new(),
            &Kept::default(),
            blaze_store_check::Setting::OFF,
            heap_to_stack,
        );
        let (incremental, clean) = match (incremental, clean) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(why), Ok(_)) => panic!(
                "after edits {:?}, the incremental build failed and the clean built: {why}\n{text}",
                &edits[..=n]
            ),
            (Ok(_), Err(why)) => panic!(
                "after edits {:?}, the clean build failed and the incremental built: {why}\n{text}",
                &edits[..=n]
            ),
            (Err(a), Err(b)) => panic!(
                "the fixture after {:?} builds neither way:\nincremental: {a}\nclean: {b}\n{text}",
                &edits[..=n]
            ),
        };
        let (inc_trace, inc_rows) = traced(&incremental, &store);
        let (cl_trace, cl_rows) = traced(&clean, &Kept::default());
        let found =
            verdict(&before, (&incremental, &inc_trace, &inc_rows), (&clean, &cl_trace, &cl_rows));
        if !found.is_empty() {
            panic!(
                "after edits {:?}, the incremental build is not the clean one ({} findings):\n- {}\n{text}",
                &edits[..=n],
                found.len(),
                found.join("\n- ")
            );
        }
        store.0.extend(incremental.puts.into_iter().map(|it| (it.key, it.bytes)));
        before = inc_trace;
    }
}

/// The database of fixture `text`, and the edited crate's file.
fn database(text: &str) -> (TestDB, base_db::FileId) {
    let fx = fixture::db(text);
    let file = fx.file_at("/lib.rs");
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

fn edits(tc: &TestCase) -> Vec<Edit> {
    let count: usize = tc.draw(gs::integers().min_value(1).max_value(4));
    (0..count).map(|_| edit(tc)).collect()
}

fn incremental_is_clean(tc: &TestCase) {
    let heap_to_stack = tc.draw(gs::booleans());
    replay(&edits(tc), heap_to_stack);
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// Each edit kind once, in a fixed order, with allocations moving to the stack and without:
/// the fuzzer's smoke run, and the first thing to read when it fails. `ToggleDrop` twice: the
/// box of `boxed` goes back to the heap, then to the stack again.
#[test]
fn each_edit_kind_builds_incrementally_as_clean() {
    let edits = [
        Edit::EditComment(1),
        Edit::ChangeConst(0, 7),
        Edit::ChangeInlined(4),
        Edit::FlipCfg,
        Edit::Reorder(0, 4),
        Edit::Reformat(2),
        Edit::AddField(Width::U8),
        Edit::ToggleDrop,
        Edit::Reorder(7, 1),
        Edit::ToggleDrop,
    ];
    for heap_to_stack in [false, true] {
        replay(&edits, heap_to_stack);
    }
}

#[test]
fn any_edit_sequence_builds_incrementally_as_clean() {
    run_cases(12, |tc| incremental_is_clean(&tc));
}
