//! The lint tables: rustc's (`rustc.rs`, generated from its source at the pinned commit by
//! scripts/src/bin/lint_table.rs), and blaze's own lints, written here.
//!
//! These are `const`s so that the registry numbers and indexes them as blaze is built; the
//! lints with their ids are `index::ALL`'s, which is where every `&'static Lint` points.

pub(super) use super::rustc::{BUILTIN, INTERNAL, UNREGISTERED};
use super::{
    Level::{Allow, Deny, Warn},
    Lint, ready,
};

const fn l(name: &'static str, level: super::Level, desc: &'static str) -> Lint {
    Lint::new(name, level, desc)
}

/// The lints of the tool `blaze`, blaze's own, always registered: rustc has none of them. The
/// front end's (docs/fn-cache.md), then blaze-lints' in the order of its `blaze::Lint::ALL`,
/// which its tests hold to this table; those run under `blaze check` and are allowed unless a
/// level is set (`[workspace.metadata.blaze.lints]`).
pub(super) const BLAZE: &[Lint] = &[
    l(
        "blaze::const_impure",
        Warn,
        "detects a `#[blaze::cache]` function whose result depends on more than its arguments",
    ),
    // The cross-configuration reachability lints: one check over every configuration of a
    // package (its lib, bins, tests, ...), docs/v2/plan/design/multiconfig-check.md.
    l("blaze::dead", Warn, "detects an item no configuration of its package that was checked uses (rustc's `dead_code`, over every configuration at once)"),
    l("blaze::test_only", Warn, "detects an item only the tests use, which `#[cfg(test)]` would leave out of every other build"),
    l("blaze::cfg_only", Warn, "detects an item only some configurations use (a feature, a platform, a target), which a `#[cfg]` naming them would leave out of the rest"),
    l("blaze::hot_clone", Allow, "detects a clone in hot code that costs more than a copy")
        .externally_loaded(),
    l("blaze::hot_alloc", Allow, "detects a call in hot code that reaches the global allocator")
        .externally_loaded(),
    l("blaze::hot_dyn", Allow, "detects a call in hot code that goes through a vtable")
        .externally_loaded(),
    l("blaze::default_hasher", Allow, "detects a hash map or set whose hasher is left to its default or seeded at random")
        .externally_loaded(),
    l("blaze::parallelizable", Allow, "detects a loop whose iterations are independent and could run in parallel")
        .externally_loaded(),
    l("blaze::serial_dependence", Allow, "detects a loop kept serial by exactly one dependence")
        .externally_loaded(),
    l("blaze::fiber_boundary", Allow, "detects code that reaches past `blaze_graph::sched` into fibers")
        .externally_loaded(),
    l("blaze::hot_alloc_call", Allow, "detects a call in hot code that allocates through the workspace's code")
        .externally_loaded(),
    l("blaze::error_debug", Allow, "detects an error's message that can panic or names no item")
        .externally_loaded(),
    l("blaze::wall_clock", Allow, "detects a read of the wall clock outside the injected clock")
        .externally_loaded(),
    l("blaze::untyped_error", Allow, "detects a function returning an error that is not an enum of what fails")
        .externally_loaded(),
    l("blaze::dropped_error", Allow, "detects an error thrown away for a value")
        .externally_loaded(),
    l("blaze::hash_order_output", Allow, "detects output that depends on hash-map iteration order")
        .externally_loaded(),
    l("blaze::held_across_wait", Allow, "detects a thread-tied value held across a wait")
        .externally_loaded(),
    l("blaze::lock_across_query", Allow, "detects a lock guard held across a query call")
        .externally_loaded(),
    l("blaze::blocking_on_pool", Allow, "detects a blocking call on a pool thread or fiber")
        .externally_loaded(),
    l("blaze::unnamed_wait", Allow, "detects a wait with no span naming what it waits for")
        .externally_loaded(),
    l("blaze::shared_collection_lock", Allow, "detects a locked map or `Vec` shared between parallel items")
        .externally_loaded(),
    l("blaze::open_wait_loop", Allow, "detects a loop that waits for progress with no bound")
        .externally_loaded(),
    l("blaze::unrecorded_span", Allow, "detects a `tracing` span whose target the flight recorder drops")
        .externally_loaded(),
    l(
        "blaze::implicit_allocator",
        Allow,
        "detects a type that leaves its allocator to the default `Global`, or a global-heap type that takes none",
    )
    .externally_loaded(),
    l("blaze::fixed_key_lookup", Allow, "detects a lookup of keys known at compile time made at run time instead of by a perfect hash")
        .externally_loaded(),
    l("blaze::struct_update", Allow, "detects a struct literal of the workspace that takes fields from a base instead of writing each one")
        .externally_loaded(),
    l("blaze::bool_param", Allow, "detects a `bool` parameter of a workspace function where a two-variant enum would name the choice")
        .externally_loaded(),
    l("blaze::many_bools", Allow, "detects a call passing two or more bare `bool` literals by position, where a record parameter would name them")
        .externally_loaded(),
    l("blaze::default_policy", Allow, "detects `impl Default`, `#[derive(Default)]` or a `new()` that takes nothing on a type with a field marked `#[blaze::policy]`, a cost or a guarantee its maker decides")
        .externally_loaded(),
    l("blaze::sans_io_core", Allow, "detects a function of a `#![blaze::sans_io]` core that reaches the wall clock or IO")
        .externally_loaded(),
    l("blaze::key_ignores_field", Allow, "detects a hand-written key (`Hash`, `PartialEq`, `Ord` or a key builder) that leaves a field out unnamed")
        .externally_loaded(),
    l("blaze::wildcard_arm", Allow, "detects a catch-all `match` arm over an enum of the workspace, where listing the variants makes a new one a compile error")
        .externally_loaded(),
    l("blaze::false_sharing", Warn, "detects a struct shared between threads whose atomics or locks, written by different methods, can share one cache line")
        .externally_loaded(),
    l("blaze::seqcst_unexplained", Warn, "detects `Ordering::SeqCst` with no `// seqcst: <why>` saying why the operation needs one total order")
        .externally_loaded(),
    l("blaze::large_move", Allow, "detects a value over 512 bytes moved by value in hot code, and an enum whose largest variant is over 512 bytes and 4x its second")
        .externally_loaded(),
    l("blaze::collect_then_consume", Warn, "detects a `collect` into a collection that is only iterated, asked its length or emptiness, or passed on as an iterator")
        .externally_loaded(),
    l("blaze::hash_twice", Warn, "detects a key looked up in a hash map or set and then inserted, removed or entered on the same map, hashing it twice")
        .externally_loaded(),
    l("blaze::syscall_in_loop", Allow, "detects a stat or open of a path once per element of a loop in a crate whose file system work is batched")
        .externally_loaded(),
    l("blaze::query_in_loop", Allow, "detects a query called once per element of a loop when a query beside it takes the whole batch")
        .externally_loaded(),
    l("blaze::string_key", Allow, "detects a hash map or set keyed by a string whose keys are all a `Name`'s or `Symbol`'s text, where `intern::Symbol` would do")
        .externally_loaded(),
    l("blaze::outline_generic_body", Warn, "detects a generic function whose type parameters only feed a conversion at entry, so every instantiation copies a large non-generic body")
        .externally_loaded(),
    l("blaze::cold_error", Warn, "detects a `Result` whose error is over 128 bytes (`large-error-threshold`), and in hot code an error built inline with a `String` or a large struct outside a `#[cold]` function")
        .externally_loaded(),
    l("blaze::frozen_collection", Warn, "detects a non-public `Vec` or `String` field nothing in the crate changes after the struct is built, where a boxed slice or `Box<str>` would do")
        .externally_loaded(),
    l("blaze::intern_candidate", Warn, "detects a recursive or deep `Hash + Eq + Clone` type used as a map key and compared or cloned often, where an interned `Copy` id would make `==` and hashing one integer")
        .externally_loaded(),
    l("blaze::small_map", Warn, "detects a hash map or set, or a `BTreeMap`, that provably holds a few entries, where a sorted array or a `match` would do")
        .externally_loaded(),
    l("blaze::arc_clone_only_borrowed", Warn, "detects an `Arc` cloned (or an `ArcSwap` loaded in full) where the clone is only borrowed, paying two atomic updates of a shared count for nothing")
        .externally_loaded(),
    l("blaze::blocking_in_async", Warn, "detects a blocking call (file system, sleep, socket, channel receive, child process, thread join) inside an async fn, block or closure, which stalls every task on the executor thread")
        .externally_loaded(),
    l("blaze::relaxed_publish", Warn, "detects an atomic flag or pointer stored with `Ordering::Relaxed` in one function and loaded in another to guard other data, which `Relaxed` doesn't order")
        .externally_loaded(),
    l("blaze::unwind_across_ffi", Warn, "detects a panic (an `unwrap`, `expect`, `panic!`, index or division) in a function with a foreign ABI that can't unwind, where a panic aborts the process")
        .externally_loaded(),
    l("blaze::unbounded_from_input", Warn, "detects a loop that reads input and grows a collection or an unbounded channel every round with no cap on its size in the loop")
        .externally_loaded(),
    l("blaze::dos_hasher_on_input", Warn, "detects a hash map or set with a fixed-seed hasher keyed by text a caller of a public library function hands in, open to collision attacks (HashDoS)")
        .externally_loaded(),
    l("blaze::panic_in_drop", Warn, "detects a `Drop::drop` that can panic, which aborts the process when it runs during unwinding")
        .externally_loaded(),
    l("blaze::bounds_check_in_loop", Warn, "detects a slice or `Vec` indexed by a range loop's variable where nothing before the loop proves the range's end is within its length, so the bounds check runs every iteration")
        .externally_loaded(),
    l("blaze::unbuffered_io", Warn, "detects a `File`, socket or pipe written several times, or inside a loop, or read in small pieces, with no `BufWriter` or `BufReader`, so each call is a system call")
        .externally_loaded(),
    l("blaze::stdout_lock_in_loop", Warn, "detects `println!`, `print!` or a write to `stdout()` or `stderr()` inside a loop, which locks the stream once per call")
        .externally_loaded(),
    l("blaze::front_vec_ops", Warn, "detects `Vec::remove(0)`, `Vec::insert(0, ..)` or `drain(..1)` inside a loop, which shifts every element each time")
        .externally_loaded(),
    l("blaze::missing_size_hint", Warn, "detects an `Iterator` impl that steps a counter over what it holds with no `size_hint`, or an exact `size_hint` with no `ExactSizeIterator`")
        .externally_loaded(),
    l("blaze::fmt_allocates", Warn, "detects a `String` built inside a `fmt` method only to be written to the formatter")
        .externally_loaded(),
    l("blaze::jagged_vec", Warn, "detects a `Vec<Vec<T>>`, `Vec<String>` or `Vec<Box<[T]>>` field nothing changes after the struct is built, one heap allocation per row")
        .externally_loaded(),
    l("blaze::stable_sort_needless", Warn, "detects a stable sort of elements whose equal values can't be told apart, where `sort_unstable` gives the same order without the buffer")
        .externally_loaded(),
    l("blaze::dyn_in_hot_field", Warn, "detects a `Box<dyn Trait>` or `&dyn Trait` field the struct's methods call inside a loop, an indirect call that can't be inlined")
        .externally_loaded(),
    l("blaze::profile_hot", Warn, "gives the perf lints of a function the configured profile shows hot (`profile` in `clippy.toml`) this level where nothing else sets theirs")
        .externally_loaded(),
    l("blaze::stale_hot_marker", Warn, "detects a function marked hot (`#[deny(blaze::hot)]`) that the configured profile counts as cold or never run")
        .externally_loaded(),
    l("blaze::hot_marked_cold", Warn, "detects a `#[cold]` function that the configured profile shows hot")
        .externally_loaded(),
    l("blaze::push_without_reserve", Warn, "detects an empty `Vec` or `String` pushed to once per iteration of a loop whose count is known before it starts, which `with_capacity` would allocate once")
        .externally_loaded(),
    l("blaze::scratch_in_loop", Warn, "detects a `Vec`, `String` or hash map made, grown and dropped in every iteration of a loop, which made once and cleared would keep its capacity")
        .externally_loaded(),
    l("blaze::boxed_recursive_node", Warn, "detects a crate-private type that links to itself through `Box` or `Rc<RefCell<..>>`, one allocation per node where an arena holds them in one")
        .externally_loaded(),
    l("blaze::wide_index", Warn, "detects a `usize` or `u64` field only ever used to index one container, which a `u32` would hold in half the room")
        .externally_loaded(),
    l("blaze::bounded_heap_field", Warn, "detects a `String` or `Vec` field every construction fills with a small, known bound and nothing grows, which could be held inline")
        .externally_loaded(),
    l("blaze::enum_vec_padded", Warn, "detects a `Vec` of an enum whose largest variant is several times its smallest, only pushed and walked, where every element pays the largest variant's room")
        .externally_loaded(),
    l("blaze::deep_drop_at_exit", Allow, "detects a collection of heap-owning elements alive when `main` returns, freed one element at a time just before the process exits")
        .externally_loaded(),
    l("blaze::dyn_fn_param", Warn, "detects a `&dyn Fn`, `&mut dyn FnMut` or `Box<dyn FnOnce>` parameter called inside a loop, an indirect call that can't be inlined")
        .externally_loaded(),
    l("blaze::box_large_new", Warn, "detects `Box::new` of a value over `large-box-threshold` bytes, built on the stack and then copied into the heap")
        .externally_loaded(),
    l("blaze::pod_serialized_by_field", Allow, "detects a `#[repr(C)]` struct of plain numbers with no padding that goes through serde field by field")
        .externally_loaded(),
    l("blaze::checkfree", Deny, "detects a bounds check, overflow check or panic left in the optimized code of a `#[blaze::checkfree]` function")
        .externally_loaded(),
    l("blaze::nopanic", Deny, "detects a `#[blaze::nopanic]` function that may panic or abort, through any call")
        .externally_loaded(),
    l("blaze::noalloc", Deny, "detects a `#[blaze::noalloc]` function that may call the global allocator, through any call")
        .externally_loaded(),
    l("blaze::nonblocking", Deny, "detects a `#[blaze::nonblocking]` function that may block the thread, through any call")
        .externally_loaded(),
    l("blaze::pure", Deny, "detects a `#[blaze::pure]` function that may read or change the world outside its arguments (the environment, the clock, files, threads, state that changes, an address, randomness, foreign code), through any call")
        .externally_loaded(),
    l("blaze::capabilities", Deny, "detects a function of a `#![blaze::capabilities]` crate that may reach ambient IO, run `unsafe` code or call the global allocator past the crates its workspace trusts, through any call")
        .externally_loaded(),
    l("blaze::packable", Allow, "detects a struct held in a collection whose `bool` and fieldless enum fields need far fewer bits than they take, which `#[blaze::bits]` would pack")
        .externally_loaded(),
    l("blaze::soa_candidate", Allow, "detects a struct held in a `Vec`, slice or array whose loops read or write only a small part of each element, which a struct of arrays would store as columns")
        .externally_loaded(),
    l("blaze::borrow_workaround", Warn, "detects a value looked up twice, first only to test that it is there, where one lookup kept in an `if let` passes every borrow checker")
        .externally_loaded(),
    l("blaze::nll_workaround", Allow, "detects a value looked up twice so a borrow ends where the branch is not taken, which blaze's borrow checker accepts as one lookup in an `if let` and stable Rust's rejects")
        .externally_loaded(),
    l("blaze::native_contracts", Warn, "detects nightly's `#[core::contracts::requires]` and `#[core::contracts::ensures]` on a function of a crate with `#![feature(blaze_verify)]`, whose native `requires` and `ensures` clauses say the contract in the signature")
        .externally_loaded(),
    l("blaze::thread_local_in_waiting_fn", Allow, "detects a thread-local read in, or inlinable into, a function that may park the fiber, which can keep the first thread's address after resuming on another")
        .externally_loaded(),
    l("blaze::lenient_deserialize", Allow, "detects a struct or enum deriving serde's `Deserialize` without `#[serde(deny_unknown_fields)]`, which silently drops a field of its input it doesn't know")
        .externally_loaded(),
    // In blaze-lints' `Lint::ALL` order (its discriminant), after the lints added before it.
    l("blaze::lock_held_across_slow", Allow, "detects a lock guard held across IO or a global-heap allocation")
        .externally_loaded(),
    l("blaze::as_cast", Allow, "detects every `as` cast, which says neither whether it loses value, sign or provenance nor how, and writes it as the operation it is: `From`, a named widening, truncation or sign change, a float conversion naming its rounding, or a pointer method")
        .externally_loaded(),
    l("blaze::implicit_overflow", Allow, "detects integer `+`, `-`, `*`, `<<`, `>>`, unary `-` and a value-losing `as` that blaze cannot show fit, whose meaning (a panic or a wrap) is the profile's; `checked_*`, `wrapping_*`, `saturating_*`, `overflowing_*` or `try_from` says which is meant")
        .externally_loaded(),
    l("blaze::global_state", Allow, "detects a `static mut`, a `static` whose type has interior mutability and a `thread_local!` item, global state the blaze edition removes")
        .externally_loaded(),
    l("blaze::async_fn", Allow, "detects an `async fn`, `async` block or `async` closure, which the blaze edition removes: a wait parks the fiber and the may-suspend effect is inferred")
        .externally_loaded(),
    l("blaze::unmarked_pure", Allow, "detects a function the effects walk proves pure that lacks `#[blaze::pure]`, which callers and the cache could rely on")
        .externally_loaded(),
    l("blaze::unmarked_nopanic", Allow, "detects a function the effects walk proves cannot panic that lacks `#[blaze::nopanic]`, which callers could rely on")
        .externally_loaded(),
    l("blaze::unmarked_noalloc", Allow, "detects a function the effects walk proves never calls the global allocator that lacks `#[blaze::noalloc]`, which callers could rely on")
        .externally_loaded(),
    l("blaze::unmarked_nonblocking", Allow, "detects a function the effects walk proves never blocks the thread that lacks `#[blaze::nonblocking]`, which callers could rely on")
        .externally_loaded(),
    l("blaze::mut_ref", Allow, "detects a unique reference spelled `&mut` or `ref mut` in a crate that says `#![blaze::uniq]`, which spells it `&uniq` and `ref uniq`")
        .externally_loaded(),
    l("blaze::undeclared_effect", Deny, "detects a function that may panic, allocate from the global heap, block the thread or not return (`diverge`), through any call, where its `#[blaze::expect(..)]`, its trait method's, the default of a module around it or `#![blaze::effects]` does not allow it; and an impl's method that lists an effect its trait's method does not; a list that is not one; and the deleted `#[blaze::may(..)]`, respelled `#[blaze::expect(..)]`")
        .externally_loaded(),
    l("blaze::unfulfilled_effect", Deny, "detects a word of a function's own `#[blaze::expect(..)]` for an effect the function is proved not to have, private or public: a function's own list is exact")
        .externally_loaded(),
    l("blaze::negative_effect", Warn, "detects `#[blaze::nopanic]`, `#[blaze::noalloc]` or `#[blaze::nonblocking]` where effects are declared by what a function is expected to do, `#[blaze::expect(..)]`")
        .externally_loaded(),
    l("blaze::durable_write", Allow, "detects a write that waits for the disk (a synced effect, `sync_all`, `fsync`) with no `// durable: <why>` saying what must survive a crash")
        .externally_loaded(),
    l("blaze::unbounded_wait", Allow, "detects a park or a condition wait with no deadline and no `// wakes: <who>` naming what always wakes it")
        .externally_loaded(),
    l("blaze::combined_identity", Allow, "detects hashes combined into an identity by XOR or a wrapping sum outside `blaze_ids::keyed`")
        .externally_loaded(),
    // The lints of code blaze cannot analyse (group `blaze::edition`): each warns by default once
    // what its help names is in the tree (`ready`), and is allowed until then.
    l("blaze::proc_macro_derive", ready::level(ready::REFLECTION_REPLACES_MACROS), "detects a derive procedural macro a workspace crate defines, opaque code that rewrites tokens at build time, which a generic impl over the type's fields by compile-time reflection replaces")
        .externally_loaded(),
    l("blaze::proc_macro_attribute", ready::level(ready::REFLECTION_REPLACES_MACROS), "detects an attribute procedural macro a workspace crate defines, opaque code that rewrites tokens at build time, which reflection over the item and a generic function replace")
        .externally_loaded(),
    l("blaze::proc_macro_fn_like", Warn, "detects a function-like procedural macro a workspace crate defines, opaque code that rewrites tokens at build time, which a `const fn` or a `const` block replaces unless it parses a foreign syntax")
        .externally_loaded(),
    l("blaze::build_script_ambient", ready::level(ready::CAPABILITIES_FOR_BUILD_SCRIPTS), "detects a build script of the workspace that reaches the file system, the environment, a process, the network or the clock through the standard library by itself, where a capability it is handed says what it can touch")
        .externally_loaded(),
    l("blaze::not_latest_edition", ready::level(ready::EDITION_SHIPS), "detects a package of the workspace that has not opted into the latest blaze edition (a Rust edition, or an older blaze edition), once per package, naming the manifest line that opts in")
        .externally_loaded(),
    // `allow` whatever `ready::BSTD_IN_TREE` says: a crate asks for it (`#![blaze::capabilities]`),
    // and a workspace does in its `lints-when-ready` table.
    l("blaze::std_use", Allow, "detects a use of an item of Rust's standard library that blaze's own standard library replaces or forbids (ambient authority, a hidden cost, a global-heap allocation, a renamed type), once per item per function, naming what is written instead")
        .externally_loaded(),
    l("blaze::program_lifetime", Deny, "detects a type declared `#[blaze::program_lifetime]` whose instances could not all live until the program exits: it has a parameter or a `Drop`, holds a value that does, borrows for less than `'static`, or is `pub` in a crate that did not opt in")
        .externally_loaded(),
    l("blaze::could_be_program_lifetime", Allow, "detects a private type with no `Drop` whose every instance is built into a `static` or leaked, and so already lives until the program exits: `#[blaze::program_lifetime]` and the program arena would hold it by 4-byte references with no count")
        .externally_loaded(),
    l("blaze::could_be_ref32", Allow, "detects a field or collection element of a struct stored many times that holds a `#[blaze::program_lifetime]` type as `&'static T`, `Arc<T>`, `Rc<T>` or `Box<T>`, where a `Ref32<T>` is half the size")
        .externally_loaded(),
    l("blaze::span_field_unrecorded", Allow, "detects a field of a `tracing` span or event of the target `blaze` that the profile keeps nothing of")
        .externally_loaded(),
    l("blaze::unreachable", Deny, "detects a `blaze::unreachable!()` that blaze cannot prove no path reaches")
        .externally_loaded(),
    l("blaze::todo", Warn, "detects a `todo!()` outside test code, with the type the hole must have; the merge gate and a build for a release deny it")
        .externally_loaded(),
    l("blaze::env_read", Allow, "detects a read of the process's environment (`std::env::var`, `var_os`, `vars`, `vars_os`) outside the settings modules `env-read-allowed` names")
        .externally_loaded(),
    l("blaze::untied_child", Allow, "detects test code that starts a process whose `Child` no `stdx::JodChild` owns, or runs `blaze` without `BLAZE_DAEMON_EXIT_UNHELD` on its command")
        .externally_loaded(),
    l("blaze::unbounded_test_wait", Allow, "detects a test that waits with no deadline: a park, a condition wait, a channel `recv` or a child's `wait`, or a loop around `Child::try_wait` that compares no deadline")
        .externally_loaded(),
    l("blaze::reads_positions", Allow, "detects a function of the front end's query crates that reads where code is written (a source map, the live parse, an ast id's range) without being marked `#[blaze::renders_positions]`, so its result depends on every edit above it")
        .externally_loaded(),
    l("blaze::absolute_position", Allow, "detects a field holding an absolute source position (a `core::panic::Location`, a `TextRange`, a `LineCol`, a `FileRange`) in a type that is hashed or persisted, or a `Location` copied into a field, so the key or record moves with every line added above the code")
        .externally_loaded(),
    l("blaze::cbor_field_order", Allow, "detects a struct or struct variant deriving serde's `Serialize` in a `#![blaze::canonical_cbor]` module with its fields declared out of their keys' deterministic-CBOR order (shorter first, then bytewise), which makes the encoder sort and copy the map")
        .externally_loaded(),
    l("blaze::gapped_literal", Allow, "detects a string literal one line of which holds a run of eight or more spaces between two characters of its text: a lost `\\` line continuation, which prints the message with a gap")
        .externally_loaded(),
    l("blaze::retry", Allow, "detects an operation tried again up to a constant: a comparison of a count named for retries, attempts, tries or failures with a constant bound, or a `for` loop over such a range")
        .externally_loaded(),
    l("blaze::unwired", Allow, "detects `#[allow(dead_code)]` or `#[allow(unused)]` (or `expect`) on an item, field or variant outside tests: a design kept quiet instead of wired in or deleted")
        .externally_loaded(),
    l("blaze::tmp_path", Allow, "detects a path under `/tmp`, `/private/tmp` or `/var/tmp` named by hand, and a use of `std::env::temp_dir` outside `blaze_core::temp`, whose `TempDir` removes what it made when it drops")
        .externally_loaded(),
    l("blaze::narrowed_len", Allow, "detects a length narrowed to a smaller integer type by `try_from` or `try_into` with an `expect` or `unwrap`, which panics on a large input instead of refusing it with a typed error")
        .externally_loaded(),
    l("blaze::process_static", Allow, "detects a `static` or `thread_local!` key holding a growable collection beneath a lock or a cell, or a concurrent collection: state that grows for the process's life with no owner to drop it")
        .externally_loaded(),
    l("blaze::loop_turn_effects", Deny, "detects a `#[blaze::bounded_turn]` function, a loop thread's turn, that may block the thread, do IO, sync a file to the disk or take a lock, through any call")
        .externally_loaded(),
    l("blaze::sync_on_answer_path", Deny, "detects a `#[blaze::answers_request]` function, an answer path, that may sync a file to the disk, through any call")
        .externally_loaded(),
    l("blaze::lock_order", Allow, "detects locks taken in a cycle: one function holds A while taking B and another holds B while taking A, through any call, which can deadlock two threads")
        .externally_loaded(),
    l("blaze::lock_across_wait", Allow, "detects a lock guard held across a call that blocks the thread (a condvar it is not handed, a channel, a sleep, a join, a blocking syscall), through any call")
        .externally_loaded(),
    l("blaze::truncating_cast", Allow, "lists each integer truncation and sign change (`truncate`, `cast_signed`, `cast_unsigned`, blaze-cast's `*_truncated_from_*`), which wraps a value that does not fit, for a decision: a typed error, a bound said with `expect`, or the wrap kept")
        .externally_loaded(),
    l("blaze::partial_write", Allow, "detects a call of `std::io::Write::write` whose count of bytes written is thrown away, so the bytes it did not write are lost without an error")
        .externally_loaded(),
    l("blaze::partial_cmp_unwrap", Allow, "detects `partial_cmp(..).unwrap()` or `.expect(..)` of two floats, which panics on the first NaN")
        .externally_loaded(),
    l("blaze::unbounded_read", Allow, "detects a read of std's that takes as much as its input holds (`read_to_end`, `read_to_string`, `fs::read`, `Command::output`, `lines`, `read_line`) with no `take` bound")
        .externally_loaded(),
    l("blaze::len_minus", Allow, "detects a length minus something in an unsigned type where nothing before it says the length is the larger, which underflows")
        .externally_loaded(),
    l("blaze::str_byte_slice", Allow, "detects a `str` sliced or split at a byte index nothing says is on a character boundary, which panics inside a multi-byte character")
        .externally_loaded(),
    l("blaze::forget", Allow, "detects a `ManuallyDrop` never dropped, taken apart or handed on, and a `Box::leak` outside a `static` or a `#[blaze::program_lifetime]` type")
        .externally_loaded(),
    l("blaze::path_to_str_unwrap", Allow, "detects `Path::to_str` or `OsStr::to_str` unwrapped, which panics on a name that is not UTF-8, and `to_string_lossy` outside display code, which changes it silently")
        .externally_loaded(),
    l("blaze::detached_thread", Allow, "detects a `thread::spawn` or `thread::Builder::spawn` whose `JoinHandle` is dropped or never named, so nobody joins the thread")
        .externally_loaded(),
    l("blaze::unbounded_channel", Allow, "detects a channel made with no bound (`mpsc::channel`, `crossbeam_channel::unbounded`, tokio's `unbounded_channel`, `flume::unbounded`), which a fast sender grows until memory runs out")
        .externally_loaded(),
    l("blaze::transmute", Allow, "detects `std::mem::transmute` and `transmute_copy`, which check nothing but the sizes, and writes std's safe function where one exists for the pair")
        .externally_loaded(),
    l("blaze::clone_to_satisfy", Allow, "detects a clone of a heap-owning value only borrowed at once (`&x.clone()`, `x.clone().len()`), which copies the whole value for a read")
        .externally_loaded(),
    l("blaze::unchecked_index_in_loop", Allow, "detects a `for i in 0..v.len()` loop that only indexes `v`, indexes another sequence with `i`, or indexes past the range with `i + k`/`i - k`")
        .externally_loaded(),
    l("blaze::default_numeric_fallback_len", Allow, "detects an integer literal binding that falls back to `i32` and is cast `as usize` where it is used: a length, count or index in the wrong type")
        .externally_loaded(),
    l("blaze::refcell_borrow_across_call", Allow, "detects a `RefCell` guard held while the function borrows the cell again or calls something handed the cell or what holds it, which panics on re-entry")
        .externally_loaded(),
    l("blaze::process_exit", Allow, "detects `std::process::exit` and `abort` outside a binary's entry module, which end the process with no destructor run and no buffer flushed")
        .externally_loaded(),
    l("blaze::print_in_lib", Allow, "detects `print!`, `println!`, `eprint!` and `eprintln!` in library code, which writes to a stream the caller owns")
        .externally_loaded(),
    l("blaze::lock_unwrap", Allow, "detects std's `Mutex::lock`, `RwLock::read` or `write` unwrapped, which turns one panic into a panic of every thread that takes the lock after it")
        .externally_loaded(),
    // Emitted by the release build, which reads it from the code LLVM made, never by `blaze check`.
    l("blaze::vecreport", Allow, "reports a bounds check, overflow check or panic path the optimizer kept inside a loop, and a loop it left unvectorized with its reason, read from the optimized code; only hot loops when the build has a profile"),
];
