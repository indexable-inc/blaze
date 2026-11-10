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
    expander::Engine,
    keep::Calling,
    run::Spent,
    stream::{IdentKind, Spacing, Stream, Tree},
};

const WHAT: &str = "the wasm module of the proc-macro crate `demo_derive`";

/// A module of the host's ABI whose one macro gives back the stream it was given: the fourth
/// handle of its input (after the three sites), as `Ok(Some(handle))`.
const ECHO: &str = r#"
(module
  (@custom "blaze.statics" "")
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

/// The binary of the module `text`: what blaze's link writes, whose `blaze.statics` section the
/// host reads.
fn binary(text: &str) -> Vec<u8> {
    wat::parse_str(text).unwrap()
}

/// [`ECHO`]'s binary.
fn echo() -> Vec<u8> {
    binary(ECHO)
}

/// [`ECHO`] with an import of `env::open`: a way to the system around the host's shims.
fn reaching_around() -> String {
    ECHO.replacen(
        "(memory",
        "(import \"env\" \"open\" (func (param i32 i32) (result i32)))\n  (memory",
        1,
    )
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

/// [`ECHO`] that first does what blaze's glue and wasi-libc do for
/// `std::fs::read_to_string(relative)`: asks the host its working directory (`current_dir`),
/// enters it as wasi-libc's `chdir` does (a look at it from `/`, the preopened directory 3), then
/// opens `relative` as wasi-libc hands it over, the directory's path without its leading `/`
/// and `relative` after it, and reads it. A status of 10 and up says which step failed.
fn reading_relative(relative: &str) -> String {
    let suffix = format!("/{relative}");
    ECHO.replacen(
        "(memory (export \"memory\") 1)",
        &format!(
            "(import \"blaze_proc_macro\" \"current_dir\" (func $current_dir (param i32 i32) (result i32)))\n  \
             (import \"wasi_snapshot_preview1\" \"path_filestat_get\" (func $look (param i32 i32 i32 i32 i32) (result i32)))\n  \
             (import \"wasi_snapshot_preview1\" \"path_open\" (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))\n  \
             (import \"wasi_snapshot_preview1\" \"fd_read\" (func $read (param i32 i32 i32 i32) (result i32)))\n  \
             (memory (export \"memory\") 1)\n  (data (i32.const 512) \"{suffix}\")"
        ),
        1,
    )
    .replacen(
        "(param $input i32) (param $len i32) (result i32)\n",
        &format!(
            "(param $input i32) (param $len i32) (result i32)\n    (local $n i32)\n    \
             (local.set $n (call $current_dir (i32.const 8192) (i32.const 4096)))\n    \
             (if (i32.lt_s (local.get $n) (i32.const 2)) (then (return (i32.const 10))))\n    \
             (if (call $look (i32.const 3) (i32.const 1) (i32.const 8193) (i32.sub (local.get $n) (i32.const 1)) (i32.const 16384))\n      \
             (then (return (i32.const 11))))\n    \
             (memory.copy (i32.add (i32.const 8192) (local.get $n)) (i32.const 512) (i32.const {len}))\n    \
             (if (call $open (i32.const 3) (i32.const 1) (i32.const 8193) (i32.add (i32.sub (local.get $n) (i32.const 1)) (i32.const {len})) \
             (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 16448))\n      \
             (then (return (i32.const 12))))\n    \
             (i32.store (i32.const 16500) (i32.const 20000))\n    \
             (i32.store (i32.const 16504) (i32.const 64))\n    \
             (if (call $read (i32.load (i32.const 16448)) (i32.const 16500) (i32.const 1) (i32.const 16520))\n      \
             (then (return (i32.const 13))))\n",
            len = suffix.len()
        ),
        1,
    )
}

/// A workspace whose member `pkg` holds `data/a.txt`, with `secret.txt` in its root outside
/// the member, in a fresh directory of its own: its real path.
struct Workspace(std::path::PathBuf);

impl Workspace {
    fn new(name: &str) -> Workspace {
        let top = std::env::temp_dir()
            .join(format!("blaze-macro-eval-wasm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("pkg/data")).unwrap();
        std::fs::write(top.join("pkg/data/a.txt"), "hello, macro").unwrap();
        std::fs::write(top.join("secret.txt"), "not yours").unwrap();
        Workspace(std::fs::canonicalize(&top).unwrap())
    }

    fn path(&self, rel: &str) -> String {
        let top = self.0.to_str().unwrap();
        if rel.is_empty() { top.to_owned() } else { format!("{top}/{rel}") }
    }

    /// The member's variables: its package and an `OUT_DIR` that is not there.
    fn env(&self) -> Env {
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", self.path("pkg"));
        env.set("OUT_DIR", self.path("pkg/target/out"));
        env
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs macro 0 of `library` on `input` in a fresh instance whose working directory is `cwd`
/// (the call's directory, [`crate::os::Os::wasm_in`]): what it gave, or why it stopped, and
/// what it read.
fn expand_in(
    library: &Library,
    env: Env,
    cwd: String,
) -> (Result<(), String>, Vec<(Seen, String)>) {
    use std::sync::Arc;
    let input = [a_plus()];
    let g = given(&input);
    let new_server = || crate::server::Server::new(g.sites, g.edition, g.forced_keywords);
    let mut server = new_server();
    let mut wire = Vec::new();
    server.input(g.inputs, &mut wire);
    let mut calls = vec![super::host::Call { index: 0, server, input: wire }];
    let placeholder = new_server();
    let pre = library.pre.clone();
    let statics = Arc::clone(&library.statics);
    let db = Db::new();
    super::thread::run(
        &db,
        move |link: &super::thread::Link<(Result<(), String>, Vec<(Seen, String)>)>| {
            let mut watch = super::host::Watch {
                statics,
                writes: Vec::new(),
                before: Vec::new(),
                after: Vec::new(),
                memory_len: 0,
                started: false,
            };
            let mut cx =
                super::host::Cx::new(link, placeholder, crate::os::Os::wasm_in(&env, &cwd));
            let (output, _) = super::host::expand(&pre, &mut cx, &mut calls, &mut watch);
            let seen = cx.os.seen().map(|(seen, what)| (seen, what.to_owned())).collect();
            let output = match output {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(panic)) => Err(format!("panicked: {panic}")),
                Err(stop) => Err(stop.0),
            };
            (output, seen)
        },
    )
    .unwrap()
}

/// A macro's relative path starts where cargo runs rustc for the calling crate, as wasi-libc
/// resolves it against the directory the glue entered: for a registry package, its own
/// directory, `CARGO_MANIFEST_DIR`.
#[test]
fn a_relative_path_a_module_reads_starts_in_the_calling_packages_directory() {
    let ws = Workspace::new("manifest");
    let (library, _) =
        Library::load_with(&modules(), &binary(&reading_relative("data/a.txt")), None, WHAT)
            .unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let ran = library
        .expand(&Db::new(), &ws.env(), &[(0, given(&input))], &[], &ws.path("pkg"), &mut spent)
        .unwrap();
    assert!(ran.output.is_ok(), "{:?}", ran.output.err());
    assert!(ran.seen.contains(&(Seen::Read, ws.path("pkg/data/a.txt"))), "{:?}", ran.seen);
}

/// For a workspace member, cargo runs rustc in the workspace's root, outside the member's
/// package: a relative path into the package is read from there, entering the root is no read
/// of it, and a file of the root outside the package stops the expansion naming it.
#[test]
fn a_member_modules_relative_path_starts_at_the_workspaces_root_and_only_the_roots_are_read() {
    let ws = Workspace::new("member");
    let (library, _) =
        Library::load_with(&modules(), &binary(&reading_relative("pkg/data/a.txt")), None, WHAT)
            .unwrap();
    let (output, seen) = expand_in(&library, ws.env(), ws.path(""));
    assert_eq!(output, Ok(()));
    assert!(seen.contains(&(Seen::Read, ws.path("pkg/data/a.txt"))), "{seen:?}");
    assert!(!seen.iter().any(|it| it.1 == ws.path("")), "entering the root was noted: {seen:?}");

    let (library, _) =
        Library::load_with(&modules(), &binary(&reading_relative("secret.txt")), None, WHAT)
            .unwrap();
    let (output, _) = expand_in(&library, ws.env(), ws.path(""));
    let stop = output.unwrap_err();
    assert!(stop.contains(&format!("(which is `{}`)", ws.path("secret.txt"))), "{stop}");
}

/// A module whose glue could not enter its working directory (status `UNENTERED` with
/// `ENOENT`, 44, above its low byte) stops naming the directory and why, never expanding.
#[test]
fn a_module_that_cannot_enter_its_directory_stops_naming_it_and_its_errno() {
    let ws = Workspace::new("unentered");
    let status = super::abi::UNENTERED | 44 << 8;
    let text = ECHO.replacen(
        "(if (i32.ne (local.get $index)",
        &format!("(return (i32.const {status}))\n    (if (i32.ne (local.get $index)"),
        1,
    );
    let (library, _) = Library::load_with(&modules(), &binary(&text), None, WHAT).unwrap();
    let (output, _) = expand_in(&library, ws.env(), ws.path("pkg"));
    let stop = output.unwrap_err();
    assert!(stop.contains(&format!("could not enter `{}`", ws.path("pkg"))), "{stop}");
    assert!(stop.contains("failed with ENOENT (WASI errno 44)"), "{stop}");
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
    }).expect("the effects end");
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
    }).expect("the effects end");
    assert!(store.ready(), "{:?}", store.broken());
    store
}

/// `store`'s puts made durable.
fn flushed(sim: &mut Sim, store: &mut Store) {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    }).expect("the effects end");
}

/// Keeps `puts` in `store` as a daemon's owner does: none is durable (no expansion of these
/// tests is verified across checkouts), so no daemon's queue or epoch is in it.
fn publish(store: &mut Store, puts: Vec<Put>) -> Published {
    proc_macro_api::kept::Keeps::Nothing("a test that publishes its own puts").publish(store, puts)
}

/// How `store` took `bytes` under `key`, sent by a remote as a value of `kind`.
fn fetched(
    store: &mut Store,
    key: blaze_cas::Key,
    kind: blaze_cas::Kind,
    bytes: &[u8],
) -> blaze_cas::Fetched {
    struct Far;
    impl blaze_cas::Remote for Far {
        fn fetch(
            &mut self,
            _: &blaze_cas::Key,
            _: blaze_core::Tag,
            _: &mut Vec<blaze_core::Effect>,
        ) -> blaze_cas::Asked {
            blaze_cas::Asked::Sent
        }
    }
    let mut effects = Vec::new();
    // What this checks is the store's own refusal of a kind (machine code): every value is let in.
    let tag = store
        .fetch(&key, &mut Far, |_, _| Ok(()), &mut effects)
        .expect("a store without the key asks for it");
    let answer = blaze_cas::remote::Answer::Value {
        key,
        kind,
        content: blaze_cas::content_of(bytes),
        bytes: bytes.to_vec(),
    };
    store.complete(
        blaze_core::Completion { tag, done: blaze_core::Done::Read(answer.encode_framed()) },
        &mut effects,
    );
    let mut all = store.fetched();
    assert_eq!(all.len(), 1, "{all:?}");
    all.pop().unwrap().1
}

/// [`ECHO`]'s machine code as a daemon's compile kept it: its key and its bytes, which
/// `Module::deserialize` loads.
fn echo_code() -> (blaze_cas::Key, Vec<u8>) {
    let mut sim = Sim::new(9, Faults::default());
    let store = opened(&mut sim);
    let (_, put) = Library::load_with(&modules(), &echo(), Some(&store.reader()), WHAT).unwrap();
    match put {
        Some(Put { keep: Keep::Machine(key), value, .. }) => (key, value),
        other => panic!("a compile with a store keeps its machine code: {other:?}"),
    }
}

/// The stream of `a` then `+`.
fn a_plus() -> Stream {
    let mut stream = Stream::default();
    let trees = stream.trees_mut();
    trees.push(Tree::Ident {
        sym: Symbol::intern("a"),
        kind: IdentKind::Normal,
        span: at(0, 1),
        spacing: Spacing::Alone,
    });
    trees.push(Tree::Punct { ch: b'+', spacing: Spacing::Alone, span: at(1, 2) });
    stream
}

#[test]
fn a_module_that_imports_around_the_shims_is_refused_naming_its_macro_and_the_import() {
    let error =
        Library::load_with(&modules(), reaching_around().as_bytes(), None, WHAT).unwrap_err();
    assert_eq!(
        error,
        LoadError::Mismatch(Mismatch::Import { module: "env".to_owned(), name: "open".to_owned() })
    );
    let said = format!("{WHAT} {error}");
    assert!(said.contains("`demo_derive`"), "{said}");
    assert!(said.contains("`env::open`"), "{said}");
}

/// The macro sandbox's WASI is the reads, stdio, clocks and exits it always was: these 24 calls
/// and no other. A module importing a p1 call that writes the file system, the 64-bit calls,
/// or any of the seed's own (`blaze_wasi::seed`, which only the seed runner links) is refused
/// before it runs.
#[test]
fn a_macro_is_served_only_the_sandboxs_wasi_and_never_a_call_that_writes() {
    let sandbox = [
        "args_get",
        "args_sizes_get",
        "clock_res_get",
        "clock_time_get",
        "environ_get",
        "environ_sizes_get",
        "fd_close",
        "fd_fdstat_get",
        "fd_fdstat_set_flags",
        "fd_filestat_get",
        "fd_prestat_dir_name",
        "fd_prestat_get",
        "fd_read",
        "fd_readdir",
        "fd_seek",
        "fd_tell",
        "fd_write",
        "path_filestat_get",
        "path_open",
        "path_readlink",
        "poll_oneoff",
        "proc_exit",
        "random_get",
        "sched_yield",
    ];
    assert_eq!(super::abi::WASI_SHIMS, &sandbox[..]);
    let writes = [
        "fd_allocate",
        "fd_datasync",
        "fd_filestat_set_size",
        "fd_filestat_set_times",
        "fd_pwrite",
        "fd_renumber",
        "fd_sync",
        "path_create_directory",
        "path_filestat_set_times",
        "path_link",
        "path_remove_directory",
        "path_rename",
        "path_symlink",
        "path_unlink_file",
    ];
    let refused = writes
        .iter()
        .map(|name| ("wasi_snapshot_preview1", *name))
        .chain([("wasi_snapshot_preview1_64", "fd_write")])
        .chain(blaze_wasi::seed::FUNCTIONS.iter().map(|name| (blaze_wasi::seed::MODULE, *name)));
    for (module, name) in refused {
        let import = format!("(import \"{module}\" \"{name}\" (func))\n  (memory");
        let error = Library::load_with(
            &modules(),
            ECHO.replacen("(memory", &import, 1).as_bytes(),
            None,
            WHAT,
        )
        .unwrap_err();
        assert_eq!(
            error,
            LoadError::Mismatch(Mismatch::Import {
                module: module.to_owned(),
                name: name.to_owned()
            })
        );
    }
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

/// C1 of the fekey review: what a crate's macros run in a module is of the module too, so the
/// key of a crate that calls them ([`crate::expander`]'s `ProcMacroExpander::code`) moves when
/// the code generator makes another module of the same source, which the front end's code
/// identity does not cover.
#[test]
fn another_module_of_the_same_crate_runs_other_code() {
    let modules = modules();
    let code = blaze_ids::ContentId([9; 32]);
    let (one, _) = Library::load_with(&modules, &echo(), None, WHAT).unwrap();
    let (again, _) = Library::load_with(&modules, &echo(), None, WHAT).unwrap();
    let rebuilt = binary(&ECHO.replacen(
        "(func (export \"_initialize\"))",
        "(func (export \"_initialize\") nop)",
        1,
    ));
    let (other, _) = Library::load_with(&modules, &rebuilt, None, WHAT).unwrap();
    assert_ne!(one.id(), other.id(), "the rebuilt module is other bytes");
    let (one, again, other) = (Engine::Wasm(one), Engine::Wasm(again), Engine::Wasm(other));
    assert_eq!(one.runs(code), again.runs(code), "one module of one crate runs one code");
    assert_ne!(
        one.runs(code),
        other.runs(code),
        "another module of the same crate runs other code"
    );
    assert_ne!(one.runs(code), code, "a module's code is not the crate's alone");
    assert_ne!(one.runs(code), one.runs(blaze_ids::ContentId([10; 32])), "nor the module's alone");
}

#[test]
fn a_kept_modules_machine_code_loads_in_another_process_without_compiling() {
    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let first = modules();
    let reader = store.reader();
    let (_, put) = Library::load_with(&first, &echo(), Some(&reader), WHAT).unwrap();
    assert_eq!(first.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    let put =
        put.expect("a module compiled with a store installed leaves its machine code to keep");
    assert!(put.what.contains("`demo_derive`"), "{}", put.what);
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    // Another process: nothing in its memo, the module's code in the store.
    let second = modules();
    let reader = store.reader();
    let (library, put) = Library::load_with(&second, &echo(), Some(&reader), WHAT).unwrap();
    assert_eq!(
        second.loads(),
        Loads { memo: 0, store: 1, compiled: 0 },
        "the kept machine code was compiled again"
    );
    assert!(put.is_none(), "a module found in the store is kept again");

    // And what it loaded runs.
    let db = Db::new();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let ran =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    let output = ran.output.unwrap();
    assert!(matches!(output.trees(), [Tree::Ident { .. }, Tree::Punct { ch: b'+', .. }]));
}

fn given(inputs: &[Stream]) -> Given<'_> {
    Given {
        inputs,
        sites: [at(0, 0), at(1, 1), at(2, 2)],
        edition: Edition::CURRENT,
        forced_keywords: false,
    }
}

#[test]
fn an_expansion_in_a_module_gives_back_its_streams_tokens_and_reads_nothing() {
    let db = Db::new();
    let (library, _) = Library::load_with(&modules(), &echo(), None, WHAT).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let first =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert!(!spent.evaluated && spent.steps > 0 && spent.over.is_none(), "{spent:?}");
    assert!(
        first.seen.is_empty() && first.unkeyable.is_none() && !first.asked,
        "an echo read nothing"
    );
    // A fresh instance each time: the second run is the first again.
    let second =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert_eq!(first.output.unwrap().trees().len(), second.output.unwrap().trees().len());

    let missing = library
        .expand(&db, &Env::default(), &[(7, given(&input))], &[], "/", &mut spent)
        .unwrap_err();
    assert!(missing.0.contains("no macro at index 7"), "{missing}");
}

/// A per-run variable such as `BLAZE_PROFILE`, which the macro never reads, differs between
/// two builds; the variable it reads does not.
fn env(a: &str, profile: &str) -> Env {
    [("A", a), ("BLAZE_PROFILE", profile)]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn an_expansion_is_found_again_when_only_a_variable_it_never_read_differs() {
    let db = Db::new();
    let (library, _) = Library::load_with(&modules(), &binary(&reading_a()), None, WHAT).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let first = env("1", "/tmp/blaze-lanes/profile-1855");
    let ran = library.expand(&db, &first, &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert_eq!(
        ran.seen,
        [(Seen::Env, "A".to_owned())],
        "it read `A` by name, not the whole environment"
    );

    let mut sim = Sim::new(5, Faults::default());
    let mut store = opened(&mut sim);
    let key = RunKey {
        code: library.id(),
        given: blaze_ids::Hasher::new("a test's call of the macro").finish(),
    };
    let traced = assemble_seen(
        ran.seen.iter().map(|(seen, what)| (*seen, what.as_str())),
        &mut Calling::new(&first),
    )
    .unwrap();
    let put =
        Put { keep: Keep::Run { run: key, traced }, value: b"a +".to_vec(), what: WHAT.to_owned() };
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    let reader = store.reader();
    let guard = blaze_cas::enter();
    let found = |env: &Env| {
        matches!(index::find(&reader, &guard, &key, &mut Calling::new(env)), Ok(Found::Hit { .. }))
    };
    assert!(
        found(&env("1", "/tmp/blaze-lanes/profile-1910")),
        "a variable it never read made it miss"
    );
    assert!(
        !found(&env("2", "/tmp/blaze-lanes/profile-1855")),
        "the variable it read changed and it was found"
    );
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
    let (_, put) = Library::load_with(&first, &echo(), Some(&reader), what).unwrap();
    let put = put.expect("a compile with a store keeps its machine code");
    let bytes = put.value.len() as u64;
    let (_, again) = Library::load_with(&first, &echo(), Some(&reader), what).unwrap();
    assert!(again.is_none(), "a module from the memo is kept again");
    assert_eq!(first.loads(), Loads { memo: 1, store: 0, compiled: 1 });
    let published = publish(&mut store, vec![put]);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    let second = modules();
    let reader = store.reader();
    Library::load_with(&second, &echo(), Some(&reader), what).unwrap();
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
        assert!(
            read.counters.iter().any(|it| it.plot == plot && it.value >= 1),
            "{plot:?} was not plotted"
        );
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
    assert!(
        matches!(&refused, Fetched::Refused(why) if why.contains("machine code")),
        "{refused:?}"
    );
    let daemon = modules();
    Library::load_with(&daemon, &echo(), Some(&store.reader()), WHAT).unwrap();
    assert_eq!(daemon.loads(), Loads { memo: 0, store: 0, compiled: 1 });

    // Sent as another kind under the machine code's key, it is kept, and the load refuses it.
    let mut sim = Sim::new(6, Faults::default());
    let mut store = opened(&mut sim);
    let kept = fetched(&mut store, key, Kind::Run, &code);
    assert!(matches!(kept, Fetched::Kept(_)), "{kept:?}");
    let daemon = modules();
    let error = Library::load_with(&daemon, &echo(), Some(&store.reader()), WHAT).unwrap_err();
    assert!(
        matches!(error, LoadError::NotLocal { origin: Origin::Own, kind: Kind::Run }),
        "{error:?}"
    );
    assert_eq!(
        daemon.loads(),
        Loads::default(),
        "a refused value was deserialized or compiled around"
    );
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
    let error = Library::load_with(&daemon, &echo(), Some(&mine.reader()), WHAT).unwrap_err();
    let base = FileName::new("base").unwrap();
    assert!(
        matches!(&error, LoadError::NotLocal { origin: Origin::Behind(dir), kind: Kind::Machine } if *dir == base),
        "{error:?}"
    );
    assert!(
        format!("{WHAT} {error}").contains("base"),
        "the refusal does not name the store it came from: {error}"
    );
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
    let mine = |puts: Vec<Put>| {
        puts.into_iter().filter(|it| it.what.contains("`two_daemons_derive`")).collect::<Vec<_>>()
    };
    let mut sim = Sim::new(5, Faults::default());
    let mut one = opened_in(&mut sim, "one", &[]);
    let other = opened_in(&mut sim, "other", &[]);
    let (one_keeps, other_keeps) = (keeps_in(&one), keeps_in(&other));

    // The first daemon compiles the module and keeps its machine code in its own store.
    let daemon = modules();
    Library::load(&daemon, &one_keeps, &echo(), what).unwrap();
    assert_eq!(daemon.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    let puts = mine(proc_macro_api::kept::take_puts());
    assert_eq!(puts.len(), 1, "a compile leaves its machine code to keep once: {puts:?}");
    let published = publish(&mut one, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);

    // Each daemon restarted, in the same process: the first finds it, the second never sees it.
    let again = modules();
    Library::load(&again, &one_keeps, &echo(), what).unwrap();
    assert_eq!(again.loads(), Loads { memo: 0, store: 1, compiled: 0 });
    assert!(
        mine(proc_macro_api::kept::take_puts()).is_empty(),
        "a module found in the store is kept again"
    );
    let others = modules();
    Library::load(&others, &other_keeps, &echo(), what).unwrap();
    assert_eq!(
        others.loads(),
        Loads { memo: 0, store: 0, compiled: 1 },
        "the second daemon loaded the first's machine code"
    );
    assert_eq!(mine(proc_macro_api::kept::take_puts()).len(), 1);
    drop(other);

    // Keeping nothing, a load compiles and leaves nothing to keep.
    let alone = modules();
    Library::load(
        &alone,
        &proc_macro_api::kept::Keeps::Nothing("a test that keeps nothing"),
        &echo(),
        what,
    )
    .unwrap();
    assert_eq!(alone.loads(), Loads { memo: 0, store: 0, compiled: 1 });
    assert!(mine(proc_macro_api::kept::take_puts()).is_empty());
}

/// A module whose list of statics is missing is refused at load, saying so: its macros' state
/// between calls can't be seen, and none runs as if it had none.
#[test]
fn a_module_without_its_statics_list_is_refused() {
    let bare = ECHO.replacen("  (@custom \"blaze.statics\" \"\")\n", "", 1);
    assert_ne!(bare, ECHO);
    let error = Library::load_with(&modules(), &binary(&bare), None, WHAT).unwrap_err();
    assert!(
        matches!(&error, LoadError::Statics(why) if why.contains("has no `blaze.statics` section")),
        "{error}"
    );
}

/// [`ECHO`] whose macro first adds one to the `u32` at 64, which its `blaze.statics` list says
/// is the static whose item key is `item`; with `heap`, it stores an address of its heap there
/// instead (the heap starts at 4096).
fn counting(item: &[u8; 32], heap: bool) -> String {
    let entry = super::statics::encode::entry(4096, item, &[(64, 4)]);
    let bump = match heap {
        false => "(i32.store (i32.const 64) (i32.add (i32.load (i32.const 64)) (i32.const 1)))",
        true => "(i32.store (i32.const 64) (i32.const 5000))",
    };
    ECHO.replacen(
        "(@custom \"blaze.statics\" \"\")",
        &format!("(@custom \"blaze.statics\" \"{}\")", super::statics::encode::wat_string(&entry)),
        1,
    )
    .replacen(
        "(i32.store8 (i32.const 0) (i32.const 0))",
        &format!("{bump}\n    (i32.store8 (i32.const 0) (i32.const 0))"),
        1,
    )
}

/// A crate with `pub static mut COUNT: u32`, its static, and the static's item key.
fn counter_crate() -> (Db, base_db::Crate, hir_def::StaticId, [u8; 32]) {
    use hir_def::{ModuleDefId, test_items};
    let db = hir_expand::fixture::db("pub static mut COUNT: u32 = 0;").into_db();
    let krate = hir_expand::fixture::test_crate(&db);
    let id = blaze_query::db::attach(&db, || {
        test_items::declarations(&db, krate)
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::StaticId(id) => Some(id),
                _ => None,
            })
            .expect("the fixture's static")
    });
    let item = blaze_items::item_key(&db, blaze_items::ItemId::Static(id)).0.0;
    (db, krate, id, item)
}

/// A static a macro's run changes is what it left, and the next call's fresh instance is given
/// it: the counter goes 0, 1, 2 across three instances, as it does in one process under rustc.
#[test]
fn a_static_a_run_changes_is_carried_into_the_next_calls_instance() {
    let (db, krate, id, item) = counter_crate();
    let (mut library, _) =
        Library::load_with(&modules(), &binary(&counting(&item, false)), None, WHAT).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    // Unresolved, it runs nothing.
    let error = library
        .expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent)
        .unwrap_err();
    assert!(error.0.contains("not resolved"), "{error}");

    library.watch(&db, krate).unwrap();
    let first =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert_eq!(first.statics, [(id, vec![1, 0, 0, 0], false)]);
    let preset = [(id, first.statics[0].1.clone())];
    let second = library
        .expand(&db, &Env::default(), &[(0, given(&input))], &preset, "/", &mut spent)
        .unwrap();
    assert_eq!(
        second.statics,
        [(id, vec![2, 0, 0, 0], false)],
        "the second instance started from the first's 1"
    );

    // A preset of another size is refused, naming nothing it can't.
    let error = library
        .expand(&db, &Env::default(), &[(0, given(&input))], &[(id, vec![0; 3])], "/", &mut spent)
        .unwrap_err();
    assert!(error.0.contains("3 bytes of state"), "{error}");
}

/// [`counting`] whose macro keeps a pointer into its heap in the static: the first call stores
/// the heap's 5000 there, each later call in the same instance moves it on by 4. A fresh
/// instance has 0 there whatever calls ran before in others.
fn pointing(item: &[u8; 32]) -> String {
    let entry = super::statics::encode::entry(4096, item, &[(64, 4)]);
    ECHO.replacen(
        "(@custom \"blaze.statics\" \"\")",
        &format!("(@custom \"blaze.statics\" \"{}\")", super::statics::encode::wat_string(&entry)),
        1,
    )
    .replacen(
        "(i32.store8 (i32.const 0) (i32.const 0))",
        "(i32.store (i32.const 64) (select (i32.const 5000) (i32.add (i32.load (i32.const 64)) \
         (i32.const 4)) (i32.eqz (i32.load (i32.const 64)))))\n    \
         (i32.store8 (i32.const 0) (i32.const 0))",
        1,
    )
}

/// A static left holding an address of the heap is said to hold one, with its bytes, as the
/// evaluator says it, and the expansion's output is still given.
#[test]
fn a_static_left_holding_a_heap_address_is_said_to_hold_one() {
    let (db, krate, id, item) = counter_crate();
    let (mut library, _) =
        Library::load_with(&modules(), &binary(&counting(&item, true)), None, WHAT).unwrap();
    library.watch(&db, krate).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let ran =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert!(ran.output.is_ok(), "{:?}", ran.output);
    assert_eq!(ran.statics, [(id, 5000u32.to_le_bytes().to_vec(), true)]);
}

/// The calls of a run run in order in one instance, each from what the one before left on its
/// heap, as the session's calls run in one process under rustc: three calls leave the pointer
/// moved on twice, where one call alone leaves it where a fresh instance starts it.
#[test]
fn a_runs_calls_run_in_order_in_one_instance_from_the_heap_the_ones_before_left() {
    let (db, krate, id, item) = counter_crate();
    let (mut library, _) =
        Library::load_with(&modules(), &binary(&pointing(&item)), None, WHAT).unwrap();
    library.watch(&db, krate).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let one =
        library.expand(&db, &Env::default(), &[(0, given(&input))], &[], "/", &mut spent).unwrap();
    assert_eq!(one.statics, [(id, 5000u32.to_le_bytes().to_vec(), true)]);
    let alone = spent.steps;
    let calls = [(0, given(&input)), (0, given(&input)), (0, given(&input))];
    let three = library.expand(&db, &Env::default(), &calls, &[], "/", &mut spent).unwrap();
    assert!(three.output.is_ok(), "{:?}", three.output);
    assert_eq!(
        three.statics,
        [(id, 5008u32.to_le_bytes().to_vec(), true)],
        "the second and third calls ran from the first's heap, in one instance"
    );
    assert!(spent.steps > alone, "the fuel of all three calls: {} after {alone}", spent.steps);
}

/// A call run again before the one expanded that stops stops the run, naming which of the
/// earlier calls it was and why: here one asking for a macro the module has not.
#[test]
fn an_earlier_call_that_stops_stops_the_run_naming_it() {
    let (db, krate, _, item) = counter_crate();
    let (mut library, _) =
        Library::load_with(&modules(), &binary(&pointing(&item)), None, WHAT).unwrap();
    library.watch(&db, krate).unwrap();
    let input = [a_plus()];
    let mut spent = Spent::default();
    let calls = [(0, given(&input)), (7, given(&input)), (0, given(&input))];
    let error = library.expand(&db, &Env::default(), &calls, &[], "/", &mut spent).unwrap_err();
    assert!(
        error.0.contains("call 2 of the 2 its session made before this one")
            && error.0.contains("no macro at index 7"),
        "{error}"
    );
}

/// A list naming a key no crate of the macro's has is refused at load.
#[test]
fn a_listed_static_of_no_crate_is_refused() {
    let (db, krate, _, _) = counter_crate();
    let (mut library, _) =
        Library::load_with(&modules(), &binary(&counting(&[7; 32], false)), None, WHAT).unwrap();
    let error = library.watch(&db, krate).unwrap_err();
    assert!(matches!(&error, LoadError::Statics(why) if why.contains("of no crate")), "{error}");
}
