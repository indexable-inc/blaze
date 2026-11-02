// blaze's rules for agents: the one source. `info` is the rule, short and imperative, and goes
// into AGENTS.md, which Claude Code, Codex and Cursor read at the start of every session; the detail
// lives in the linked doc. `reason` is why (Andrew's words, dates, history) and stays here.
//
//   bun tools/agents/rules.ts --write    regenerate AGENTS.md
//   bun tools/agents/rules.ts --check    fail if AGENTS.md is not what --write would write (CI, fmt job)
//
// AGENTS.md also carries `reviewRules`, the "## Code Review Rules" section Codex code review reads
// on every pull request.
//
// A rule's identity is its `id` (kebab case, never reused); its number is only its position in the
// printed list and moves when a rule is inserted.

type Rule = {
	/** Stable kebab-case name. */
	id: string;
	/** The rule itself: what an agent must do, in one to three sentences. */
	info: string;
	/** Why: quotes, dates, issues, history. Not in AGENTS.md. */
	reason?: string;
	/** Where the detail is, relative to the repository root. */
	link?: string;
};

const header = `# blaze

How to build, test and change blaze: docs/contributing.md. Every doc: docs/README.md. What the harnesses check: docs/harness.md. These rules are generated from tools/agents/rules.ts (edit there, then \`bun tools/agents/rules.ts --write\`). Skills: .agents/skills/.`;

export const rules = [
	{
		id: "no-rustc",
		info: "blaze never runs rustc: no function, crate, std, build script, proc macro, test or link step goes to rustc, rustdoc or a cargo that runs them. Each place that still does is a bug with a `no-rustc` issue, to be removed, never extended.",
		link: "docs/no-rustc.md",
	},
	{
		id: "blaze-builds-blaze",
		info: "Build and test blaze with blaze (`blaze build`, `blaze test`), never `cargo build`/`cargo test` or sccache. Where blaze can't build itself yet, fix that.",
		reason: "\"never use sccache never use rustc\" and \"we should make sure we get blaze to fully build itself! basically blaze sucks until then\" (Andrew, 2026-10-26).",
		link: "docs/contributing.md",
	},
	{
		id: "no-fallback",
		info: "No fallback, ever. When blaze can't do something it stops with an error naming what and where, never quietly takes another path (rustc, a full link or rebuild instead of a patch, a slower backend, an older cached result, a default guessed for a missing value). A fallback that only logs its reason is still a fallback. A change that touches an existing fallback turns it into an error, and a bench or test that hits one fails instead of timing the slow path.",
		reason: "\"No fallback EVER, no rustc, prefer quick error so bug can be fixed\" (Andrew, 2026-10-26). \"remove fallbacks, should just fail (so can hill climb easier)\" (Andrew, 2026-10-30). A silent fallback hides the bug and turns it into wrong output (#386, #392); an error gets it fixed. A fallback also hides a speed bug: on 2026-10-30 a generic edit took 33 s on Linux against cargo's 32 s because the patch path quietly fell back to a full link (\"no executable to add to\"), while the same edit patched in 6.3 s on macOS. A failure points at the one broken step; a slow number does not.",
	},
	{
		id: "superset-of-rust",
		info: "Own every layer from source to running code, never by narrowing what blaze accepts: every program the pinned nightly builds must build and behave the same under blaze, with the real std, C dependencies, build scripts and proc macros. The one allowed difference is sandboxing. Superset means blaze may also accept more than Rust: an extension (new syntax, attributes, semantics) only has to work under blaze, never under rustc, as long as it is opt-in and every program that doesn't use it behaves exactly as under rustc.",
		reason: "Owning every layer is what makes blaze fast. Sandboxing has been on by default since 2026-10-26 (https://github.com/indexable-inc/blaze/issues/147). Andrew, 2026-10-28: \"we are superset and if using superset dont need rustc to compile it either\".",
		link: "docs/no-rustc.md",
	},
	{
		id: "useful-not-parity",
		info: "rustc and cargo are references, never specs. Judge everything blaze does by how useful it is to the person or agent using it, not by how closely it matches them: errors, warnings, lints, CLI commands and flags, printed and JSON output, progress, the target directory's layout, file formats, caches, symbol names, debug info, what a build or test run does and in what order. Match them only where real code or tools depend on it (a program's behavior, a published crate's manifest, an editor reading the JSON); everywhere else pick the better design and delete the copy. Tests check what makes the result useful and correct, never that it equals rustc's or cargo's. `superset-of-rust` is unchanged: every program the pinned nightly builds still builds and runs as it does under rustc.",
		reason: "\"we dont need word for word ... be clear functional parity also not needed we care about usefulness just in general wrt rustc\" and \"make the rule MORE GENERAL than that not just errors\" (Andrew, 2026-10-31). On 2026-10-31 166 of blaze-diagnostics' 1223 tests failed, most on text that differed from rustc's (`{integer}` against `i32`, a span one column off, a lint count) and not on whether the error was useful. The plan's \"conformance only where real code depends on it\" (2fb77406fb) said this for behavior; this rule says it for everything.",
	},
	{
		id: "own-nightly",
		info: "blaze is its own toolchain: it builds every project with its pinned nightly (`harness/toolchain`, bumped daily), ignoring rust-toolchain files, rustup, `RUSTUP_TOOLCHAIN` and `RUSTC`. Projects pin the blaze version, not Rust's.",
		reason: "\"You should use your own nightly that you control since you are trying to replace rustc\" (Andrew, 2026-10-26). \"blaze should be the latest-nightly equivalent\" (Andrew, 2026-10-27, #404). \"ZERO Rust toolchain pinning\" (Andrew, 2026-10-28).",
		link: "docs/toolchain.md",
	},
	{
		id: "sandbox-everything",
		info: "Build scripts and proc macros always run sandboxed, on every platform; where no sandbox can be made, blaze stops. Sandboxed tests are an opt-in experiment. The only ways out are the documented manifest opt-outs and `BLAZE_SANDBOX=off`; nothing new that runs user code runs outside by default.",
		reason: "\"Everything should by default be in sandbox and yea it should have sandbox support macos\" (Andrew, 2026-10-26). Sandboxed tests became an opt-in experiment on 2026-10-27 (Andrew).",
		link: "docs/cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox",
	},
	{
		id: "proc-macros-on-wasm",
		info: "A proc macro runs as a WebAssembly module: blaze compiles the macro crate to `wasm32-wasip1` with its own front end and code generator (never rustc), and Wasmtime runs it in process, Cranelift as its compiler, behind the `proc_macro` bridge, whose shims are its only imports. Cranelift's code generator runs only there, inside Wasmtime for wasm. Native code (every target blaze builds for, debug or release) is never generated by Cranelift: it is copy-and-patch stencils or LLVM, a function with no stencil is a typed error naming it, and there is no Cranelift tier between them. Cranelift's IR types (`cranelift_codegen::ir`) may stay as the stencils' input until blaze has its own IR. The MIR evaluator stays for const eval and as the reference a macro's wasm output is tested against; nothing hand-rolls a wasm-style sandbox (`blaze-macro-eval`'s `tier.rs` is replaced by this).",
		reason: "Andrew, 2026-11-01: \"modify cranelift ONLY for wasm stuff for native code dont use it ... properly use cranelift for proc macros\". On 2026-10-30 the wasm host was deleted for one engine on blaze's own compiler, but the compiled tier that was to make it fast stayed off, so every derive ran interpreted: two samples of build 53 showed the busy threads in the MIR evaluator while about 56 waited, on about 2 of 18 cores. `tier.rs` rebuilt by hand what a wasm runtime does (a 4 GiB region, guard pages, a shadow stack).",
		link: "docs/v2/plan/lanes/usercode/macros.md",
	},
	{
		id: "bump-allocate",
		info: "Allocate by lifetime, never from the global heap by default: a collection that lives within one task (a query run, a codegen unit, a maintainer pass) names its allocator, `Vec::new_in(frame)` / `Box::new_in(.., frame)` from `blaze_mem::frame::current`, and longer lifetimes use the request, revision or image allocator. A global allocation (`Vec::new`, `vec![]`, `collect` into a heap `Vec`, `Box::new`, `to_owned`) needs `// alloc: <why>` on its line. `blaze::hot_alloc` enforces this wherever `blaze::hot` is denied.",
		reason: "\"Allocate like a highly optimized game\" (Andrew, 2026-10-29, decisions.md). Andrew, 2026-10-31, on a fan-out that collected results into a heap Vec it did not need: heavily bump allocate, always give the Vec its allocator, and lint it.",
		link: "docs/v2/plan/design/memory.md",
	},
	{
		id: "standard-structures",
		info: "One choice per need, never a hand-rolled or std stand-in: locks are `parking_lot::{Mutex, RwLock}`, held only for a field update and never across a query call; a shared map is `papaya::HashMap`; a single-owner map or set is `blaze_hash::{HashMap, HashSet}` (foldhash with a fixed seed), with `blaze_hash::KeyHasher` for keys that are already content hashes and `blaze_hash::Blake3Hasher` for persisted content IDs; results of parallel items come back from `blaze_graph::par` in order, never pushed into a `Mutex<Vec>`; a queue is `crossbeam::queue::{SegQueue, ArrayQueue}`; a channel is `crossbeam-channel`; a counter or flag is an atomic. On a pool thread or fiber, a wait goes through the scheduler (a key, a counter, a job), never a `Condvar`, a channel receive, `thread::sleep` or a raw `spin_loop`; elsewhere a spin is the one bounded spin-then-park helper and a delay is a deadline in the sans-IO core or a driver's timer. `BTreeMap` only where iteration order is part of the output.",
		reason: "Andrew, 2026-11-01: \"use the best data structure in general\", \"use proper hasher too\", parking_lot with std's Mutex denied. On macOS std's Mutex parks in the kernel with no spin; FxHash clusters on integer and pointer keys; a startup stall on 2026-11-01 was workers spinning with nothing recording why.",
	},
	{
		id: "fix-through-blaze-lints",
		info: "A change that moves a type, path, API or idiom across the code is a ban with a `replacement` in `.clippy.toml` (mirrored in the hot crates' `clippy.toml`), which blaze-lints' `disallowed_types`/`disallowed_methods`/`disallowed_macros` enforce with a MachineApplicable fix, or a `blaze::` lint where the rule needs types or MIR; either is applied with `blaze check --fix`, and its count lives in `crates/blaze-lints/baselines/`, which may only shrink; a one-off move of a path is `blaze check --replace A=B`. Both find code by name resolution (imports, renames, globs, re-exports, macro output), never by text, and leave it compiling and lint-clean, so the rule keeps holding after the port. `sed`, `rg` replace and syntax-only rewriters (astlog) are for syntax-only shapes, scoped and checked by a build afterward.",
		reason: "Andrew, 2026-11-01: \"make our own version of clippy that is incremental and has suggestions for replace\" and \"recommend doing find replace with blaze lints\". A text or tree-sitter rewrite of `std::sync::Mutex` misses `use std::sync::{Arc, Mutex}`, renames and globs, and can hit an unrelated `Mutex`; blaze-lints already runs per body as front-end queries.",
	},
	{
		id: "sans-io-cores",
		info: "Every core (a scheduler decision, a protocol, a retry or backoff policy, a lease, a maintainer's plan) is a synchronous state machine that reads time only from an injected clock (`advance_clock(now)`), owns no socket, file, thread, timer or sleep, and hands back decisions and deadlines (`take_decisions`, `next_deadline`). Drivers do the IO and stay thin. A test replays a transcript with no real clock.",
		reason: "Andrew's rule for ix (2026-10-25), and `blaze_graph::sched` is built this way. A core that sleeps or reads wall time can only be tested by waiting, and those tests flake.",
	},
	{
		id: "every-wait-says-what",
		info: "A thread or fiber that blocks says what it waits for: the wait opens a span naming the awaited key, lock, channel, IO or child, and its owner when known. A wait with no name, and time on a worker covered by no span, are bugs; the profile test fails on them.",
		reason: "2026-11-01: 18 codegen workers sat 77 s each in build 24 with no span saying why; only a bisect found part D's apply phase. 19,342 wait spans in that build carried no target because the table dropped the field.",
	},
	{
		id: "slow-is-p0-traced",
		info: "Anything slow is a P0 until a span names it, and the trace comes before the speed fix. Every stretch of worker time sits under a `tracing::info_span!(target: \"blaze\", ...)` that names the work: the macro path for a proc-macro expansion, the package, version and target kind for a unit, the query and key, the creator of a scope being waited on. Its own CPU time (`self_cpu_ns`) is filled, so a DuckDB query over `<BLAZE_HOME>/v2/profile/*.spans.parquet` says which item took the time and whether it was busy or waiting. A sampler stack or a guess from a log is a lead. The PR that makes something faster quotes the before and after from those spans.",
		reason: "Andrew, 2026-11-01: \"whenever something is slow p0 make sure it is traced properly\". Build 53 ran 917 s on about 2 of 18 cores and its spans could not say why: no span for proc-macro expansion, `self_cpu_ns` NULL in every row, every build script recorded as `build-script-build`, and waits named `scope 13`. A claim that one derive took minutes came from the sampler, and the spans did not back it.",
	},
	{
		id: "bounded-progress",
		info: "Every loop that waits for progress (a solver, a fixpoint, a readiness probe, a retry) has a bound (fuel, depth or a deadline from the injected clock) and ends in a typed outcome: done, or a failure naming what it was doing, the goal or key, and how far it got. Never an open loop whose only exit is success, and never a spin with no cap.",
		reason: "2026-11-01: two self-builds ran 300 s with workers busy in type inference and the trait solver and no unit finishing; three more stalled spinning in `Flights::get_or_run_named` before any crate started. Each needed a sampler and a person to say what was stuck.",
	},
	{
		id: "errors-name-the-thing",
		info: "An error names the item it is about (the function or instance with its path, the crate, the key, the file and line) and the rule that refused it, in the message itself. \"a template made against 6 references is given 7\" is unfinished until it says which function and which template.",
		reason: "2026-11-01: several self-build errors named only a hash or a count, so each round spent a person's time finding the function before anyone could fix it.",
	},
	{
		id: "panics-fail-one-job",
		info: "A panic inside a job (a query, a codegen unit, a proc-macro evaluation) fails that job's units with a typed error naming the panic and the job; the daemon keeps serving and `--keep-going` keeps building the rest. Formatting an error must not panic: render what needs the database while the database is attached.",
		reason: "2026-11-01: a proc-macro evaluation error panicked while being formatted (its type needed the attached database), and the daemon dropped whole self-builds at 84 crates until it was found.",
	},
	{
		id: "keys-cover-every-input",
		info: "A query key, store key, fold key or template key covers every input its value depends on, including the data a constant points to and the identity of a closure callee. Two different values under one key is a refusal with a typed error, never an overwrite, and the fix is the key, never the check.",
		reason: "2026-11-01: function folding merged bodies whose constants pointed at different data, two closures of one signature shared one instance, and the store refused other bytes under `Key(b75576429138)`. Each was a key missing an input.",
	},
	{
		id: "measure-the-exact-tree",
		info: "A measurement, bisect or checkpoint names the exact tree it ran on (a content digest of the sources and the blaze binary's hash) and keeps that tree; never re-sync or rebuild between measuring and committing. A result you can't tie to a tree is not evidence.",
		reason: "2026-11-01: round 30 was committed from a snapshot taken 10 s after the next sync began, so main got a tree that built 214 crates instead of the measured 334, and a warm-cache theory was built on it before a cold run disproved it.",
	},
	{
		id: "test-the-cause",
		info: "A causal claim (this change caused the regression, this lock causes the convoy, this index makes it fast) is tested by removing the cause and watching the effect go, not by checking that its premises hold. Pin a load-bearing one with a test named for the property.",
		reason: "2026-11-01: part D was blamed for the codegen convoy on timing alone; one run with part D's files reverted took codegen from 1,485 s to 473 s and the over-30 s units from 18 to 0, which is what made it a finding.",
	},
	{
		id: "whole-ports",
		info: "A change to a shared signature or type (a struct's fields, a trait, a lock type) updates every caller in the same batch, so the shared tree compiles at every save point. Do it through `blaze check --fix`, `blaze check --replace` or a scoped rewrite, then build, then hand the batch over. A half-ported tree blocks every lane's run.",
		reason: "2026-11-01: the self-build loop sat blocked on `Maintainer { run }` after its fields changed, on `LifetimeParamData::is_fresh` callers, and on `into_inner().ok()` after the parking_lot port, each a signature changed without its callers.",
	},
	{
		id: "deterministic-output",
		info: "No output (an object, a diagnostic's order, a store key, a profile row's identity) depends on hash-map iteration order, thread timing, addresses or the wall clock. Hashers have fixed seeds; anything iterated into output is sorted or ordered by construction; a test builds twice and compares bytes.",
		reason: "Content-addressed caching and replay need identical bytes from identical inputs; a nondeterministic key shows up as a store refusal or a cache that never hits.",
	},
	{
		id: "persisted-state-round-trips",
		info: "Anything blaze persists (the daemon image, the store, the plane's history) has a test that restores it and checks the result equals a fresh build byte for byte, and a budget for restart to first answer. A restore that silently starts empty is a failure the daemon reports, naming why.",
		reason: "2026-11-01: the self-build home held a 1.8 GB image while the daemon log said \"the database starts empty: there is no daemon image\", and nothing measured whether a restart used it.",
	},
	{
		id: "no-wait-across-thread-locals",
		info: "A fiber can resume on another thread after any wait, so nothing held across a wait may be tied to a thread: no thread-local borrow, lock guard or `Rc`. Mark regions that must not wait with `NoWait`; read thread-locals in `#[inline(never)]` functions that end before any wait.",
		reason: "2026-10-31: a job's carried thread-local was put back on a thread holding another job's (`stdx/src/lend.rs:94`), and a frame's state crashed in `blaze_mem::frame::Frame::inner`, both from state crossing a fiber switch.",
	},
	{
		id: "best-long-term-design",
		info: "Pick the best long-term design, not the least work: own the layer or port rustc's rule properly, and spend the effort on proving it right: real crates' own test suites passing under blaze, and fuzzing of blaze's own invariants.",
		reason: "Agents write most of blaze, so the amount of code a design takes is not what limits it; what stays expensive is proving it right. Andrew, 2026-10-30, deleting the harnesses that compared blaze with rustc or cargo: \"we use other ppl['s] test[s] [of] the other crates ... it over-constrains us\".",
		link: "docs/contributing.md#rules-for-code",
	},
	{
		id: "semantics-in-front-end",
		info: "Language semantics (MIR building and passes, borrow checking, const eval, layout, code-identity hashes) go in the front end's salsa crates in `crates/` (`hir-ty`, `hir-def` and the rest, derived from rust-analyzer but no longer synced with it), in a crate of their own where possible. Codegen, the store, the daemon, cargo integration and the runtime stay outside the front end.",
		link: "docs/contributing.md#where-new-code-goes",
	},
	{
		id: "no-mcp",
		info: "No MCP. Agent-facing interfaces are a Unix socket or HTTP with one versioned schema in deterministic CBOR; the CLI is a client of the same API.",
		reason: "MCP is cringe and outdated.",
		link: "docs/roadmap.md#8-the-agent-api",
	},
	{
		id: "work-in-ix-vms",
		info: "Builds, tests, benchmarks and experiments run in an ix VM, one per lane (`ix new`, `ix shell`, `ix rm`), never on the Mac or AWS. Where an ix VM can't do it, stop and say what is missing.",
		reason: "Andrew, 2026-10-27. It replaced heavy-work-on-dev-host (AWS dev hosts claimed in the host ledger, the Mac for quick checks).",
		link: "docs/contributing.md#working-in-parallel",
	},
	{
		id: "worktree-per-lane",
		info: "One uniquely named worktree per lane with its own `CARGO_TARGET_DIR`; never `git stash`.",
		reason: "The stash is shared by every worktree.",
	},
	{
		id: "land-through-pr",
		info: "Land small and often through a pull request: `python3 .github/scripts/land.py \"what the branch does\"` opens it, turns on auto-merge and waits for the merge gate (`--wait` resumes). Harnesses run after merge in the release train. Never push to main.",
		link: "docs/contributing.md#landing-a-change",
	},
	{
		id: "own-prs-to-merge",
		info: "Any agent may take any open pull request or issue to done (merge main, resolve conflicts, fix its gate, finish a draft) rather than hand it back; tell the owning lane what changed. Close one only when it is wrong, saying why.",
		reason: "Andrew, 2026-10-28: \"u can work on drafts and get them completed, dont care about draft state; u can control and fix issues instead of sending back to owner, u should resolve ideally\", and \"be agentic wrt prs\".",
	},
	{
		id: "clean-up-lane",
		info: "When a lane is done, delete its worktree, target directory, ix VM and merged branch.",
	},
	{
		id: "credit-people",
		info: "A commit or pull request built on someone's suggestion, diagnosis or patch gets a `Co-authored-by: Name <id+login@users.noreply.github.com>` trailer for each of them and says where it came from.",
		link: "docs/contributing.md#credit",
	},
	{
		id: "docs-say-what-they-are",
		info: "Every doc under `docs/` has front matter: `current` (kept true), `plan`, `record` (dated history) or `superseded` (naming its replacement). Trust current docs; read plans and records as of their date. A change that makes a current doc untrue fixes it or marks it superseded in the same change; `python3 tools/docs.py check` holds them to the tree.",
		reason: "Andrew, 2026-10-28: docs should \"be structured or something so can say things are outdated filter those out\". Agents followed AGENTS.md's stale `vendor/rust-analyzer` after the front end moved to crates/.",
		link: "docs/contributing.md#documents",
	},
	{
		id: "scripts-take-typed-input",
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

- Flag any change that can make a program built by blaze behave differently from the same program built by the pinned nightly rustc (\`harness/toolchain\`): layout, ABI, drop order, overflow and panic behavior, const eval, trait and method resolution, MIR passes, borrow checking, linking. Safe path: port rustc's rule and add a test (a crate whose own tests exercise it, or a unit test). The one allowed difference is sandboxing build scripts and proc macros. Code that opts into a blaze extension is exempt: it need not build under rustc, but enabling the extension must not change any program that doesn't use it.
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

/** AGENTS.md: the rules, numbered, each with its link. */
export function agentsMd(): string {
	const items = rules.map((rule: Rule, i) => {
		const n = `${i + 1}. `;
		const indent = " ".repeat(n.length);
		const see = rule.link === undefined ? "" : ` See [${rule.link}](${rule.link}).`;
		return `${n}${wrap(`**${rule.id}**: ${rule.info}${see}`, 100, indent)}`;
	});
	const note =
		"<!-- Generated by `bun tools/agents/rules.ts --write` from tools/agents/rules.ts; edit that file, not this one. -->";
	const review = reviewRules
		.split("\n")
		.map((line) => (line.startsWith("- ") ? wrap(line, 100, "  ") : wrap(line, 100, "")))
		.join("\n");
	return `${note}\n${header.split("\n").map((line) => wrap(line, 100, "")).join("\n")}\n\n${items.join("\n")}\n\n${review}\n`;
}

const agentsPath = `${import.meta.dir}/../../AGENTS.md`;
const [flag, ...rest] = process.argv.slice(2);
if (flag === undefined || rest.length > 0) throw new Error(`tools/agents/rules.ts: one flag, --write or --check, got ${process.argv.slice(2).join(" ")}`);
switch (flag) {
	case "--write":
		await Bun.write(agentsPath, agentsMd());
		break;
	case "--check": {
		if ((await Bun.file(agentsPath).text()) !== agentsMd()) {
			console.error("AGENTS.md is stale: run `bun tools/agents/rules.ts --write` and commit it");
			process.exit(1);
		}
		break;
	}
	default:
		throw new Error(`tools/agents/rules.ts: unknown flag ${flag} (--write or --check)`);
}
