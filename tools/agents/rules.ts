// blaze's rules for agents: the one source. Each rule lives in two generated places:
//
// - AGENTS.md, which Claude Code, Codex and Cursor read at the start of every session, carries every
//   rule as one short `line` and names the skill that holds the rest.
// - .agents/skills/<skill>/SKILL.md (Claude Code reads it through the .claude/skills symlink) carries
//   the rule's full text, `info`, with its link, between the file's front matter and its end marker.
//   An agent loads the skill when its `description` matches the work. Hand-written how-to goes
//   below the end marker and is kept as it is by --write.
//
// `reason` is why (Andrew's words, dates, history) and stays here.
//
//   bun tools/agents/rules.ts --write    regenerate AGENTS.md and each skill's generated part
//   bun tools/agents/rules.ts --check    fail if any of them is not what --write would write (CI, fmt job)
//
// AGENTS.md also carries `reviewRules`, the "## Code Review Rules" section Codex code review reads
// on every pull request.
//
// A rule's identity is its `id` (kebab case, never reused); its number is only its position in the
// printed list and moves when a rule is inserted (`.clippy.toml` cites rules by number: keep the
// order, append new rules).

/** The skills the rules are grouped into, by the task an agent loads them for. */
const skills = {
	"build-and-test": {
		title: "Building, testing and measuring blaze",
		description:
			"How blaze is built, checked, tested and measured, and how a lane runs: blaze builds blaze (never cargo), its own pinned nightly, the Mac or an ix VM (never AWS), subagents that only write code while the main agent runs one `blaze check` and `blaze test`, one worktree per lane, cleaning up, measuring the exact tree and testing a causal claim. Load before any build, check, test, benchmark, bisect or measurement, before splitting work across subagents, and when starting or ending a lane.",
	},
	"compiler-correctness": {
		title: "Compiler correctness",
		description:
			"What blaze must compile and how: never rustc, every program the pinned nightly builds behaves the same (except rustc soundness holes), usefulness over parity with rustc and cargo, semantics in the front end's query crates (blaze-query, derived from salsa), keys that cover every input, deterministic output, proc macros on wasm and Cranelift's place, sandboxed build scripts and proc macros. Load before changing the front end, codegen, the linker, caching or store keys, diagnostics, proc-macro or build-script execution, or the sandbox.",
	},
	"runtime-code": {
		title: "Writing blaze's runtime code",
		description:
			"How code inside blaze is written: no fallbacks, allocation by lifetime, the one standard data structure per need (locks, maps, queues, channels, waits), sans-IO cores, named and bounded waits, errors that name the thing, panics that fail one job, nothing thread-bound across a fiber wait. Load before writing or reviewing Rust in blaze's crates, especially the scheduler, daemon, query engine, store or anything concurrent.",
	},
	"perf-and-tracing": {
		title: "Performance and tracing",
		description:
			"How a slow thing is found and fixed: a span names every stretch of worker time, `self_cpu_ns` is filled, the profile parquet answers which item took the time, and a speed PR quotes before and after from the spans. Load before investigating anything slow, adding tracing, or making a speed claim.",
	},
	"changing-code": {
		title: "Changing code across blaze",
		description:
			"How a change that spans the tree is made: moves through blaze-lints bans and `blaze check --fix`/`--replace`, whole ports of shared signatures, one concept in one home, the best long-term design, and round-trip tests for persisted state. Load before renaming or moving a type, path or API, changing a shared signature, removing a duplicate, choosing a design, or touching the daemon image, the store or other persisted state.",
	},
	landing: {
		title: "Landing changes",
		description:
			"How a change gets to main: `land.py` and its merge gate, taking any open pull request or issue to done, and crediting people with `Co-authored-by:`. Load before committing, opening or finishing a pull request, or picking up someone else's.",
	},
	"docs-and-scripts": {
		title: "Docs, scripts and agent interfaces",
		description:
			"Conventions for blaze's documents, scripts and agent-facing interfaces: docs/ front matter (current, plan, record, superseded) and `tools/docs.py`, scripts that take one typed JSON value, and no MCP. Load before writing or editing a doc under docs/, writing or changing a script, or designing an interface agents call.",
	},
} as const satisfies Record<string, { title: string; description: string }>;

type SkillId = keyof typeof skills;

type Rule = {
	/** Stable kebab-case name. */
	id: string;
	/** The skill whose SKILL.md carries this rule's full text. */
	skill: SkillId;
	/** The rule in one short imperative sentence, for AGENTS.md. */
	line: string;
	/** The rule in full: what an agent must do. Goes into its skill, every sentence of it. */
	info: string;
	/** Why: quotes, dates, issues, history. In neither AGENTS.md nor the skill. */
	reason?: string;
	/** Where the detail is, relative to the repository root. */
	link?: string;
};

const header = `# blaze

How to build, test and change blaze: docs/contributing.md. Every doc: docs/README.md. What the harnesses check: docs/harness.md. These rules are generated from tools/agents/rules.ts (edit there, then \`bun tools/agents/rules.ts --write\`).

Each rule below is one line; its full text, with how-to, is in the skill named after it, under .agents/skills/ (also .claude/skills/). Load that skill before work it covers.`;

export const rules = [
	{
		id: "no-rustc",
		skill: "compiler-correctness",
		line: "blaze never runs rustc, rustdoc or a cargo that runs them; each place that still does is a bug to remove, never to extend.",
		info: "blaze never runs rustc: no function, crate, std, build script, proc macro, test or link step goes to rustc, rustdoc or a cargo that runs them. Each place that still does is a bug with a `no-rustc` issue, to be removed, never extended.",
		link: "docs/no-rustc.md",
	},
	{
		id: "blaze-builds-blaze",
		skill: "build-and-test",
		line: "Build, check, test, bench and release blaze only with blaze, never cargo, rustc or sccache, anywhere (dev loop, CI, harnesses, release); stage 0 is a blaze binary; when blaze fails on blaze, that failure is the work.",
		info: "Build, check and test blaze with blaze (`blaze build`, `blaze check`, `blaze test`), never `cargo`, `rustc` or sccache, in any place: an agent's loop, a harness, a CI workflow, a benchmark, the release build or a doc's instructions. Stage 0 is a blaze binary (the release `.github/blaze-version` names, or a blaze built earlier), never cargo, until the wasm seed replaces it (docs/future/bootstrap.md). Stage 0 is never overwritten: a build of the tree (stage 1) becomes stage 0 only after it rebuilds the tree (stage 2) byte for byte and the tests pass, and moving `.github/blaze-version` forward is that promotion. When stage 0 can't build the tree, that is a named error: write the source around it for one release or pin an older release, never cargo. Every build, check, test and verdict comes from blaze. When blaze fails on blaze (a compile error, a link error, a panic, a hang, a wrong result, too slow, too much memory), that failure is the work: fix blaze, never route around it through cargo. A crate blaze can't check or test yet is a ranked, owned blocker, not a reason to keep using cargo for it.",
		reason: "\"never use sccache never use rustc\" and \"we should make sure we get blaze to fully build itself! basically blaze sucks until then\" (Andrew, 2026-10-26). \"we should try to do this all in blaze and if things dont work [that] doesnt mean use cargo, means fix blaze\" (Andrew, 2026-11-03): that day every lane test still went through `cargo test` (median 14 min a request, with no skipping of unchanged tests), while blaze's failures on blaze waited for an occasional self-build round. \"can u compeltly delete all rustc ... have no rustc or cago even for blaze ... and just use blaze for eveyrhign\" (Andrew, 2026-11-03): cargo had stayed as the bootstrap and the daily build in contributing.md, CI and the harnesses.",
		link: "docs/contributing.md",
	},
	{
		id: "no-fallback",
		skill: "runtime-code",
		line: "No fallback, ever: when blaze can't do something it stops with an error naming what and where.",
		info: "No fallback, ever. When blaze can't do something it stops with an error naming what and where, never quietly takes another path (rustc, a full link or rebuild instead of a patch, a slower backend, an older cached result, a default guessed for a missing value). A fallback that only logs its reason is still a fallback. A change that touches an existing fallback turns it into an error, and a bench or test that hits one fails instead of timing the slow path.",
		reason: "\"No fallback EVER, no rustc, prefer quick error so bug can be fixed\" (Andrew, 2026-10-26). \"remove fallbacks, should just fail (so can hill climb easier)\" (Andrew, 2026-10-30). A silent fallback hides the bug and turns it into wrong output (#386, #392); an error gets it fixed. A fallback also hides a speed bug: on 2026-10-30 a generic edit took 33 s on Linux against cargo's 32 s because the patch path quietly fell back to a full link (\"no executable to add to\"), while the same edit patched in 6.3 s on macOS. A failure points at the one broken step; a slow number does not.",
	},
	{
		id: "superset-of-rust",
		skill: "compiler-correctness",
		line: "Every program the pinned nightly builds builds and behaves the same under blaze; blaze refuses only rustc soundness holes, and its extensions are opt-in.",
		info: "Own every layer from source to running code, never by narrowing what blaze accepts: every program the pinned nightly builds must build and behave the same under blaze, with the real std, C dependencies, build scripts and proc macros. The allowed differences are sandboxing and soundness: where rustc accepts a program because of an objective bug in rustc (a soundness hole that lets safe code cause undefined behavior, such as rust-lang/rust#25860, or another clear safety defect), blaze refuses it with a hard error naming the hole, never by default copying the bug; a mere disagreement with rustc's design is not such a bug. Superset means blaze may also accept more than Rust: an extension (new syntax, attributes, semantics) only has to work under blaze, never under rustc, as long as it is opt-in and every program that doesn't use it behaves exactly as under rustc.",
		reason: "Owning every layer is what makes blaze fast. Sandboxing has been on by default since 2026-10-26 (https://github.com/indexable-inc/blaze/issues/147). Andrew, 2026-10-28: \"we are superset and if using superset dont need rustc to compile it either\". Andrew, 2026-11-02, on rejecting the rust-lang/rust#25860 hole (cve-rs builds use-after-free in safe code; blaze accepted its form that day): \"hard error ... revise superset UNLESS there is a safety issue / objective issue w rustc\".",
		link: "docs/no-rustc.md",
	},
	{
		id: "useful-not-parity",
		skill: "compiler-correctness",
		line: "rustc and cargo are references, not specs: design every output and interface as if from scratch, the best way for people and agents, and match them only where real code or tools depend on it.",
		info: "rustc and cargo are references, never specs. Judge everything blaze does by how useful it is to the person or agent using it, not by how closely it matches them: errors, warnings, lints, CLI commands and flags, printed and JSON output, progress, the target directory's layout, file formats, caches, symbol names, debug info, what a build or test run does and in what order. Match them only where real code or tools depend on it (a program's behavior, a published crate's manifest, an editor reading the JSON); everywhere else pick the better design and delete the copy. Their style is no constraint either: how cargo or rustc words, colours, lays out or orders a message, a progress line, a summary or a flag is not a reason for blaze to do the same; design it as blaze would from scratch (structured data first, rendered for a person on a terminal and as data for an agent) and change it freely when a better way is found. Tests check what makes the result useful and correct, never that it equals rustc's or cargo's. `superset-of-rust` is unchanged: every program the pinned nightly builds still builds and runs as it does under rustc, except one that only builds through a rustc soundness bug.",
		reason: "\"we dont need word for word ... be clear functional parity also not needed we care about usefulness just in general wrt rustc\" and \"make the rule MORE GENERAL than that not just errors\" (Andrew, 2026-10-31). On 2026-10-31 166 of blaze-diagnostics' 1223 tests failed, most on text that differed from rustc's (`{integer}` against `i32`, a span one column off, a lint count) and not on whether the error was useful. The plan's \"conformance only where real code depends on it\" (2fb77406fb) said this for behavior; this rule says it for everything. \"dont care about cargo/rustc output/style can change since blaze do things the BEST way if made from scratch\" (Andrew, 2026-11-04), when `blaze test` printed 250 identical green `Compiling x v0.0.0 (/abs/path)` lines in cargo's style.",
	},
	{
		id: "blaze-uses-its-extensions",
		skill: "compiler-correctness",
		line: "blaze's own crates use blaze's language extensions (`#[blaze::pure]`, effects, contracts, ...) directly, enforced, with no rustc-compatible fallback or cfg guard.",
		info: "Only blaze builds blaze, so blaze's own source is blaze-only Rust: use its extensions (`#[blaze::pure]`, `nopanic`/`noalloc`/`nonblocking`, `#[blaze::linear]`, contracts, comptime, sealed traits) wherever they make blaze more correct or more incremental, and rely on them being checked. No `cfg_attr` guard, no rustc-compatible copy, no inert-under-rustc fallback: an extension blaze can't check yet is an error to fix in blaze. The one limit is stage 0: an extension blaze's source uses must be one the stage-0 blaze (`.github/blaze-version`) already reads, so a new extension lands in a release before blaze's source depends on it.",
		reason: "\"now that we are only blaze can we do proper blaze pure invalidation ... with our custom language features? mention in AGENTS.md we can use custom language features in blaze with no fallback cause building with blaze\" (Andrew, 2026-11-03). Until then `#[blaze::pure]` was written to stay inert under rustc, so it promised nothing the build relied on.",
		link: "docs/future/bootstrap.md",
	},
	{
		id: "verify-on-real-projects",
		skill: "build-and-test",
		line: "Judge blaze by building big real projects with blaze and running their own test suites, never by building them with rustc or cargo to compare.",
		info: "A verdict on blaze comes from real projects built by blaze: blaze itself, then large open-source workspaces (ripgrep, nushell, bevy, wasmtime, tokio, ...), each built with `blaze build` and its own tests run with `blaze test`. A project whose tests pass under blaze is correct for what they cover; a failing test is a blaze bug to fix. Never build a project with rustc or cargo to diff against: no reference build, no output comparison with rustc's, no harness that runs cargo for an oracle. A test's expectation states what is right, not what rustc printed.",
		reason: "\"we shold not even comapre to rustc/carog if we want to see hitns work biuld other main big proejects with blaze and see if tests owrk etc\" (Andrew, 2026-11-03).",
		link: "docs/harness.md",
	},
	{
		id: "own-nightly",
		skill: "build-and-test",
		line: "blaze builds every project with its own pinned nightly (`harness/toolchain`), ignoring rust-toolchain files, rustup and `RUSTC`.",
		info: "blaze is its own toolchain: it builds every project with its pinned nightly (`harness/toolchain`, bumped daily), ignoring rust-toolchain files, rustup, `RUSTUP_TOOLCHAIN` and `RUSTC`. Projects pin the blaze version, not Rust's.",
		reason: "\"You should use your own nightly that you control since you are trying to replace rustc\" (Andrew, 2026-10-26). \"blaze should be the latest-nightly equivalent\" (Andrew, 2026-10-27, #404). \"ZERO Rust toolchain pinning\" (Andrew, 2026-10-28).",
		link: "docs/toolchain.md",
	},
	{
		id: "sandbox-everything",
		skill: "compiler-correctness",
		line: "Build scripts and proc macros always run sandboxed; where no sandbox can be made, blaze stops.",
		info: "Build scripts and proc macros always run sandboxed, on every platform; where no sandbox can be made, blaze stops. Sandboxed tests are an opt-in experiment. The only ways out are the documented manifest opt-outs and `BLAZE_SANDBOX=off`; nothing new that runs user code runs outside by default.",
		reason: "\"Everything should by default be in sandbox and yea it should have sandbox support macos\" (Andrew, 2026-10-26). Sandboxed tests became an opt-in experiment on 2026-10-27 (Andrew).",
		link: "docs/cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox",
	},
	{
		id: "proc-macros-on-wasm",
		skill: "compiler-correctness",
		line: "Proc macros compile to `wasm32-wasip1` with blaze's own front end and run in Wasmtime; Cranelift never generates native code.",
		info: "A proc macro runs as a WebAssembly module: blaze compiles the macro crate to `wasm32-wasip1` with its own front end and code generator (never rustc), and Wasmtime runs it in process, Cranelift as its compiler, behind the `proc_macro` bridge, whose shims are its only imports. Cranelift's code generator runs only there, inside Wasmtime for wasm. Native code (every target blaze builds for, debug or release) is never generated by Cranelift: it is copy-and-patch stencils or LLVM, a function with no stencil is a typed error naming it, and there is no Cranelift tier between them. Cranelift's IR types (`cranelift_codegen::ir`) may stay as the stencils' input until blaze has its own IR. The MIR evaluator stays for const eval and as the reference a macro's wasm output is tested against; nothing hand-rolls a wasm-style sandbox (`blaze-macro-eval`'s `tier.rs` is replaced by this).",
		reason: "Andrew, 2026-11-01: \"modify cranelift ONLY for wasm stuff for native code dont use it ... properly use cranelift for proc macros\". On 2026-10-30 the wasm host was deleted for one engine on blaze's own compiler, but the compiled tier that was to make it fast stayed off, so every derive ran interpreted: two samples of build 53 showed the busy threads in the MIR evaluator while about 56 waited, on about 2 of 18 cores. `tier.rs` rebuilt by hand what a wasm runtime does (a 4 GiB region, guard pages, a shadow stack).",
		link: "docs/v2/plan/lanes/usercode/macros.md",
	},
	{
		id: "bump-allocate",
		skill: "runtime-code",
		line: "Allocate by lifetime (`Vec::new_in(frame)`); a global-heap allocation needs `// alloc: <why>` on its line.",
		info: "Allocate by lifetime, never from the global heap by default: a collection that lives within one task (a query run, a codegen unit, a maintainer pass) names its allocator, `Vec::new_in(frame)` / `Box::new_in(.., frame)` from `blaze_mem::frame::current`, and longer lifetimes use the request, revision or image allocator. A global allocation (`Vec::new`, `vec![]`, `collect` into a heap `Vec`, `Box::new`, `to_owned`) needs `// alloc: <why>` on its line. `blaze::implicit_allocator` enforces this, warning everywhere and denied wherever `blaze::hot` is, with `blaze::hot_alloc` for what a call reaches.",
		reason: "\"Allocate like a highly optimized game\" (Andrew, 2026-10-29, decisions.md). Andrew, 2026-10-31, on a fan-out that collected results into a heap Vec it did not need: heavily bump allocate, always give the Vec its allocator, and lint it.",
		link: "docs/v2/plan/design/memory.md",
	},
	{
		id: "standard-structures",
		skill: "runtime-code",
		line: "One standard structure per need (parking_lot locks, papaya and blaze_hash maps, crossbeam queues and channels, atomics); a wait on a pool thread goes through the scheduler.",
		info: "One choice per need, never a hand-rolled or std stand-in: locks are `parking_lot::{Mutex, RwLock}`, held only for a field update and never across a query call; a shared map is `papaya::HashMap`; a single-owner map or set is `blaze_hash::{HashMap<K, V, A>, HashSet<K, A>}` (foldhash with a fixed seed, its allocator named: a frame's for a task, `Global` with a reason otherwise), with `blaze_hash::KeyHasher` for keys that are already content hashes and `blaze_hash::Blake3Hasher` for persisted content IDs; results of parallel items come back from `blaze_graph::par` in order, never pushed into a `Mutex<Vec>`; a queue is `crossbeam::queue::{SegQueue, ArrayQueue}`; a channel is `crossbeam-channel`; a counter or flag is an atomic. On a pool thread or fiber, a wait goes through the scheduler (a key, a counter, a job), never a `Condvar`, a channel receive, `thread::sleep` or a raw `spin_loop`; elsewhere a spin is the one bounded spin-then-park helper and a delay is a deadline in the sans-IO core or a driver's timer. `BTreeMap` only where iteration order is part of the output.",
		reason: "Andrew, 2026-11-01: \"use the best data structure in general\", \"use proper hasher too\", parking_lot with std's Mutex denied. On macOS std's Mutex parks in the kernel with no spin; FxHash clusters on integer and pointer keys; a startup stall on 2026-11-01 was workers spinning with nothing recording why.",
	},
	{
		id: "fix-through-blaze-lints",
		skill: "changing-code",
		line: "Move a type, path, API or idiom across the code with a `.clippy.toml` ban and `blaze check --fix` (or `blaze check --replace A=B`), by name resolution, never by text.",
		info: "A change that moves a type, path, API or idiom across the code is a ban with a `replacement` in `.clippy.toml` (mirrored in the hot crates' `clippy.toml`), which blaze-lints' `disallowed_types`/`disallowed_methods`/`disallowed_macros` enforce with a MachineApplicable fix, or a `blaze::` lint where the rule needs types or MIR; either is applied with `blaze check --fix`, and its count lives in `crates/blaze-lints/baselines/`, which may only shrink; a one-off move of a path is `blaze check --replace A=B`. Both find code by name resolution (imports, renames, globs, re-exports, macro output), never by text, and leave it compiling and lint-clean, so the rule keeps holding after the port. `sed`, `rg` replace and syntax-only rewriters (astlog) are for syntax-only shapes, scoped and checked by a build afterward.",
		reason: "Andrew, 2026-11-01: \"make our own version of clippy that is incremental and has suggestions for replace\" and \"recommend doing find replace with blaze lints\". A text or tree-sitter rewrite of `std::sync::Mutex` misses `use std::sync::{Arc, Mutex}`, renames and globs, and can hit an unrelated `Mutex`; blaze-lints already runs per body as front-end queries.",
	},
	{
		id: "sans-io-cores",
		skill: "runtime-code",
		line: "Every core is a synchronous state machine on an injected clock that owns no IO; drivers do the IO and stay thin.",
		info: "Every core (a scheduler decision, a protocol, a retry or backoff policy, a lease, a maintainer's plan) is a synchronous state machine that reads time only from an injected clock (`advance_clock(now)`), owns no socket, file, thread, timer or sleep, and hands back decisions and deadlines (`take_decisions`, `next_deadline`). Drivers do the IO and stay thin. A test replays a transcript with no real clock.",
		reason: "Andrew's rule for ix (2026-10-25), and `blaze_graph::sched` is built this way. A core that sleeps or reads wall time can only be tested by waiting, and those tests flake.",
	},
	{
		id: "every-wait-says-what",
		skill: "runtime-code",
		line: "Every blocking wait opens a span naming what it waits for.",
		info: "A thread or fiber that blocks says what it waits for: the wait opens a span naming the awaited key, lock, channel, IO or child, and its owner when known. A wait with no name, and time on a worker covered by no span, are bugs; the profile test fails on them.",
		reason: "2026-11-01: 18 codegen workers sat 77 s each in build 24 with no span saying why; only a bisect found part D's apply phase. 19,342 wait spans in that build carried no target because the table dropped the field.",
	},
	{
		id: "slow-is-p0-traced",
		skill: "perf-and-tracing",
		line: "Anything slow is a P0 until a span names it; the trace comes before the speed fix, and the fix quotes before and after from the spans.",
		info: "Anything slow is a P0 until a span names it, and the trace comes before the speed fix. Every stretch of worker time sits under a `tracing::info_span!(target: \"blaze\", ...)` that names the work: the macro path for a proc-macro expansion, the package, version and target kind for a unit, the query and key, the creator of a scope being waited on. Its own CPU time (`self_cpu_ns`) is filled, so a DuckDB query over `<BLAZE_HOME>/v2/profile/*.spans.parquet` says which item took the time and whether it was busy or waiting. A sampler stack or a guess from a log is a lead. The PR that makes something faster quotes the before and after from those spans.",
		reason: "Andrew, 2026-11-01: \"whenever something is slow p0 make sure it is traced properly\". Build 53 ran 917 s on about 2 of 18 cores and its spans could not say why: no span for proc-macro expansion, `self_cpu_ns` NULL in every row, every build script recorded as `build-script-build`, and waits named `scope 13`. A claim that one derive took minutes came from the sampler, and the spans did not back it.",
	},
	{
		id: "bounded-progress",
		skill: "runtime-code",
		line: "Every loop that waits for progress has a bound and ends in a typed outcome; never an open loop or an uncapped spin.",
		info: "Every loop that waits for progress (a solver, a fixpoint, a readiness probe, a retry) has a bound (fuel, depth or a deadline from the injected clock) and ends in a typed outcome: done, or a failure naming what it was doing, the goal or key, and how far it got. Never an open loop whose only exit is success, and never a spin with no cap.",
		reason: "2026-11-01: two self-builds ran 300 s with workers busy in type inference and the trait solver and no unit finishing; three more stalled spinning in `Flights::get_or_run_named` before any crate started. Each needed a sampler and a person to say what was stuck.",
	},
	{
		id: "errors-name-the-thing",
		skill: "runtime-code",
		line: "An error names the item it is about and the rule that refused it.",
		info: "An error names the item it is about (the function or instance with its path, the crate, the key, the file and line) and the rule that refused it, in the message itself. \"a template made against 6 references is given 7\" is unfinished until it says which function and which template.",
		reason: "2026-11-01: several self-build errors named only a hash or a count, so each round spent a person's time finding the function before anyone could fix it.",
	},
	{
		id: "panics-fail-one-job",
		skill: "runtime-code",
		line: "A panic fails only its job, with a typed error; the daemon keeps serving and the build keeps going.",
		info: "A panic inside a job (a query, a codegen unit, a proc-macro evaluation) fails that job's units with a typed error naming the panic and the job; the daemon keeps serving and keep-going (the default; `--fail-fast` opts out) keeps building the rest. Formatting an error must not panic: render what needs the database while the database is attached.",
		reason: "2026-11-01: a proc-macro evaluation error panicked while being formatted (its type needed the attached database), and the daemon dropped whole self-builds at 84 crates until it was found.",
	},
	{
		id: "keys-cover-every-input",
		skill: "compiler-correctness",
		line: "A key covers every input its value depends on; two values under one key is a typed refusal, and the fix is the key.",
		info: "A query key, store key, fold key or template key covers every input its value depends on, including the data a constant points to and the identity of a closure callee. Two different values under one key is a refusal with a typed error, never an overwrite, and the fix is the key, never the check.",
		reason: "2026-11-01: function folding merged bodies whose constants pointed at different data, two closures of one signature shared one instance, and the store refused other bytes under `Key(b75576429138)`. Each was a key missing an input.",
	},
	{
		id: "measure-the-exact-tree",
		skill: "build-and-test",
		line: "A measurement, bisect or checkpoint names and keeps the exact tree it ran on.",
		info: "A measurement, bisect or checkpoint names the exact tree it ran on (a content digest of the sources and the blaze binary's hash) and keeps that tree; never re-sync or rebuild between measuring and committing. A result you can't tie to a tree is not evidence.",
		reason: "2026-11-01: round 30 was committed from a snapshot taken 10 s after the next sync began, so main got a tree that built 214 crates instead of the measured 334, and a warm-cache theory was built on it before a cold run disproved it.",
	},
	{
		id: "test-the-cause",
		skill: "build-and-test",
		line: "Test a causal claim by removing the cause and watching the effect go.",
		info: "A causal claim (this change caused the regression, this lock causes the convoy, this index makes it fast) is tested by removing the cause and watching the effect go, not by checking that its premises hold. Pin a load-bearing one with a test named for the property.",
		reason: "2026-11-01: part D was blamed for the codegen convoy on timing alone; one run with part D's files reverted took codegen from 1,485 s to 473 s and the over-30 s units from 18 to 0, which is what made it a finding.",
	},
	{
		id: "whole-ports",
		skill: "changing-code",
		line: "A change to a shared signature or type updates every caller in the same batch, so the tree compiles at every save point.",
		info: "A change to a shared signature or type (a struct's fields, a trait, a lock type) updates every caller in the same batch, so the shared tree compiles at every save point. Do it through `blaze check --fix`, `blaze check --replace` or a scoped rewrite, then build, then hand the batch over. A half-ported tree blocks every lane's run.",
		reason: "2026-11-01: the self-build loop sat blocked on `Maintainer { run }` after its fields changed, on `LifetimeParamData::is_fresh` callers, and on `into_inner().ok()` after the parking_lot port, each a signature changed without its callers.",
	},
	{
		id: "deterministic-output",
		skill: "compiler-correctness",
		line: "No output depends on hash-map order, thread timing, addresses or the wall clock; a test builds twice and compares bytes.",
		info: "No output (an object, a diagnostic's order, a store key, a profile row's identity) depends on hash-map iteration order, thread timing, addresses or the wall clock. Hashers have fixed seeds, except a table whose keys an attacker may pick (names, paths, literals), which is keyed by a `blaze_hash::SipKey`: random when nothing persists its layout and nothing output depends on where a key lands, or `blaze_ids::index_key` of its content when an image lays it out; a hot one may instead be adaptive (`blaze_hash::adaptive`): the fixed hash until a lookup walks a run no honest keys make, then a random `SipKey` for good. Anything iterated into output is sorted or ordered by construction; a test builds twice and compares bytes.",
		reason: "Content-addressed caching and replay need identical bytes from identical inputs; a nondeterministic key shows up as a store refusal or a cache that never hits.",
	},
	{
		id: "persisted-state-round-trips",
		skill: "changing-code",
		line: "Anything persisted has a restore test equal byte for byte to a fresh build; a restore never silently starts empty.",
		info: "Anything blaze persists (the daemon image, the store, the plane's history) has a test that restores it and checks the result equals a fresh build byte for byte, and a budget for restart to first answer. A restore that silently starts empty is a failure the daemon reports, naming why.",
		reason: "2026-11-01: the self-build home held a 1.8 GB image while the daemon log said \"the database starts empty: there is no daemon image\", and nothing measured whether a restart used it.",
	},
	{
		id: "no-wait-across-thread-locals",
		skill: "runtime-code",
		line: "Hold nothing tied to a thread (a thread-local borrow, a lock guard, an `Rc`) across a wait.",
		info: "A fiber can resume on another thread after any wait, so nothing held across a wait may be tied to a thread: no thread-local borrow, lock guard or `Rc`. Mark regions that must not wait with `NoWait`; read thread-locals in `#[inline(never)]` functions that end before any wait.",
		reason: "2026-10-31: a job's carried thread-local was put back on a thread holding another job's (`stdx/src/lend.rs:94`), and a frame's state crashed in `blaze_mem::frame::Frame::inner`, both from state crossing a fiber switch.",
	},
	{
		id: "best-long-term-design",
		skill: "changing-code",
		line: "Pick the best long-term design, not the least work, and spend the effort proving it right.",
		info: "Pick the best long-term design, not the least work: own the layer or port rustc's rule properly, and spend the effort on proving it right: real crates' own test suites passing under blaze, and fuzzing of blaze's own invariants.",
		reason: "Agents write most of blaze, so the amount of code a design takes is not what limits it; what stays expensive is proving it right. Andrew, 2026-10-30, deleting the harnesses that compared blaze with rustc or cargo: \"we use other ppl['s] test[s] [of] the other crates ... it over-constrains us\".",
		link: "docs/contributing.md#rules-for-code",
	},
	{
		id: "one-concept-one-home",
		skill: "changing-code",
		line: "One type, one rule and one place per concept; merge duplicates and ban the removed name.",
		info: "Each concept has one type, one rule and one place it is computed: two types for the same thing (a `BuiltFor` in two crates), two copies of a rule (where code runs, what a key covers), two paths that do the same job (two schedulers, two evaluators, two tiers for one level) are a bug. Merge them into the better one, delete the other, and ban its name with a `replacement` (`fix-through-blaze-lints`); a change that finds a duplicate removes it or files an issue naming both.",
		reason: "Andrew, 2026-11-02: \"try to remove duplicate concepts do everything best long term way\". Duplicates drift apart: two `BuiltFor` types and two copies of the runs-here rule let a dev `--target <host>` build link a std made for the wrong CPU.",
	},
	{
		id: "semantics-in-front-end",
		skill: "compiler-correctness",
		line: "Language semantics go in the front end's query crates in `crates/` (blaze-query's engine, derived from salsa, not salsa); codegen, the store, the daemon and the runtime stay outside.",
		info: "Language semantics (MIR building and passes, borrow checking, const eval, layout, code-identity hashes) go in the front end's crates in `crates/` (`hir-ty`, `hir-def` and the rest, derived from rust-analyzer but no longer synced with it), whose queries run on blaze's own engine (`blaze_query::db`, crates/blaze-query: derived from salsa but not salsa; dense ids, in memory, one view at a time; persisted, shared results go through blaze-kernel), in a crate of their own where possible. Codegen, the store, the daemon, cargo integration and the runtime stay outside the front end.",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "library-over-language",
		skill: "compiler-correctness",
		line: "Build a feature as a library on the language that exists; when no library can express it, add the smallest general primitive, never an ad hoc language feature.",
		info: "Before adding an attribute, intrinsic or special case to the compiler, write the feature as library code on what the language already has: traits, generics, const eval, compile-time reflection (`core::mem::type_info`), allocators, effects and the purity proof. When it cannot be written that way, the language is missing a primitive: add the smallest general one that makes the library possible (nightly's API where one exists, otherwise an opt-in blaze extension), and keep the set of primitives small. The compiler proves and exposes facts (purity, code identity, layout, reflection); policy (caching, storage, hash formats, how code is executed) lives in libraries anyone can read or replace. Example: `Canonical` is library code over `type_info`, and `#[blaze::cache]` lowers to a call on a library `Executor`; the compiler generates neither the hashing nor the caching.",
		reason: "Andrew, 2026-11-03, designing `#[blaze::pinned]`: \"you don't need to add ad hoc language features when you can just do things with [the] existing language, and if you can't you probably do not have a powerful enough language and you need to improve the primitives\".",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "no-mcp",
		skill: "docs-and-scripts",
		line: "No MCP: agent interfaces are a Unix socket or HTTP with one versioned schema in deterministic CBOR.",
		info: "No MCP. Agent-facing interfaces are a Unix socket or HTTP with one versioned schema in deterministic CBOR; the CLI is a client of the same API.",
		reason: "MCP is cringe and outdated.",
		link: "docs/roadmap.md#8-the-agent-api",
	},
	{
		id: "work-in-ix-vms",
		skill: "build-and-test",
		line: "Builds, tests, benchmarks and experiments run on the Mac or in an ix VM, never on AWS.",
		info: "Builds, tests, benchmarks and experiments run on the Mac or in an ix VM (`ix new`, `ix shell`, `ix rm`), never on AWS. Where neither can do it, stop and say what is missing.",
		reason: "Andrew, 2026-10-27 (ix VMs, no AWS dev hosts); the Mac allowed again 2026-11-01. It replaced heavy-work-on-dev-host (AWS dev hosts claimed in the host ledger).",
		link: "docs/contributing.md#working-in-parallel",
	},
	{
		id: "fan-out-write-only",
		skill: "build-and-test",
		line: "Subagents only write code; the main agent applies all of it to one tree and runs `blaze check` and `blaze test` once.",
		info: "Fan out code, not builds. Subagents only read and write code, including the tests for it. They never run a build, check, test, clippy, benchmark or any other expensive step, and never start a daemon or a VM. The main agent waits until every subagent has finished writing, applies all their changes to one tree, and then runs it once: `blaze check` and `blaze test` (the central loop), with keep-going. It hands each failure back to a fresh subagent for the code that broke, with the exact tree, the command and the error, and runs again. Checks that need a variant tree, such as reverting a fix to show its test fails (test-the-cause), are also run by the main agent, as patches.",
		reason: "\"sub agents should NEVER do tests/check any expensive operation, just write code, and then when all subagents are done writing code the main agent actually tests them (blaze test) and compiles\" (Andrew, 2026-11-03). That night about 20 lanes each built and tested their own snapshot through a shared queue: a median 14 min per request, 312 s of it waiting behind other lanes' compiles. Twenty-one background waiters piled up, and the same crates were rebuilt for every lane. One build of the combined tree, kept warm, does that work once.",
	},
	{
		id: "worktree-per-lane",
		skill: "build-and-test",
		line: "One uniquely named worktree per lane; never `git stash`.",
		info: "One uniquely named worktree per lane; its build directory is its own under blaze's root, never in the checkout, so it needs no `CARGO_TARGET_DIR`; never `git stash`.",
		reason: "The stash is shared by every worktree.",
	},
	{
		id: "land-through-pr",
		skill: "landing",
		line: "Land small and often through a pull request with `.github/scripts/land.py`; never push to main.",
		info: "Land small and often through a pull request: `python3 .github/scripts/land.py \"what the branch does\"` opens it, turns on auto-merge and waits for the merge gate (`--wait` resumes). Harnesses run after merge in the release train. Never push to main.",
		link: "docs/contributing.md#landing-a-change",
	},
	{
		id: "own-prs-to-merge",
		skill: "landing",
		line: "Any agent may take any open pull request or issue to done; tell the owning lane what changed.",
		info: "Any agent may take any open pull request or issue to done (merge main, resolve conflicts, fix its gate, finish a draft) rather than hand it back; tell the owning lane what changed. Close one only when it is wrong, saying why.",
		reason: "Andrew, 2026-10-28: \"u can work on drafts and get them completed, dont care about draft state; u can control and fix issues instead of sending back to owner, u should resolve ideally\", and \"be agentic wrt prs\".",
	},
	{
		id: "clean-up-lane",
		skill: "build-and-test",
		line: "When a lane is done, delete its build directory, worktree, ix VM and merged branch.",
		info: "When a lane is done, delete its build directory (`blaze clean`), worktree, ix VM and merged branch.",
	},
	{
		id: "credit-people",
		skill: "landing",
		line: "Credit a suggestion, diagnosis or patch with a `Co-authored-by:` trailer.",
		info: "A commit or pull request built on someone's suggestion, diagnosis or patch gets a `Co-authored-by: Name <id+login@users.noreply.github.com>` trailer for each of them and says where it came from.",
		link: "docs/contributing.md#credit",
	},
	{
		id: "docs-say-what-they-are",
		skill: "docs-and-scripts",
		line: "Every doc under `docs/` says in front matter whether it is current, plan, record or superseded; keep current docs true.",
		info: "Every doc under `docs/` has front matter: `current` (kept true), `plan`, `record` (dated history) or `superseded` (naming its replacement). Trust current docs; read plans and records as of their date. A change that makes a current doc untrue fixes it or marks it superseded in the same change; `python3 tools/docs.py check` holds them to the tree.",
		reason: "Andrew, 2026-10-28: docs should \"be structured or something so can say things are outdated filter those out\". Agents followed AGENTS.md's stale `vendor/rust-analyzer` after the front end moved to crates/.",
		link: "docs/contributing.md#documents",
	},
	{
		id: "scripts-take-typed-input",
		skill: "docs-and-scripts",
		line: "A script takes one JSON value parsed into a frozen dataclass, not flags or environment variables.",
		info: "A script takes one JSON value (a file path, `-` or inline), parsed into a frozen dataclass that refuses unknown fields, not a set of flags or hidden environment variables. An existing script moves to this when next changed.",
		reason: "Andrew, 2026-10-28: \"for parser things it should be typed like json instead of cli args, json through a file or stdin or a single value\". Flags drift and are untyped; one JSON value is checkable and the same from a workflow, a person or another script.",
	},
] as const satisfies readonly Rule[];

/**
 * The "## Code Review Rules" section of AGENTS.md, which Codex code review follows on pull requests
 * (https://learn.chatgpt.com/docs/third-party/github). Only checks a reviewer has to reason about:
 * formatting, lint and the harnesses stay in CI.
 */
const reviewRules = `## Code Review Rules

These guide Codex code review on pull requests. They add to the harnesses and a maintainer's approval; they replace neither.

### Compiler correctness

- Flag any change that can make a program built by blaze behave differently from the same program built by the pinned nightly rustc (\`harness/toolchain\`): layout, ABI, drop order, overflow and panic behavior, const eval, trait and method resolution, MIR passes, borrow checking, linking. Safe path: port rustc's rule and add a test (a crate whose own tests exercise it, or a unit test). The allowed differences are sandboxing build scripts and proc macros, and refusing, with a hard error naming the hole, a program rustc accepts only through a soundness bug (\`superset-of-rust\`). Code that opts into a blaze extension is exempt: it need not build under rustc, but enabling the extension must not change any program that doesn't use it.
- Flag a fallback: a path that, when blaze can't do something, quietly takes another route (rustc or cargo, rustc's copy of a function or static, a whole rebuild in place of a refused patch, a slower backend, an older cached result). Safe path: stop with an error that names what and where.
- Flag a cache key, code-identity hash or live-patch check that misses an input the output depends on (source, cfg, features, target, toolchain, layout of a type it uses), so stale code can be reused or a patch applied after a layout or ABI change.
- Flag unsafe code in the loader, the splice and the live runtime whose soundness rests on an unchecked assumption (alignment, lifetime of patched code, aliasing, thread safety of a swap), unless a comment states the invariant and where it is upheld.

### Security

- Flag anything that runs user code (build scripts, proc macros, tests in the sandboxed-tests experiment) outside the sandbox by default, or widens what the sandbox allows (writes outside \`OUT_DIR\`, network, new syscalls or paths), unless it goes through one of the documented opt-outs.
- Flag the store, remote cache or daemon trusting bytes it did not verify: an artifact used without checking its content hash, archive or path entries that can escape their directory, a socket or HTTP endpoint reachable by other users, or CBOR from the network decoded without size limits.
- Flag secrets (tokens, keys) written to logs, CI output, artifacts or the store, and workflows that expose secrets to code from forked pull requests.`;

/** Hard-wraps `text` at `width` columns, continuation lines indented by `indent`. */
function wrap(text: string, width: number, indent: string): string {
	const out: string[] = [];
	let line = "";
	for (const word of text.split(" ")) {
		const next = line === "" ? word : `${line} ${word}`;
		if (next.length > width - (out.length === 0 ? 0 : indent.length) && line !== "") {
			out.push(line);
			line = word;
		} else {
			line = next;
		}
	}
	out.push(line);
	return out.join(`\n${indent}`);
}

const generatedNote = "Generated by `bun tools/agents/rules.ts --write` from tools/agents/rules.ts";

/** AGENTS.md: the rules, numbered, one line each with its skill. */
export function agentsMd(): string {
	const items = rules.map((rule: Rule, i) => {
		const n = `${i + 1}. `;
		const indent = " ".repeat(n.length);
		return `${n}${wrap(`**${rule.id}**: ${rule.line} Skill: \`${rule.skill}\`.`, 100, indent)}`;
	});
	const note = `<!-- ${generatedNote}; edit that file, not this one. -->`;
	const review = reviewRules
		.split("\n")
		.map((line) => (line.startsWith("- ") ? wrap(line, 100, "  ") : wrap(line, 100, "")))
		.join("\n");
	return `${note}\n${header.split("\n").map((line) => wrap(line, 100, "")).join("\n")}\n\n${items.join("\n")}\n\n${review}\n`;
}

const root = `${import.meta.dir}/../..`;
const skillPath = (id: SkillId) => `${root}/.agents/skills/${id}/SKILL.md`;
/** Ends a skill's generated part; what follows it is hand-written and kept by --write. */
const endMarker = "<!-- End of the generated rules. Hand-written notes follow. -->";

/** A skill's generated part, through the end marker: front matter, title and its rules in full. */
export function skillHead(id: SkillId): string {
	const skill = skills[id];
	const sections = rules.flatMap((rule: Rule, i) => {
		if (rule.skill !== id) return [];
		// Links are written from the repository root; the skill sits three directories down.
		const see = rule.link === undefined ? "" : `\n\nSee [${rule.link}](../../../${rule.link}).`;
		return [`## ${rule.id} (rule ${i + 1})\n\n${wrap(rule.info, 100, "")}${see}`];
	});
	if (sections.length === 0) throw new Error(`tools/agents/rules.ts: skill ${id} has no rules`);
	return `---\nname: ${id}\ndescription: ${JSON.stringify(skill.description)}\n---\n\n# ${skill.title}\n\n<!-- ${generatedNote}, down to the end marker; edit the rules there. AGENTS.md carries each rule as one line. -->\n\n${sections.join("\n\n")}\n\n${endMarker}\n`;
}

/** The hand-written part of a skill's file: everything after the end marker. */
function skillTail(id: SkillId, text: string | undefined): string {
	if (text === undefined) return "";
	const at = text.indexOf(endMarker);
	if (at === -1) throw new Error(`${skillPath(id)}: no end marker \`${endMarker}\`; restore it, or delete the file and run --write`);
	return text.slice(at + endMarker.length + 1);
}

async function readIfExists(path: string): Promise<string | undefined> {
	const file = Bun.file(path);
	return (await file.exists()) ? await file.text() : undefined;
}

const agentsPath = `${root}/AGENTS.md`;
const skillIds = Object.keys(skills) as SkillId[];
const [flag, ...rest] = process.argv.slice(2);
if (flag === undefined || rest.length > 0) throw new Error(`tools/agents/rules.ts: one flag, --write or --check, got ${process.argv.slice(2).join(" ")}`);
switch (flag) {
	case "--write":
		await Bun.write(agentsPath, agentsMd());
		for (const id of skillIds) {
			const tail = skillTail(id, await readIfExists(skillPath(id)));
			await Bun.write(skillPath(id), skillHead(id) + tail);
		}
		break;
	case "--check": {
		const stale: string[] = [];
		if ((await Bun.file(agentsPath).text()) !== agentsMd()) stale.push("AGENTS.md");
		for (const id of skillIds) {
			const text = await readIfExists(skillPath(id));
			if (text === undefined || !text.startsWith(skillHead(id))) stale.push(`.agents/skills/${id}/SKILL.md`);
		}
		if (stale.length > 0) {
			console.error(`stale: ${stale.join(", ")}: run \`bun tools/agents/rules.ts --write\` and commit them`);
			process.exit(1);
		}
		break;
	}
	default:
		throw new Error(`tools/agents/rules.ts: unknown flag ${flag} (--write or --check)`);
}
