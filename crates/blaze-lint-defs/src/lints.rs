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
    l("blaze::implied_bound_written", Warn, "detects a bound written on a function or impl of a crate with `#![feature(blaze_implied_bounds)]` that the types and traits its header names already declare")
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
    l("blaze::refinement", Deny, "detects a value built, changed, passed or returned where blaze cannot prove what its type's `#[blaze::invariant(..)]` or a function's `requires` or `ensures` needs, and an invariant blaze cannot check")
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
    // The suspicious-code lints: what compiles and reads as right but leaves something implicit.
    l("blaze::skipped_pair", Allow, "detects the first call of a pair (`push`/`pop`, `begin`/`end`, `#[blaze::pairs_with]`) whose second an early exit between them skips")
        .externally_loaded(),
    l("blaze::half_done", Allow, "detects a write through a `&mut` parameter, then a fallible exit, then more writes to it: the error leaves the value half changed")
        .externally_loaded(),
    l("blaze::hidden_exit", Allow, "detects a call of a workspace macro whose expansion returns, `?`s, breaks or continues out of the caller, unseen at the call")
        .externally_loaded(),
    l("blaze::question_mark_converts", Allow, "detects a `?` whose `From` conversion drops what the error was about: a subject-less std error, or one turned into a general error type")
        .externally_loaded(),
    l("blaze::option_question_mark_hides", Allow, "detects a `?` on an `Option` from a failed lookup or parse in a function returning `Option`, which makes missing and broken one `None`")
        .externally_loaded(),
    l("blaze::unseen_target", Allow, "detects `.into()`, `.parse()`, `.collect()`, `Default::default()`, `.sum()` and the like whose target type only distant inference fixes")
        .externally_loaded(),
    l("blaze::glob_import", Allow, "detects a private `use path::*`, which brings names into scope unseen")
        .externally_loaded(),
    l("blaze::shadow_changes_meaning", Allow, "detects a `let` that shadows a binding still in scope with a value not made from it")
        .externally_loaded(),
    l("blaze::versioned_name", Allow, "detects an item named another's with a version on it (`_v2`, `_new`, `_old`, `_legacy`, `_tmp`, `2`) while the other is still declared beside it")
        .externally_loaded(),
    l("blaze::unused_parameter", Allow, "detects a parameter of a function that its body never reads, an underscore name included")
        .externally_loaded(),
    l("blaze::fallback", Allow, "detects an error branch that computes the same kind of result another way without looking at the error")
        .externally_loaded(),
    l("blaze::placeholder_body", Allow, "detects a function whose whole body is a constant success while its inputs go unread or its doc promises an effect")
        .externally_loaded(),
    l("blaze::test_asserts_nothing", Allow, "detects a test with no path that can fail")
        .externally_loaded(),
    l("blaze::assertion_cannot_fail", Allow, "detects an assertion that cannot fail: a constant, a value compared with itself, or one after an unconditional exit")
        .externally_loaded(),
    l("blaze::test_misses_its_subject", Allow, "detects a test that calls none of its crate's non-test code")
        .externally_loaded(),
    l("blaze::ignored_without_reason", Allow, "detects `#[ignore]` with no reason")
        .externally_loaded(),
    l("blaze::doc_names_nothing", Allow, "detects a path in backticks in a doc comment that resolves to no item")
        .externally_loaded(),
    // The magic literals: a number or a name stated in place of its one home.
    l("blaze::magic_number", Allow, "detects a number written in code that is neither derived from what it depends on nor a named `const`")
        .externally_loaded(),
    l("blaze::magic_string", Allow, "detects a string the program branches or looks up by, or writes in more than one place, that is neither a named `const` nor an enum")
        .externally_loaded(),
    l("blaze::repeated_format", Allow, "detects one format template of several words written in two or more places of a crate")
        .externally_loaded(),
    // The workspace pass's (blaze-findings): sibling crates compared, neither depending on the other.
    l("blaze::string_shared_across_crates", Allow, "detects one key-like string written in two or more crates of the workspace")
        .externally_loaded(),
    l("blaze::string_near_miss_across_crates", Allow, "detects two key-like strings of two crates that differ only by case, separators, a plural or one character")
        .externally_loaded(),
    // Code that carries no weight: what the workspace writes and never uses, abstraction with
    // one user, and arguments in the wrong order.
    l("blaze::swapped_arguments", Allow, "detects two same-typed arguments whose names match each other's parameters better than their own")
        .externally_loaded(),
    l("blaze::write_only_field", Allow, "detects a field the workspace writes and production code never reads, only a derive, a serde impl or a test")
        .externally_loaded(),
    l("blaze::only_used_in_tests", Allow, "detects an exported function production code never calls or names, that tests do")
        .externally_loaded(),
    l("blaze::single_impl_trait", Allow, "detects a trait the whole workspace implements once")
        .externally_loaded(),
    l("blaze::forwarding_fn", Allow, "detects a function whose whole body calls another with its own parameters, in order, unchanged")
        .externally_loaded(),
    l("blaze::constant_argument", Allow, "detects a parameter every call in the workspace passes the same constant")
        .externally_loaded(),
    l("blaze::always_same_result", Allow, "detects a function returning the same constant at every return, a `Result` never `Err` or an `Option` never `None`")
        .externally_loaded(),
    l("blaze::never_constructed", Allow, "detects a variant of an exported enum no production code of the workspace constructs")
        .externally_loaded(),
    l("blaze::never_matched", Allow, "detects a variant production code constructs that no pattern names, of an enum others are matched by")
        .externally_loaded(),
    l("blaze::swappable_parameters", Allow, "detects two adjacent parameters of one primitive or string type")
        .externally_loaded(),
    // How hard code is to hold in one's head: each measure over its bound (docs/lints.md,
    // "Complexity"), an existing item held at its value.
    l("blaze::function_weight", Allow, "detects a function whose cognitive complexity is over its bound")
        .externally_loaded(),
    l("blaze::path_count", Allow, "detects a function with more acyclic paths through it, loops taken once, than its bound")
        .externally_loaded(),
    l("blaze::live_state", Allow, "detects a function holding more mutable places live across one call than its bound")
        .externally_loaded(),
    l("blaze::effect_breadth", Allow, "detects a function whose own calls into other crates bring more kinds of effect than its bound")
        .externally_loaded(),
    l("blaze::exits", Allow, "detects a function with more early exits than its bound while it holds a value whose drop finishes something")
        .externally_loaded(),
    l("blaze::long_function", Allow, "detects a function with more lines of code, or MIR statements, than its bound")
        .externally_loaded(),
    l("blaze::long_file", Allow, "detects a file with more lines of code, its tests left out, than its bound")
        .externally_loaded(),
    l("blaze::many_params", Allow, "detects a function with more parameters than its bound")
        .externally_loaded(),
    l("blaze::wide_type", Allow, "detects a struct with more fields, or an enum with data with more variants, than its bound")
        .externally_loaded(),
    l("blaze::deep_nesting", Allow, "detects a function whose control flow nests deeper than its bound")
        .externally_loaded(),
    l("blaze::fan_out", Allow, "detects a function calling more distinct functions than its bound")
        .externally_loaded(),
    l("blaze::module_cycle", Allow, "detects modules of one crate that import from each other")
        .externally_loaded(),
    // Correct by construction: a check at run time, a convention or a name standing in for a type.
    l("blaze::proof_forgotten", Allow, "detects an `unwrap`, `expect` or index that a check of the same value just before already proved, so the proof is dropped and asked again at run time")
        .externally_loaded(),
    l("blaze::unit_mismatch", Allow, "detects a sum, difference or comparison of two numbers whose names carry different units")
        .externally_loaded(),
    l("blaze::primitive_meaning", Allow, "detects a parameter or field of a bare number whose name carries a unit of time or the identity of a thing")
        .externally_loaded(),
    l("blaze::sentinel_value", Allow, "detects a number compared with `-1`, `MAX` or a zero id standing in for \"none\"")
        .externally_loaded(),
    l("blaze::state_in_flags", Allow, "detects a struct keeping the state of its life in two or more `bool` fields")
        .externally_loaded(),
    l("blaze::bool_pair_state", Allow, "detects a tuple of `bool`s, an `Option<bool>` or a `match` on a tuple of `bool`s: states with no names")
        .externally_loaded(),
    l("blaze::out_parameter", Allow, "detects a `&mut` parameter only written through, a second return value beside a `bool` or `()`")
        .externally_loaded(),
    l("blaze::parallel_collections", Allow, "detects two `Vec` fields of one value pushed together or read at the same index, kept the same length by convention")
        .externally_loaded(),
    l("blaze::untyped_index", Allow, "detects an integer field used only as an index into one collection, with no type saying which")
        .externally_loaded(),
    l("blaze::validated_but_raw", Allow, "detects a function that checks its primitive parameter and returns the same raw value")
        .externally_loaded(),
    // A promise of a doc comment (docs/v2/plan/design/claims.md).
    l("blaze::claim_contradicts_facts", Allow, "detects a doc comment saying an effect (`never panics`, `does not allocate`) the effect walk refutes, or a `# Panics` section on a function proved not to panic")
        .externally_loaded(),
    l("blaze::claim_is_derivable", Allow, "detects a doc comment saying an effect the effect walk proves, which an attribute would state and check")
        .externally_loaded(),
    l("blaze::claim_without_evidence", Allow, "detects a promise of a doc comment that is not derived, not an attribute, not evidenced and not declared unverified")
        .externally_loaded(),
    l("blaze::evidence_is_stale", Allow, "detects an `Evidence:` pointer that resolves to nothing, to what is not a test, or to a test that does not reach the item")
        .externally_loaded(),
    l("blaze::safety_without_invariant", Allow, "detects a `// SAFETY:` comment that does not name its invariant by a resolving path or say where it is upheld")
        .externally_loaded(),
    l("blaze::comment_claims", Allow, "detects a body comment claiming what nothing checks (`cannot overflow`, `unreachable`, `can't be None`)")
        .externally_loaded(),
    // Duplicated code and copy-paste mistakes, by the shape of code (docs/lints.md, Duplicates).
    l("blaze::same_fn", Allow, "detects two functions with the same body and signature up to the names of their locals")
        .externally_loaded(),
    l("blaze::same_impl_fn", Allow, "detects one method body written in several impls, which a default method or blanket impl would hold once")
        .externally_loaded(),
    l("blaze::same_block", Allow, "detects a run of statements over the size floor written more than once, up to the names of its locals")
        .externally_loaded(),
    l("blaze::near_duplicate", Allow, "detects two functions most of whose runs of tokens agree, with the lines that differ")
        .externally_loaded(),
    l("blaze::paste_mismatch", Allow, "detects a pasted block that renames a name everywhere but one place, and a row of a run of rows that repeats the row above")
        .externally_loaded(),
    l("blaze::same_type", Allow, "detects two structs or enums with the same fields or variants")
        .externally_loaded(),
    l("blaze::same_const", Allow, "detects two constants or statics with the same type and value")
        .externally_loaded(),
    l("blaze::extractable_duplicate", Allow, "detects two functions that differ only in a few literals, paths, fields or expressions of one type, which one function with those as parameters would replace")
        .externally_loaded(),
    l("blaze::tests_differ_by_literal", Allow, "detects tests that differ only in their literals, which one test over a table of values would replace")
        .externally_loaded(),
    l("blaze::impls_differ_by_type", Allow, "detects functions that differ only in their types, which one generic function or blanket impl would replace")
        .externally_loaded(),
    l("blaze::duplicate_across_crates", Allow, "detects the same function, constant or type written in two crates of a workspace, and names the crate it belongs in")
        .externally_loaded(),
    // Test strength (`blaze-lints`' `test_strength`): what no test observes, without running one.
    l("blaze::result_part_unobserved", Allow, "detects a part of what a function returns (a field, the payload of `Ok`) that no test calling it ever looks at")
        .externally_loaded(),
    l("blaze::weak_assertion", Allow, "detects a test that checks only one property of a result (`is_ok()`, `len()`, `contains`) where the value has more in it")
        .externally_loaded(),
    l("blaze::mirror_assertion", Allow, "detects an assertion whose expected value is computed by the code under test")
        .externally_loaded(),
    l("blaze::pseudo_tested", Allow, "detects a function tests call whose result no test looks at, so any body that does not panic passes them")
        .externally_loaded(),
    l("blaze::unreached_by_tests", Allow, "detects a public function no test reaches, directly or through what it calls")
        .externally_loaded(),
    l("blaze::error_path_untested", Allow, "detects a function returning `Result` whose tests never check which error it returns")
        .externally_loaded(),
    l("blaze::property_over_examples", Allow, "detects a function with an inverse beside it (`encode` and `decode`) that no property test checks the round trip of")
        .externally_loaded(),
    // Suggestions (`blaze::suggest`, the adoption ladder's second step).
    l("blaze::could_be_narrower", Allow, "detects a function whose every result is an integer literal that a narrower integer type than its declared one holds")
        .externally_loaded(),
    l("blaze::could_be_sans_io", Allow, "detects a module whose code reaches no ambient effect but is not marked `#![blaze::sans_io]`")
        .externally_loaded(),
    l("blaze::could_be_linear", Allow, "detects a type whose `Drop` does the work of a method that takes it by value, where `#[blaze::linear]` would make every value finished explicitly")
        .externally_loaded(),
    l("blaze::could_have_invariant", Allow, "detects a struct whose constructors and `&mut self` methods start with the same assertion, a condition `#[blaze::invariant(..)]` states once")
        .externally_loaded(),
    // Code written by hand that the type's structure gives (`blaze-lints`' `structural`): one
    // analysis, whether a body is uniform over the type's fields or variants.
    l("blaze::hand_written_derivable", Allow, "detects a `Clone`, `Debug`, `Default`, `PartialEq`, `Hash`, `PartialOrd` or `Ord` impl whose body is exactly what the derive writes")
        .externally_loaded(),
    l("blaze::impl_misses_field", Allow, "detects a field-wise body that covers every field but one the same way, and a `PartialEq`, `Ord` or decoder that leaves out a field its `Hash` or encoder covers")
        .externally_loaded(),
    l("blaze::hand_kept_variant_list", Allow, "detects an enum's variants listed again by hand (`ALL`, `COUNT`, `fn name`, `fn index`, a names table) where the list is derivable from the declaration")
        .externally_loaded(),
    l("blaze::variant_list_drift", Allow, "detects a hand-kept list of an enum's variants that misses or repeats one, miscounts them, misspells a name, is out of order, or leaves exactly one variant to `_`")
        .externally_loaded(),
    l("blaze::hand_written_codec", Allow, "detects an encoder or decoder written field by field, the same call on each, which one record layout or generic codec would replace")
        .externally_loaded(),
    l("blaze::field_wise_conversion", Allow, "detects a conversion between two structs that copies each field into its namesake by hand")
        .externally_loaded(),
    l("blaze::conversion_misses_field", Allow, "detects a field-by-field conversion that fills a field from another field's namesake, or never reads a field the target has too")
        .externally_loaded(),
    l("blaze::hand_written_visitor", Allow, "detects a walk (`visit`, `walk`, `fold`) written by hand for three or more types, each the same call on every field")
        .externally_loaded(),
    l("blaze::repeated_field_list", Allow, "detects a struct whose whole field list is written out, field by field, in three or more methods")
        .externally_loaded(),
    l("blaze::registry_spread", Allow, "detects an enum whose variants' facts are spread over five or more parallel lists, which one table of rows would replace")
        .externally_loaded(),
    // Duplicated behaviour, by a canonical form of runtime MIR.
    l("blaze::same_behaviour", Allow, "detects a function written differently from another but compiled to the same behaviour, by a canonical form of their runtime MIR")
        .externally_loaded(),
    // Trust: what a dependency may do only when the workspace grants it.
    l("blaze::dependency_capability", Deny, "detects a dependency that uses a capability (files, sockets, processes, the environment, the clock, threads, global state, randomness, foreign code, a build script, a proc macro) the workspace's `[workspace.metadata.blaze.trust]` does not grant it")
        .externally_loaded(),
    l("blaze::dependency_unsafe", Deny, "detects a dependency with `unsafe` code the workspace's `[workspace.metadata.blaze.trust]` does not grant it")
        .externally_loaded(),
    l("blaze::trust_grant_unused", Warn, "detects a grant in `[workspace.metadata.blaze.trust]` for no dependency, for a version the build does not have, or of a capability its dependency does not use")
        .externally_loaded(),
    l("blaze::dependency_capability_unreached", Warn, "detects a dependency's code that nothing the workspace builds reaches yet and that uses a capability the workspace does not grant it")
        .externally_loaded(),
    l("blaze::workspace_unsafe", Allow, "detects a workspace crate with `unsafe` code whose root does not say why with `#![blaze::trust(unsafe, reason = \"..\")]`")
        .externally_loaded(),
    // Emitted by the release build, which reads it from the code LLVM made, never by `blaze check`.
    l("blaze::vecreport", Allow, "reports a bounds check, overflow check or panic path the optimizer kept inside a loop, and a loop it left unvectorized with its reason, read from the optimized code; only hot loops when the build has a profile"),
];
