use base_db::{Db, Env};
use blaze_cas::{Config, Store};
use blaze_core::{
    FileName, run_until_idle,
    sim::{Faults, Sim},
};
use blaze_usercode::{
    RunKey,
    index::{self, Found},
    trace::{Seen, assemble_seen},
};
use proc_macro_api::kept::{Keep, Published, Put};
use span::{Edition, EditionedFileId, Span, SpanAnchor, SyntaxContext, TextRange};

use intern::Symbol;

use super::{Given, Library, LoadError, Loads, Modules, abi::Mismatch};
use crate::{
    keep::Calling,
    run::Spent,
    stream::{IdentKind, Spacing, Stream, Tree},
};

const WHAT: &str = "the wasm module of the proc-macro crate `demo_derive`";

/// A module of the host's ABI whose one macro gives back the stream it was given: the fourth
/// handle of its input (after the three sites), as `Ok(Some(handle))`.
const ECHO: &str = r#"
(module
  (import "blaze_proc_macro" "output" (func $output (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "_initialize"))
  (global $next (mut i32) (i32.const 1024))
  (func (export "blaze_proc_macro_alloc") (param $len i32) (result i32)
    (local $at i32)
    (local.set $at (global.get $next))
    (global.set $next (i32.add (global.get $next) (local.get $len)))
    (local.get $at))
  (func (export "blaze_proc_macro_expand") (param $index i32) (param $input i32) (param $len i32) (result i32)
    (if (i32.ne (local.get $index) (i32.const 0)) (then (return (i32.const 1))))
    (i32.store8 (i32.const 0) (i32.const 0))
    (i32.store8 (i32.const 1) (i32.const 0))
    (i32.store (i32.const 2) (i32.load (i32.add (local.get $input) (i32.const 12))))
    (call $output (i32.const 0) (i32.const 6))
    (i32.const 0)))
"#;

/// [`ECHO`] with an import of `env::open`: a way to the system around the host's shims.
fn reaching_around() -> String {
    ECHO.replacen("(memory", "(import \"env\" \"open\" (func (param i32 i32) (result i32)))\n  (memory", 1)
}

/// [`ECHO`] that first reads the variable `A` through the bridge's `env_var`, as the glue's
/// `getenv` does for `env::var("A")`.
fn reading_a() -> String {
    ECHO.replacen(
        "(memory (export \"memory\") 1)",
        "(import \"blaze_proc_macro\" \"env_var\" (func $env_var (param i32 i32 i32 i32) (result i32)))\n  \
         (memory (export \"memory\") 1)\n  (data (i32.const 512) \"A\")",
        1,
    )
    .replacen(
        "(if (i32.ne (local.get $index)",
        "(drop (call $env_var (i32.const 512) (i32.const 1) (i32.const 600) (i32.const 16)))\n    \
         (if (i32.ne (local.get $index)",
        1,
    )
}

fn at(start: u32, end: u32) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(3), Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// A store in `sim`, open and ready.
fn opened(sim: &mut Sim) -> Store {
    let mut effects = Vec::new();
    let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
    let mut store = Store::open(config, &mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        let durable = store.complete(completion, effects);
        assert!(durable.is_empty(), "opening a store made {} puts durable", durable.len());
    });
    assert!(store.ready());
    store
}

/// A daemon's modules, with nothing loaded.
fn modules() -> Modules {
    Modules::new().unwrap()
}

/// A store in `sim` in `dir`, with the stores in `layers` behind it, open and ready.
fn opened_in(sim: &mut Sim, dir: &str, layers: &[&str]) -> Store {
    let mut effects = Vec::new();
    let layers = layers.iter().map(|it| FileName::new(it).unwrap()).collect();
    let config = Config { dir: FileName::new(dir).unwrap(), layers, tags: 1 << 48 };
    let mut store = Store::open(config, &mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    });
    assert!(store.ready(), "{:?}", store.broken());
    store
}

/// `store`'s puts made durable.
fn flushed(sim: &mut Sim, store: &mut Store) {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    });
}

/// Keeps `puts` in `store` as a daemon's owner does: none is durable (no expansion of these
/// tests is verified across checkouts), so no daemon's queue or epoch is in it.
fn publish(store: &mut Store, puts: Vec<Put>) -> Published {
    proc_macro_api::kept::Keeps::Nothing("a test that publishes its own puts").publish(store, puts)
}

/// How `store` took `bytes` under `key`, sent by a remote as a value of `kind`.
fn fetched(store: &mut Store, key: blaze_cas::Key, kind: blaze_cas::Kind, bytes: &[u8]) -> blaze_cas::Fetched {
    struct Far;
    impl blaze_cas::Remote for Far {
        fn fetch(&mut self, _: &blaze_cas::Key, _: blaze_core::Tag, _: &mut Vec<blaze_core::Effect>) -> blaze_cas::Asked {
            blaze_cas::Asked::Sent
        }
    }
    let mut effects = Vec::new();
    // What this checks is the store's own refusal of a kind (machine code): every value is let in.
    let tag = store.fetch(&key, &mut Far, |_, _| Ok(()), &mut effects).expect("a store without the key asks for it");
    let answer = blaze_cas::remote::Answer::Value { key, kind, content: blaze_cas::content_of(bytes), bytes: bytes.to_vec() };
    store.complete(blaze_core::Completion { tag, done: blaze_core::Done::Read(answer.encode_framed()) }, &mut effects);
    let mut all = store.fetched();
    assert_eq!(all.len(), 1, "{all:?}");
    all.pop().unwrap().1
}

/// [`ECHO`]'s machine code as a daemon's compile kept it: its key and its bytes, which
/// `Module::deserialize` loads.
fn echo_code() -> (blaze_cas::Key, Vec<u8>) {
    let mut sim = Sim::new(9, Faults::default());
    let store = opened(&mut sim);
    let (_, put) = Library::load_with(&modules(), ECHO.as_bytes(), Some(&store.reader()), WHAT).unwrap();
    match put {
        Some(Put { keep: Keep::Machine(key), value, .. }) => (key, value),
        other => panic!("a compile with a store keeps its machine code: {other:?}"),
    }
}

/// The stream of `a` then `+`.
fn a_plus() -> Stream {
    let mut stream = Stream::default();
    let trees = stream.trees_mut();
    trees.push(Tree::Ident { sym: Symbol::intern("a"), kind: IdentKind::Normal, span: at(0, 1), spacing: Spacing::Alone });
    trees.push(Tree::Punct { ch: b'+', spacing: Spacing::Alone, span: at(1, 2) });
    stream
}

#[test]
fn a_module_that_imports_around_the_shims_is_refused_naming_its_macro_and_the_import() {
    let error = Library::load_with(&modules(), reaching_around().as_bytes(), None, WHAT).unwrap_err();
    assert_eq!(error, LoadError::Mismatch(Mismatch::Import { module: "env".to_owned(), name: "open".to_owned() }));
    let said = format!("{WHAT} {error}");
    assert!(said.contains("`demo_derive`"), "{said}");
    assert!(said.contains("`env::open`"), "{said}");
}

#[test]
fn a_module_without_the_abis_exports_is_refused_naming_the_export() {
    let bare = r#"(module (memory (export "memory") 1))"#;
    let error = Library::load_with(&modules(), bare.as_bytes(), None, WHAT).unwrap_err();
    assert_eq!(error, LoadError::Mismatch(Mismatch::Export { name: super::abi::ALLOC }));
}

/// A reactor's constructors run in `_initialize`: a module without it is refused before any
/// expansion, not run with them skipped.
#[test]
fn a_module_without_initialize_is_refused() {
    let uninitialized = ECHO.replacen("  (func (export \"_initialize\"))\n", "", 1);
    assert_ne!(uninitialized, ECHO);
    let error = Library::load_with(&modules(), uninitialized.as_bytes(), None, WHAT).unwrap_err();
    assert_eq!(error, LoadError::Mismatch(Mismatch::Export { name: super::abi::INITIALIZE }));
}

#[test]
fn bytes_that_are_no_module_are_refused_as_not_compiling() {
    let error = Library::load_with(&modules(), b"\0asm\x01\0\0\0\xff", None, WHAT).unwrap_err();
    assert!(matches!(error, LoadError::Compile(_)), "{error:?}");
}

#[test]
fn a_kept_modules_machine_code_loads_in_another_process_without_compiling() {
    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let first = modules();
    let reader = store.reader();
    let (_, put) = Library::load_with(&first, ECHO.as_bytes(), Some(&reader), WHAT).unwrap();
    assert_eq!(first.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    let put = put.expect("a module compiled with a store installed leaves its machine code to keep");
    assert!(put.what.contains("`demo_derive`"), "{}", put.what);
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    // Another process: nothing in its memo, the module's code in the store.
    let second = modules();
    let reader = store.reader();
    let (library, put) = Library::load_with(&second, ECHO.as_bytes(), Some(&reader), WHAT).unwrap();
    assert_eq!(second.loads(), Loads { memo: 0, store: 1, compiled: 0 }, "the kept machine code was compiled again");
    assert!(put.is_none(), "a module found in the store is kept again");

    // And what it loaded runs.
    let db = Db::new();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let ran = library.expand(&db, &Env::default(), 0, &given(&input), &mut spent).unwrap();
    let output = ran.output.unwrap();
    assert!(matches!(output.trees(), [Tree::Ident { .. }, Tree::Punct { ch: b'+', .. }]));
}

fn given(inputs: &[Stream]) -> Given<'_> {
    Given { inputs, sites: [at(0, 0), at(1, 1), at(2, 2)], edition: Edition::CURRENT, forced_keywords: false }
}

#[test]
fn an_expansion_in_a_module_gives_back_its_streams_tokens_and_reads_nothing() {
    let db = Db::new();
    let (library, _) = Library::load_with(&modules(), ECHO.as_bytes(), None, WHAT).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let first = library.expand(&db, &Env::default(), 0, &given(&input), &mut spent).unwrap();
    assert!(!spent.evaluated && spent.steps > 0 && spent.over.is_none(), "{spent:?}");
    assert!(first.seen.is_empty() && first.unkeyable.is_none() && !first.asked, "an echo read nothing");
    // A fresh instance each time: the second run is the first again.
    let second = library.expand(&db, &Env::default(), 0, &given(&input), &mut spent).unwrap();
    assert_eq!(first.output.unwrap().trees().len(), second.output.unwrap().trees().len());

    let missing = library.expand(&db, &Env::default(), 7, &given(&input), &mut spent).unwrap_err();
    assert!(missing.0.contains("no macro at index 7"), "{missing}");
}

/// A per-run variable such as `BLAZE_PROFILE`, which the macro never reads, differs between
/// two builds; the variable it reads does not.
fn env(a: &str, profile: &str) -> Env {
    [("A", a), ("BLAZE_PROFILE", profile)].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

#[test]
fn an_expansion_is_found_again_when_only_a_variable_it_never_read_differs() {
    let db = Db::new();
    let (library, _) = Library::load_with(&modules(), reading_a().as_bytes(), None, WHAT).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let first = env("1", "/tmp/blaze-lanes/profile-1855");
    let ran = library.expand(&db, &first, 0, &given(&input), &mut spent).unwrap();
    assert_eq!(ran.seen, [(Seen::Env, "A".to_owned())], "it read `A` by name, not the whole environment");

    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let key = RunKey { code: library.id(), given: blaze_ids::Hasher::new("a test's call of the macro").finish() };
    let traced = assemble_seen(ran.seen.iter().map(|(seen, what)| (*seen, what.as_str())), &mut Calling::new(&first)).unwrap();
    let put = Put { keep: Keep::Run { run: key, traced }, value: b"a +".to_vec(), what: WHAT.to_owned() };
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    let reader = store.reader();
    let guard = blaze_cas::enter();
    let found = |env: &Env| matches!(index::find(&reader, &guard, &key, &mut Calling::new(env)), Ok(Found::Hit { .. }));
    assert!(found(&env("1", "/tmp/blaze-lanes/profile-1910")), "a variable it never read made it miss");
    assert!(!found(&env("2", "/tmp/blaze-lanes/profile-1855")), "the variable it read changed and it was found");
}

#[test]
fn each_way_a_module_loads_is_counted_plotted_and_spanned_with_where_from_and_its_bytes() {
    use blaze_profile::{
        Plot,
        flight::{self, Read},
        span::{self, MacroModule, MacroModuleLoaded, ModuleFrom},
    };
    blaze_profile::install();
    // Its own crate name, so the spans of tests running beside it are told apart.
    let what = "the wasm module of the proc-macro crate `spanned_derive`";
    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);

    let first = modules();
    let reader = store.reader();
    let (_, put) = Library::load_with(&first, ECHO.as_bytes(), Some(&reader), what).unwrap();
    let put = put.expect("a compile with a store keeps its machine code");
    let bytes = put.value.len() as u64;
    let (_, again) = Library::load_with(&first, ECHO.as_bytes(), Some(&reader), what).unwrap();
    assert!(again.is_none(), "a module from the memo is kept again");
    assert_eq!(first.loads(), Loads { memo: 1, store: 0, compiled: 1 });
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    let second = modules();
    let reader = store.reader();
    Library::load_with(&second, ECHO.as_bytes(), Some(&reader), what).unwrap();
    assert_eq!(second.loads(), Loads { memo: 0, store: 1, compiled: 0 });

    let mut read = Read::default();
    flight::window(std::time::Duration::from_secs(600), &mut read);
    let mut ended: Vec<_> = read
        .spans
        .iter()
        .filter_map(|row| span::read::<MacroModule>(row).unwrap())
        .filter(|it| it.fields.module.text().as_deref() == Some(what))
        .map(|it| it.ended.expect("a load that returned left its span not ended"))
        .collect();
    ended.sort_by_key(|it| it.from.index());
    assert_eq!(
        ended,
        [
            MacroModuleLoaded { from: ModuleFrom::Memo, code_bytes: None },
            MacroModuleLoaded { from: ModuleFrom::Store, code_bytes: Some(bytes) },
            MacroModuleLoaded { from: ModuleFrom::Compiled, code_bytes: Some(bytes) },
        ],
    );
    // Only this test loads from the memo; the counts themselves are `Modules::loads`'s above.
    for plot in [Plot::MacroModuleMemo, Plot::MacroModuleStore, Plot::MacroModuleCompiled] {
        assert!(read.counters.iter().any(|it| it.plot == plot && it.value >= 1), "{plot:?} was not plotted");
    }
}

#[test]
fn machine_code_a_remote_sent_is_refused_by_where_it_came_from_and_never_deserialized() {
    use blaze_cas::{Fetched, Kind, Origin};
    // Real machine code of the module, which `Module::deserialize` would load: only where it
    // came from stands between it and the engine.
    let (key, code) = echo_code();

    // Sent as machine code, the store refuses to keep it, and the load compiles its own.
    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let refused = fetched(&mut store, key, Kind::Machine, &code);
    assert!(matches!(&refused, Fetched::Refused(why) if why.contains("machine code")), "{refused:?}");
    let daemon = modules();
    Library::load_with(&daemon, ECHO.as_bytes(), Some(&store.reader()), WHAT).unwrap();
    assert_eq!(daemon.loads(), Loads { memo: 0, store: 0, compiled: 1 });

    // Sent as another kind under the machine code's key, it is kept, and the load refuses it.
    let mut sim = Sim::new(6, Faults::default());
    let mut store = opened(&mut sim);
    let kept = fetched(&mut store, key, Kind::Run, &code);
    assert!(matches!(kept, Fetched::Kept(_)), "{kept:?}");
    let daemon = modules();
    let error = Library::load_with(&daemon, ECHO.as_bytes(), Some(&store.reader()), WHAT).unwrap_err();
    assert!(matches!(error, LoadError::NotLocal { origin: Origin::Own, kind: Kind::Run }), "{error:?}");
    assert_eq!(daemon.loads(), Loads::default(), "a refused value was deserialized or compiled around");
}

#[test]
fn machine_code_in_a_store_behind_the_daemons_own_is_refused_by_name() {
    use blaze_cas::{Kind, Origin};
    let (key, code) = echo_code();
    let mut sim = Sim::new(5, Faults::default());
    let mut base = opened_in(&mut sim, "base", &[]);
    let put = Put { keep: Keep::Machine(key), value: code, what: WHAT.to_owned() };
    let published = publish(&mut base, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    flushed(&mut sim, &mut base);
    drop(base);

    let mine = opened_in(&mut sim, "mine", &["base"]);
    let daemon = modules();
    let error = Library::load_with(&daemon, ECHO.as_bytes(), Some(&mine.reader()), WHAT).unwrap_err();
    let base = FileName::new("base").unwrap();
    assert!(matches!(&error, LoadError::NotLocal { origin: Origin::Behind(dir), kind: Kind::Machine } if *dir == base), "{error:?}");
    assert!(format!("{WHAT} {error}").contains("base"), "the refusal does not name the store it came from: {error}");
    assert_eq!(daemon.loads(), Loads::default());
}

/// A daemon's [`Keeps`](proc_macro_api::kept::Keeps): its expansions kept in `store`.
fn keeps_in(store: &Store) -> proc_macro_api::kept::Keeps {
    let expansions = proc_macro_api::kept::Expansions {
        store: store.reader(),
        toolchain: blaze_ids::Hasher::new("a test's toolchain").finish(),
        environment: proc_macro_api::kept::Environment::new(),
    };
    proc_macro_api::kept::Keeps::In(proc_macro_api::kept::Keeping::new(expansions).unwrap())
}

#[test]
fn two_daemons_in_one_process_each_load_machine_code_from_their_own_store() {
    // Its own crate name: the puts a load leaves are this thread's, and those of threads that
    // ended before theirs were taken.
    let what = "the wasm module of the proc-macro crate `two_daemons_derive`";
    let mine = |puts: Vec<Put>| puts.into_iter().filter(|it| it.what.contains("`two_daemons_derive`")).collect::<Vec<_>>();
    let mut sim = Sim::new(5, Faults::default());
    let mut one = opened_in(&mut sim, "one", &[]);
    let other = opened_in(&mut sim, "other", &[]);
    let (one_keeps, other_keeps) = (keeps_in(&one), keeps_in(&other));

    // The first daemon compiles the module and keeps its machine code in its own store.
    let daemon = modules();
    Library::load(&daemon, &one_keeps, ECHO.as_bytes(), what).unwrap();
    assert_eq!(daemon.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    let puts = mine(proc_macro_api::kept::take_puts());
    assert_eq!(puts.len(), 1, "a compile leaves its machine code to keep once: {puts:?}");
    let published = publish(&mut one, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    // Each daemon restarted, in the same process: the first finds it, the second never sees it.
    let again = modules();
    Library::load(&again, &one_keeps, ECHO.as_bytes(), what).unwrap();
    assert_eq!(again.loads(), Loads { memo: 0, store: 1, compiled: 0 });
    assert!(mine(proc_macro_api::kept::take_puts()).is_empty(), "a module found in the store is kept again");
    let others = modules();
    Library::load(&others, &other_keeps, ECHO.as_bytes(), what).unwrap();
    assert_eq!(others.loads(), Loads { memo: 0, store: 0, compiled: 1 }, "the second daemon loaded the first's machine code");
    assert_eq!(mine(proc_macro_api::kept::take_puts()).len(), 1);
    drop(other);

    // Keeping nothing, a load compiles and leaves nothing to keep.
    let alone = modules();
    Library::load(&alone, &proc_macro_api::kept::Keeps::Nothing("a test that keeps nothing"), ECHO.as_bytes(), what).unwrap();
    assert_eq!(alone.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    assert!(mine(proc_macro_api::kept::take_puts()).is_empty());
}
